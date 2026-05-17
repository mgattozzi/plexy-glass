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
pub mod grid;
pub mod hyperlinks;
pub mod modes;
pub mod reflow;
pub mod scrollback;
pub mod tabs;

// These land in later tasks; declared here as placeholders so downstream
// modules can `use` them the moment they exist.
pub mod parser;
pub mod screen;
// pub mod events;
pub mod emulator;

pub use attrs::Attrs;
pub use cell::Cell;
pub use color::Color;
pub use cursor::{Cursor, CursorShape};
pub use emulator::Emulator;
pub use grid::{Grid, Row, WrapOrigin};
pub use hyperlinks::HyperlinkTable;
pub use modes::Modes;
pub use screen::Screen;
pub use scrollback::{Scrollback, DEFAULT_SCROLLBACK_LINES};
pub use tabs::TabStops;
