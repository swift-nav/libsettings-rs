use sbp_settings::{setting, Setting};

#[test]
fn test_load_from_path() {
    setting::load_from_path("tests/data/settings.yaml").unwrap();
    assert!(Setting::find("solution", "soln_freq").is_none());
    assert!(Setting::find("surveyed_position", "broadcast").is_some());
}
