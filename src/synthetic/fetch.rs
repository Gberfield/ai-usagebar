//! Fetch Synthetic usage from `GET https://api.synthetic.new/v2/quotas`.

use std::time::Duration;

use crate::cache::{Cache, acquire_lock_async};
use crate::error::{AppError, Result};
use crate::usage::SyntheticSnapshot;
use crate::vendor::{MAX_BODY_BYTES, read_body_capped};

use super::types::Quotas;

pub const BASE_URL: &str = "https://api.synthetic.new";
const LOCK_TIMEOUT: Duration = Duration::from_secs(15);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

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

pub type FetchOutcome = crate::outcome::Outcome<SyntheticSnapshot>;

pub async fn fetch_snapshot(
    client: &reqwest::Client,
    api_key: &str,
    cache: &Cache,
    ttl: Duration,
) -> Result<FetchOutcome> {
    fetch_snapshot_at(client, api_key, cache, &Endpoints::default(), ttl).await
}

/// Test seam: same fetch against explicit endpoints (mockito).
pub async fn fetch_snapshot_at(
    client: &reqwest::Client,
    api_key: &str,
    cache: &Cache,
    endpoints: &Endpoints,
    ttl: Duration,
) -> Result<FetchOutcome> {
    cache.ensure_dir()?;
    let _lock = acquire_lock_async(&cache.lock_path(), LOCK_TIMEOUT).await?;
    if let Some(bytes) = cache.fresh_payload(ttl)?
        && let Ok(snapshot) = parse_cache(&bytes)
    {
        return Ok(crate::outcome::Outcome::cached(snapshot, cache, false));
    }
    match fetch_live(client, api_key, &endpoints.quotas).await {
        Ok(snapshot) => {
            cache.write_payload(&serde_json::to_vec(
                &serde_json::json!({"snapshot": snapshot_to_json(&snapshot)}),
            )?)?;
            Ok(crate::outcome::Outcome::fresh(snapshot))
        }
        Err(e) if e.is_transient() => crate::outcome::fallback(cache, None, e, parse_cache),
        Err(e) => {
            cache.mark_stale();
            let status = match &e {
                AppError::Http { status, .. } => *status,
                _ => 0,
            };
            // The upstream body is not echoed into the shared error cache:
            // it is arbitrary remote text, redacted like every other vendor.
            let safe = match &e {
                AppError::Http { status, .. } => AppError::Http {
                    status: *status,
                    body: "Synthetic request failed".into(),
                },
                _ => AppError::Other("Synthetic request failed".into()),
            };
            let diag = cache.write_last_error(status, &safe.to_string());
            crate::outcome::fallback(cache, Some(diag), safe, parse_cache)
        }
    }
}

/// Cached windows are required, not optional: a truncated payload must be
/// refetched rather than rendered as an empty plan.
fn parse_cache(bytes: &[u8]) -> Result<SyntheticSnapshot> {
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    let snapshot = value
        .get("snapshot")
        .ok_or_else(|| AppError::Schema("synthetic cache missing snapshot".into()))?;
    let plan = snapshot
        .get("plan")
        .and_then(serde_json::Value::as_str)
        .filter(|p| !p.trim().is_empty())
        .ok_or_else(|| AppError::Schema("synthetic cache missing 'plan'".into()))?;
    let parse_window = |name: &str| -> Result<Option<crate::usage::UsageWindow>> {
        let Some(w) = snapshot.get(name).and_then(|w| w.as_object()) else {
            return Ok(None);
        };
        let pct = w
            .get("pct")
            .and_then(serde_json::Value::as_i64)
            .filter(|p| (0..=100).contains(p));
        let secs = w
            .get("window_secs")
            .and_then(serde_json::Value::as_i64)
            .filter(|s| *s > 0);
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
                    .map_err(|e| AppError::Schema(format!("synthetic cache '{name}' reset: {e}")))?
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
    let credits = match snapshot.get("credits") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Object(o)) => {
            let remaining = o
                .get("remaining")
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.trim().is_empty());
            let max = o
                .get("max")
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.trim().is_empty());
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
        Some(_) => {
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

fn snapshot_to_json(snap: &SyntheticSnapshot) -> serde_json::Value {
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

async fn fetch_live(client: &reqwest::Client, key: &str, url: &str) -> Result<SyntheticSnapshot> {
    let response = tokio::time::timeout(
        HTTP_TIMEOUT,
        client
            .get(url)
            .bearer_auth(key)
            .header("Accept", "application/json")
            .send(),
    )
    .await
    .map_err(|_| AppError::Transport("synthetic request timed out".into()))??;
    let status = response.status();
    let body = read_body_capped(response, MAX_BODY_BYTES).await?;
    if !status.is_success() {
        return Err(AppError::Http {
            status: status.as_u16(),
            body: "Synthetic request failed".into(),
        });
    }
    Quotas::from_slice(&body)?.into_snapshot()
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
        let error = parse_cache(
            br#"{"snapshot": {"plan": "Synthetic Pro", "session": {"pct": 42}}}"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("incomplete"), "{error}");
    }

    #[test]
    fn cache_roundtrip_preserves_the_snapshot() {
        let snap = Quotas::from_slice(FULL_PAYLOAD.as_bytes())
            .unwrap()
            .into_snapshot()
            .unwrap();
        let bytes = serde_json::to_vec(&serde_json::json!({"snapshot": snapshot_to_json(&snap)}))
            .unwrap();
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
        let endpoints = Endpoints {
            quotas: format!("{}/v2/quotas", server.url()),
        };
        let out = fetch_snapshot_at(
            &reqwest::Client::new(),
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
        let bytes = serde_json::to_vec(&serde_json::json!({
            "snapshot": snapshot_to_json(&seed)
        }))
        .unwrap();
        cache.write_payload(&bytes).unwrap();

        let endpoints = Endpoints {
            quotas: format!("{}/v2/quotas", server.url()),
        };
        let out = fetch_snapshot_at(
            &reqwest::Client::new(),
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

    /// An auth-error envelope must never overwrite a good cache with an empty
    /// plan: it is a schema error here, and with no cache to fall back on the
    /// fetch fails loudly instead of rendering "Synthetic".
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
        let endpoints = Endpoints {
            quotas: format!("{}/v2/quotas", server.url()),
        };
        let err = fetch_snapshot_at(
            &reqwest::Client::new(),
            "sk-test",
            &cache,
            &endpoints,
            Duration::from_secs(0),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("no recognizable quota lanes")
                || err.to_string().contains("Synthetic"),
            "{err}"
        );
    }
}
