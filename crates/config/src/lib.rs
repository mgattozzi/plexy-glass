//! plexy-glass configuration types and KDL loader.

mod default;
mod kdl_config;
mod load;
mod types;

pub use default::{built_in_default, built_in_keymap, kanagawa_dragon_palette};
pub use kdl_config::parse_config;
pub use load::{load_or_default, load_from_path, ConfigError};
pub use types::{
    BlocksConfig, Config, GlyphTier, HintsConfig, KeymapBinding, KeymapConfig, NotificationsConfig,
    PaletteConfig, Padding, PaneNode, PaneTemplate, Position, SessionTemplate, SplitDirection,
    StatusConfig, StyleConfig, WidgetSpec, WindowTemplate,
};

#[cfg(test)]
mod tests;
