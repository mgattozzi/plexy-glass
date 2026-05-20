use crate::{Config, PaletteConfig, Padding, Position, StatusConfig, StyleConfig, WidgetSpec};
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
        status: StatusConfig {
            position: Position::Bottom,
            refresh: Duration::from_secs(5),
            left: vec![
                WidgetSpec::Session {
                    style: StyleConfig::new("bg", "accent").bold(),
                    padding: Padding { left: 1, right: 1 },
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
                WidgetSpec::CpuLoad {
                    style: StyleConfig::new("fg", "bg_bar"),
                    interval: None,
                },
                WidgetSpec::Battery {
                    style: StyleConfig::new("fg", "bg_bar"),
                    interval: None,
                },
                WidgetSpec::Time {
                    style: StyleConfig::new("fg", "bg_bar"),
                    format: "%H:%M".into(),
                    interval: None,
                },
            ],
        },
    }
}
