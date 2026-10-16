use ai_usagebar::config::Config;
use ai_usagebar::vendor::VendorId;
use ai_usagebar::widget::cli::Cli;
use clap::Parser;

#[test]
fn synthetic_is_opt_in_and_wired_like_the_other_key_vendors() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    std::fs::write(
        &path,
        r#"
[synthetic]
enabled = true
api_key = "test-key"
"#,
    )
    .unwrap();
    let config = Config::load_from(&path).unwrap();
    assert!(config.is_enabled(VendorId::Synthetic));
    assert_eq!(
        Config::default().synthetic.api_key_env,
        "SYNTHETIC_API_KEY"
    );
    assert!(!Config::default().synthetic.enabled);
    assert!(Config::default().synthetic.api_key.is_none());
    assert_eq!(VendorId::Synthetic.slug(), "synthetic");
    assert_eq!(VendorId::Synthetic.short_name(), "syn");
    assert_eq!(VendorId::Synthetic.display_name(), "Synthetic");
    assert_eq!(VendorId::Synthetic.auth_kind(), ai_usagebar::vendor::AuthKind::ApiKey);
    assert_eq!(
        Config::default().api_key_env_for(VendorId::Synthetic),
        "SYNTHETIC_API_KEY"
    );
    let cli = Cli::try_parse_from(["ai-usagebar", "--vendor", "synthetic"]).unwrap();
    assert_eq!(cli.vendor.unwrap().to_id(), VendorId::Synthetic);
}
