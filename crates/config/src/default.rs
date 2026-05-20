use crate::{Config, PaletteConfig, Padding, Position, StatusConfig, StyleConfig, WidgetSpec};
use std::time::Duration;

pub fn kanagawa_dragon_palette() -> PaletteConfig {
    let entries = [
        ("bg", "#181616"),
        ("bg_bar", "#0d0c0c"),
        ("fg", "#c5c9c5"),
        ("accent", "#8ba4b0"),
        ("alert", "#c4746e"),
        ("muted", "#625e5a"),
        ("ok", "#8a9a7b"),
        ("warn", "#c4b28a"),
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
