//! Fetch the ClinePass caps and the usage ledger they are measured against.
//!
//! Cline has no aggregate usage endpoint (see [`super::types`]), so a refresh
//! is a plan call plus a walk down the charge ledger. Two things keep that
//! from being expensive:
//!
//! - the ledger is stored in the cache and the walk stops as soon as it meets
//!   a charge already stored, so a steady-state refresh reads exactly one
//!   page whatever the length of the 30-day history behind it;
//! - the walk is bounded by [`MAX_PAGES`], and a walk that ends on that bound
//!   marks the snapshot `truncated` rather than quietly under-reporting.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::cache::{Cache, acquire_lock_async};
use crate::error::{AUTH_FAILURE_MESSAGE, AppError, Result};
use crate::vendor::{MAX_BODY_BYTES, read_body_capped};

use super::creds::Credential;
use super::types::{
    Caps, Entry, Plan, Snapshot, merge, parse_plan, parse_usage_page, windows_from,
};

pub const BASE_URL: &str = "https://api.cline.bot";

const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const LOCK_TIMEOUT: Duration = Duration::from_secs(15);
const SCHEMA_ERROR: &str = "Cline response schema mismatch";

/// Rows per ledger page. 200 is the server's own ceiling — asking for more
/// returns 200 anyway.
const PAGE_LIMIT: usize = 200;

/// Ledger pages one refresh will walk. Only a first run on a busy account
/// reaches this; every later refresh stops at the stored ledger after one.
const MAX_PAGES: usize = 30;

#[derive(Debug, Clone)]
pub struct Endpoints {
    pub base: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            base: BASE_URL.to_string(),
        }
    }
}

impl Endpoints {
    fn me(&self) -> String {
        format!("{}/api/v1/users/me", self.base)
    }

    fn plan(&self) -> String {
        format!("{}/api/v1/users/me/plan", self.base)
    }

    /// The ledger is walked newest-first. The page cursor is *named*
    /// `nextToken` in the response and *read* as `cursor` in the request —
    /// sending it back under its own name is silently ignored, and the walk
    /// then re-reads page one until it runs out of budget.
    fn usages(&self, user_id: &str, cursor: Option<&str>) -> String {
        let user = urlencode(user_id);
        let mut url = format!(
            "{}/api/v1/users/{user}/usages?limit={PAGE_LIMIT}",
            self.base
        );
        if let Some(cursor) = cursor {
            let _ = write!(url, "&cursor={}", urlencode(cursor));
        }
        url
    }
}

/// This vendor's [`Outcome`](crate::outcome::Outcome).
pub type FetchOutcome = crate::outcome::Outcome<Snapshot>;

/// What the cache holds between refreshes.
///
/// `complete_from` is the earliest instant the ledger is known to be complete
/// from — `None` meaning "back to the first charge on the account". A first
/// walk that runs out of page budget records where it stopped rather than a
/// sticky "truncated" bit, so the snapshot stops flagging a short history by
/// itself once the 30-day window has rolled past that point.
#[derive(Debug, Clone, Default)]
struct State {
    plan: Plan,
    ledger: Vec<Entry>,
    complete_from: Option<DateTime<Utc>>,
}

impl State {
    fn snapshot(&self, now: DateTime<Utc>) -> Snapshot {
        let [five_hour, seven_day, thirty_day] = windows_from(&self.ledger, &self.plan.caps, now);
        Snapshot {
            plan: self.plan.name.clone(),
            five_hour,
            seven_day,
            thirty_day,
            period_end: self.plan.period_end,
            truncated: self
                .complete_from
                .is_some_and(|from| from > now - super::types::horizon()),
        }
    }
}

pub async fn fetch_snapshot(
    client: &reqwest::Client,
    credential: &Credential,
    cache: &Cache,
    endpoints: &Endpoints,
    ttl: Duration,
    now: DateTime<Utc>,
) -> Result<FetchOutcome> {
    cache.ensure_dir()?;
    let _lock = acquire_lock_async(&cache.lock_path(), LOCK_TIMEOUT).await?;

    let user_id = match credential.account_id.clone() {
        Some(id) => id,
        None if credential.expired => {
            return Err(AppError::Credentials(super::creds::EXPIRED.to_string()));
        }
        None => fetch_user_id(client, endpoints, &credential.token).await?,
    };
    let target = target_key(endpoints, &user_id);

    // A lapsed token would only earn a 401. The stored ledger still answers
    // the question the bar asks — and answers it *correctly*, because the
    // windows are re-folded at the current instant, so an idle night drains
    // the 5-hour window on screen exactly as it drains at Cline.
    if credential.expired {
        cache.mark_stale();
        let recorded = cache.write_last_error(401, super::creds::EXPIRED);
        return fallback_or_error(
            cache,
            Some(recorded),
            &target,
            AppError::Credentials(super::creds::EXPIRED.to_string()),
            now,
        );
    }
    let stored = cache
        .maybe_payload()?
        .and_then(|bytes| parse_cache(&bytes, &target).ok());

    // The cached ledger is re-folded at the current instant, so even a
    // within-TTL read reports windows that have moved on since it was written.
    if let Some(state) = &stored
        && cache.fresh_payload(ttl)?.is_some()
    {
        return Ok(crate::outcome::Outcome::cached(
            state.snapshot(now),
            cache,
            false,
        ));
    }

    match fetch_live(
        client,
        endpoints,
        &credential.token,
        &user_id,
        stored.as_ref(),
        now,
    )
    .await
    {
        Ok(state) => {
            cache.write_payload(&serde_json::to_vec(&state_repr(&target, &state))?)?;
            Ok(crate::outcome::Outcome::fresh(state.snapshot(now)))
        }
        Err(error @ AppError::Transport(_)) => fallback_or_error(cache, None, &target, error, now),
        Err(AppError::Http { status, .. }) => {
            let message = status_message(status).to_string();
            cache.mark_stale();
            cache.write_last_error(status, &message);
            fallback_or_error(
                cache,
                Some((status, message.clone())),
                &target,
                AppError::Http {
                    status,
                    body: message,
                },
                now,
            )
        }
        Err(AppError::Schema(_)) => {
            let message = SCHEMA_ERROR.to_string();
            cache.mark_stale();
            cache.write_last_error(0, &message);
            fallback_or_error(
                cache,
                Some((0, message.clone())),
                &target,
                AppError::Schema(message),
                now,
            )
        }
        Err(error) => fallback_or_error(cache, None, &target, error, now),
    }
}

async fn fetch_live(
    client: &reqwest::Client,
    endpoints: &Endpoints,
    token: &str,
    user_id: &str,
    stored: Option<&State>,
    now: DateTime<Utc>,
) -> Result<State> {
    let plan = parse_plan(&get_json(client, &endpoints.plan(), token).await?)
        .map_err(|_| AppError::Schema(SCHEMA_ERROR.to_string()))?;

    // A plan with no ClinePass caps has no windows to fill, so the ledger is
    // not worth walking: nothing would be measured against it.
    if plan.caps.is_empty() {
        return Ok(State {
            plan,
            ledger: Vec::new(),
            complete_from: None,
        });
    }

    let known: HashSet<&str> = stored
        .map(|state| state.ledger.iter().map(|entry| entry.id.as_str()).collect())
        .unwrap_or_default();
    let cutoff = now - super::types::horizon();

    let mut fetched: Vec<Entry> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut cursor: Option<String> = None;
    // Where this walk left off, unless it reaches something that proves the
    // ledger complete: the horizon, the end of history, or the stored ledger.
    let mut complete_from = stored.and_then(|state| state.complete_from);

    for _ in 0..MAX_PAGES {
        let url = endpoints.usages(user_id, cursor.as_deref());
        let page = parse_usage_page(&get_json(client, &url, token).await?)
            .map_err(|_| AppError::Schema(SCHEMA_ERROR.to_string()))?;

        let overlaps = page
            .items
            .iter()
            .any(|entry| known.contains(entry.id.as_str()));
        let reached_horizon = page
            .items
            .last()
            .is_some_and(|entry| entry.created_at <= cutoff);
        // A server that stopped honouring the cursor would hand back page one
        // forever, and the walk would spend its whole budget re-reading it.
        let advanced = page.items.iter().any(|entry| !seen.contains(&entry.id));
        seen.extend(page.items.iter().map(|entry| entry.id.clone()));
        if let Some(oldest) = page.items.last() {
            complete_from = Some(oldest.created_at);
        }
        fetched.extend(page.items);

        // Meeting the stored ledger proves the new charges are all in hand;
        // how far back the far end reaches is whatever it reached before.
        if overlaps {
            complete_from = stored.and_then(|state| state.complete_from);
            break;
        }
        if reached_horizon || page.next_token.is_none() {
            complete_from = None;
            break;
        }
        if !advanced {
            break;
        }
        cursor = page.next_token;
    }

    Ok(State {
        ledger: merge(
            stored.map(|state| state.ledger.clone()).unwrap_or_default(),
            fetched,
            now,
        ),
        plan,
        complete_from,
    })
}

/// Resolve the signed-in user id when the credential store did not record it.
async fn fetch_user_id(
    client: &reqwest::Client,
    endpoints: &Endpoints,
    token: &str,
) -> Result<String> {
    let value = get_json(client, &endpoints.me(), token).await?;
    value
        .get("data")
        .unwrap_or(&value)
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| AppError::Schema(SCHEMA_ERROR.to_string()))
}

async fn get_json(client: &reqwest::Client, url: &str, token: &str) -> Result<Value> {
    let response = tokio::time::timeout(
        HTTP_TIMEOUT,
        client
            .get(url)
            .bearer_auth(token)
            .header(reqwest::header::ACCEPT, "application/json")
            .send(),
    )
    .await
    .map_err(|_| AppError::Transport("Cline request timed out".to_string()))??;

    let status = response.status();
    let body = read_body_capped(response, MAX_BODY_BYTES).await?;
    if !status.is_success() {
        return Err(AppError::Http {
            status: status.as_u16(),
            body: status_message(status.as_u16()).to_string(),
        });
    }
    serde_json::from_slice(&body).map_err(|_| AppError::Schema(SCHEMA_ERROR.to_string()))
}

/// Stable, non-secret identity for the endpoint and account. The access token
/// rotates every few hours, so it cannot be what a cache is keyed on; the user
/// id it resolves to is what the cached figures actually belong to.
fn target_key(endpoints: &Endpoints, user_id: &str) -> String {
    let digest = Sha256::digest(user_id.as_bytes());
    let mut fingerprint = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(fingerprint, "{byte:02x}");
    }
    // `v2` marks the ledger schema that records each charge's wallet. A cache
    // written before it summed every wallet against the subscription caps, so
    // it must be re-walked rather than re-read.
    format!("{}|user:{fingerprint}|v2", endpoints.base)
}

fn state_repr(target: &str, state: &State) -> Value {
    serde_json::json!({
        "target": target,
        "complete_from": state.complete_from.map(|at| at.to_rfc3339()),
        "plan": {
            "name": state.plan.name,
            "caps": {
                "five_hour": state.plan.caps.five_hour,
                "seven_day": state.plan.caps.seven_day,
                "thirty_day": state.plan.caps.thirty_day,
            },
            "period_end": state.plan.period_end.map(|at| at.to_rfc3339()),
        },
        // Positional rows: a 30-day ledger is thousands of charges, and field
        // names would be most of the file.
        "ledger": state
            .ledger
            .iter()
            .map(|entry| {
                serde_json::json!([
                    entry.id,
                    entry.created_at.to_rfc3339(),
                    entry.cost,
                    entry.wallet
                ])
            })
            .collect::<Vec<_>>(),
    })
}

fn parse_cache(body: &[u8], target: &str) -> Result<State> {
    let invalid = || AppError::Schema("Cline cache is invalid".to_string());
    let value: Value = serde_json::from_slice(body).map_err(|_| invalid())?;
    if value.get("target").and_then(Value::as_str) != Some(target) {
        return Err(AppError::Schema(
            "Cline cache belongs to a different account".to_string(),
        ));
    }
    let plan = value.get("plan").ok_or_else(invalid)?;
    let caps = plan.get("caps").ok_or_else(invalid)?;
    let cap = |name: &str| caps.get(name).and_then(Value::as_f64);
    let ledger = value
        .get("ledger")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?
        .iter()
        .filter_map(|row| {
            let row = row.as_array()?;
            Some(Entry {
                id: row.first()?.as_str()?.to_string(),
                created_at: row
                    .get(1)?
                    .as_str()?
                    .parse::<DateTime<chrono::FixedOffset>>()
                    .ok()?
                    .with_timezone(&Utc),
                cost: row.get(2)?.as_f64()?,
                wallet: row.get(3)?.as_str()?.to_string(),
            })
        })
        .collect();

    Ok(State {
        plan: Plan {
            name: plan.get("name").and_then(Value::as_str).map(str::to_string),
            caps: Caps {
                five_hour: cap("five_hour"),
                seven_day: cap("seven_day"),
                thirty_day: cap("thirty_day"),
            },
            period_end: plan
                .get("period_end")
                .and_then(Value::as_str)
                .and_then(|at| at.parse::<DateTime<chrono::FixedOffset>>().ok())
                .map(|at| at.with_timezone(&Utc)),
        },
        ledger,
        complete_from: value
            .get("complete_from")
            .and_then(Value::as_str)
            .and_then(|at| at.parse::<DateTime<chrono::FixedOffset>>().ok())
            .map(|at| at.with_timezone(&Utc)),
    })
}

fn status_message(status: u16) -> &'static str {
    match status {
        401 | 403 => AUTH_FAILURE_MESSAGE,
        429 => "Cline request was rate limited",
        500..=599 => "Cline service is temporarily unavailable",
        _ => "Cline request failed",
    }
}

fn fallback_or_error(
    cache: &Cache,
    last_error: Option<(u16, String)>,
    target: &str,
    error: AppError,
    now: DateTime<Utc>,
) -> Result<FetchOutcome> {
    crate::outcome::fallback(cache, last_error, error, |body| {
        parse_cache(body, target).map(|state| state.snapshot(now))
    })
}

fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    const PLAN_BODY: &str = r#"{"data":{"plan":{"displayName":"Cline Pass (Monthly)","entitlements":{"cline_pass":{"enabled":true,"inferenceCapThreshold":{
        "last5HoursUsageCostUSDPerUser": 1000,
        "last7daysUsageCostUSDPerUser": 2500,
        "last30daysUsageCostUSDPerUser": 5000
    }}}},"currentPeriodEnd":"2026-10-01T05:17:51Z"},"success":true}"#;

    fn now() -> DateTime<Utc> {
        "2026-09-09T20:00:00Z".parse().expect("timestamp")
    }

    fn credential() -> Credential {
        Credential {
            token: "workos:test-token".to_string(),
            account_id: Some("usr-1".to_string()),
            source: "test".to_string(),
            expired: false,
        }
    }

    fn usage_body(rows: &[(&str, &str, i64)], next: Option<&str>) -> String {
        let items = rows
            .iter()
            .map(|(id, at, cost)| {
                format!(
                    r#"{{"id":"{id}","createdAt":"{at}","costUsd":{cost},"aiModelTypeName":"cline-pass"}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let next = next
            .map(|token| format!(r#","nextToken":"{token}""#))
            .unwrap_or_default();
        format!(r#"{{"data":{{"items":[{items}]{next}}},"success":true}}"#)
    }

    fn cache_fixture() -> (TempDir, Cache) {
        let dir = TempDir::new().expect("temporary cache directory");
        let cache = Cache::at(dir.path().join("cline"));
        cache.ensure_dir().expect("cache directory");
        (dir, cache)
    }

    #[tokio::test]
    async fn folds_the_ledger_into_the_three_windows() {
        let mut server = mockito::Server::new_async().await;
        let plan = server
            .mock("GET", "/api/v1/users/me/plan")
            .match_header("authorization", "Bearer workos:test-token")
            .match_header("accept", "application/json")
            .with_body(PLAN_BODY)
            .create_async()
            .await;
        let usages = server
            .mock("GET", "/api/v1/users/usr-1/usages")
            .match_query(mockito::Matcher::UrlEncoded("limit".into(), "200".into()))
            .with_body(usage_body(
                &[
                    ("usg-3", "2026-09-09T19:00:00Z", 100),
                    ("usg-2", "2026-09-08T19:00:00Z", 250),
                    ("usg-1", "2026-09-01T19:00:00Z", 500),
                ],
                None,
            ))
            .create_async()
            .await;
        let (_dir, cache) = cache_fixture();

        let output = fetch_snapshot(
            &reqwest::Client::new(),
            &credential(),
            &cache,
            &Endpoints { base: server.url() },
            Duration::from_secs(60),
            now(),
        )
        .await
        .expect("a successful refresh");

        let snapshot = output.snapshot;
        assert_eq!(snapshot.plan.as_deref(), Some("Cline Pass (Monthly)"));
        assert_eq!(snapshot.five_hour.as_ref().expect("5h").used, 100.0);
        assert_eq!(snapshot.five_hour.as_ref().expect("5h").pct(), 10);
        assert_eq!(snapshot.seven_day.as_ref().expect("7d").used, 350.0);
        assert_eq!(snapshot.thirty_day.as_ref().expect("30d").used, 850.0);
        assert!(!snapshot.truncated);
        assert!(!output.stale);
        plan.assert_async().await;
        usages.assert_async().await;
    }

    #[tokio::test]
    async fn a_later_refresh_stops_at_the_stored_ledger() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/v1/users/me/plan")
            .with_body(PLAN_BODY)
            .expect(2)
            .create_async()
            .await;
        let first = server
            .mock("GET", "/api/v1/users/usr-1/usages")
            .match_query(mockito::Matcher::UrlEncoded("limit".into(), "200".into()))
            .with_body(usage_body(
                &[("usg-1", "2026-09-09T18:00:00Z", 100)],
                Some("page-2"),
            ))
            .expect(1)
            .create_async()
            .await;
        // The first walk follows nextToken to the end of the history…
        let deep = server
            .mock("GET", "/api/v1/users/usr-1/usages")
            .match_query(mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded("limit".into(), "200".into()),
                mockito::Matcher::UrlEncoded("cursor".into(), "page-2".into()),
            ]))
            .with_body(usage_body(&[("usg-0", "2026-09-08T18:00:00Z", 50)], None))
            .expect(1)
            .create_async()
            .await;
        let (_dir, cache) = cache_fixture();
        let endpoints = Endpoints { base: server.url() };

        fetch_snapshot(
            &reqwest::Client::new(),
            &credential(),
            &cache,
            &endpoints,
            Duration::ZERO,
            now(),
        )
        .await
        .expect("first refresh");
        first.assert_async().await;
        deep.assert_async().await;

        // …the second meets `usg-1` on page one and stops there, keeping the
        // charge only the deep walk ever saw.
        let second = server
            .mock("GET", "/api/v1/users/usr-1/usages")
            .match_query(mockito::Matcher::UrlEncoded("limit".into(), "200".into()))
            .with_body(usage_body(
                &[
                    ("usg-2", "2026-09-09T19:00:00Z", 400),
                    ("usg-1", "2026-09-09T18:00:00Z", 100),
                ],
                Some("page-2"),
            ))
            .expect(1)
            .create_async()
            .await;

        let output = fetch_snapshot(
            &reqwest::Client::new(),
            &credential(),
            &cache,
            &endpoints,
            Duration::ZERO,
            now(),
        )
        .await
        .expect("second refresh");

        assert_eq!(output.snapshot.five_hour.expect("5h").used, 500.0);
        assert_eq!(output.snapshot.seven_day.expect("7d").used, 550.0);
        second.assert_async().await;
    }

    #[tokio::test]
    async fn a_cursor_the_server_ignores_stops_the_walk_instead_of_burning_the_budget() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/v1/users/me/plan")
            .with_body(PLAN_BODY)
            .create_async()
            .await;
        // Page one, handed back for every cursor, with a nextToken that
        // promises more — the shape a renamed cursor parameter produces.
        let stuck = server
            .mock("GET", "/api/v1/users/usr-1/usages")
            .match_query(mockito::Matcher::Any)
            .with_body(usage_body(
                &[("usg-1", "2026-09-09T19:00:00Z", 100)],
                Some("page-2"),
            ))
            .expect(2)
            .create_async()
            .await;
        let (_dir, cache) = cache_fixture();

        let output = fetch_snapshot(
            &reqwest::Client::new(),
            &credential(),
            &cache,
            &Endpoints { base: server.url() },
            Duration::ZERO,
            now(),
        )
        .await
        .expect("a stalled walk still reports what it read");

        assert_eq!(output.snapshot.five_hour.expect("5h").used, 100.0);
        assert!(output.snapshot.truncated);
        stuck.assert_async().await;
    }

    #[tokio::test]
    async fn a_short_history_stops_being_flagged_once_the_window_rolls_past_it() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/v1/users/me/plan")
            .with_body(PLAN_BODY)
            .create_async()
            .await;
        server
            .mock("GET", "/api/v1/users/usr-1/usages")
            .match_query(mockito::Matcher::Any)
            .with_body(usage_body(
                &[("usg-1", "2026-09-09T19:00:00Z", 100)],
                Some("page-2"),
            ))
            .create_async()
            .await;
        let (_dir, cache) = cache_fixture();
        let endpoints = Endpoints { base: server.url() };

        let fresh = fetch_snapshot(
            &reqwest::Client::new(),
            &credential(),
            &cache,
            &endpoints,
            Duration::from_secs(600),
            now(),
        )
        .await
        .expect("first refresh");
        assert!(fresh.snapshot.truncated);

        // 31 days on, everything the walk never reached is older than the
        // longest window, so the ledger is complete for every window shown.
        let later = fetch_snapshot(
            &reqwest::Client::new(),
            &credential(),
            &cache,
            &endpoints,
            Duration::from_secs(600),
            now() + chrono::Duration::days(31),
        )
        .await
        .expect("cached refresh");
        assert!(!later.snapshot.truncated);
    }

    #[tokio::test]
    async fn a_lapsed_token_serves_the_cached_ledger_aged_to_now() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/v1/users/me/plan")
            .with_body(PLAN_BODY)
            .expect(1)
            .create_async()
            .await;
        server
            .mock("GET", "/api/v1/users/usr-1/usages")
            .match_query(mockito::Matcher::Any)
            .with_body(usage_body(&[("usg-1", "2026-09-09T19:00:00Z", 100)], None))
            .expect(1)
            .create_async()
            .await;
        let (_dir, cache) = cache_fixture();
        let endpoints = Endpoints { base: server.url() };
        fetch_snapshot(
            &reqwest::Client::new(),
            &credential(),
            &cache,
            &endpoints,
            Duration::ZERO,
            now(),
        )
        .await
        .expect("a refresh while the token was live");

        // Two hours later the CLI has not run, so the token has lapsed. No
        // request is made, and the 5-hour window has aged accordingly.
        let lapsed = Credential {
            expired: true,
            ..credential()
        };
        let output = fetch_snapshot(
            &reqwest::Client::new(),
            &lapsed,
            &cache,
            &endpoints,
            Duration::ZERO,
            now() + chrono::Duration::hours(2),
        )
        .await
        .expect("the cached ledger is still worth showing");

        assert!(output.stale);
        assert_eq!(output.snapshot.five_hour.expect("5h").used, 100.0);
        assert_eq!(output.snapshot.seven_day.expect("7d").used, 100.0);
        let (code, message) = output.last_error.expect("a recorded reason");
        assert_eq!(code, 401);
        assert!(message.contains("expired"));
    }

    #[tokio::test]
    async fn a_lapsed_token_with_no_cache_reports_the_sign_in_instead() {
        let (_dir, cache) = cache_fixture();
        let lapsed = Credential {
            expired: true,
            ..credential()
        };

        let error = fetch_snapshot(
            &reqwest::Client::new(),
            &lapsed,
            &cache,
            &Endpoints {
                base: "http://127.0.0.1:1".to_string(),
            },
            Duration::ZERO,
            now(),
        )
        .await
        .expect_err("nothing to show and nothing to fetch");
        assert!(error.to_string().contains("expired"));
    }

    #[tokio::test]
    async fn a_free_model_charge_does_not_move_the_subscription_gauge() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/v1/users/me/plan")
            .with_body(PLAN_BODY)
            .create_async()
            .await;
        // The ledger mixes wallets: the same account's free-model call sits
        // beside the subscription one and is billed to `z-ai`.
        server
            .mock("GET", "/api/v1/users/usr-1/usages")
            .match_query(mockito::Matcher::Any)
            .with_body(
                r#"{"data":{"items":[
                    {"id":"usg-2","createdAt":"2026-09-09T19:30:00Z","costUsd":900,
                     "aiModelTypeName":"z-ai"},
                    {"id":"usg-1","createdAt":"2026-09-09T19:00:00Z","costUsd":100,
                     "aiModelTypeName":"cline-pass"}
                ]}}"#,
            )
            .create_async()
            .await;
        let (_dir, cache) = cache_fixture();

        let output = fetch_snapshot(
            &reqwest::Client::new(),
            &credential(),
            &cache,
            &Endpoints { base: server.url() },
            Duration::ZERO,
            now(),
        )
        .await
        .expect("a successful refresh");

        assert_eq!(output.snapshot.five_hour.expect("5h").used, 100.0);
    }

    #[tokio::test]
    async fn a_ledger_cached_before_wallets_were_recorded_is_rewalked() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/v1/users/me/plan")
            .with_body(PLAN_BODY)
            .create_async()
            .await;
        server
            .mock("GET", "/api/v1/users/usr-1/usages")
            .match_query(mockito::Matcher::Any)
            .with_body(usage_body(&[("usg-1", "2026-09-09T19:00:00Z", 100)], None))
            .expect(1)
            .create_async()
            .await;
        let (_dir, cache) = cache_fixture();
        // A payload in the previous schema: three-column rows, no wallet, and
        // the old target key. Serving it would keep reporting inflated windows.
        cache
            .write_payload(
                br#"{"target":"http://example.invalid|user:deadbeef",
                     "complete_from":null,
                     "plan":{"name":"Cline Pass (Monthly)","caps":{"five_hour":1000.0,
                             "seven_day":2500.0,"thirty_day":5000.0},"period_end":null},
                     "ledger":[["usg-old","2026-09-09T19:45:00Z",900.0]]}"#,
            )
            .expect("seed the old cache");

        let output = fetch_snapshot(
            &reqwest::Client::new(),
            &credential(),
            &cache,
            &Endpoints { base: server.url() },
            Duration::from_secs(600),
            now(),
        )
        .await
        .expect("the stale-schema cache is replaced, not served");

        assert_eq!(output.snapshot.five_hour.expect("5h").used, 100.0);
    }

    #[tokio::test]
    async fn a_plan_without_caps_never_walks_the_ledger() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/v1/users/me/plan")
            .with_body(
                r#"{"data":{"plan":{"displayName":"Cline","entitlements":{}}},"success":true}"#,
            )
            .create_async()
            .await;
        let usages = server
            .mock("GET", "/api/v1/users/usr-1/usages")
            .match_query(mockito::Matcher::Any)
            .expect(0)
            .create_async()
            .await;
        let (_dir, cache) = cache_fixture();

        let output = fetch_snapshot(
            &reqwest::Client::new(),
            &credential(),
            &cache,
            &Endpoints { base: server.url() },
            Duration::ZERO,
            now(),
        )
        .await
        .expect("a plan with no caps is not an error");

        assert!(output.snapshot.worst_pct().is_none());
        usages.assert_async().await;
    }

    #[tokio::test]
    async fn unauthorized_without_cache_returns_a_redacted_error() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/v1/users/me/plan")
            .with_status(401)
            .with_body("workos:test-token should never be echoed")
            .create_async()
            .await;
        let (_dir, cache) = cache_fixture();

        let error = fetch_snapshot(
            &reqwest::Client::new(),
            &credential(),
            &cache,
            &Endpoints { base: server.url() },
            Duration::ZERO,
            now(),
        )
        .await
        .expect_err("401 without a cache must fail");

        let rendered = error.to_string();
        assert!(rendered.contains("401"));
        assert!(!rendered.contains("test-token"));
    }

    #[tokio::test]
    async fn a_cache_written_for_another_account_is_not_served() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/v1/users/me/plan")
            .with_body(PLAN_BODY)
            .create_async()
            .await;
        server
            .mock("GET", "/api/v1/users/usr-1/usages")
            .match_query(mockito::Matcher::Any)
            .with_body(usage_body(&[("usg-1", "2026-09-09T19:00:00Z", 100)], None))
            .create_async()
            .await;
        server
            .mock("GET", "/api/v1/users/usr-2/usages")
            .match_query(mockito::Matcher::Any)
            .with_status(503)
            .create_async()
            .await;
        let (_dir, cache) = cache_fixture();
        let endpoints = Endpoints { base: server.url() };
        fetch_snapshot(
            &reqwest::Client::new(),
            &credential(),
            &cache,
            &endpoints,
            Duration::ZERO,
            now(),
        )
        .await
        .expect("first account refresh");

        let other = Credential {
            account_id: Some("usr-2".to_string()),
            ..credential()
        };
        let error = fetch_snapshot(
            &reqwest::Client::new(),
            &other,
            &cache,
            &endpoints,
            Duration::ZERO,
            now(),
        )
        .await
        .expect_err("another account's cache must not be served");
        assert!(matches!(error, AppError::Http { status: 503, .. }));
    }

    #[tokio::test]
    async fn a_fresh_cache_is_refolded_rather_than_refetched() {
        let mut server = mockito::Server::new_async().await;
        let plan = server
            .mock("GET", "/api/v1/users/me/plan")
            .with_body(PLAN_BODY)
            .expect(1)
            .create_async()
            .await;
        let usages = server
            .mock("GET", "/api/v1/users/usr-1/usages")
            .match_query(mockito::Matcher::Any)
            .with_body(usage_body(&[("usg-1", "2026-09-09T19:00:00Z", 100)], None))
            .expect(1)
            .create_async()
            .await;
        let (_dir, cache) = cache_fixture();
        let endpoints = Endpoints { base: server.url() };

        fetch_snapshot(
            &reqwest::Client::new(),
            &credential(),
            &cache,
            &endpoints,
            Duration::from_secs(600),
            now(),
        )
        .await
        .expect("first refresh");

        // Six hours later the same stored charge has left the 5-hour window,
        // and the cached read says so without touching the network.
        let later = now() + chrono::Duration::hours(6);
        let output = fetch_snapshot(
            &reqwest::Client::new(),
            &credential(),
            &cache,
            &endpoints,
            Duration::from_secs(600),
            later,
        )
        .await
        .expect("cached refresh");

        assert_eq!(output.snapshot.five_hour.expect("5h").used, 0.0);
        assert_eq!(output.snapshot.seven_day.expect("7d").used, 100.0);
        plan.assert_async().await;
        usages.assert_async().await;
    }
}
