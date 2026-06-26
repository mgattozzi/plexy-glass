//! plexy-glass multiplexing core.
//!
//! Pure logic, no async, no I/O. Holds the layout tree, the compositor that
//! turns a window's panes into a single virtual screen, a cell-diff renderer
//! that emits minimal ANSI to keep a host TTY in sync, and the prefix-key
//! keymap state machine. The daemon owns the thin wiring on top.

pub mod block_mode;
pub mod blocks;
pub mod borders;
pub mod buffer;
pub mod command_prompt;
pub mod compositor;
pub mod copy_mode;
pub mod diff;
pub mod direction;
pub mod hint;
pub mod history;
pub mod key;
pub mod keymap;
pub mod layout;
pub mod mouse;
pub mod overlay;
pub mod pane_id;
pub mod preset;
pub mod rect;
pub mod selection;
pub mod status;
pub mod tree;
pub mod virtual_screen;

pub use block_mode::{BlockMode, BlockModeAction, Filter};
pub use blocks::{
    BlockLineStatus, all_prompt_lines, block_command_line, block_extent, block_output_range,
    block_text, first_prompt_line, last_completed_block, last_prompt_line, next_prompt_line,
    prev_prompt_line, prompt_at_or_above, viewport_block_status,
};
pub use borders::BlockBorderColors;
pub use buffer::{BufferAction, BufferEntry, BufferOutcome, BufferPickerState, handle_buffers};
pub use command_prompt::{Completion, FocusTarget, ParseError, PromptCommand, SwapTarget};
pub use compositor::{
    HintColors, MessageView, OverlayView, PaneDragRole, PaneView, PopupView, StatusPlacement,
    popup_rect,
};
pub use copy_mode::{CopyMode, CopyModeAction, MatchSpan, SearchState};
pub use diff::{DiffRenderer, GraphicsCaps};
pub use virtual_screen::VisiblePlacement;
pub use direction::{Direction, SplitDir};
pub use hint::{
    DEFAULT_ALPHABET, HintAction, HintKind, HintOutcome, HintPick, HintState, HintTarget,
    assign_labels, effective_alphabet, handle_hint, scan_hints,
};
pub use history::{HistoryEntry, HistoryOutcome, HistoryState, HistoryTarget, handle_history};
pub use key::{Key, KeyEvent, KeyEventKind, Modifiers};
pub use keymap::{Chord, Command, Keymap, KeymapAction};
pub use layout::{BorderHit, BorderSide, CloseOutcome, LayoutError, LayoutTree, SplitPosition};
pub use mouse::{encode_for_child, MouseButton, MouseEncoding, MouseEvent, MouseKind, MouseModifiers, MouseParseAction, MouseParser};
pub use overlay::{
    Overlay, OverlayAction, PickerEntry, RenameTarget, picker_filtered_indices,
};
pub use pane_id::{PaneId, WindowId};
pub use preset::LayoutPreset;
pub use rect::Rect;
pub use selection::{
    Selection, extract_text, line_at, screen_text, viewport_content_row, word_at,
};
pub use status::StatusLine;
pub use tree::{
    NodeKey, TreeAction, TreeKind, TreeMode, TreeNode, TreeOutcome, TreeState, handle_tree,
    pane_label, session_label, window_label,
};
pub use virtual_screen::VirtualScreen;
