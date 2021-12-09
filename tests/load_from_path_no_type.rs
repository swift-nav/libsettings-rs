use sbp_settings::{setting, Setting, SettingKind};

#[test]
fn test_load_from_path_default_type() {
    setting::load_from_path("tests/data/settings-missing-type.yaml").unwrap();
    assert_eq!(
        Setting::find("surveyed_position", "broadcast").map(|s| s.kind),
        Some(SettingKind::String)
    );
}
