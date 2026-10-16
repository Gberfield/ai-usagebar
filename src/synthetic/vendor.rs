//! Synthetic renderer — bar text + bordered Pango tooltip.
//!
//! The two subscription lanes regenerate rather than reset (see
//! `SyntheticSnapshot`), so their tooltip countdowns say "next top-up",
//! not "resets"; the hourly search lane is a real window and keeps the
//! usual "resets" wording.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::countdown;
use crate::format::{placeholders, substitute, updated_at_hm};
use crate::pacing::PaceSeverity;
use crate::pango::{color_span, escape, severity_color, severity_for};
use crate::theme::Theme;
use crate::tooltip::{Line as TooltipLine, render_bordered};
use crate::usage::{SyntheticSnapshot, UsageWindow};
use crate::vendor::{RenderOpts, VendorOutcome};
use crate::waybar::{Class, WaybarOutput};

use super::fetch::FetchOutcome;

pub const DEFAULT_FORMAT: &str = "{syn_session_pct}% · {syn_credits_left}";
const WEEKLY_ONLY_FORMAT: &str = "{syn_weekly_pct}% · {syn_credits_left}";
const NO_CREDITS_FORMAT_SESSION: &str = "{syn_session_pct}%";
const NO_CREDITS_FORMAT_WEEKLY: &str = "{syn_weekly_pct}%";
const NO_WINDOWS_FORMAT: &str = "{syn_plan}";

pub fn build_placeholders(snap: &SyntheticSnapshot, now: DateTime<Utc>) -> HashMap<&'static str, String> {
    let session = lane_placeholders(snap.session.as_ref(), now);
    let weekly = lane_placeholders(snap.weekly.as_ref(), now);
    let search = lane_placeholders(snap.search.as_ref(), now);
    placeholders(vec![
        ("icon", "󰚩".to_string()),
        ("vendor_short", "syn".to_string()),
        // Cross-vendor aliases — same names work across all vendors so a
        // single `--format '{vendor_short} {session_pct}% · {session_reset}'`
        // renders correctly during scroll-cycle. Synthetic has no windows at
        // all on a non-subscription key, so the aliases degrade to the same
        // placeholders DeepSeek (a vendor without windows) uses.
        ("session_pct", session.pct.clone()),
        ("session_reset", session.reset.clone()),
        ("weekly_pct", weekly.pct.clone()),
        ("weekly_reset", weekly.reset.clone()),
        ("plan", snap.plan.clone()),
        ("syn_plan", snap.plan.clone()),
        ("syn_session_pct", session.pct),
        ("syn_session_reset", session.reset),
        ("syn_weekly_pct", weekly.pct),
        ("syn_weekly_reset", weekly.reset),
        ("syn_search_pct", search.pct),
        ("syn_search_reset", search.reset),
        (
            "syn_credits_left",
            snap.credits
                .as_ref()
                .map(|c| c.remaining.clone())
                .unwrap_or_default(),
        ),
        (
            "syn_credits_max",
            snap.credits
                .as_ref()
                .map(|c| c.max.clone())
                .unwrap_or_default(),
        ),
    ])
}

struct LanePlaceholderValues {
    pct: String,
    reset: String,
}

fn lane_placeholders(window: Option<&UsageWindow>, now: DateTime<Utc>) -> LanePlaceholderValues {
    window
        .map(|w| LanePlaceholderValues {
            pct: w.utilization_pct.to_string(),
            reset: countdown::format(w.resets_at, now),
        })
        .unwrap_or(LanePlaceholderValues {
            pct: "0".to_string(),
            reset: "—".to_string(),
        })
}

/// The most useful one-liner depends on what the account reports: the 5h
/// lane plus remaining weekly credits on a subscription, the weekly lane
/// alone if the 5h lane is missing, and just the plan name on a key with no
/// subscription at all.
fn default_format(snap: &SyntheticSnapshot) -> &'static str {
    let has_credits = snap.credits.is_some();
    match (snap.session.is_some(), snap.weekly.is_some()) {
        (true, _) if has_credits => DEFAULT_FORMAT,
        (true, _) => NO_CREDITS_FORMAT_SESSION,
        (false, true) if has_credits => WEEKLY_ONLY_FORMAT,
        (false, true) => NO_CREDITS_FORMAT_WEEKLY,
        (false, false) => NO_WINDOWS_FORMAT,
    }
}

pub fn severity(snap: &SyntheticSnapshot) -> PaceSeverity {
    let max = [
        snap.session.as_ref(),
        snap.weekly.as_ref(),
        snap.search.as_ref(),
    ]
    .into_iter()
    .flatten()
    .map(|w| w.utilization_pct)
    .max()
    .unwrap_or(0);
    severity_for(max)
}

pub fn render(
    outcome: &VendorOutcome,
    snap: &SyntheticSnapshot,
    theme: &Theme,
    opts: &RenderOpts,
    now: DateTime<Utc>,
) -> WaybarOutput {
    let class = Class::from(severity(snap));
    let format = opts
        .format
        .clone()
        .unwrap_or_else(|| default_format(snap).to_string());
    let values = build_placeholders(snap, now);

    let mut text = substitute(&format, &values);
    if outcome.stale {
        text.push_str(" ⏸");
    }

    let wrapper_color = severity_color(severity(snap), theme).to_string();
    let icon_prefix = match opts.icon.as_deref() {
        Some(ic) if !ic.is_empty() => format!("{ic} "),
        _ => String::new(),
    };
    let bar_text = color_span(&wrapper_color, &format!("{icon_prefix}{text}"));

    let tooltip = if let Some(fmt) = opts.tooltip_format.as_deref() {
        substitute(fmt, &values)
    } else {
        render_tooltip(outcome, snap, theme, now)
    };

    WaybarOutput {
        text: bar_text,
        tooltip,
        class,
    }
}

fn render_tooltip(
    outcome: &VendorOutcome,
    snap: &SyntheticSnapshot,
    theme: &Theme,
    now: DateTime<Utc>,
) -> String {
    let blue = &theme.blue;
    let dim = &theme.dim;

    let mut lines: Vec<TooltipLine> = Vec::new();
    lines.push(TooltipLine::Center(format!(
        "<span font_weight='bold' foreground='{blue}'>Synthetic</span>"
    )));
    if snap.plan != "Synthetic" {
        lines.push(TooltipLine::Center(format!(
            "<span foreground='{dim}'>{}</span>",
            escape(&snap.plan)
        )));
    }
    lines.push(TooltipLine::Sep);
    lines.push(TooltipLine::Body("".into()));

    if let Some(session) = snap.session.as_ref() {
        push_lane(
            &mut lines,
            "  󰔟  5h requests",
            session,
            "top-up",
            theme,
            now,
        );
    }
    if let Some(weekly) = snap.weekly.as_ref() {
        if snap.session.is_some() {
            lines.push(TooltipLine::Body("".into()));
        }
        push_lane(
            &mut lines,
            "  󰃰  Weekly credits",
            weekly,
            "top-up",
            theme,
            now,
        );
        if let Some(c) = snap.credits.as_ref() {
            lines.push(TooltipLine::Body(format!(
                " <span foreground='{dim}'>     {} of {} left</span>",
                escape(&c.remaining),
                escape(&c.max)
            )));
        }
    }
    if let Some(search) = snap.search.as_ref() {
        if snap.session.is_some() || snap.weekly.is_some() {
            lines.push(TooltipLine::Body("".into()));
        }
        push_lane(
            &mut lines,
            "  󰊪  Web search (hourly)",
            search,
            "reset",
            theme,
            now,
        );
    }
    if snap.session.is_none() && snap.weekly.is_none() && snap.search.is_none() {
        lines.push(TooltipLine::Body(format!(
            " <span foreground='{dim}'>no usage windows reported</span>"
        )));
    }

    if let Some((code, msg)) = outcome.last_error.as_ref()
        && *code != 0
    {
        let (icon, ecolor) = if *code >= 500 {
            ("󰅚", theme.red.as_str())
        } else {
            ("󰀪", theme.orange.as_str())
        };
        lines.push(TooltipLine::Body("".into()));
        lines.push(TooltipLine::Sep);
        lines.push(TooltipLine::Body(format!(
            " <span foreground='{ecolor}'>  {icon}  HTTP {code}</span>"
        )));
        lines.push(TooltipLine::Body(format!(
            "     <span foreground='{dim}'>{}</span>",
            escape(msg)
        )));
    }

    let updated = updated_at_hm(now, outcome.cache_age);
    lines.push(TooltipLine::Body("".into()));
    lines.push(TooltipLine::Sep);
    lines.push(TooltipLine::Body(format!(
        " <span foreground='{dim}'>  󰅐  Updated {updated}</span>"
    )));

    render_bordered(&lines, theme)
}

/// A local variant of `tooltip::push_window` whose countdown verb matches the
/// lane's semantics: "next top-up" for the regenerating subscription lanes,
/// "resets" for the hourly search window.
fn push_lane(
    lines: &mut Vec<TooltipLine>,
    label: &str,
    w: &UsageWindow,
    verb: &str,
    theme: &Theme,
    now: DateTime<Utc>,
) {
    let color = severity_color(severity_for(w.utilization_pct), theme);
    let bar = crate::pango::progress_bar(w.utilization_pct, color, theme, None);
    let fg = &theme.fg;
    let dim = &theme.dim;
    lines.push(TooltipLine::Body(format!(
        " <span foreground='{fg}'>{label}</span>"
    )));
    lines.push(TooltipLine::Body(format!(
        "   {bar}  <span font_weight='bold' foreground='{color}'>{pct}%</span>",
        pct = w.utilization_pct
    )));
    lines.push(TooltipLine::Body(format!(
        " <span foreground='{dim}'>  ⏱  Next {verb} in {cd}</span>",
        cd = escape(&countdown::format(w.resets_at, now))
    )));
}

impl From<FetchOutcome> for VendorOutcome {
    fn from(o: FetchOutcome) -> Self {
        Self {
            snapshot: crate::usage::VendorSnapshot::Synthetic(o.snapshot),
            stale: o.stale,
            last_error: o.last_error,
            cache_age: o.cache_age,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_snap() -> SyntheticSnapshot {
        SyntheticSnapshot {
            plan: "Synthetic Pro".into(),
            session: Some(UsageWindow {
                utilization_pct: 42,
                resets_at: Some(Utc::now() + chrono::Duration::minutes(12)),
                window_duration: chrono::Duration::hours(5),
            }),
            weekly: Some(UsageWindow {
                utilization_pct: 3,
                resets_at: Some(Utc::now() + chrono::Duration::hours(3)),
                window_duration: chrono::Duration::days(7),
            }),
            credits: Some(crate::usage::SyntheticCredits {
                remaining: "$46.45".into(),
                max: "$48.00".into(),
            }),
            search: Some(UsageWindow {
                utilization_pct: 10,
                resets_at: Some(Utc::now() + chrono::Duration::minutes(40)),
                window_duration: chrono::Duration::hours(1),
            }),
        }
    }

    fn sample_outcome(snap: SyntheticSnapshot) -> VendorOutcome {
        VendorOutcome {
            snapshot: crate::usage::VendorSnapshot::Synthetic(snap),
            stale: false,
            last_error: None,
            cache_age: Some(std::time::Duration::from_secs(10)),
        }
    }

    fn opts() -> RenderOpts {
        RenderOpts {
            format: None,
            tooltip_format: None,
            icon: None,
            pace_tolerance: 5,
            format_pace_color: false,
            tooltip_pace_pts: false,
        }
    }

    #[test]
    fn default_render_shows_session_and_credits() {
        let snap = sample_snap();
        let out = render(
            &sample_outcome(snap.clone()),
            &snap,
            &Theme::default(),
            &opts(),
            Utc::now(),
        );
        assert!(out.text.contains("42%"), "{}", out.text);
        assert!(out.text.contains("$46.45"), "{}", out.text);
    }

    #[test]
    fn weekly_only_account_leads_with_weekly_lane() {
        let mut snap = sample_snap();
        snap.session = None;
        let out = render(
            &sample_outcome(snap.clone()),
            &snap,
            &Theme::default(),
            &opts(),
            Utc::now(),
        );
        assert!(out.text.contains("3%"), "{}", out.text);
        assert!(out.text.contains("$46.45"), "{}", out.text);
        assert!(!out.text.contains("42%"), "{}", out.text);
    }

    #[test]
    fn key_without_subscription_shows_plan_only() {
        let snap = SyntheticSnapshot {
            plan: "Synthetic".into(),
            session: None,
            weekly: None,
            credits: None,
            search: None,
        };
        let out = render(
            &sample_outcome(snap.clone()),
            &snap,
            &Theme::default(),
            &opts(),
            Utc::now(),
        );
        assert!(out.text.contains("Synthetic"), "{}", out.text);
    }

    #[test]
    fn tooltip_names_topups_and_search_reset() {
        let snap = sample_snap();
        let out = render(
            &sample_outcome(snap.clone()),
            &snap,
            &Theme::default(),
            &opts(),
            Utc::now(),
        );
        assert!(out.tooltip.contains("Next top-up in"), "{}", out.tooltip);
        assert!(out.tooltip.contains("Web search"), "{}", out.tooltip);
        // The credits line stays wire-formatted.
        assert!(out.tooltip.contains("$46.45 of $48.00 left"), "{}", out.tooltip);
    }

    #[test]
    fn tooltip_without_windows_says_so() {
        let snap = SyntheticSnapshot {
            plan: "Synthetic".into(),
            session: None,
            weekly: None,
            credits: None,
            search: None,
        };
        let out = render(
            &sample_outcome(snap.clone()),
            &snap,
            &Theme::default(),
            &opts(),
            Utc::now(),
        );
        assert!(out.tooltip.contains("no usage windows reported"), "{}", out.tooltip);
    }

    #[test]
    fn severity_picks_worst_window() {
        let mut snap = sample_snap();
        assert_eq!(severity(&snap), PaceSeverity::Low);
        snap.session = Some(UsageWindow {
            utilization_pct: 95,
            resets_at: None,
            window_duration: chrono::Duration::hours(5),
        });
        assert_eq!(severity(&snap), PaceSeverity::Critical);
    }

    #[test]
    fn stale_appends_pause() {
        let snap = sample_snap();
        let mut outcome = sample_outcome(snap.clone());
        outcome.stale = true;
        let out = render(&outcome, &snap, &Theme::default(), &opts(), Utc::now());
        assert!(out.text.contains("⏸"));
    }

    // The prefix the native surfaces request verbatim — see the `FORMAT`
    // constants in gnome-extension/marker-logic.js and
    // macos/ai-usagebar-menubar.swift.
    #[test]
    fn desktop_format_header_shows_plan() {
        let values = build_placeholders(&sample_snap(), Utc::now());
        let out = substitute(
            "{plan};;{session_pct};;{session_reset};;{weekly_pct};;{weekly_reset}",
            &values,
        );
        let fields: Vec<&str> = out.split(";;").collect();
        assert_eq!(fields[0], "Synthetic Pro");
        assert_eq!(fields[1], "42");
        assert_eq!(fields[3], "3");
    }
}
