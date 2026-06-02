//! plexy-glass multiplexing core.
//!
//! Pure logic, no async, no I/O. Holds the layout tree, the compositor that
//! turns a window's panes into a single virtual screen, a cell-diff renderer
//! that emits minimal ANSI to keep a host TTY in sync, and the prefix-key
//! keymap state machine. The daemon owns the thin wiring on top.

pub mod borders;
pub mod buffer;
pub mod command_prompt;
pub mod compositor;
pub mod copy_mode;
pub mod diff;
pub mod direction;
pub mod key;
pub mod keymap;
pub mod layout;
pub mod mouse;
pub mod overlay;
pub mod pane_id;
pub mod rect;
pub mod selection;
pub mod status;
pub mod tree;
pub mod virtual_screen;

pub use buffer::{BufferAction, BufferEntry, BufferOutcome, BufferPickerState, handle_buffers};
pub use command_prompt::{Completion, FocusTarget, ParseError, PromptCommand, SwapTarget};
pub use compositor::{Compositor, OverlayView, PaneView, StatusPlacement};
pub use copy_mode::{CopyMode, CopyModeAction, CopyModeHandler, MatchSpan, SearchState};
pub use diff::DiffRenderer;
pub use direction::{Direction, SplitDir};
pub use key::{Key, KeyEvent, KeyEventKind, Modifiers};
pub use keymap::{Chord, Command, Keymap, KeymapAction};
pub use layout::{BorderHit, BorderSide, CloseOutcome, LayoutError, LayoutTree, SplitPosition};
pub use mouse::{encode_for_child, MouseButton, MouseEncoding, MouseEvent, MouseKind, MouseModifiers, MouseParseAction, MouseParser};
pub use overlay::{
    Overlay, OverlayAction, OverlayHandler, PickerEntry, RenameTarget, picker_filtered_indices,
};
pub use pane_id::{PaneId, WindowId};
pub use rect::Rect;
pub use selection::{Selection, SelectionKind, extract_text, line_at, word_at};
pub use status::StatusLine;
pub use tree::{
    TreeAction, TreeKind, TreeMode, TreeNode, TreeOutcome, TreeState, handle_tree, pane_label,
    window_label,
};
pub use virtual_screen::VirtualScreen;
