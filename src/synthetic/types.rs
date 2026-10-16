use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, Result};
use crate::usage::{SyntheticSnapshot, UsageWindow};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Response {
    pub subscription: Subscription,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Subscription {
    pub limit: f64,
    pub requests: f64,
    pub renews_at: String,
}

impl Response {
    pub fn snapshot(&self) -> Result<SyntheticSnapshot> {
        let sub = &self.subscription;
        if !sub.limit.is_finite()
            || sub.limit <= 0.0
            || !sub.requests.is_finite()
            || sub.requests < 0.0
        {
            return Err(AppError::Schema(
                "Synthetic subscription quota contains an invalid limit or request count".into(),
            ));
        }
        let renews_at = DateTime::parse_from_rfc3339(&sub.renews_at)
            .map_err(|_| AppError::Schema("Synthetic subscription renewal is invalid".into()))?
            .with_timezone(&Utc);
        let utilization_pct = (sub.requests / sub.limit * 100.0).round().clamp(0.0, 100.0) as i32;
        Ok(SyntheticSnapshot {
            subscription: UsageWindow {
                utilization_pct,
                resets_at: Some(renews_at),
                // The API documents the next renewal, not a fixed cycle
                // length. Zero deliberately disables guessed pacing.
                window_duration: chrono::Duration::zero(),
            },
            requests: sub.requests,
            limit: sub.limit,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_example_projects_without_guessing_a_cycle() {
        let response: Response = serde_json::from_str(
            r#"{"subscription":{"limit":135,"requests":27,"renewsAt":"2026-11-01T00:00:00Z"}}"#,
        )
        .unwrap();
        let snapshot = response.snapshot().unwrap();
        assert_eq!(snapshot.subscription.utilization_pct, 20);
        assert_eq!(snapshot.requests, 27.0);
        assert_eq!(snapshot.limit, 135.0);
        assert_eq!(
            snapshot.subscription.window_duration,
            chrono::Duration::zero()
        );
    }

    #[test]
    fn invalid_or_ambiguous_numbers_are_rejected() {
        for raw in [
            r#"{"subscription":{"limit":0,"requests":0,"renewsAt":"2026-11-01T00:00:00Z"}}"#,
            r#"{"subscription":{"limit":135,"requests":-1,"renewsAt":"2026-11-01T00:00:00Z"}}"#,
            r#"{"subscription":{"limit":135,"requests":1,"renewsAt":"not-a-time"}}"#,
            r#"{"subscription":{"limit":135,"requests":1}}"#,
            r#"{"error":"unauthorized"}"#,
        ] {
            let result = serde_json::from_str::<Response>(raw)
                .map_err(AppError::from)
                .and_then(|response| response.snapshot());
            assert!(result.is_err(), "accepted {raw}");
        }
    }

    #[test]
    fn over_limit_usage_is_bounded_for_frontends() {
        let response: Response = serde_json::from_str(
            r#"{"subscription":{"limit":10,"requests":12,"renewsAt":"2026-11-01T00:00:00Z"}}"#,
        )
        .unwrap();
        assert_eq!(
            response.snapshot().unwrap().subscription.utilization_pct,
            100
        );
    }
}
