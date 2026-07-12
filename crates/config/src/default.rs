use std::time::Duration;

use crate::{
    BlocksConfig, ColorSource, Config, GlyphTier, HintsConfig, KeymapBinding, KeymapConfig,
    MouseConfig, NotificationsConfig, Padding, PaletteConfig, Position, Rgb, StatusConfig,
    StyleConfig, WidgetSpec,
};

/// A palette-role color spec for the built-in status styles. Every default
/// style points at a role name, never a literal, so the built-in stays in sync
/// with the kanagawa-dragon palette.
fn role(name: &str) -> ColorSource {
    ColorSource::Name(name.to_string())
}

pub fn kanagawa_dragon_palette() -> PaletteConfig {
    // Mirrors the upstream tmux-kanagawa "dragon" mapping
    // (themes/kanagawa/dragon.sh + palette.sh):
    //   text       -> old_white       (#c8c093)
    //   bg_pane    -> dragon_black_2  (#1D1C19)  -- used as our `bg`
    //   bg_bar     -> dragon_black_4  (#282727)
    //   accent     -> dragon_ash      (#737c73)
    //   highlight  -> dragon_orange   (#b6927b)
    //   selection  -> dragon_black_5  (#393836)
    //   info       -> dragon_teal     (#949fb5)
    //   notice     -> dragon_yellow   (#c4b28a)  -- our `warn`
    //   error      -> dragon_red      (#c4746e)  -- our `alert`
    //   muted      -> dragon_orange   (#b6927b)
    let entries = [
        ("bg", "#1D1C19"),
        ("bg_bar", "#282727"),
        ("fg", "#c8c093"),
        ("accent", "#737c73"),
        ("highlight", "#b6927b"),
        ("selection", "#393836"),
        ("info", "#949fb5"),
        ("alert", "#c4746e"),
        ("warn", "#c4b28a"),
        ("muted", "#b6927b"),
        ("ok", "#87a987"),
    ]
    .iter()
    // invariant: the built-in palette values are valid `#rrggbb` literals.
    .map(|(k, v)| {
        (
            (*k).to_string(),
            Rgb::parse_hex(v).expect("built-in palette hex"),
        )
    })
    .collect();
    PaletteConfig { entries }
}

pub fn built_in_default() -> Config {
    Config {
        palette: kanagawa_dragon_palette(),
        keymap: built_in_keymap(),
        // The built-in default declares no sessions (Feature B); declared
        // sessions are opt-in via `session` nodes in the user's config.
        sessions: Vec::new(),
        blocks: BlocksConfig::default(),
        hints: HintsConfig::default(),
        mouse: MouseConfig::default(),
        notifications: NotificationsConfig::default(),
        glyph_tier: GlyphTier::Unicode,
        auto_rename: true,
        welcome: true,
        remotes: Vec::new(),
        status: StatusConfig {
            position: Position::Bottom,
            refresh: Duration::from_secs(5),
            left: vec![
                WidgetSpec::Ssh {
                    style: StyleConfig {
                        fg: Some(role("accent")),
                        bg: Some(role("bg_bar")),
                        bold: true,
                        ..Default::default()
                    },
                    content: " ssh ".into(),
                },
                WidgetSpec::Session {
                    style: StyleConfig {
                        fg: Some(role("bg")),
                        bg: Some(role("accent")),
                        bold: true,
                        ..Default::default()
                    },
                    padding: Padding { left: 1, right: 1 },
                },
                WidgetSpec::PrefixIndicator {
                    style: StyleConfig {
                        fg: Some(role("bg")),
                        bg: Some(role("highlight")),
                        bold: true,
                        ..Default::default()
                    },
                    content: " PFX ".into(),
                },
            ],
            middle: vec![WidgetSpec::WindowList {
                // Active tab in `highlight` so it pops against both the `accent`
                // session pill on its left and the `bg_bar` inactive tabs. The
                // most-glanced-at "which window" cue needs a clear boundary.
                active_style: StyleConfig {
                    fg: Some(role("bg")),
                    bg: Some(role("highlight")),
                    bold: true,
                    ..Default::default()
                },
                inactive_style: StyleConfig {
                    fg: Some(role("muted")),
                    bg: Some(role("bg_bar")),
                    ..Default::default()
                },
            }],
            right: vec![
                WidgetSpec::CpuLoad {
                    style: StyleConfig {
                        fg: Some(role("fg")),
                        bg: Some(role("selection")),
                        ..Default::default()
                    },
                    interval: None,
                },
                WidgetSpec::Battery {
                    style: StyleConfig {
                        fg: Some(role("fg")),
                        bg: Some(role("bg_bar")),
                        ..Default::default()
                    },
                    interval: None,
                },
                WidgetSpec::Hostname {
                    style: StyleConfig {
                        fg: Some(role("fg")),
                        bg: Some(role("selection")),
                        ..Default::default()
                    },
                    interval: None,
                },
                // Far-right clock: 24-hour LOCAL time annotated with the location's
                // UTC offset (e.g. `14:42 UTC-04:00`). `Local` uses the host TZ, no
                // network. Weather is intentionally NOT a shipped default: it makes a
                // network call (flaky/slow in tests) and varies in width; add a
                // `shell` widget in your own config, see docs/configuration.md.
                WidgetSpec::Time {
                    format: "%H:%M UTC%:z".into(),
                    interval: None,
                    style: StyleConfig {
                        fg: Some(role("fg")),
                        bg: Some(role("bg_bar")),
                        ..Default::default()
                    },
                    utc: false,
                },
            ],
        },
    }
}

pub fn built_in_keymap() -> KeymapConfig {
    KeymapConfig {
        prefix: "Ctrl+a".into(),
        inherit_defaults: true,
        bindings: vec![
            binding("prefix c", "new_window"),
            binding("prefix v", "split_v"),
            binding("prefix s", "split_h"),
            binding("prefix x", "kill_pane"),
            binding("prefix z", "zoom_toggle"),
            binding("prefix n", "next_window"),
            binding("prefix p", "prev_window"),
            binding("prefix &", "kill_window"),
            binding("prefix d", "detach"),
            binding("prefix [", "enter_copy_mode"),
            binding("prefix y", "toggle_sync_panes"),
            binding("prefix R", "reload_config"),
            binding("prefix 1", "select_window:0"),
            binding("prefix 2", "select_window:1"),
            binding("prefix 3", "select_window:2"),
            binding("prefix 4", "select_window:3"),
            binding("prefix 5", "select_window:4"),
            binding("prefix 6", "select_window:5"),
            binding("prefix 7", "select_window:6"),
            binding("prefix 8", "select_window:7"),
            binding("prefix 9", "select_window:8"),
            binding("prefix h", "select_pane_left"),
            binding("prefix j", "select_pane_down"),
            binding("prefix k", "select_pane_up"),
            binding("prefix l", "select_pane_right"),
            binding("Alt+Left", "select_pane_left"),
            binding("Alt+Down", "select_pane_down"),
            binding("Alt+Up", "select_pane_up"),
            binding("Alt+Right", "select_pane_right"),
            binding("prefix H", "resize_pane_left"),
            binding("prefix J", "resize_pane_down"),
            binding("prefix K", "resize_pane_up"),
            binding("prefix L", "resize_pane_right"),
            binding("prefix Tab", "select_last_window"),
            binding("prefix ;", "select_last_pane"),
            binding("prefix ,", "rename_window"),
            binding("prefix .", "rename_pane"),
            binding("prefix ?", "show_help"),
            binding("prefix :", "command_prompt"),
            binding("prefix w", "choose_session"),
            binding("prefix W", "choose_tree"),
            binding("prefix /", "history"),
            binding("prefix f", "hints"),
            binding("prefix m", "mark_pane"),
            binding("prefix !", "break_pane"),
            binding("prefix {", "swap_pane_prev"),
            binding("prefix }", "swap_pane_next"),
            binding("prefix ]", "paste_buffer"),
            binding("prefix =", "choose_buffer"),
            binding("prefix M", "toggle_monitor_activity"),
            binding("prefix P", "popup"),
            binding("prefix q", "close_popup"),
            binding("prefix i", "next_layout"),
            binding("prefix Space", "command_palette"),
            binding("prefix <", "prev_prompt"),
            binding("prefix >", "next_prompt"),
            binding("prefix b", "enter_block_mode"),
        ],
    }
}

fn binding(keys: &str, command: &str) -> KeymapBinding {
    KeymapBinding {
        keys: keys.into(),
        command: command.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_status_is_lean_and_divider_free() {
        let c = built_in_default();
        // right cluster: CpuLoad, Battery, Hostname, Shell(weather), no Text dividers
        assert!(
            c.status
                .right
                .iter()
                .all(|w| !matches!(w, WidgetSpec::Text { .. }))
        );
        assert!(
            c.status
                .right
                .iter()
                .any(|w| matches!(w, WidgetSpec::CpuLoad { .. }))
        );
        assert!(
            c.status
                .right
                .iter()
                .any(|w| matches!(w, WidgetSpec::Battery { .. }))
        );
        assert!(
            c.status
                .right
                .iter()
                .any(|w| matches!(w, WidgetSpec::Hostname { .. }))
        );
        // far-right clock present
        assert!(
            c.status
                .right
                .iter()
                .any(|w| matches!(w, WidgetSpec::Time { .. }))
        );
        // git/cwd/weather are not in the shipped default right cluster
        // (weather is a network widget, opt in via your own config)
        assert!(c.status.right.iter().all(|w| !matches!(
            w,
            WidgetSpec::GitBranch { .. } | WidgetSpec::Cwd { .. } | WidgetSpec::Shell { .. }
        )));
        assert!(
            c.status
                .left
                .iter()
                .all(|w| !matches!(w, WidgetSpec::Text { value, .. } if value.trim().is_empty()))
        );
    }

    #[test]
    fn default_left_cluster_leads_with_ssh_marker() {
        let cfg = built_in_default();
        match cfg.status.left.first() {
            Some(WidgetSpec::Ssh { style, content }) => {
                assert_eq!(content, " ssh ");
                assert_eq!(style.fg, Some(role("accent")));
            }
            other => panic!("expected leading Ssh widget, got {other:?}"),
        }
    }
}
