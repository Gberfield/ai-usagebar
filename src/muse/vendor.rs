use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::format::{placeholders, substitute, updated_at_hm, window_placeholders};
use crate::pacing::PaceSeverity;
use crate::pango::{color_span, escape, severity_color, severity_for};
use crate::theme::Theme;
use crate::tooltip::{Line, push_window, render_bordered};
use crate::usage::MuseSnapshot;
use crate::vendor::{RenderOpts, VendorId, VendorOutcome};
use crate::waybar::{Class, WaybarOutput};

impl From<super::fetch::FetchOutcome> for VendorOutcome {
    fn from(outcome: super::fetch::FetchOutcome) -> Self {
        outcome.map(crate::usage::VendorSnapshot::MuseCode)
    }
}

pub fn build_placeholders(
    snap: &MuseSnapshot,
    opts: &RenderOpts,
    now: DateTime<Utc>,
) -> HashMap<&'static str, String> {
    let session = window_placeholders(snap.session.as_ref(), opts, now);
    let weekly = window_placeholders(snap.weekly.as_ref(), opts, now);
    placeholders([
        ("plan", snap.plan.clone()),
        ("vendor_short", VendorId::MuseCode.short_name().into()),
        ("session_pct", session.pct.clone()),
        ("session_reset", session.reset.clone()),
        ("weekly_pct", weekly.pct.clone()),
        ("weekly_reset", weekly.reset.clone()),
        ("muse_plan", snap.plan.clone()),
        ("muse_session_pct", session.pct),
        ("muse_session_reset", session.reset),
        ("muse_weekly_pct", weekly.pct),
        ("muse_weekly_reset", weekly.reset),
    ])
}

pub fn severity(snap: &MuseSnapshot) -> PaceSeverity {
    severity_for(
        [snap.session.as_ref(), snap.weekly.as_ref()]
            .into_iter()
            .flatten()
            .map(|window| window.utilization_pct)
            .max()
            .unwrap_or(0),
    )
}

pub fn render(
    outcome: &VendorOutcome,
    snap: &MuseSnapshot,
    theme: &Theme,
    opts: &RenderOpts,
    now: DateTime<Utc>,
) -> WaybarOutput {
    let mut values = build_placeholders(snap, opts, now);
    for value in values.values_mut() {
        *value = escape(value);
    }
    let default = match (snap.session.is_some(), snap.weekly.is_some()) {
        (true, true) => "{muse_session_pct}% · {muse_weekly_pct}%w",
        (true, false) => "{muse_session_pct}% · {muse_session_reset}",
        (false, true) => "{muse_weekly_pct}%w · {muse_weekly_reset}",
        _ => "{plan}",
    };
    let mut text = substitute(opts.format.as_deref().unwrap_or(default), &values);
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
            let mut lines = vec![Line::Center(escape(&snap.plan)), Line::Sep];
            if let Some(session) = &snap.session {
                push_window(&mut lines, "Session", session, theme, now, None);
            }
            if let Some(weekly) = &snap.weekly {
                push_window(&mut lines, "Weekly", weekly, theme, now, None);
            }
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
        text: color_span(severity_color(severity(snap), theme), &text),
        tooltip,
        class: Class::from(severity(snap)),
    }
}
