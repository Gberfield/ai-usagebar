use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::format::{placeholders, substitute, updated_at_hm, window_placeholders};
use crate::pacing::PaceSeverity;
use crate::pango::{color_span, escape, severity_color, severity_for};
use crate::theme::Theme;
use crate::tooltip::{Line, push_window, render_bordered};
use crate::usage::SyntheticSnapshot;
use crate::vendor::{RenderOpts, VendorId, VendorOutcome};
use crate::waybar::{Class, WaybarOutput};

impl From<super::fetch::FetchOutcome> for VendorOutcome {
    fn from(outcome: super::fetch::FetchOutcome) -> Self {
        outcome.map(crate::usage::VendorSnapshot::Synthetic)
    }
}

pub fn build_placeholders(
    snapshot: &SyntheticSnapshot,
    opts: &RenderOpts,
    now: DateTime<Utc>,
) -> HashMap<&'static str, String> {
    let window = window_placeholders(Some(&snapshot.subscription), opts, now);
    placeholders([
        ("icon", VendorId::Synthetic.bar_icon().into()),
        ("vendor_short", VendorId::Synthetic.short_name().into()),
        ("plan", "Synthetic".into()),
        ("session_pct", window.pct.clone()),
        ("session_reset", window.reset.clone()),
        ("weekly_pct", String::new()),
        ("weekly_reset", String::new()),
        ("synthetic_pct", window.pct),
        ("synthetic_reset", window.reset),
        ("synthetic_requests", format_number(snapshot.requests)),
        ("synthetic_limit", format_number(snapshot.limit)),
    ])
}

fn format_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.2}")
    }
}

pub fn severity(snapshot: &SyntheticSnapshot) -> PaceSeverity {
    severity_for(snapshot.subscription.utilization_pct)
}

pub fn render(
    outcome: &VendorOutcome,
    snapshot: &SyntheticSnapshot,
    theme: &Theme,
    opts: &RenderOpts,
    now: DateTime<Utc>,
) -> WaybarOutput {
    let mut values = build_placeholders(snapshot, opts, now);
    for value in values.values_mut() {
        *value = escape(value);
    }
    let mut text = substitute(
        opts.format.as_deref().unwrap_or("{synthetic_pct}%"),
        &values,
    );
    if let Some(icon) = &opts.icon {
        text = format!("{} {text}", escape(icon));
    }
    if outcome.stale {
        text.push_str(" ⏸");
    }
    let tooltip = opts
        .tooltip_format
        .as_deref()
        .map(|format| substitute(format, &values))
        .unwrap_or_else(|| {
            let mut lines = vec![Line::Center("Synthetic".into()), Line::Sep];
            push_window(
                &mut lines,
                "Subscription",
                &snapshot.subscription,
                theme,
                now,
                None,
            );
            lines.push(Line::Body(format!(
                "Requests {} / {}",
                escape(&format_number(snapshot.requests)),
                escape(&format_number(snapshot.limit))
            )));
            if outcome.stale {
                lines.push(Line::Body(
                    "⏸ Previous quota observation; current allowance is unverified".into(),
                ));
            }
            if let Some((_, error)) = &outcome.last_error {
                lines.push(Line::Body(escape(error)));
            }
            lines.push(Line::Body(format!(
                "Updated {}",
                updated_at_hm(now, outcome.cache_age)
            )));
            render_bordered(&lines, theme)
        });
    WaybarOutput {
        text: color_span(severity_color(severity(snapshot), theme), &text),
        tooltip,
        class: Class::from(severity(snapshot)),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::usage::UsageWindow;

    #[test]
    fn renderer_exports_documented_count_and_percent() {
        let now = DateTime::from_timestamp(1_791_014_400, 0).unwrap();
        let snapshot = SyntheticSnapshot {
            subscription: UsageWindow {
                utilization_pct: 20,
                resets_at: Some(now + chrono::Duration::days(2)),
                window_duration: chrono::Duration::zero(),
            },
            requests: 27.0,
            limit: 135.0,
        };
        let opts = RenderOpts {
            format: None,
            tooltip_format: None,
            icon: None,
            pace_tolerance: 5,
            format_pace_color: false,
            tooltip_pace_pts: false,
        };
        let outcome = VendorOutcome {
            snapshot: crate::usage::VendorSnapshot::Synthetic(snapshot.clone()),
            stale: false,
            last_error: None,
            cache_age: Some(Duration::from_secs(1)),
        };
        let rendered = render(&outcome, &snapshot, &Theme::default(), &opts, now);
        assert!(rendered.text.contains("20%"));
        assert!(rendered.tooltip.contains("Requests 27 / 135"));
    }
}
