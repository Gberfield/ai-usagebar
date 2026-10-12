//! Cline plan/usage response types, and the window arithmetic the API does
//! not do for us.
//!
//! Cline publishes the three ClinePass windows — 5 hours, 7 days, 30 days —
//! as *caps on the plan* (`entitlements.cline_pass.inferenceCapThreshold`),
//! and publishes consumption only as a paginated ledger of individual
//! inference charges (`/usages`). There is no aggregate endpoint: the
//! dashboard sums the ledger itself. So does this module.
//!
//! **On the cost unit.** `costUsd` and the caps are integers in the same
//! undocumented fixed-point unit (Cline's own client code calls the related
//! auto-top-up threshold "microcredits"), and Cline documents neither the
//! scale nor the cap values. Every figure here is therefore a *ratio* of two
//! numbers in that shared unit, which is exact whatever the scale turns out
//! to be. Nothing in this vendor renders a dollar amount, because the only
//! honest way to print one would be to guess the divisor.

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

/// The three ClinePass rolling windows, in the order the plan lists them.
pub const WINDOWS: [(&str, i64); 3] = [
    ("last5HoursUsageCostUSDPerUser", 5 * 3600),
    ("last7daysUsageCostUSDPerUser", 7 * 24 * 3600),
    ("last30daysUsageCostUSDPerUser", 30 * 24 * 3600),
];

/// How far back the ledger is worth keeping: the longest window.
pub fn horizon() -> Duration {
    Duration::seconds(WINDOWS[2].1)
}

/// A plan's inference caps. Every field is optional because a plan without
/// ClinePass entitlements (Cline's pay-as-you-go credits, or a seat on an
/// organisation plan) carries no caps at all.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Caps {
    pub five_hour: Option<f64>,
    pub seven_day: Option<f64>,
    pub thirty_day: Option<f64>,
}

impl Eq for Caps {}

impl Caps {
    pub fn is_empty(&self) -> bool {
        self.five_hour.is_none() && self.seven_day.is_none() && self.thirty_day.is_none()
    }

    fn get(&self, index: usize) -> Option<f64> {
        match index {
            0 => self.five_hour,
            1 => self.seven_day,
            _ => self.thirty_day,
        }
    }
}

/// The subscription as the API describes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// `displayName` — "Cline Pass (Monthly)" and the like. Untrusted text.
    pub name: Option<String>,
    pub caps: Caps,
    /// End of the billing period. Display only: the quota windows below roll
    /// continuously and do not reset with the invoice.
    pub period_end: Option<DateTime<Utc>>,
}

/// One inference charge from the ledger.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub cost: f64,
}

impl Eq for Entry {}

/// One page of the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub items: Vec<Entry>,
    pub next_token: Option<String>,
}

/// Consumption against one cap.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub used: f64,
    pub limit: f64,
    /// Length of the rolling window.
    pub span: Duration,
    /// Timestamp of the oldest charge still inside the window, which is what
    /// dictates when capacity next frees up.
    pub oldest: Option<DateTime<Utc>>,
}

impl Eq for Window {}

impl Window {
    /// Consumed percentage, rounded and clamped for display. Over-cap usage
    /// is reported as 100 rather than as an impossible bar.
    pub fn pct(&self) -> i32 {
        if !(self.limit.is_finite() && self.limit > 0.0) {
            return 0;
        }
        ((self.used / self.limit) * 100.0).round().clamp(0.0, 100.0) as i32
    }

    /// When the window next frees capacity: the oldest charge inside it ages
    /// out one span after it was made. `None` when nothing is inside the
    /// window, which is also when there is nothing to wait for.
    pub fn resets_at(&self) -> Option<DateTime<Utc>> {
        self.oldest.map(|oldest| oldest + self.span)
    }
}

/// What the widget and TUI render for Cline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub plan: Option<String>,
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    pub thirty_day: Option<Window>,
    pub period_end: Option<DateTime<Utc>>,
    /// The ledger walk stopped on its page budget before reaching the far end
    /// of the 30-day window, so the longer windows are lower bounds. Shown,
    /// not hidden: a bar that under-reports without saying so is worse than
    /// one that says "at least".
    pub truncated: bool,
}

impl Snapshot {
    pub fn windows(&self) -> [(&'static str, Option<&Window>); 3] {
        [
            ("5h", self.five_hour.as_ref()),
            ("7d", self.seven_day.as_ref()),
            ("30d", self.thirty_day.as_ref()),
        ]
    }

    /// The most-exhausted window — the single number the bar leads with.
    pub fn worst_pct(&self) -> Option<i32> {
        self.windows()
            .into_iter()
            .filter_map(|(_, window)| window.map(Window::pct))
            .max()
    }
}

/// Parse `/api/v1/users/me/plan`.
pub fn parse_plan(value: &Value) -> Result<Plan, String> {
    let data = envelope(value, "plan")?;
    let plan = data
        .get("plan")
        .ok_or_else(|| "Cline plan response has no plan".to_string())?;

    let caps = plan
        .get("entitlements")
        .and_then(|entitlements| entitlements.get("cline_pass"))
        .filter(|pass| pass.get("enabled").and_then(Value::as_bool) != Some(false))
        .and_then(|pass| pass.get("inferenceCapThreshold"))
        .map(parse_caps)
        .unwrap_or_default();

    Ok(Plan {
        name: plan
            .get("displayName")
            .or_else(|| plan.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string),
        caps,
        period_end: data
            .get("currentPeriodEnd")
            .and_then(Value::as_str)
            .and_then(parse_timestamp),
    })
}

fn parse_caps(thresholds: &Value) -> Caps {
    let cap = |name: &str| {
        thresholds
            .get(name)
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite() && *value > 0.0)
    };
    Caps {
        five_hour: cap(WINDOWS[0].0),
        seven_day: cap(WINDOWS[1].0),
        thirty_day: cap(WINDOWS[2].0),
    }
}

/// Parse one page of `/api/v1/users/{id}/usages`.
///
/// Malformed rows are skipped rather than failing the page: the ledger is a
/// long tail of independent charges, and one row a future version shapes
/// differently must not blank the bar.
pub fn parse_usage_page(value: &Value) -> Result<Page, String> {
    let data = envelope(value, "usage")?;
    let items = data
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| "Cline usage response has no items array".to_string())?;

    Ok(Page {
        items: items.iter().filter_map(parse_entry).collect(),
        next_token: data
            .get("nextToken")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .map(str::to_string),
    })
}

fn parse_entry(value: &Value) -> Option<Entry> {
    let cost = value.get("costUsd").and_then(Value::as_f64)?;
    if !cost.is_finite() || cost < 0.0 {
        return None;
    }
    Some(Entry {
        id: value.get("id").and_then(Value::as_str)?.to_string(),
        created_at: value
            .get("createdAt")
            .and_then(Value::as_str)
            .and_then(parse_timestamp)?,
        cost,
    })
}

/// Unwrap Cline's `{"data": …, "success": true}` envelope.
fn envelope<'a>(value: &'a Value, what: &str) -> Result<&'a Value, String> {
    if value.get("success").and_then(Value::as_bool) == Some(false) {
        return Err(format!("Cline {what} response reports failure"));
    }
    value
        .get("data")
        .or(Some(value))
        .filter(|data| data.is_object())
        .ok_or_else(|| format!("Cline {what} response must be a JSON object"))
}

fn parse_timestamp(text: &str) -> Option<DateTime<Utc>> {
    text.parse::<DateTime<chrono::FixedOffset>>()
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

/// Fold a ledger into the three windows.
///
/// Charges are counted from `now` backwards, so a clock skew that puts a
/// charge slightly in the future still counts against the window it belongs
/// to rather than vanishing.
pub fn windows_from(entries: &[Entry], caps: &Caps, now: DateTime<Utc>) -> [Option<Window>; 3] {
    std::array::from_fn(|index| {
        let limit = caps.get(index)?;
        let span = Duration::seconds(WINDOWS[index].1);
        let start = now - span;
        let inside = entries
            .iter()
            .filter(|entry| entry.created_at > start)
            .collect::<Vec<_>>();
        Some(Window {
            used: inside.iter().map(|entry| entry.cost).sum(),
            limit,
            span,
            oldest: inside.iter().map(|entry| entry.created_at).min(),
        })
    })
}

/// Merge freshly fetched charges into the stored ledger, newest first, and
/// drop everything past the 30-day horizon.
///
/// De-duplication is by charge id: the ledger is re-read from the top on
/// every refresh, so the overlap between a new page and the stored ledger is
/// the norm, not an error.
pub fn merge(stored: Vec<Entry>, fetched: Vec<Entry>, now: DateTime<Utc>) -> Vec<Entry> {
    let cutoff = now - horizon();
    let mut seen = std::collections::HashSet::new();
    let mut merged: Vec<Entry> = fetched
        .into_iter()
        .chain(stored)
        .filter(|entry| entry.created_at > cutoff)
        .filter(|entry| seen.insert(entry.id.clone()))
        .collect();
    merged.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(value: &str) -> DateTime<Utc> {
        value.parse().expect("RFC3339 timestamp")
    }

    fn entry(id: &str, created_at: &str, cost: f64) -> Entry {
        Entry {
            id: id.into(),
            created_at: at(created_at),
            cost,
        }
    }

    #[test]
    fn parses_the_official_plan_fixture() {
        let value: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/cline/plan.json"))
                .expect("fixture JSON must be valid");

        let plan = parse_plan(&value).expect("plan must parse");
        assert_eq!(plan.name.as_deref(), Some("Cline Pass (Monthly)"));
        assert_eq!(plan.caps.five_hour, Some(1_000_000_000.0));
        assert_eq!(plan.caps.seven_day, Some(2_500_000_000.0));
        assert_eq!(plan.caps.thirty_day, Some(5_000_000_000.0));
        assert_eq!(plan.period_end, Some(at("2026-10-01T05:17:51Z")));
    }

    #[test]
    fn parses_the_official_usage_fixture() {
        let value: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/cline/usages.json"))
                .expect("fixture JSON must be valid");

        let page = parse_usage_page(&value).expect("usage page must parse");
        assert_eq!(page.items.len(), 3);
        assert_eq!(page.items[0].cost, 928_612.0);
        assert!(page.next_token.is_some());
    }

    #[test]
    fn a_plan_without_cline_pass_entitlements_has_no_caps() {
        let value = serde_json::json!({
            "data": {"plan": {"displayName": "Cline Seats: Pro (Monthly)", "entitlements": {}}}
        });
        let plan = parse_plan(&value).expect("plan must parse");
        assert!(plan.caps.is_empty());
        assert_eq!(plan.name.as_deref(), Some("Cline Seats: Pro (Monthly)"));
    }

    #[test]
    fn a_disabled_cline_pass_entitlement_is_not_a_cap() {
        let value = serde_json::json!({
            "data": {"plan": {"entitlements": {"cline_pass": {
                "enabled": false,
                "inferenceCapThreshold": {"last5HoursUsageCostUSDPerUser": 1}
            }}}}
        });
        assert!(parse_plan(&value).expect("plan").caps.is_empty());
    }

    #[test]
    fn rejects_failure_envelopes_and_non_objects() {
        for value in [
            serde_json::json!({"success": false, "error": "Unauthorized"}),
            serde_json::json!({"data": "not-an-object"}),
        ] {
            assert!(parse_plan(&value).is_err(), "unexpectedly accepted {value}");
            assert!(
                parse_usage_page(&value).is_err(),
                "unexpectedly accepted {value}"
            );
        }
    }

    #[test]
    fn skips_unusable_ledger_rows_without_dropping_the_page() {
        let value = serde_json::json!({"data": {"items": [
            {"id": "usg-1", "createdAt": "2026-09-09T19:00:00Z", "costUsd": 10},
            {"id": "usg-2", "createdAt": "not-a-date", "costUsd": 10},
            {"createdAt": "2026-09-09T19:00:00Z", "costUsd": 10},
            {"id": "usg-4", "createdAt": "2026-09-09T19:00:00Z", "costUsd": -1},
            {"id": "usg-5", "createdAt": "2026-09-09T18:00:00Z", "costUsd": 5}
        ]}});

        let page = parse_usage_page(&value).expect("page must parse");
        assert_eq!(
            page.items.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            ["usg-1", "usg-5"]
        );
        assert!(page.next_token.is_none());
    }

    #[test]
    fn windows_sum_only_the_charges_inside_them() {
        let now = at("2026-09-09T20:00:00Z");
        let entries = vec![
            entry("a", "2026-09-09T19:00:00Z", 100.0),
            entry("b", "2026-09-09T10:00:00Z", 200.0),
            entry("c", "2026-09-04T10:00:00Z", 400.0),
            entry("d", "2026-08-01T10:00:00Z", 800.0),
        ];
        let caps = Caps {
            five_hour: Some(1000.0),
            seven_day: Some(1000.0),
            thirty_day: Some(1000.0),
        };

        let [five, seven, thirty] = windows_from(&entries, &caps, now);
        assert_eq!(five.expect("5h").used, 100.0);
        assert_eq!(seven.expect("7d").used, 700.0);
        assert_eq!(thirty.expect("30d").used, 700.0);
    }

    #[test]
    fn a_window_resets_when_its_oldest_charge_ages_out() {
        let now = at("2026-09-09T20:00:00Z");
        let entries = vec![
            entry("a", "2026-09-09T19:30:00Z", 1.0),
            entry("b", "2026-09-09T17:00:00Z", 1.0),
        ];
        let caps = Caps {
            five_hour: Some(100.0),
            ..Caps::default()
        };

        let window = windows_from(&entries, &caps, now)[0]
            .clone()
            .expect("5h window");
        assert_eq!(window.resets_at(), Some(at("2026-09-09T22:00:00Z")));
        assert_eq!(window.pct(), 2);
    }

    #[test]
    fn an_empty_window_has_no_reset_and_no_usage() {
        let caps = Caps {
            five_hour: Some(100.0),
            ..Caps::default()
        };
        let window = windows_from(&[], &caps, at("2026-09-09T20:00:00Z"))[0]
            .clone()
            .expect("5h window");
        assert_eq!(window.used, 0.0);
        assert_eq!(window.pct(), 0);
        assert!(window.resets_at().is_none());
    }

    #[test]
    fn a_window_without_a_cap_is_absent_rather_than_zero() {
        let windows = windows_from(
            &[entry("a", "2026-09-09T19:00:00Z", 5.0)],
            &Caps::default(),
            at("2026-09-09T20:00:00Z"),
        );
        assert!(windows.iter().all(Option::is_none));
    }

    #[test]
    fn over_cap_usage_is_reported_as_full_not_beyond() {
        let window = Window {
            used: 250.0,
            limit: 100.0,
            span: Duration::hours(5),
            oldest: None,
        };
        assert_eq!(window.pct(), 100);
    }

    #[test]
    fn merging_deduplicates_by_id_and_drops_the_far_past() {
        let now = at("2026-09-09T20:00:00Z");
        let stored = vec![
            entry("a", "2026-09-09T19:00:00Z", 1.0),
            entry("old", "2026-07-01T10:00:00Z", 9.0),
        ];
        let fetched = vec![
            entry("b", "2026-09-09T19:30:00Z", 2.0),
            entry("a", "2026-09-09T19:00:00Z", 1.0),
        ];

        let merged = merge(stored, fetched, now);
        assert_eq!(
            merged.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            ["b", "a"]
        );
    }
}
