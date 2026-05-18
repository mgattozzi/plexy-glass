//! plexy-glass multiplexing core.
//!
//! Pure logic, no async, no I/O. Holds the layout tree, the compositor that
//! turns a window's panes into a single virtual screen, a cell-diff renderer
//! that emits minimal ANSI to keep a host TTY in sync, and the prefix-key
//! keymap state machine. The daemon owns the thin wiring on top.

pub mod borders;
pub mod compositor;
pub mod diff;
pub mod direction;
pub mod keymap;
pub mod layout;
pub mod pane_id;
pub mod rect;
pub mod status;
pub mod virtual_screen;

pub use compositor::{Compositor, PaneView};
pub use diff::DiffRenderer;
pub use direction::{Direction, SplitDir};
pub use keymap::{Command, Keymap, KeymapAction};
pub use layout::{CloseOutcome, LayoutError, LayoutTree, SplitPosition};
pub use pane_id::{PaneId, WindowId};
pub use rect::Rect;
pub use status::{StatusLine, WindowEntry};
pub use virtual_screen::VirtualScreen;
