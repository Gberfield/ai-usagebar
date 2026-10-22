use std::time::Duration;

use crate::cache::{Cache, acquire_lock_async};
use crate::error::{AppError, Result};
use crate::usage::SyntheticSnapshot;
use crate::vendor::{MAX_BODY_BYTES, read_body_capped};

use super::types::Response;

pub const BASE_URL: &str = "https://api.synthetic.new";
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const LOCK_TIMEOUT: Duration = Duration::from_secs(15);

pub type FetchOutcome = crate::outcome::Outcome<SyntheticSnapshot>;

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

pub async fn fetch_snapshot(
    client: &reqwest::Client,
    api_key: &str,
    cache: &Cache,
    endpoints: &Endpoints,
    cache_ttl: Duration,
) -> Result<FetchOutcome> {
    cache.ensure_dir()?;
    let _lock = acquire_lock_async(&cache.lock_path(), LOCK_TIMEOUT).await?;
    let parse_cache = |bytes: &[u8]| -> Result<SyntheticSnapshot> {
        let response: Response = serde_json::from_slice(bytes)
            .map_err(|_| AppError::Schema("Synthetic quota cache is invalid".into()))?;
        response.snapshot()
    };

    if let Some(bytes) = cache.fresh_payload(cache_ttl)?
        && let Ok(snapshot) = parse_cache(&bytes)
    {
        return Ok(crate::outcome::Outcome::cached(snapshot, cache, false));
    }

    match request(client, api_key, endpoints).await {
        Ok((response, snapshot)) => {
            cache.write_payload(&serde_json::to_vec(&response)?)?;
            Ok(crate::outcome::Outcome::fresh(snapshot))
        }
        Err(error) if error.is_transient() => {
            crate::outcome::fallback(cache, None, error, parse_cache)
        }
        Err(error) => {
            let code = match &error {
                AppError::Http { status, .. } => *status,
                _ => 0,
            };
            cache.mark_stale();
            let diagnostic = cache.write_last_error(code, &error.to_string());
            crate::outcome::fallback(cache, Some(diagnostic), error, parse_cache)
        }
    }
}

async fn request(
    client: &reqwest::Client,
    api_key: &str,
    endpoints: &Endpoints,
) -> Result<(Response, SyntheticSnapshot)> {
    let response = tokio::time::timeout(
        HTTP_TIMEOUT,
        client.get(&endpoints.quotas).bearer_auth(api_key).send(),
    )
    .await
    .map_err(|_| AppError::Transport("Synthetic quota request timed out".into()))??;
    let status = response.status();
    let bytes = read_body_capped(response, MAX_BODY_BYTES).await?;
    if !status.is_success() {
        return Err(AppError::Http {
            status: status.as_u16(),
            body: String::from_utf8_lossy(&bytes).chars().take(200).collect(),
        });
    }
    let response: Response = serde_json::from_slice(&bytes)
        .map_err(|_| AppError::Schema("Synthetic quota response schema mismatch".into()))?;
    let snapshot = response.snapshot()?;
    Ok((response, snapshot))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn cache() -> (TempDir, Cache) {
        let dir = TempDir::new().unwrap();
        let cache = Cache::at(dir.path().join("synthetic"));
        cache.ensure_dir().unwrap();
        (dir, cache)
    }

    #[tokio::test]
    async fn sends_bearer_key_and_parses_documented_shape() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/v2/quotas")
            .match_header("authorization", "Bearer syn-test")
            .with_status(200)
            .with_body(
                r#"{"subscription":{"limit":135,"requests":27,"renewsAt":"2026-11-01T00:00:00Z"}}"#,
            )
            .create_async()
            .await;
        let (_dir, cache) = cache();
        let outcome = fetch_snapshot(
            &reqwest::Client::new(),
            "syn-test",
            &cache,
            &Endpoints {
                quotas: format!("{}/v2/quotas", server.url()),
            },
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(outcome.snapshot.subscription.utilization_pct, 20);
        assert!(!outcome.stale);
    }

    const INVALID_QUOTAS: [&str; 3] = [
        r#"{"subscription":{"limit":0,"requests":0,"renewsAt":"2026-11-01T00:00:00Z"}}"#,
        r#"{"subscription":{"limit":135,"requests":-1,"renewsAt":"2026-11-01T00:00:00Z"}}"#,
        r#"{"subscription":{"limit":135,"requests":27,"renewsAt":"not-a-time"}}"#,
    ];

    #[tokio::test]
    async fn invalid_quota_values_keep_the_last_good_snapshot_stale() {
        for body in INVALID_QUOTAS {
            let mut server = mockito::Server::new_async().await;
            let request = server
                .mock("GET", "/v2/quotas")
                .with_status(200)
                .with_body(body)
                .create_async()
                .await;
            let (_dir, cache) = cache();
            let good = br#"{"subscription":{"limit":135,"requests":27,"renewsAt":"2026-11-01T00:00:00Z"}}"#;
            cache.write_payload(good).unwrap();

            let outcome = fetch_snapshot(
                &reqwest::Client::new(),
                "syn-test",
                &cache,
                &Endpoints {
                    quotas: format!("{}/v2/quotas", server.url()),
                },
                Duration::ZERO,
            )
            .await
            .expect("invalid wire values should fall back to the good cache");

            assert_eq!(outcome.snapshot.subscription.utilization_pct, 20);
            assert!(outcome.stale);
            assert!(!outcome.off_the_wire());
            let (code, message) = outcome.last_error.expect("schema diagnostic");
            assert_eq!(code, 0);
            assert!(message.contains("schema mismatch"));
            assert_eq!(std::fs::read(cache.payload_path()).unwrap(), good);
            assert!(cache.is_stale());
            request.assert_async().await;
        }
    }

    #[tokio::test]
    async fn invalid_quota_values_without_cache_record_the_schema_failure() {
        let mut server = mockito::Server::new_async().await;
        let request = server
            .mock("GET", "/v2/quotas")
            .with_status(200)
            .with_body(INVALID_QUOTAS[0])
            .create_async()
            .await;
        let (_dir, cache) = cache();

        let error = fetch_snapshot(
            &reqwest::Client::new(),
            "syn-test",
            &cache,
            &Endpoints {
                quotas: format!("{}/v2/quotas", server.url()),
            },
            Duration::ZERO,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, AppError::Schema(_)));
        assert!(!cache.payload_path().exists());
        assert!(cache.is_stale());
        assert_eq!(cache.read_last_error(), Some((0, error.to_string())));
        request.assert_async().await;
    }

    #[tokio::test]
    async fn auth_body_is_redacted_when_warm_cache_is_used() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/v2/quotas")
            .with_status(401)
            .with_body("secret@example.test credential=canary")
            .create_async()
            .await;
        let (_dir, cache) = cache();
        cache
            .write_payload(
                br#"{"subscription":{"limit":10,"requests":1,"renewsAt":"2026-11-01T00:00:00Z"}}"#,
            )
            .unwrap();
        let outcome = fetch_snapshot(
            &reqwest::Client::new(),
            "syn-test",
            &cache,
            &Endpoints {
                quotas: format!("{}/v2/quotas", server.url()),
            },
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert!(outcome.stale);
        let (_, message) = outcome.last_error.unwrap();
        assert_eq!(message, crate::error::AUTH_FAILURE_MESSAGE);
        assert!(!message.contains("canary"));
    }
}
