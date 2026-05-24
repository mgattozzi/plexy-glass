//! Per-pane copy-mode state and a pure handler that consumes typed key
//! events to navigate scrollback, select content, and search.

use crate::{Direction, Key, KeyEvent, Modifiers, MouseButton, MouseEvent, MouseKind};
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

    /// Translate a mouse event in pane-viewport coords into copy-mode state
    /// changes. `click_count` (1/2/3) is computed by the caller from a
    /// 400ms-window same-target classifier. Returns `Render` so the caller
    /// repaints; copy-mode mouse never auto-yanks (use `y` to yank, matching
    /// the keyboard flow).
    pub fn handle_mouse(
        &mut self,
        event: &MouseEvent,
        click_count: u8,
        screen: &Screen,
    ) -> CopyModeAction {
        let max_line = self.total_lines.saturating_sub(1);
        let line = self
            .viewport_top
            .saturating_add(event.row as u32)
            .min(max_line);
        match (event.kind, event.button) {
            (MouseKind::Press, MouseButton::Left) => {
                self.cursor = (line, event.col);
                self.anchor = Some(self.cursor);
                if click_count >= 3 {
                    self.select_line_at_cursor(screen);
                } else if click_count == 2 {
                    self.select_word_at_cursor(screen);
                }
                CopyModeAction::Render
            }
            (MouseKind::Move, MouseButton::Left) => {
                if self.anchor.is_none() {
                    self.anchor = Some(self.cursor);
                }
                self.cursor = (line, event.col);
                CopyModeAction::Render
            }
            (MouseKind::Release, MouseButton::Left) => CopyModeAction::Render,
            (MouseKind::Wheel { delta }, _) => {
                if delta > 0 {
                    self.viewport_top = self.viewport_top.saturating_sub(delta as u32);
                } else {
                    let max_top = self
                        .total_lines
                        .saturating_sub(self.pane_rows as u32);
                    self.viewport_top = (self.viewport_top + (-delta) as u32).min(max_top);
                }
                CopyModeAction::Render
            }
            _ => CopyModeAction::Render,
        }
    }

    /// Expand cursor + anchor to span the word containing `cursor`. Walks
    /// cells outward on the unified line.
    fn select_word_at_cursor(&mut self, screen: &Screen) {
        let (line, col) = self.cursor;
        let Some(cells) = unified_line_cells(screen, line) else { return };
        let cols = cells.len();
        if (col as usize) >= cols {
            return;
        }
        let on_word = cells
            .get(col as usize)
            .map(|c| is_word_grapheme(c.grapheme.as_str()))
            .unwrap_or(false);
        if !on_word {
            return;
        }
        let mut start = col;
        while start > 0 {
            let prev = start - 1;
            if cells
                .get(prev as usize)
                .map(|c| is_word_grapheme(c.grapheme.as_str()))
                .unwrap_or(false)
            {
                start = prev;
            } else {
                break;
            }
        }
        let mut end = col;
        while (end as usize) + 1 < cols {
            let next = end + 1;
            if cells
                .get(next as usize)
                .map(|c| is_word_grapheme(c.grapheme.as_str()))
                .unwrap_or(false)
            {
                end = next;
            } else {
                break;
            }
        }
        self.anchor = Some((line, start));
        self.cursor = (line, end);
    }

    /// Expand cursor + anchor to span the entire line (col 0 → last non-blank).
    fn select_line_at_cursor(&mut self, screen: &Screen) {
        let line = self.cursor.0;
        let Some(cells) = unified_line_cells(screen, line) else { return };
        let mut last = None;
        for (i, cell) in cells.iter().enumerate() {
            if !cell.is_blank() {
                last = Some(i as u16);
            }
        }
        let Some(end) = last else { return };
        self.anchor = Some((line, 0));
        self.cursor = (line, end);
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
        // Escape / q priority chain (works in both prompt and motion modes):
        //   1. Close prompt → Render
        //   2. Clear selection anchor → Render
        //   3. Exit copy mode
        if event.mods.is_empty()
            && matches!(event.key, Key::Escape | Key::Char('q'))
        {
            if state.search.prompt_active {
                state.search.prompt_active = false;
                state.search.prompt_buf.clear();
                return CopyModeAction::Render;
            }
            if state.anchor.is_some() {
                state.anchor = None;
                return CopyModeAction::Render;
            }
            return CopyModeAction::Exit;
        }

        // In prompt mode, route everything else to the prompt handler.
        if state.search.prompt_active {
            return handle_search_prompt(event, state, screen);
        }

        // Otherwise: motion + selection + search-jump + yank dispatch.
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
            (m, Key::Char('/')) if m.is_empty() => {
                state.search.prompt_active = true;
                state.search.prompt_buf.clear();
            }
            (m, Key::Char('n')) if m.is_empty() => {
                jump_to_next_match(state);
            }
            (m, Key::Char('N')) if m == Modifiers::SHIFT => {
                jump_to_prev_match(state);
            }
            _ => {}
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

/// Fetch a unified-line's cells (scrollback rows come first, then active
/// rows). Used by mouse word/line selection inside copy mode.
fn unified_line_cells(screen: &Screen, line: u32) -> Option<Vec<plexy_glass_emulator::Cell>> {
    let scrollback_rows = screen.scrollback.rows();
    let scrollback_len = scrollback_rows.len() as u32;
    if line < scrollback_len {
        scrollback_rows
            .get(line as usize)
            .map(|row| row.cells.clone())
    } else {
        let active_row = (line - scrollback_len) as usize;
        screen.active.rows.get(active_row).map(|r| r.cells.clone())
    }
}

/// Word-char predicate shared with `selection::word_at` semantics.
fn is_word_grapheme(g: &str) -> bool {
    let mut chars = g.chars();
    let Some(ch) = chars.next() else { return false };
    if chars.next().is_some() {
        return true;
    }
    ch.is_alphanumeric() || matches!(ch, '_' | '.' | '-' | '/' | '~')
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

fn handle_search_prompt(
    event: &KeyEvent,
    state: &mut CopyMode,
    screen: &Screen,
) -> CopyModeAction {
    match (event.mods, event.key) {
        (m, Key::Enter) if m.is_empty() => {
            state.search.query = std::mem::take(&mut state.search.prompt_buf);
            state.search.prompt_active = false;
            if state.search.query.is_empty() {
                state.search.matches.clear();
                state.search.current = 0;
                return CopyModeAction::Render;
            }
            state.search.matches = find_matches(screen, &state.search.query);
            if state.search.matches.is_empty() {
                state.search.current = 0;
                return CopyModeAction::Render;
            }
            let next = state
                .search
                .matches
                .iter()
                .position(|m| m.line_idx >= state.cursor.0)
                .unwrap_or(0);
            state.search.current = next;
            let m = &state.search.matches[next];
            state.cursor = (m.line_idx, m.col_start);
            ensure_visible(state);
            CopyModeAction::Render
        }
        (m, Key::Backspace) if m.is_empty() => {
            state.search.prompt_buf.pop();
            CopyModeAction::Render
        }
        (m, Key::Char(c)) if m.is_empty() || m == Modifiers::SHIFT => {
            state.search.prompt_buf.push(c);
            CopyModeAction::Render
        }
        _ => CopyModeAction::Render,
    }
}

fn jump_to_next_match(state: &mut CopyMode) {
    if state.search.matches.is_empty() {
        return;
    }
    state.search.current = (state.search.current + 1) % state.search.matches.len();
    let m = &state.search.matches[state.search.current];
    state.cursor = (m.line_idx, m.col_start);
    ensure_visible(state);
}

fn jump_to_prev_match(state: &mut CopyMode) {
    if state.search.matches.is_empty() {
        return;
    }
    state.search.current = if state.search.current == 0 {
        state.search.matches.len() - 1
    } else {
        state.search.current - 1
    };
    let m = &state.search.matches[state.search.current];
    state.cursor = (m.line_idx, m.col_start);
    ensure_visible(state);
}

fn find_matches(screen: &Screen, query: &str) -> Vec<MatchSpan> {
    let mut out = Vec::new();
    if query.is_empty() {
        return out;
    }
    let cols = screen.active.num_cols();
    let scrollback_rows = screen.scrollback.rows();
    let scrollback_len = scrollback_rows.len() as u32;
    let total = scrollback_len + screen.active.num_rows() as u32;
    for line_idx in 0..total {
        let cells = if line_idx < scrollback_len {
            scrollback_rows
                .get(line_idx as usize)
                .map(|row| row.cells.clone())
        } else {
            let active_row = (line_idx - scrollback_len) as usize;
            screen.active.rows.get(active_row).map(|r| r.cells.clone())
        };
        let Some(cells) = cells else { continue };
        let line_text: String = cells
            .iter()
            .filter(|c| !c.is_wide_spacer())
            .map(|c| c.grapheme.as_str())
            .collect();
        let mut start = 0usize;
        while let Some(idx) = line_text[start..].find(query) {
            let col = (start + idx) as u16;
            let end_col = (col + query.chars().count() as u16).min(cols.saturating_sub(1));
            out.push(MatchSpan {
                line_idx,
                col_start: col,
                col_end: end_col,
            });
            start += idx + query.len();
        }
    }
    out
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

    #[test]
    fn slash_opens_search_prompt() {
        let mut s = CopyMode::new(10, 5, 0, 0);
        let scr = screen(5, 80);
        CopyModeHandler::handle(&ev(Modifiers::empty(), Key::Char('/')), &mut s, &scr);
        assert!(s.search.prompt_active);
    }

    #[test]
    fn search_prompt_appends_typed_chars() {
        let mut s = CopyMode::new(10, 5, 0, 0);
        let scr = screen(5, 80);
        s.search.prompt_active = true;
        for c in ['f', 'o', 'o'] {
            CopyModeHandler::handle(&ev(Modifiers::empty(), Key::Char(c)), &mut s, &scr);
        }
        assert_eq!(s.search.prompt_buf, "foo");
    }

    #[test]
    fn search_prompt_backspace_deletes() {
        let mut s = CopyMode::new(10, 5, 0, 0);
        let scr = screen(5, 80);
        s.search.prompt_active = true;
        s.search.prompt_buf = "foo".into();
        CopyModeHandler::handle(&ev(Modifiers::empty(), Key::Backspace), &mut s, &scr);
        assert_eq!(s.search.prompt_buf, "fo");
    }

    #[test]
    fn search_submit_jumps_to_first_match_below_cursor() {
        let scr = screen_with_lines(3, 30, &["alpha", "beta passwd here", "gamma"]);
        let mut s = CopyMode::new(3, 3, 0, 0);
        s.search.prompt_active = true;
        s.search.prompt_buf = "passwd".into();
        CopyModeHandler::handle(&ev(Modifiers::empty(), Key::Enter), &mut s, &scr);
        assert!(!s.search.prompt_active);
        assert_eq!(s.search.query, "passwd");
        assert_eq!(s.search.matches.len(), 1);
        assert_eq!(s.cursor.0, 1);
        assert_eq!(s.cursor.1, 5);
    }

    #[test]
    fn n_cycles_to_next_match() {
        let scr = screen_with_lines(3, 30, &["foo", "foo bar foo", "foo baz"]);
        let mut s = CopyMode::new(3, 3, 0, 0);
        s.search.prompt_active = true;
        s.search.prompt_buf = "foo".into();
        CopyModeHandler::handle(&ev(Modifiers::empty(), Key::Enter), &mut s, &scr);
        assert!(s.search.matches.len() >= 2);
        let first_idx = s.search.current;
        CopyModeHandler::handle(&ev(Modifiers::empty(), Key::Char('n')), &mut s, &scr);
        assert_ne!(s.search.current, first_idx);
    }

    #[test]
    fn search_empty_query_clears_state() {
        let scr = screen_with_lines(3, 30, &["alpha", "beta", "gamma"]);
        let mut s = CopyMode::new(3, 3, 0, 0);
        s.search.prompt_active = true;
        s.search.prompt_buf = String::new();
        CopyModeHandler::handle(&ev(Modifiers::empty(), Key::Enter), &mut s, &scr);
        assert!(s.search.matches.is_empty());
        assert!(s.search.query.is_empty());
    }

    #[test]
    fn search_no_match_leaves_cursor() {
        let scr = screen_with_lines(3, 30, &["alpha", "beta", "gamma"]);
        let mut s = CopyMode::new(3, 3, 1, 0);
        s.search.prompt_active = true;
        s.search.prompt_buf = "zzzzz".into();
        CopyModeHandler::handle(&ev(Modifiers::empty(), Key::Enter), &mut s, &scr);
        assert!(s.search.matches.is_empty());
        assert_eq!(s.cursor.0, 1);
    }

    #[test]
    fn escape_in_prompt_closes_prompt_only() {
        let mut s = CopyMode::new(10, 5, 0, 0);
        let scr = screen(5, 80);
        s.search.prompt_active = true;
        s.search.prompt_buf = "abc".into();
        let action = CopyModeHandler::handle(&ev(Modifiers::empty(), Key::Escape), &mut s, &scr);
        assert!(matches!(action, CopyModeAction::Render));
        assert!(!s.search.prompt_active);
        assert!(s.search.prompt_buf.is_empty());
    }

    #[test]
    fn escape_with_selection_clears_selection() {
        let mut s = CopyMode::new(10, 5, 5, 3);
        s.anchor = Some((0, 0));
        let scr = screen(5, 80);
        let action = CopyModeHandler::handle(&ev(Modifiers::empty(), Key::Escape), &mut s, &scr);
        assert!(matches!(action, CopyModeAction::Render));
        assert!(s.anchor.is_none());
    }

    #[test]
    fn escape_in_normal_mode_exits() {
        let mut s = CopyMode::new(10, 5, 5, 3);
        let scr = screen(5, 80);
        let action = CopyModeHandler::handle(&ev(Modifiers::empty(), Key::Escape), &mut s, &scr);
        assert!(matches!(action, CopyModeAction::Exit));
    }

    #[test]
    fn q_in_normal_mode_exits() {
        let mut s = CopyMode::new(10, 5, 5, 3);
        let scr = screen(5, 80);
        let action = CopyModeHandler::handle(&ev(Modifiers::empty(), Key::Char('q')), &mut s, &scr);
        assert!(matches!(action, CopyModeAction::Exit));
    }
}
