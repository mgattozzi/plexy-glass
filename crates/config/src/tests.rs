use super::*;

#[test]
fn built_in_default_parses_back_to_same_shape() {
    let original = built_in_default();
    let serialized = toml::to_string(&original).expect("serialize");
    let round: Config = toml::from_str(&serialized).expect("parse back");
    assert_eq!(round.status.position, original.status.position);
    assert_eq!(round.status.left.len(), original.status.left.len());
    assert_eq!(round.status.right.len(), original.status.right.len());
    assert!(round.palette.entries.contains_key("accent"));
}

#[test]
fn built_in_default_has_expected_shape() {
    let cfg = built_in_default();
    assert_eq!(cfg.status.position, Position::Bottom);
    assert!(!cfg.status.left.is_empty());
    assert!(!cfg.status.right.is_empty());
    assert!(!cfg.status.middle.is_empty());
}

#[test]
fn load_from_path_with_minimal_toml() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        r##"
[palette]
fg = "#ffffff"
bg = "#000000"

[status]
refresh = "10s"

[[status.right]]
type = "text"
value = "hello"
"##,
    )
    .unwrap();
    let cfg = crate::load::load_from_path(&path).expect("parse");
    assert_eq!(cfg.palette.entries.get("fg").map(String::as_str), Some("#ffffff"));
    assert_eq!(cfg.status.refresh.as_secs(), 10);
    assert_eq!(cfg.status.right.len(), 1);
    match &cfg.status.right[0] {
        WidgetSpec::Text { value, .. } => assert_eq!(value, "hello"),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn invalid_widget_type_is_a_parse_error() {
    let toml_src = r#"
[[status.left]]
type = "not_a_widget"
"#;
    let result: Result<Config, _> = toml::from_str(toml_src);
    assert!(result.is_err());
}

#[test]
fn kanagawa_dragon_palette_has_expected_keys() {
    let p = kanagawa_dragon_palette();
    for key in &["bg", "bg_bar", "fg", "accent", "alert", "muted"] {
        assert!(p.entries.contains_key(*key), "missing palette key: {key}");
    }
}

#[test]
fn load_from_nonexistent_path_returns_error() {
    let result = crate::load::load_from_path(std::path::Path::new("/nonexistent/x.toml"));
    assert!(result.is_err());
}

#[test]
fn load_or_default_returns_a_config() {
    // Smoke test: must always succeed (never panic).
    let (_cfg, _err) = load_or_default();
}

#[test]
fn built_in_keymap_has_prefix_bindings() {
    let km = built_in_keymap();
    assert_eq!(km.prefix, "Ctrl+a");
    assert!(km.inherit_defaults);
    assert!(
        km.bindings
            .iter()
            .any(|b| b.keys == "Ctrl+a c" && b.command == "new_window")
    );
    assert!(
        km.bindings
            .iter()
            .any(|b| b.keys == "Alt+Right" && b.command == "select_pane_right")
    );
}

#[test]
fn built_in_keymap_includes_enter_copy_mode() {
    let km = built_in_keymap();
    assert!(km
        .bindings
        .iter()
        .any(|b| b.keys == "Ctrl+a [" && b.command == "enter_copy_mode"));
}

#[test]
fn built_in_keymap_includes_toggle_sync_panes() {
    let km = built_in_keymap();
    assert!(km
        .bindings
        .iter()
        .any(|b| b.keys == "Ctrl+a y" && b.command == "toggle_sync_panes"));
}

#[test]
fn built_in_keymap_includes_reload_config() {
    let km = built_in_keymap();
    assert!(km
        .bindings
        .iter()
        .any(|b| b.keys == "Ctrl+a R" && b.command == "reload_config"));
}

#[test]
fn built_in_keymap_includes_resize_and_last_bindings() {
    let km = built_in_keymap();
    assert!(km.bindings.iter().any(|b| b.keys == "Ctrl+a L" && b.command == "resize_pane_right"));
    assert!(km.bindings.iter().any(|b| b.keys == "Ctrl+a Tab" && b.command == "select_last_window"));
    assert!(km.bindings.iter().any(|b| b.keys == "Ctrl+a ;" && b.command == "select_last_pane"));
}

#[test]
fn built_in_keymap_includes_overlay_bindings() {
    let km = built_in_keymap();
    assert!(km.bindings.iter().any(|b| b.keys == "Ctrl+a ," && b.command == "rename_window"));
    assert!(km.bindings.iter().any(|b| b.keys == "Ctrl+a ." && b.command == "rename_pane"));
    assert!(km.bindings.iter().any(|b| b.keys == "Ctrl+a ?" && b.command == "show_help"));
}
