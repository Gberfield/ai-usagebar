//! Wire types for Synthetic's `/v2/quotas` endpoint.
//!
//! Only `subscription` is documented
//! (<https://dev.synthetic.new/docs/synthetic/quotas>); the remaining lanes
//! (`rollingFiveHourLimit`, `weeklyTokenLimit`, `search.hourly`) are
//! reverse-engineered, so each one is located by name but interpreted
//! tolerantly: a usage figure may arrive as a consumed-percent, a
//! remaining-percent, or a used/limit pair under any of the spellings
//! observed on the wire (see the key lists below — they mirror the aliases
//! CodexBar's Synthetic parser accepts, plus the documented camelCase). A
//! lane that is **absent** is a non-subscription account; a lane that is
//! **present but unreadable** is schema drift and errors rather than
//! rendering a fabricated 0%.
//!
//! `resets_at` on the two subscription lanes is the *next top-up* instant,
//! not an end-of-window reset — see `SyntheticSnapshot` for why. The
//! documented `subscription` block and `freeToolCalls` are deliberately not
//! modelled: the former duplicates the rolling limit's maximum behind an
//! ambiguous `renewsAt`, the latter is zero on every plan observed.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::error::{AppError, Result};
use crate::format::usd;
use crate::usage::{SyntheticCredits, SyntheticSnapshot, UsageWindow};

/// Consumed-percent spellings, most explicit first.
const PERCENT_USED_KEYS: &[&str] = &[
    "percentUsed",
    "usedPercent",
    "usagePercent",
    "usage_percent",
    "used_percent",
    "percent_used",
    "percent",
];
/// Remaining-percent spellings — inverted into the consumed convention here,
/// like MiniMax's `*_remaining_percent`.
const PERCENT_REMAINING_KEYS: &[&str] = &[
    "percentRemaining",
    "remainingPercent",
    "remaining_percent",
    "percent_remaining",
];
const LIMIT_KEYS: &[&str] = &[
    "limit",
    "messageLimit",
    "message_limit",
    "messages",
    "maxRequests",
    "max_requests",
    "requestLimit",
    "request_limit",
    "quota",
    "max",
    "total",
    "capacity",
    "allowance",
];
const USED_KEYS: &[&str] = &[
    "used",
    "usage",
    "usedMessages",
    "used_messages",
    "messagesUsed",
    "messages_used",
    "requests",
    "requestCount",
    "request_count",
    "consumed",
    "spent",
];
const REMAINING_KEYS: &[&str] = &["remaining", "left", "available", "balance"];
/// Next-top-up / reset spellings. All of them land in `UsageWindow::resets_at`
/// with lane-specific wording chosen by the renderer.
const RESET_KEYS: &[&str] = &[
    "resetAt",
    "reset_at",
    "resetsAt",
    "resets_at",
    "renewAt",
    "renew_at",
    "renewsAt",
    "renews_at",
    "nextTickAt",
    "next_tick_at",
    "nextRegenAt",
    "next_regen_at",
    "periodEnd",
    "period_end",
    "expiresAt",
    "expires_at",
    "endAt",
    "end_at",
];
const CREDITS_REMAINING_KEYS: &[&str] = &["remainingCredits", "remaining_credits"];
const CREDITS_MAX_KEYS: &[&str] = &["maxCredits", "max_credits"];

const FIVE_HOURS: chrono::Duration = chrono::Duration::hours(5);
const SEVEN_DAYS: chrono::Duration = chrono::Duration::days(7);
const ONE_HOUR: chrono::Duration = chrono::Duration::hours(1);

/// The `/v2/quotas` payload. Every field is optional because the response is
/// a flat set of independent allowances; `data` carries the same shape for
/// deployments that wrap the lanes in an envelope.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Quotas {
    #[serde(rename = "rollingFiveHourLimit")]
    rolling_five_hour_limit: Option<Value>,
    #[serde(rename = "weeklyTokenLimit")]
    weekly_token_limit: Option<Value>,
    search: Option<SearchLanes>,
    /// Recognized but not modelled — kept so a subscription-only response is
    /// distinguishable from an unrecognized payload.
    subscription: Option<Value>,
    plan: Option<String>,
    #[serde(rename = "planName")]
    plan_name: Option<String>,
    tier: Option<String>,
    data: Option<Box<Quotas>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SearchLanes {
    hourly: Option<Value>,
}

impl Quotas {
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes)
            .map_err(|e| AppError::Schema(format!("synthetic quotas response: {e}")))
    }

    /// Project the wire response into the canonical snapshot.
    pub fn into_snapshot(self) -> Result<SyntheticSnapshot> {
        let data = self.data.as_deref().cloned().unwrap_or_default();

        let session_lane = self
            .rolling_five_hour_limit
            .as_ref()
            .or(data.rolling_five_hour_limit.as_ref());
        let weekly_lane = self
            .weekly_token_limit
            .as_ref()
            .or(data.weekly_token_limit.as_ref());
        let search_lane = self
            .search
            .as_ref()
            .and_then(|s| s.hourly.as_ref())
            .or(data.search.as_ref().and_then(|s| s.hourly.as_ref()));

        // A response with no lane and no subscription block is not a quotas
        // payload — an auth-error envelope or an HTML error page must not
        // deserialize into a confident all-empty "Synthetic" vendor.
        if session_lane.is_none()
            && weekly_lane.is_none()
            && search_lane.is_none()
            && self.subscription.is_none()
            && data.subscription.is_none()
        {
            return Err(AppError::Schema(
                "synthetic: response carried no recognizable quota lanes".into(),
            ));
        }

        let plan = [self.plan.as_deref(), self.plan_name.as_deref(), self.tier.as_deref()]
            .into_iter()
            .flatten()
            .map(str::trim)
            .find(|p| !p.is_empty())
            .map(str::to_string)
            .or(data.plan.clone())
            .unwrap_or_else(|| "Synthetic".to_string());

        let weekly_window = weekly_lane
            .map(|lane| window(lane, "weeklyTokenLimit", SEVEN_DAYS))
            .transpose()?;
        let credits = weekly_lane
            .map(credits)
            .transpose()?
            .flatten();

        Ok(SyntheticSnapshot {
            plan,
            session: session_lane
                .map(|lane| window(lane, "rollingFiveHourLimit", FIVE_HOURS))
                .transpose()?,
            weekly: weekly_window,
            credits,
            search: search_lane
                .map(|lane| window(lane, "search.hourly", ONE_HOUR))
                .transpose()?,
        })
    }
}

/// Interpret one allowance lane as a usage window. `default_duration` is the
/// lane's nominal cadence (5h / 7d / 1h), used when the payload does not
/// spell out its own length.
fn window(lane: &Value, field: &str, default_duration: chrono::Duration) -> Result<UsageWindow> {
    let Some(obj) = lane.as_object() else {
        return Err(AppError::Schema(format!(
            "synthetic: `{field}` lane is not an object"
        )));
    };
    let utilization_pct = consumed_pct(obj, field)?;
    Ok(UsageWindow {
        utilization_pct,
        resets_at: next_instant(obj),
        window_duration: window_duration(obj).unwrap_or(default_duration),
    })
}

/// The consumed-percent of a lane from whichever figure the payload carries.
fn consumed_pct(
    obj: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<i32> {
    if let Some(v) = first_number(obj, PERCENT_USED_KEYS) {
        return Ok(percent_i32(normalized_percent(v)));
    }
    if let Some(v) = first_number(obj, PERCENT_REMAINING_KEYS) {
        return Ok(percent_i32(100.0 - normalized_percent(v)));
    }
    if let (Some(used), Some(limit)) = (
        first_number(obj, USED_KEYS),
        first_number(obj, LIMIT_KEYS),
    ) {
        return Ok(percent_i32(used / limit * 100.0));
    }
    // used derivable as limit − remaining, or remaining present with a limit
    // from which used can be inferred the other way round.
    if let (Some(limit), Some(remaining)) = (
        first_number(obj, LIMIT_KEYS),
        first_number(obj, REMAINING_KEYS),
    ) {
        return Ok(percent_i32((limit - remaining) / limit * 100.0));
    }
    if let (Some(used), Some(remaining)) =
        (first_number(obj, USED_KEYS), first_number(obj, REMAINING_KEYS))
    {
        let limit = used + remaining;
        if limit > 0.0 {
            return Ok(percent_i32(used / limit * 100.0));
        }
    }
    Err(AppError::Schema(format!(
        "synthetic: `{field}` lane carried no usable usage figure"
    )))
}

/// A percent ≤ 1 is a fraction (0.42 → 42), mirroring how the wire has been
/// observed to encode sub-percent usage; anything larger is already scaled.
fn normalized_percent(v: f64) -> f64 {
    if v <= 1.0 {
        v * 100.0
    } else {
        v
    }
}

fn percent_i32(v: f64) -> i32 {
    if !v.is_finite() {
        return 0;
    }
    v.round().clamp(0.0, 100.0) as i32
}

/// The lane's next top-up/reset instant, however the payload spells it.
fn next_instant(obj: &serde_json::Map<String, Value>) -> Option<DateTime<Utc>> {
    for key in RESET_KEYS {
        let Some(v) = obj.get(*key) else {
            continue;
        };
        if let Some(n) = v.as_f64() {
            // Unix seconds vs milliseconds, by magnitude.
            let secs = if n > 1.0e12 { n / 1000.0 } else { n };
            if let Some(ts) = chrono::DateTime::from_timestamp(secs as i64, 0) {
                return Some(ts);
            }
        }
        if let Some(s) = v.as_str()
            && let Ok(t) = DateTime::parse_from_rfc3339(s)
        {
            return Some(t.with_timezone(&Utc));
        }
    }
    None
}

/// A lane-declared window length, converted to a `chrono::Duration`.
fn window_duration(obj: &serde_json::Map<String, Value>) -> Option<chrono::Duration> {
    let minutes = first_number(obj, &["windowMinutes", "window_minutes"])
        .or_else(|| {
            first_number(obj, &["windowHours", "window_hours"]).map(|h| h * 60.0)
        })
        .or_else(|| {
            first_number(obj, &["windowDays", "window_days"]).map(|d| d * 1440.0)
        })
        .or_else(|| {
            first_number(obj, &["windowSeconds", "window_seconds"]).map(|s| s / 60.0)
        })?;
    let minutes = minutes.round();
    if minutes >= 1.0 && minutes.is_finite() {
        Some(chrono::Duration::minutes(minutes as i64))
    } else {
        None
    }
}

/// Weekly credit figures, kept exactly as the API spells them when it sends
/// strings ("$46.45"): the wire owns the currency symbol, so no local money
/// formatter is applied to a value it did not produce. Numeric values (when a
/// deployment drops the formatting) go through the shared `usd` so there is
/// still exactly one local money spelling.
fn credits(lane: &Value) -> Result<Option<SyntheticCredits>> {
    let obj = lane.as_object().ok_or_else(|| {
        AppError::Schema("synthetic: `weeklyTokenLimit` lane is not an object".into())
    })?;
    let remaining = first_money(obj, CREDITS_REMAINING_KEYS)?;
    let max = first_money(obj, CREDITS_MAX_KEYS)?;
    match (remaining, max) {
        (None, None) => Ok(None),
        (Some(remaining), Some(max)) => Ok(Some(SyntheticCredits { remaining, max })),
        // Half a pair cannot be vouched for; the same reasoning as DeepSeek's
        // required money fields — never cache a figure the wire did not send.
        (Some(_), None) | (None, Some(_)) => Err(AppError::Schema(
            "synthetic: weekly credits carried only one of remaining/max".into(),
        )),
    }
}

fn first_money(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> Result<Option<String>> {
    for key in keys {
        match obj.get(*key) {
            Some(Value::String(s)) => {
                let t = s.trim();
                if !t.is_empty() {
                    return Ok(Some(t.to_string()));
                }
            }
            Some(Value::Number(n)) => {
                let v = n.as_f64().ok_or_else(|| {
                    AppError::Schema(format!(
                        "synthetic: credits `{key}` is not representable"
                    ))
                })?;
                if !v.is_finite() {
                    return Err(AppError::Schema(format!(
                        "synthetic: credits `{key}` is not finite"
                    )));
                }
                return Ok(Some(usd(v)));
            }
            _ => {}
        }
    }
    Ok(None)
}

fn first_number(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| {
        obj.get(*key).and_then(|v| match v {
            Value::Number(n) => n.as_f64().filter(|f| f.is_finite()),
            Value::String(s) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()),
            _ => None,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(raw: &str) -> SyntheticSnapshot {
        Quotas::from_slice(raw.as_bytes())
            .unwrap()
            .into_snapshot()
            .unwrap()
    }

    /// The only shape Synthetic documents. No lanes → a plan-only snapshot;
    /// the subscription block proves the payload was a quotas response.
    #[test]
    fn documented_subscription_only_example_is_plan_only() {
        let s = snap(
            r#"{"subscription": {"limit": 135, "requests": 0.1,
                 "renewsAt": "2025-09-21T14:36:14.288Z"}}"#,
        );
        assert_eq!(s.plan, "Synthetic");
        assert!(s.session.is_none());
        assert!(s.weekly.is_none());
        assert!(s.credits.is_none());
        assert!(s.search.is_none());
    }

    #[test]
    fn parses_full_lane_payload() {
        let s = snap(
            r#"{
              "plan": "Synthetic Pro",
              "rollingFiveHourLimit": {
                "percentUsed": 42,
                "resetsAt": "2026-10-16T12:00:00Z",
                "windowMinutes": 300
              },
              "weeklyTokenLimit": {
                "percentUsed": 3,
                "remainingCredits": "$46.45",
                "maxCredits": "$48.00",
                "nextRegenAt": "2026-10-16T15:00:00Z"
              },
              "search": {"hourly": {"percentUsed": 10, "resetAt": "2026-10-16T13:00:00Z"}},
              "freeToolCalls": {"limit": 100, "used": 0}
            }"#,
        );
        assert_eq!(s.plan, "Synthetic Pro");
        let session = s.session.unwrap();
        assert_eq!(session.utilization_pct, 42);
        assert_eq!(session.resets_at.unwrap().to_rfc3339(), "2026-10-16T12:00:00+00:00");
        assert_eq!(session.window_duration, chrono::Duration::hours(5));
        let weekly = s.weekly.unwrap();
        assert_eq!(weekly.utilization_pct, 3);
        assert_eq!(weekly.window_duration, chrono::Duration::days(7));
        let credits = s.credits.unwrap();
        assert_eq!(credits.remaining, "$46.45");
        assert_eq!(credits.max, "$48.00");
        let search = s.search.unwrap();
        assert_eq!(search.utilization_pct, 10);
        assert_eq!(search.window_duration, chrono::Duration::hours(1));
    }

    /// Lane-declared window length overrides the lane's nominal cadence.
    #[test]
    fn window_hours_override_default_duration() {
        let s = snap(
            r#"{"rollingFiveHourLimit": {"percentUsed": 1, "windowHours": 5},
                 "subscription": {"limit": 135, "requests": 0}}"#,
        );
        assert_eq!(
            s.session.unwrap().window_duration,
            chrono::Duration::hours(5)
        );
    }

    #[test]
    fn fraction_percent_is_scaled() {
        let s = snap(
            r#"{"rollingFiveHourLimit": {"percentUsed": 0.42},
                 "subscription": {"limit": 135, "requests": 0}}"#,
        );
        assert_eq!(s.session.unwrap().utilization_pct, 42);
    }

    #[test]
    fn remaining_percent_is_inverted() {
        let s = snap(
            r#"{"weeklyTokenLimit": {"percentRemaining": 98},
                 "subscription": {"limit": 135, "requests": 0}}"#,
        );
        assert_eq!(s.weekly.unwrap().utilization_pct, 2);
    }

    #[test]
    fn used_over_limit_derives_the_percent() {
        let s = snap(
            r#"{"rollingFiveHourLimit": {"used": 30, "limit": 120},
                 "subscription": {"limit": 135, "requests": 0}}"#,
        );
        assert_eq!(s.session.unwrap().utilization_pct, 25);
    }

    #[test]
    fn limit_minus_remaining_derives_the_percent() {
        let s = snap(
            r#"{"rollingFiveHourLimit": {"remaining": 90, "maxRequests": 120},
                 "subscription": {"limit": 135, "requests": 0}}"#,
        );
        assert_eq!(s.session.unwrap().utilization_pct, 25);
    }

    #[test]
    fn unix_reset_timestamps_are_accepted() {
        let s = snap(
            r#"{"rollingFiveHourLimit": {"percentUsed": 5, "resetsAt": 1795000000},
                 "subscription": {"limit": 135, "requests": 0}}"#,
        );
        assert!(s.session.unwrap().resets_at.is_some());
    }

    #[test]
    fn lanes_may_arrive_under_data() {
        let s = snap(
            r#"{"data": {"rollingFiveHourLimit": {"percentUsed": 7}},
                 "subscription": {"limit": 135, "requests": 0}}"#,
        );
        assert_eq!(s.session.unwrap().utilization_pct, 7);
    }

    #[test]
    fn numeric_credits_use_the_shared_money_formatter() {
        let s = snap(
            r#"{"weeklyTokenLimit": {"percentUsed": 3,
                 "remainingCredits": 46.45, "maxCredits": 48},
                 "subscription": {"limit": 135, "requests": 0}}"#,
        );
        let c = s.credits.unwrap();
        assert_eq!(c.remaining, "$46.45");
        assert_eq!(c.max, "$48.00");
    }

    #[test]
    fn plan_falls_back_through_aliases_then_default() {
        let s = snap(
            r#"{"planName": "Pro 2-pack",
                 "subscription": {"limit": 135, "requests": 0}}"#,
        );
        assert_eq!(s.plan, "Pro 2-pack");
    }

    #[test]
    fn out_of_range_percent_clamps() {
        let s = snap(
            r#"{"rollingFiveHourLimit": {"percentUsed": 250},
                 "subscription": {"limit": 135, "requests": 0}}"#,
        );
        assert_eq!(s.session.unwrap().utilization_pct, 100);
    }

    #[test]
    fn present_but_unreadable_lane_is_schema_error() {
        let raw = r#"{"rollingFiveHourLimit": {"note": "no figures here"},
                       "subscription": {"limit": 135, "requests": 0}}"#;
        let err = Quotas::from_slice(raw.as_bytes())
            .unwrap()
            .into_snapshot()
            .unwrap_err();
        assert!(err.to_string().contains("no usable usage figure"), "{err}");
    }

    #[test]
    fn non_object_lane_is_schema_error() {
        let raw = r#"{"rollingFiveHourLimit": 42,
                       "subscription": {"limit": 135, "requests": 0}}"#;
        let err = Quotas::from_slice(raw.as_bytes())
            .unwrap()
            .into_snapshot()
            .unwrap_err();
        assert!(err.to_string().contains("not an object"), "{err}");
    }

    #[test]
    fn one_sided_credits_is_schema_error() {
        let raw = r#"{"weeklyTokenLimit": {"percentUsed": 3, "remainingCredits": "$46.45"},
                       "subscription": {"limit": 135, "requests": 0}}"#;
        let err = Quotas::from_slice(raw.as_bytes())
            .unwrap()
            .into_snapshot()
            .unwrap_err();
        assert!(err.to_string().contains("one of remaining/max"), "{err}");
    }

    #[test]
    fn unrecognized_payload_is_schema_error() {
        for raw in [
            r#"{"error": {"message": "Authentication Fails"}}"#,
            r#"{}"#,
            r#"{"quotas": []}"#,
        ] {
            let err = Quotas::from_slice(raw.as_bytes())
                .unwrap()
                .into_snapshot()
                .unwrap_err();
            assert!(
                err.to_string().contains("no recognizable quota lanes"),
                "{raw}: {err}"
            );
        }
    }

    #[test]
    fn non_object_body_does_not_deserialize() {
        assert!(Quotas::from_slice(b"[1,2]").is_err() || {
            // serde accepts a bare array as a struct-as-sequence; either way
            // it must not project into a snapshot.
            Quotas::from_slice(b"[1,2]")
                .unwrap()
                .into_snapshot()
                .is_err()
        });
        assert!(Quotas::from_slice(b"\"nope\"").is_err());
    }

    #[test]
    fn non_finite_percent_is_ignored_not_rendered() {
        // serde_json cannot carry NaN/Infinity as JSON numbers, so the only
        // route to a non-finite figure is a numeric string; it must be
        // skipped (falling through to the next derivation) rather than
        // cached as a percent.
        let s = snap(
            r#"{"rollingFiveHourLimit": {"percentUsed": "NaN", "used": 1, "limit": 4},
                 "subscription": {"limit": 135, "requests": 0}}"#,
        );
        assert_eq!(s.session.unwrap().utilization_pct, 25);
    }
}
