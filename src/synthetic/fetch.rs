//! Fetch Synthetic usage from `/v2/quotas`.

use std::time::Duration;

use crate::cache::{Cache, MAX_STALE, acquire_lock_async};
use crate::error::{AppError, Result};
use crate::usage::SyntheticSnapshot;

use super::types::Quotas;
use serde_json::Value;

pub const BASE_URL: &str = "https://api.synthetic.new";
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const LOCK_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub struct Endpoints {
    pub quotas: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            quotas: format!("{BASE_URL}/v2/quotas"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FetchOutcome {
    pub snapshot: SyntheticSnapshot,
    pub stale: bool,
    pub last_error: Option<(u16, String)>,
    pub cache_age: Option<Duration>,
}

pub async fn fetch_snapshot(
    client: &reqwest::Client,
    api_key: &str,
    cache: &Cache,
    endpoints: &Endpoints,
    cache_ttl: Duration,
) -> Result<FetchOutcome> {
    cache.ensure_dir()?;
    let _lock = acquire_lock_async(&cache.lock_path(), LOCK_TIMEOUT).await?;

    if let Some(bytes) = cache.fresh_payload(cache_ttl)?
        && let Ok(outcome) = reuse_cache(bytes, cache, false)
    {
        return Ok(outcome);
    }
    // Corrupt fresh cache: fall through to live fetch rather than return a
    // fabricated empty plan.

    match fetch_live(client, &endpoints.quotas, api_key).await {
        Ok(snap) => {
            let bytes = serde_json::to_vec(&snap_to_json(&snap))?;
            cache.write_payload(&bytes)?;
            Ok(FetchOutcome {
                snapshot: snap,
                stale: false,
                last_error: None,
                cache_age: Some(Duration::ZERO),
            })
        }
        Err(e) if e.is_transient() => fallback_silent(cache),
        Err(AppError::Http { status, body }) => {
            cache.mark_stale();
            cache.write_last_error(status, &body);
            fallback_with_error(cache, Some((status, body)))
        }
        Err(e) => {
            cache.mark_stale();
            cache.write_last_error(0, &e.to_string());
            fallback_with_error(cache, Some((0, e.to_string())))
        }
    }
}

fn fallback_silent(cache: &Cache) -> Result<FetchOutcome> {
    let Some(bytes) = cache.fallback_payload(MAX_STALE)? else {
        return Err(AppError::Transport(
            "synthetic: no cache and network unreachable".into(),
        ));
    };
    reuse_cache(bytes, cache, true)
}

fn fallback_with_error(cache: &Cache, last_error: Option<(u16, String)>) -> Result<FetchOutcome> {
    let Some(bytes) = cache.fallback_payload(MAX_STALE)? else {
        return Err(AppError::Other("synthetic: no usable cache".into()));
    };
    let mut outcome = reuse_cache(bytes, cache, true)?;
    outcome.last_error = last_error;
    Ok(outcome)
}

fn reuse_cache(bytes: Vec<u8>, cache: &Cache, stale: bool) -> Result<FetchOutcome> {
    let snap = parse_cache(&bytes)?;
    Ok(FetchOutcome {
        snapshot: snap,
        stale,
        last_error: cache.read_last_error(),
        cache_age: cache.payload_age(),
    })
}

/// Cached windows are required, not optional: a truncated payload must be
/// refetched rather than rendered as an empty plan.
fn parse_cache(bytes: &[u8]) -> Result<SyntheticSnapshot> {
    let v: serde_json::Value = serde_json::from_slice(bytes)?;
    let plan = v
        .get("plan")
        .and_then(Value::as_str)
        .filter(|p| !p.trim().is_empty())
        .ok_or_else(|| AppError::Schema("synthetic cache missing 'plan'".into()))?;
    let parse_window = |name: &str| -> Result<Option<crate::usage::UsageWindow>> {
        let Some(w) = v.get(name).and_then(|w| w.as_object()) else {
            return Ok(None);
        };
        let pct = w.get("pct").and_then(Value::as_i64).filter(|p| (0..=100).contains(p));
        let secs = w.get("window_secs").and_then(Value::as_i64).filter(|s| *s > 0);
        let (pct, secs) = match (pct, secs) {
            (Some(pct), Some(secs)) => (pct, secs),
            _ => {
                return Err(AppError::Schema(format!(
                    "synthetic cache window '{name}' is incomplete"
                )))
            }
        };
        let resets_at = match w.get("resets_at") {
            Some(serde_json::Value::Null) | None => None,
            Some(serde_json::Value::String(s)) => Some(
                chrono::DateTime::parse_from_rfc3339(s)
                    .map_err(|e| {
                        AppError::Schema(format!("synthetic cache '{name}' reset: {e}"))
                    })?
                    .with_timezone(&chrono::Utc),
            ),
            Some(_) => {
                return Err(AppError::Schema(format!(
                    "synthetic cache '{name}' has a non-string reset"
                )))
            }
        };
        Ok(Some(crate::usage::UsageWindow {
            utilization_pct: pct as i32,
            resets_at,
            window_duration: chrono::Duration::seconds(secs),
        }))
    };
    let credits = match v.get("credits") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Object(o)) => {
            let remaining = o.get("remaining").and_then(Value::as_str).filter(|s| !s.trim().is_empty());
            let max = o.get("max").and_then(Value::as_str).filter(|s| !s.trim().is_empty());
            match (remaining, max) {
                (Some(remaining), Some(max)) => Some(crate::usage::SyntheticCredits {
                    remaining: remaining.to_string(),
                    max: max.to_string(),
                }),
                _ => {
                    return Err(AppError::Schema(
                        "synthetic cache credits are incomplete".into(),
                    ))
                }
            }
        }
        _ => {
            return Err(AppError::Schema(
                "synthetic cache 'credits' has an unexpected shape".into(),
            ))
        }
    };
    Ok(SyntheticSnapshot {
        plan: plan.to_string(),
        session: parse_window("session")?,
        weekly: parse_window("weekly")?,
        credits,
        search: parse_window("search")?,
    })
}

fn snap_to_json(snap: &SyntheticSnapshot) -> serde_json::Value {
    let window = |w: Option<&crate::usage::UsageWindow>| {
        w.map(|w| {
            serde_json::json!({
                "pct": w.utilization_pct,
                "resets_at": w.resets_at.map(|t| t.to_rfc3339()),
                "window_secs": w.window_duration.num_seconds(),
            })
        })
        .unwrap_or(serde_json::Value::Null)
    };
    serde_json::json!({
        "plan": snap.plan,
        "session": window(snap.session.as_ref()),
        "weekly": window(snap.weekly.as_ref()),
        "search": window(snap.search.as_ref()),
        "credits": snap.credits.as_ref().map(|c| serde_json::json!({
            "remaining": c.remaining,
            "max": c.max,
        })).unwrap_or(serde_json::Value::Null),
    })
}

async fn fetch_live(client: &reqwest::Client, url: &str, api_key: &str) -> Result<SyntheticSnapshot> {
    let resp = tokio::time::timeout(
        HTTP_TIMEOUT,
        client
            .get(url)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Accept", "application/json")
            .send(),
    )
    .await
    .map_err(|_| AppError::Transport(format!("synthetic timeout: {url}")))??;

    let status = resp.status();
    let bytes = crate::vendor::read_body_capped(resp, crate::vendor::MAX_BODY_BYTES).await?;

    if !status.is_success() {
        let body = String::from_utf8_lossy(&bytes).chars().take(200).collect();
        return Err(AppError::Http {
            status: status.as_u16(),
            body,
        });
    }

    Quotas::from_slice(&bytes)?.into_snapshot()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn cache_fixture() -> (TempDir, Cache) {
        let td = TempDir::new().unwrap();
        let cache = Cache::at(td.path().join("synthetic"));
        cache.ensure_dir().unwrap();
        (td, cache)
    }

    const FULL_PAYLOAD: &str = r#"{
        "plan": "Synthetic Pro",
        "rollingFiveHourLimit": {"percentUsed": 42, "resetsAt": "2026-10-16T12:00:00Z"},
        "weeklyTokenLimit": {"percentUsed": 3,
          "remainingCredits": "$46.45", "maxCredits": "$48.00"},
        "search": {"hourly": {"percentUsed": 10}}
    }"#;

    #[test]
    fn incomplete_cached_window_is_rejected_like_a_live_response() {
        let error = parse_cache(br#"{"plan": "Synthetic Pro", "session": {"pct": 42}}"#).unwrap_err();
        assert!(error.to_string().contains("incomplete"), "{error}");
    }

    #[test]
    fn cache_roundtrip_preserves_the_snapshot() {
        let snap = Quotas::from_slice(FULL_PAYLOAD.as_bytes())
            .unwrap()
            .into_snapshot()
            .unwrap();
        let bytes = serde_json::to_vec(&snap_to_json(&snap)).unwrap();
        let restored = parse_cache(&bytes).unwrap();
        assert_eq!(restored, snap);
    }

    #[tokio::test]
    async fn live_200_returns_snapshot() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/v2/quotas")
            .with_status(200)
            .with_body(FULL_PAYLOAD)
            .create_async()
            .await;

        let (_td, cache) = cache_fixture();
        let client = reqwest::Client::new();
        let endpoints = Endpoints {
            quotas: format!("{}/v2/quotas", server.url()),
        };
        let out = fetch_snapshot(
            &client,
            "sk-test",
            &cache,
            &endpoints,
            Duration::from_secs(0),
        )
        .await
        .unwrap();
        assert_eq!(out.snapshot.plan, "Synthetic Pro");
        assert_eq!(out.snapshot.session.unwrap().utilization_pct, 42);
        assert_eq!(out.snapshot.credits.unwrap().remaining, "$46.45");
        assert!(!out.stale);
    }

    #[tokio::test]
    async fn http_401_falls_back_to_cache() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/v2/quotas")
            .with_status(401)
            .with_body(r#"{"error": "invalid api key"}"#)
            .create_async()
            .await;

        let (_td, cache) = cache_fixture();
        let seed = Quotas::from_slice(FULL_PAYLOAD.as_bytes())
            .unwrap()
            .into_snapshot()
            .unwrap();
        cache
            .write_payload(&serde_json::to_vec(&snap_to_json(&seed)).unwrap())
            .unwrap();

        let client = reqwest::Client::new();
        let endpoints = Endpoints {
            quotas: format!("{}/v2/quotas", server.url()),
        };
        let out = fetch_snapshot(
            &client,
            "bad-key",
            &cache,
            &endpoints,
            Duration::from_secs(0),
        )
        .await
        .unwrap();
        assert!(out.stale);
        assert_eq!(out.snapshot.session.unwrap().utilization_pct, 42);
        assert_eq!(out.last_error.as_ref().map(|(c, _)| *c), Some(401));
    }

    /// An auth-error envelope must never overwrite a good cache with an
    /// empty plan: it is a schema error here, and with no cache to fall
    /// back on the fetch fails loudly instead of rendering "Synthetic".
    #[tokio::test]
    async fn unrecognized_200_body_is_schema_error_not_empty_snapshot() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/v2/quotas")
            .with_status(200)
            .with_body(r#"{"error": {"message": "Authentication Fails"}}"#)
            .create_async()
            .await;

        let (_td, cache) = cache_fixture();
        let client = reqwest::Client::new();
        let endpoints = Endpoints {
            quotas: format!("{}/v2/quotas", server.url()),
        };
        let err = fetch_snapshot(
            &client,
            "sk-test",
            &cache,
            &endpoints,
            Duration::from_secs(0),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no usable cache"), "{err}");
    }
}
