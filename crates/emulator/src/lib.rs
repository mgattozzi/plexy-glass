//! plexy-glass terminal emulator core.
//!
//! Pure-logic crate: bytes from a PTY are fed in via [`Emulator::advance`] and
//! come out as a queryable grid + scrollback model. No async runtime, no I/O.
//!
//! The public surface most callers need is the [`Emulator`] type; the
//! sub-modules are exposed for tests and internal composition.

pub mod attrs;
pub mod cell;
pub mod color;
pub mod cursor;
pub mod graphics;
pub mod grid;
pub mod hyperlinks;
pub mod keyboard;
pub mod modes;
pub mod reflow;
pub mod scrollback;
pub mod tabs;
pub mod terminfo;
pub mod width;

// These land in later tasks; declared here as placeholders so downstream
// modules can `use` them the moment they exist.
pub mod parser;
pub mod screen;
// pub mod events;
pub mod emulator;

pub use attrs::{Attrs, UnderlineStyle};
pub use cell::Cell;
pub use color::Color;
pub use cursor::{Cursor, CursorShape};
pub use emulator::Emulator;
pub use graphics::{AnimControl, Frame, Image, ImageFormat, ImageProtocol, ImageStore, Placement};
pub use grid::{Grid, Row, RowMark, WrapOrigin};
pub use hyperlinks::HyperlinkTable;
pub use keyboard::KeyboardState;
pub use modes::Modes;
pub use screen::{ColorQuery, Screen};
pub use scrollback::{DEFAULT_SCROLLBACK_LINES, Scrollback};
pub use tabs::TabStops;
pub use width::{
    char_width, display_width, grapheme_advance, graphemes_with_width, truncate_to_width,
};
