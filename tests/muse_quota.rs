use ai_usagebar::config::Config;
use ai_usagebar::vendor::VendorId;
use ai_usagebar::widget::cli::Cli;
use clap::Parser;

#[test]
fn native_muse_code_is_opt_in_and_does_not_collide_with_legacy_custom_recipe() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    std::fs::write(
        &path,
        r#"
[muse-code]
enabled = true
credentials_path = "/fixture/auth.json"
[[custom]]
id = "muse"
name = "Muse local"
short_name = "mus"
url = "http://127.0.0.1:41837/usage"
allow_http = true
[[custom.texts]]
label = "Calls"
value = "/calls/today"
"#,
    )
    .unwrap();
    let config = Config::load_from(&path).unwrap();
    assert!(config.is_enabled(VendorId::MuseCode));
    assert!(config.custom_by_id("muse").is_some());
    assert!(!Config::default().muse.enabled);
    assert!(!VendorId::MuseCode.auto_detectable());
    assert_eq!(VendorId::MuseCode.slug(), "muse-code");
    assert_eq!(VendorId::MuseCode.short_name(), "msc");
    let cli = Cli::try_parse_from(["ai-usagebar", "--vendor", "muse-code"]).unwrap();
    assert_eq!(cli.vendor.unwrap().to_id(), VendorId::MuseCode);
}

#[test]
fn muse_placeholders_export_elapsed_and_pace() {
    let now = chrono::DateTime::from_timestamp(1_791_014_400, 0).unwrap();
    let snap = ai_usagebar::usage::MuseSnapshot {
        plan: "Pro".into(),
        session: Some(ai_usagebar::usage::UsageWindow {
            utilization_pct: 45,
            resets_at: Some(now + chrono::Duration::hours(2)),
            window_duration: chrono::Duration::hours(5),
        }),
        weekly: Some(ai_usagebar::usage::UsageWindow {
            utilization_pct: 75,
            resets_at: Some(now + chrono::Duration::days(3)),
            window_duration: chrono::Duration::days(7),
        }),
    };
    let opts = ai_usagebar::vendor::RenderOpts {
        format: None,
        tooltip_format: None,
        icon: None,
        pace_tolerance: 5,
        format_pace_color: false,
        tooltip_pace_pts: false,
    };
    let placeholders = ai_usagebar::muse::vendor::build_placeholders(&snap, &opts, now);
    assert_eq!(placeholders["session_pct"], "45");
    assert_eq!(placeholders["weekly_pct"], "75");
    assert_eq!(
        placeholders["session_elapsed"],
        placeholders["muse_session_elapsed"]
    );
    assert_eq!(
        placeholders["weekly_elapsed"],
        placeholders["muse_weekly_elapsed"]
    );
    assert!(!placeholders["session_elapsed"].is_empty());
    assert!(!placeholders["weekly_elapsed"].is_empty());
    assert!(placeholders.contains_key("muse_session_pace"));
    assert!(placeholders.contains_key("muse_session_pace_indicator"));
    assert!(placeholders.contains_key("muse_weekly_pace"));
    assert!(placeholders.contains_key("muse_weekly_pace_indicator"));
}
