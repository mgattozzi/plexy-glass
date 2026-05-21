//! plexy-glass configuration types and TOML loader.

mod default;
mod load;
mod types;

pub use default::{built_in_default, built_in_keymap, kanagawa_dragon_palette};
pub use load::{load_or_default, load_from_path, ConfigError};
pub use types::{
    Config, KeymapBinding, KeymapConfig, PaletteConfig, Padding, Position, StatusConfig,
    StyleConfig, WidgetSpec,
};

#[cfg(test)]
mod tests;
