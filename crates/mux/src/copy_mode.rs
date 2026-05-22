//! Per-pane copy-mode state and a pure handler that consumes typed key
//! events to navigate scrollback, select content, and search.

use crate::{Direction, Key, KeyEvent, Modifiers};
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
    /// should take.
    pub fn handle(
        event: &KeyEvent,
        state: &mut CopyMode,
        screen: &Screen,
    ) -> CopyModeAction {
        let cols = screen.active.num_cols();
        match (event.mods, event.key) {
            (m, Key::Char('h')) | (m, Key::Arrow(Direction::Left)) if m.is_empty() => {
                state.cursor.1 = state.cursor.1.saturating_sub(1);
            }
            (m, Key::Char('l')) | (m, Key::Arrow(Direction::Right)) if m.is_empty() => {
                state.cursor.1 = (state.cursor.1 + 1).min(cols.saturating_sub(1));
            }
            (m, Key::Char('k')) | (m, Key::Arrow(Direction::Up)) if m.is_empty() => {
                state.cursor.0 = state.cursor.0.saturating_sub(1);
                ensure_visible(state);
            }
            (m, Key::Char('j')) | (m, Key::Arrow(Direction::Down)) if m.is_empty() => {
                let max_line = state.total_lines.saturating_sub(1);
                state.cursor.0 = (state.cursor.0 + 1).min(max_line);
                ensure_visible(state);
            }
            (m, Key::PageUp) if m.is_empty() => {
                state.cursor.0 = state.cursor.0.saturating_sub(u32::from(state.pane_rows));
                ensure_visible(state);
            }
            (m, Key::PageDown) if m.is_empty() => {
                let max_line = state.total_lines.saturating_sub(1);
                state.cursor.0 = (state.cursor.0 + u32::from(state.pane_rows)).min(max_line);
                ensure_visible(state);
            }
            (m, Key::Char('d')) if m == Modifiers::CTRL => {
                let half = u32::from(state.pane_rows / 2);
                let max_line = state.total_lines.saturating_sub(1);
                state.cursor.0 = (state.cursor.0 + half).min(max_line);
                ensure_visible(state);
            }
            (m, Key::Char('u')) if m == Modifiers::CTRL => {
                let half = u32::from(state.pane_rows / 2);
                state.cursor.0 = state.cursor.0.saturating_sub(half);
                ensure_visible(state);
            }
            (m, Key::Char('g')) if m.is_empty() => {
                state.cursor = (0, 0);
                ensure_visible(state);
            }
            (m, Key::Char('G')) if m == Modifiers::SHIFT => {
                state.cursor = (state.total_lines.saturating_sub(1), 0);
                ensure_visible(state);
            }
            (m, Key::Char('0')) if m.is_empty() => {
                state.cursor.1 = 0;
            }
            (m, Key::Char('$')) if m == Modifiers::SHIFT => {
                state.cursor.1 = cols.saturating_sub(1);
            }
            (m, Key::Char('v')) if m.is_empty() => {
                state.anchor = if state.anchor.is_some() {
                    None
                } else {
                    Some(state.cursor)
                };
            }
            (m, Key::Char('y')) if m.is_empty() => {
                let text = extract_selection(state, screen);
                return CopyModeAction::Yank(text);
            }
            _ => {} // Tasks 6-7 add search/escape
        }
        CopyModeAction::Render
    }
}

fn ensure_visible(state: &mut CopyMode) {
    if state.cursor.0 < state.viewport_top {
        state.viewport_top = state.cursor.0;
    }
    let bottom = state.viewport_top + u32::from(state.pane_rows.saturating_sub(1));
    if state.cursor.0 > bottom {
        state.viewport_top = state
            .cursor
            .0
            .saturating_sub(u32::from(state.pane_rows.saturating_sub(1)));
    }
}

/// Extract the selected (or current-line) text from the unified
/// scrollback + active grid line space.
fn extract_selection(state: &CopyMode, screen: &Screen) -> String {
    let (start, end) = match state.anchor {
        Some(anchor) => normalize(anchor, state.cursor),
        None => {
            let line = state.cursor.0;
            (
                (line, 0),
                (line, screen.active.num_cols().saturating_sub(1)),
            )
        }
    };
    let cols = screen.active.num_cols();
    let scrollback_rows = screen.scrollback.rows();
    let scrollback_len = scrollback_rows.len() as u32;
    let mut out = String::new();

    for line in start.0..=end.0 {
        let row_start = if line == start.0 { start.1 } else { 0 };
        let row_end = if line == end.0 {
            end.1
        } else {
            cols.saturating_sub(1)
        };
        let row_cells: Option<Vec<_>> = if line < scrollback_len {
            scrollback_rows
                .get(line as usize)
                .map(|row| row.cells.clone())
        } else {
            let active_row = (line - scrollback_len) as usize;
            screen.active.rows.get(active_row).map(|r| r.cells.clone())
        };
        let Some(cells) = row_cells else { continue };
        // Trim trailing blanks in this row.
        let mut last_significant = row_start;
        for c in row_start..=row_end {
            if let Some(cell) = cells.get(c as usize)
                && !cell.is_blank()
            {
                last_significant = c;
            }
        }
        for c in row_start..=last_significant {
            if let Some(cell) = cells.get(c as usize) {
                if cell.is_wide_spacer() {
                    continue;
                }
                out.push_str(cell.grapheme.as_str());
            }
        }
        if line < end.0 {
            out.push('\n');
        }
    }
    out
}

fn normalize(a: (u32, u16), b: (u32, u16)) -> ((u32, u16), (u32, u16)) {
    if a <= b { (a, b) } else { (b, a) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexy_glass_emulator::Emulator;

    fn screen(rows: u16, cols: u16) -> Screen {
        let mut e = Emulator::new(rows, cols);
        // Push a known byte so the active grid has at least one cell width.
        e.advance(b"x");
        e.screen().clone()
    }

    fn screen_with_lines(rows: u16, cols: u16, lines: &[&str]) -> Screen {
        let mut e = Emulator::new(rows, cols);
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                e.advance(b"\r\n");
            }
            e.advance(line.as_bytes());
        }
        // Flush any pending grapheme so the last char is in the grid.
        e.advance(b"\x1b[m");
        e.screen().clone()
    }

    fn ev(mods: Modifiers, key: Key) -> KeyEvent {
        KeyEvent::new(key, mods)
    }

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

    #[test]
    fn h_moves_cursor_left_and_clamps() {
        let mut s = CopyMode::new(10, 5, 5, 3);
        let scr = screen(10, 80);
        CopyModeHandler::handle(&ev(Modifiers::empty(), Key::Char('h')), &mut s, &scr);
        assert_eq!(s.cursor.1, 2);
        s.cursor.1 = 0;
        CopyModeHandler::handle(&ev(Modifiers::empty(), Key::Char('h')), &mut s, &scr);
        assert_eq!(s.cursor.1, 0);
    }

    #[test]
    fn l_moves_cursor_right_and_clamps_at_pane_width() {
        let mut s = CopyMode::new(10, 5, 5, 78);
        let scr = screen(10, 80);
        CopyModeHandler::handle(&ev(Modifiers::empty(), Key::Char('l')), &mut s, &scr);
        CopyModeHandler::handle(&ev(Modifiers::empty(), Key::Char('l')), &mut s, &scr);
        assert_eq!(s.cursor.1, 79);
    }

    #[test]
    fn k_moves_up_and_scrolls_viewport() {
        let mut s = CopyMode::new(20, 5, 18, 0);
        assert_eq!(s.viewport_top, 15);
        let scr = screen(5, 80);
        for _ in 0..4 {
            CopyModeHandler::handle(&ev(Modifiers::empty(), Key::Char('k')), &mut s, &scr);
        }
        assert_eq!(s.cursor.0, 14);
        assert!(s.viewport_top <= 14);
    }

    #[test]
    fn j_moves_down_and_clamps_at_total_lines() {
        let mut s = CopyMode::new(10, 5, 9, 0);
        let scr = screen(5, 80);
        CopyModeHandler::handle(&ev(Modifiers::empty(), Key::Char('j')), &mut s, &scr);
        assert_eq!(s.cursor.0, 9);
    }

    #[test]
    fn page_up_jumps_by_pane_rows() {
        let mut s = CopyMode::new(50, 10, 40, 0);
        let scr = screen(10, 80);
        CopyModeHandler::handle(&ev(Modifiers::empty(), Key::PageUp), &mut s, &scr);
        assert_eq!(s.cursor.0, 30);
    }

    #[test]
    fn ctrl_d_jumps_by_half_pane() {
        let mut s = CopyMode::new(50, 10, 20, 0);
        let scr = screen(10, 80);
        CopyModeHandler::handle(&ev(Modifiers::CTRL, Key::Char('d')), &mut s, &scr);
        assert_eq!(s.cursor.0, 25);
    }

    #[test]
    fn g_jumps_to_top() {
        let mut s = CopyMode::new(50, 10, 30, 5);
        let scr = screen(10, 80);
        CopyModeHandler::handle(&ev(Modifiers::empty(), Key::Char('g')), &mut s, &scr);
        assert_eq!(s.cursor, (0, 0));
    }

    #[test]
    fn shift_g_jumps_to_bottom() {
        let mut s = CopyMode::new(50, 10, 10, 5);
        let scr = screen(10, 80);
        CopyModeHandler::handle(&ev(Modifiers::SHIFT, Key::Char('G')), &mut s, &scr);
        assert_eq!(s.cursor, (49, 0));
    }

    #[test]
    fn zero_jumps_to_col_zero() {
        let mut s = CopyMode::new(50, 10, 10, 22);
        let scr = screen(10, 80);
        CopyModeHandler::handle(&ev(Modifiers::empty(), Key::Char('0')), &mut s, &scr);
        assert_eq!(s.cursor.1, 0);
    }

    #[test]
    fn dollar_jumps_to_last_col() {
        let mut s = CopyMode::new(50, 10, 10, 0);
        let scr = screen(10, 80);
        CopyModeHandler::handle(&ev(Modifiers::SHIFT, Key::Char('$')), &mut s, &scr);
        assert_eq!(s.cursor.1, 79);
    }

    #[test]
    fn v_toggles_anchor() {
        let mut s = CopyMode::new(10, 5, 3, 2);
        let scr = screen(5, 80);
        CopyModeHandler::handle(&ev(Modifiers::empty(), Key::Char('v')), &mut s, &scr);
        assert_eq!(s.anchor, Some((3, 2)));
        CopyModeHandler::handle(&ev(Modifiers::empty(), Key::Char('v')), &mut s, &scr);
        assert_eq!(s.anchor, None);
    }

    #[test]
    fn y_with_selection_yanks_text() {
        let scr = screen_with_lines(2, 10, &["hello", "world"]);
        let mut s = CopyMode::new(2, 2, 0, 0);
        s.anchor = Some((0, 0));
        s.cursor = (0, 4);
        let action = CopyModeHandler::handle(&ev(Modifiers::empty(), Key::Char('y')), &mut s, &scr);
        match action {
            CopyModeAction::Yank(text) => assert_eq!(text, "hello"),
            other => panic!("expected Yank, got {other:?}"),
        }
    }

    #[test]
    fn y_without_selection_yanks_current_line() {
        let scr = screen_with_lines(2, 10, &["hello", "world"]);
        let mut s = CopyMode::new(2, 2, 1, 0);
        let action = CopyModeHandler::handle(&ev(Modifiers::empty(), Key::Char('y')), &mut s, &scr);
        match action {
            CopyModeAction::Yank(text) => assert!(text.contains("world"), "got: {text:?}"),
            other => panic!("expected Yank, got {other:?}"),
        }
    }
}
