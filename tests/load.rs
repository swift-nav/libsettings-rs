use sbp_settings::{settings, Setting, SettingKind};

#[test]
fn test_load() {
    settings::load(vec![Setting {
        name: "aname".into(),
        group: "agroup".into(),
        kind: SettingKind::String,
        expert: false,
        readonly: false,
        description: None,
        default_value: None,
        notes: None,
        units: None,
        enumerated_possible_values: None,
        digits: None,
    }])
    .unwrap();
    assert!(Setting::find("solution", "soln_freq").is_none());
    assert!(Setting::find("agroup", "aname").is_some());
}
