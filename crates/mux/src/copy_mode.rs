//! Per-pane copy-mode state and a pure handler that consumes typed key
//! events to navigate scrollback, select content, and search.

use crate::KeyEvent;
use plexy_glass_emulator::Screen;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchSpan {
    pub line_idx: u32,
    pub col_start: u16,
    pub col_end: u16,
}

#[derive(Debug, Clone, Default)]
pub struct SearchState {
    pub query: String,
    pub matches: Vec<MatchSpan>,
    pub current: usize,
    pub prompt_active: bool,
    pub prompt_buf: String,
}

#[derive(Debug, Clone)]
pub struct CopyMode {
    pub cursor: (u32, u16),
    pub anchor: Option<(u32, u16)>,
    pub search: SearchState,
    pub viewport_top: u32,
    pub pane_rows: u16,
    pub total_lines: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyModeAction {
    /// Re-render the pane.
    Render,
    /// User asked to exit copy mode.
    Exit,
    /// User yanked text; caller writes to clipboard and exits copy mode.
    Yank(String),
}

impl CopyMode {
    /// Construct a new copy-mode state.
    ///
    /// `start_line` and `start_col` are the cursor's initial position in
    /// the unified line space (scrollback rows then active rows).
    pub fn new(total_lines: u32, pane_rows: u16, start_line: u32, start_col: u16) -> Self {
        let viewport_top = total_lines.saturating_sub(u32::from(pane_rows));
        Self {
            cursor: (start_line.min(total_lines.saturating_sub(1)), start_col),
            anchor: None,
            search: SearchState::default(),
            viewport_top,
            pane_rows,
            total_lines,
        }
    }

    /// Called by `Pane` on resize / scrollback growth.
    pub fn set_pane_rows(&mut self, pane_rows: u16, total_lines: u32) {
        self.pane_rows = pane_rows;
        self.total_lines = total_lines;
        if self.cursor.0 >= total_lines {
            self.cursor.0 = total_lines.saturating_sub(1);
        }
        let max_top = total_lines.saturating_sub(u32::from(pane_rows));
        if self.viewport_top > max_top {
            self.viewport_top = max_top;
        }
    }
}

pub struct CopyModeHandler;

impl CopyModeHandler {
    /// Consume one key event, mutate state, return the action the caller
    /// should take. Stub until Tasks 4-7 fill in the dispatch.
    pub fn handle(
        _event: &KeyEvent,
        _state: &mut CopyMode,
        _screen: &Screen,
    ) -> CopyModeAction {
        CopyModeAction::Render
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_clamps_cursor_to_total_lines() {
        let cm = CopyMode::new(/*total*/ 5, /*rows*/ 3, /*start_line*/ 10, /*start_col*/ 0);
        assert_eq!(cm.cursor.0, 4);
    }

    #[test]
    fn new_sets_viewport_top_to_bottom_of_history() {
        let cm = CopyMode::new(10, 3, 9, 0);
        assert_eq!(cm.viewport_top, 7);
    }

    #[test]
    fn set_pane_rows_clamps_cursor_and_viewport() {
        let mut cm = CopyMode::new(10, 5, 9, 0);
        cm.set_pane_rows(3, 4);
        assert_eq!(cm.cursor.0, 3);
        assert_eq!(cm.viewport_top, 1);
    }
}
