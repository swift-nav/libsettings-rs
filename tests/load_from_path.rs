use sbp_settings::{settings, Setting};

#[test]
fn test_load_from_path() {
    settings::load_from_path("tests/data/settings.yaml").unwrap();
    assert!(Setting::find("solution", "soln_freq").is_none());
    assert!(Setting::find("surveyed_position", "broadcast").is_some());
}
