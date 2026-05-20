//! plexy-glass configuration types and TOML loader.

mod default;
mod load;
mod types;

pub use default::{built_in_default, kanagawa_dragon_palette};
pub use load::{load_or_default, ConfigError};
pub use types::{
    Config, PaletteConfig, Padding, Position, StatusConfig, StyleConfig, WidgetSpec,
};

#[cfg(test)]
mod tests;
