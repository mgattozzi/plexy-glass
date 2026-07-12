//! plexy-glass configuration types and KDL loader.

mod color;
mod default;
mod kdl_config;
mod load;
mod types;

pub use color::{ColorSource, HexColorError, Rgb};
pub use default::{built_in_default, built_in_keymap, kanagawa_dragon_palette};
pub use kdl_config::parse_config;
pub use load::{ConfigError, load_from_path, load_or_default};
pub use types::{
    BlocksConfig, Config, DragModifier, GlyphTier, HintsConfig, KeymapBinding, KeymapConfig,
    MouseConfig, NotificationsConfig, Padding, PaletteConfig, PaneNode, PaneTemplate, Position,
    SessionTemplate, SplitDirection, StatusConfig, StyleConfig, WidgetSpec, WindowTemplate,
};

#[cfg(test)]
mod tests;
