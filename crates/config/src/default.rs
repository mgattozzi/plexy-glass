use crate::{
    Config, KeymapBinding, KeymapConfig, PaletteConfig, Padding, Position, StatusConfig,
    StyleConfig, WidgetSpec,
};
use std::time::Duration;

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
    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
    .collect();
    PaletteConfig { entries }
}

pub fn built_in_default() -> Config {
    Config {
        palette: kanagawa_dragon_palette(),
        keymap: built_in_keymap(),
        status: StatusConfig {
            position: Position::Bottom,
            refresh: Duration::from_secs(5),
            left: vec![
                WidgetSpec::Session {
                    style: StyleConfig::new("bg", "accent").bold(),
                    padding: Padding { left: 1, right: 1 },
                },
                WidgetSpec::PrefixIndicator {
                    style: StyleConfig::new("bg", "highlight").bold(),
                    content: " PFX ".into(),
                },
                WidgetSpec::Text {
                    value: " ".into(),
                    style: StyleConfig::default(),
                },
            ],
            middle: vec![WidgetSpec::WindowList {
                active_style: StyleConfig::new("fg", "accent"),
                inactive_style: StyleConfig::new("muted", "bg_bar"),
            }],
            right: vec![
                WidgetSpec::AttachedClients {
                    style: StyleConfig::new("fg", "bg_bar"),
                    min_count: 2,
                },
                WidgetSpec::Text {
                    value: "  ".into(),
                    style: StyleConfig::new("muted", "bg_bar"),
                },
                WidgetSpec::CpuLoad {
                    style: StyleConfig::new("fg", "bg_bar"),
                    interval: None,
                },
                WidgetSpec::Text {
                    value: " | ".into(),
                    style: StyleConfig::new("muted", "bg_bar"),
                },
                WidgetSpec::Battery {
                    style: StyleConfig::new("fg", "bg_bar"),
                    interval: None,
                },
                WidgetSpec::Text {
                    value: " | ".into(),
                    style: StyleConfig::new("muted", "bg_bar"),
                },
                WidgetSpec::Time {
                    style: StyleConfig::new("fg", "bg_bar"),
                    format: "%H:%M".into(),
                    interval: None,
                },
                WidgetSpec::Text {
                    value: " ".into(),
                    style: StyleConfig::new("muted", "bg_bar"),
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
            binding("Ctrl+a c", "new_window"),
            binding("Ctrl+a v", "split_v"),
            binding("Ctrl+a s", "split_h"),
            binding("Ctrl+a x", "kill_pane"),
            binding("Ctrl+a z", "zoom_toggle"),
            binding("Ctrl+a n", "next_window"),
            binding("Ctrl+a p", "prev_window"),
            binding("Ctrl+a &", "kill_window"),
            binding("Ctrl+a d", "detach"),
            binding("Ctrl+a [", "enter_copy_mode"),
            binding("Ctrl+a y", "toggle_sync_panes"),
            binding("Ctrl+a R", "reload_config"),
            binding("Ctrl+a 1", "select_window:0"),
            binding("Ctrl+a 2", "select_window:1"),
            binding("Ctrl+a 3", "select_window:2"),
            binding("Ctrl+a 4", "select_window:3"),
            binding("Ctrl+a 5", "select_window:4"),
            binding("Ctrl+a 6", "select_window:5"),
            binding("Ctrl+a 7", "select_window:6"),
            binding("Ctrl+a 8", "select_window:7"),
            binding("Ctrl+a 9", "select_window:8"),
            binding("Ctrl+a h", "select_pane_left"),
            binding("Ctrl+a j", "select_pane_down"),
            binding("Ctrl+a k", "select_pane_up"),
            binding("Ctrl+a l", "select_pane_right"),
            binding("Alt+Left", "select_pane_left"),
            binding("Alt+Down", "select_pane_down"),
            binding("Alt+Up", "select_pane_up"),
            binding("Alt+Right", "select_pane_right"),
            binding("Ctrl+a H", "resize_pane_left"),
            binding("Ctrl+a J", "resize_pane_down"),
            binding("Ctrl+a K", "resize_pane_up"),
            binding("Ctrl+a L", "resize_pane_right"),
            binding("Ctrl+a Tab", "select_last_window"),
            binding("Ctrl+a ;", "select_last_pane"),
            binding("Ctrl+a ,", "rename_window"),
            binding("Ctrl+a .", "rename_pane"),
            binding("Ctrl+a ?", "show_help"),
            binding("Ctrl+a :", "command_prompt"),
            binding("Ctrl+a w", "choose_session"),
            binding("Ctrl+a W", "choose_tree"),
            binding("Ctrl+a m", "mark_pane"),
            binding("Ctrl+a !", "break_pane"),
            binding("Ctrl+a {", "swap_pane_prev"),
            binding("Ctrl+a }", "swap_pane_next"),
        ],
    }
}

fn binding(keys: &str, command: &str) -> KeymapBinding {
    KeymapBinding {
        keys: keys.into(),
        command: command.into(),
    }
}
