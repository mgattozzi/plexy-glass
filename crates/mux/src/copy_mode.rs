//! Per-pane copy-mode state and a pure handler that consumes typed key
//! events to navigate scrollback, select content, and search.

use crate::{Direction, Key, KeyEvent, Modifiers, MouseButton, MouseEvent, MouseKind};
use plexy_glass_emulator::Screen;
use crate::blocks;
use crate::selection;
use std::mem;

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
            .saturating_add(u32::from(event.row))
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
            // Vertical wheel only scrolls copy mode; a horizontal wheel falls
            // through to the no-op arm rather than scrolling the wrong axis.
            (MouseKind::Wheel { delta, horizontal: false }, _) => {
                // Equivalent note (100:26): `> → >=` is equivalent because when delta == 0 the
                // else branch scrolls by 0 (no change), same as the if branch.
                if delta > 0 {
                    self.viewport_top = self.viewport_top.saturating_sub(delta as u32);
                } else {
                    let max_top = self
                        .total_lines
                        .saturating_sub(u32::from(self.pane_rows));
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
        let is_word = |c: usize| {
            cells
                .get(c)
                .is_some_and(|cell| selection::is_word_char(cell.grapheme.as_str()))
        };
        let is_spacer = |c: usize| cells.get(c).is_some_and(plexy_glass_emulator::Cell::is_wide_spacer);
        // A wide (CJK/emoji) grapheme occupies its cell plus a wide-spacer in the
        // next column. A click on that spacer (the glyph's right half) targets the
        // owning grapheme, and the outward walks must STEP OVER spacers, since
        // treating a spacer as a non-word cell would collapse the selection or
        // truncate the word at the first wide glyph. Mirrors selection.rs::word_at
        // (the quick-select path).
        let col = col as usize;
        let col = if col > 0 && is_spacer(col) { col - 1 } else { col };
        if !is_word(col) {
            return;
        }
        let mut start = col;
        while start > 0 {
            let candidate = start - 1;
            let grapheme = if candidate > 0 && is_spacer(candidate) { candidate - 1 } else { candidate };
            if is_word(grapheme) {
                start = grapheme;
            } else {
                break;
            }
        }
        // Include the click grapheme's own trailing spacer, then walk right.
        let mut end = if col + 1 < cols && is_spacer(col + 1) { col + 1 } else { col };
        while end + 1 < cols {
            let candidate = end + 1;
            if is_word(candidate) {
                end = if candidate + 1 < cols && is_spacer(candidate + 1) { candidate + 1 } else { candidate };
            } else {
                break;
            }
        }
        self.anchor = Some((line, start as u16));
        self.cursor = (line, end as u16);
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
        // Equivalent note (181:30): `> → >=` is equivalent because when
        // viewport_top == max_top the guard fires and sets viewport_top to
        // max_top (the value it already holds), a no-op.
        if self.viewport_top > max_top {
            self.viewport_top = max_top;
        }
    }
}

/// Consume one key event, mutate state, return the action the caller
/// should take.
pub fn handle(event: &KeyEvent, state: &mut CopyMode, screen: &Screen) -> CopyModeAction {
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
        (m, Key::Char('h') | Key::Arrow(Direction::Left)) if m.is_empty() => {
            state.cursor.1 = state.cursor.1.saturating_sub(1);
        }
        (m, Key::Char('l') | Key::Arrow(Direction::Right)) if m.is_empty() => {
            state.cursor.1 = (state.cursor.1 + 1).min(cols.saturating_sub(1));
        }
        (m, Key::Char('k') | Key::Arrow(Direction::Up)) if m.is_empty() => {
            state.cursor.0 = state.cursor.0.saturating_sub(1);
            ensure_visible(state);
        }
        (m, Key::Char('j') | Key::Arrow(Direction::Down)) if m.is_empty() => {
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
        // Shifted printables (`G`, `$`, `N`) arrive with empty mods on
        // legacy / modifyOtherKeys clients (the raw byte) and with SHIFT
        // only under Kitty disambiguation, so accept both, like the search
        // prompt's char handler below.
        (m, Key::Char('G')) if m.is_empty() || m == Modifiers::SHIFT => {
            state.cursor = (state.total_lines.saturating_sub(1), 0);
            ensure_visible(state);
        }
        (m, Key::Char('0')) if m.is_empty() => {
            state.cursor.1 = 0;
        }
        (m, Key::Char('$')) if m.is_empty() || m == Modifiers::SHIFT => {
            state.cursor.1 = cols.saturating_sub(1);
        }
        (m, Key::Char('[')) if m.is_empty() => {
            if let Some(line) = blocks::prev_prompt_line(screen, state.cursor.0) {
                state.cursor = (line, 0);
                ensure_visible(state);
            }
        }
        (m, Key::Char(']')) if m.is_empty() => {
            if let Some(line) = blocks::next_prompt_line(screen, state.cursor.0) {
                state.cursor = (line, 0);
                ensure_visible(state);
            }
        }
        (m, Key::Char('o')) if m.is_empty() => {
            // Select the current block's output region; `y` then yanks it.
            if let Some((start, end)) =
                blocks::block_output_range(screen, state.cursor.0)
            {
                state.anchor = Some((start, 0));
                state.cursor = (end, cols.saturating_sub(1));
                ensure_visible(state);
            }
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
        (m, Key::Char('N')) if m.is_empty() || m == Modifiers::SHIFT => {
            jump_to_prev_match(state);
        }
        _ => {}
    }
    CopyModeAction::Render
}

fn ensure_visible(state: &mut CopyMode) {
    // Equivalent note (319:23): `< → <=` is equivalent because when cursor.0 == vt
    // the guard fires and sets vt = cursor.0 = vt (a no-op).
    if state.cursor.0 < state.viewport_top {
        state.viewport_top = state.cursor.0;
    }
    // Equivalent note (323:23): `> → >=` is equivalent because when cursor.0 == bottom
    // the guard fires but sets vt = cursor.0 - (pane_rows-1) = vt (a no-op).
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

/// Extract the selected (or current-line) text from the unified
/// scrollback + active grid line space.
fn extract_selection(state: &CopyMode, screen: &Screen) -> String {
    let (start, end) = if let Some(anchor) = state.anchor { normalize(anchor, state.cursor) } else {
        let line = state.cursor.0;
        (
            (line, 0),
            (line, screen.active.num_cols().saturating_sub(1)),
        )
    };
    let cols = screen.active.num_cols();
    let mut out = String::new();

    for line in start.0..=end.0 {
        let row_start = if line == start.0 { start.1 } else { 0 };
        let row_end = if line == end.0 {
            end.1
        } else {
            cols.saturating_sub(1)
        };
        let Some(cells) = unified_line_cells(screen, line) else { continue };
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
            state.search.query = mem::take(&mut state.search.prompt_buf);
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
    let total = screen.scrollback.rows().len() as u32 + u32::from(screen.active.num_rows());
    for line_idx in 0..total {
        let Some(cells) = unified_line_cells(screen, line_idx) else { continue };
        // Build the line's text from non-spacer cells, recording where each
        // grapheme starts in BOTH the text (byte offset) and the grid (column).
        // A cell's grid column is its index, since the cells vector includes the
        // wide-spacer half of each wide grapheme.
        let mut line_text = String::new();
        let mut starts: Vec<(usize, u16)> = Vec::new();
        let mut grid_col = 0u16;
        for c in &cells {
            if c.is_wide_spacer() {
                grid_col += 1;
                continue;
            }
            starts.push((line_text.len(), grid_col));
            line_text.push_str(c.grapheme.as_str());
            grid_col += 1;
        }
        // The match occupies `display_width(query)` grid columns, starting at the
        // grid column of the grapheme at the matched byte offset.
        let span = plexy_glass_emulator::display_width(query).max(1);
        let mut start = 0usize;
        while let Some(idx) = line_text[start..].find(query) {
            let byte_off = start + idx;
            let col_start = starts
                .iter()
                .rev()
                .find(|(b, _)| *b <= byte_off)
                .map_or(0, |(_, gc)| *gc);
            let col_end = col_start
                .saturating_add(span.saturating_sub(1))
                .min(cols.saturating_sub(1));
            out.push(MatchSpan { line_idx, col_start, col_end });
            start += idx + query.len();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MouseModifiers;
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
    fn find_matches_maps_to_grid_columns_after_a_wide_char() {
        // 中 occupies grid columns 0-1, so "ab" begins at grid column 2.
        let s = screen_with_lines(4, 20, &["中ab"]);
        let m = find_matches(&s, "ab");
        assert_eq!(m.len(), 1);
        assert_eq!((m[0].col_start, m[0].col_end), (2, 3));
    }

    #[test]
    fn find_matches_wide_query_spans_correct_columns() {
        // x at col 0; "中文" starts at col 1 and occupies 4 grid columns (1..=4).
        let s = screen_with_lines(4, 20, &["x中文"]);
        let m = find_matches(&s, "中文");
        assert_eq!(m.len(), 1);
        assert_eq!((m[0].col_start, m[0].col_end), (1, 4));
    }

    #[test]
    fn find_matches_ascii_span_is_exact() {
        // Regression: "ab" at col 0 highlights exactly cols 0..=1, not 0..=2.
        let s = screen_with_lines(4, 20, &["ab cd"]);
        let m = find_matches(&s, "ab");
        assert_eq!(m.len(), 1);
        assert_eq!((m[0].col_start, m[0].col_end), (0, 1));
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
        handle(&ev(Modifiers::empty(), Key::Char('h')), &mut s, &scr);
        assert_eq!(s.cursor.1, 2);
        s.cursor.1 = 0;
        handle(&ev(Modifiers::empty(), Key::Char('h')), &mut s, &scr);
        assert_eq!(s.cursor.1, 0);
    }

    #[test]
    fn l_moves_cursor_right_and_clamps_at_pane_width() {
        let mut s = CopyMode::new(10, 5, 5, 78);
        let scr = screen(10, 80);
        handle(&ev(Modifiers::empty(), Key::Char('l')), &mut s, &scr);
        handle(&ev(Modifiers::empty(), Key::Char('l')), &mut s, &scr);
        assert_eq!(s.cursor.1, 79);
    }

    #[test]
    fn k_moves_up_and_scrolls_viewport() {
        let mut s = CopyMode::new(20, 5, 18, 0);
        assert_eq!(s.viewport_top, 15);
        let scr = screen(5, 80);
        for _ in 0..4 {
            handle(&ev(Modifiers::empty(), Key::Char('k')), &mut s, &scr);
        }
        assert_eq!(s.cursor.0, 14);
        assert!(s.viewport_top <= 14);
    }

    #[test]
    fn j_moves_down_and_clamps_at_total_lines() {
        let mut s = CopyMode::new(10, 5, 9, 0);
        let scr = screen(5, 80);
        handle(&ev(Modifiers::empty(), Key::Char('j')), &mut s, &scr);
        assert_eq!(s.cursor.0, 9);
    }

    #[test]
    fn page_up_jumps_by_pane_rows() {
        let mut s = CopyMode::new(50, 10, 40, 0);
        let scr = screen(10, 80);
        handle(&ev(Modifiers::empty(), Key::PageUp), &mut s, &scr);
        assert_eq!(s.cursor.0, 30);
    }

    #[test]
    fn ctrl_d_jumps_by_half_pane() {
        let mut s = CopyMode::new(50, 10, 20, 0);
        let scr = screen(10, 80);
        handle(&ev(Modifiers::CTRL, Key::Char('d')), &mut s, &scr);
        assert_eq!(s.cursor.0, 25);
    }

    #[test]
    fn g_jumps_to_top() {
        let mut s = CopyMode::new(50, 10, 30, 5);
        let scr = screen(10, 80);
        handle(&ev(Modifiers::empty(), Key::Char('g')), &mut s, &scr);
        assert_eq!(s.cursor, (0, 0));
    }

    #[test]
    fn shift_g_jumps_to_bottom() {
        let mut s = CopyMode::new(50, 10, 10, 5);
        let scr = screen(10, 80);
        handle(&ev(Modifiers::SHIFT, Key::Char('G')), &mut s, &scr);
        assert_eq!(s.cursor, (49, 0));
    }

    #[test]
    fn zero_jumps_to_col_zero() {
        let mut s = CopyMode::new(50, 10, 10, 22);
        let scr = screen(10, 80);
        handle(&ev(Modifiers::empty(), Key::Char('0')), &mut s, &scr);
        assert_eq!(s.cursor.1, 0);
    }

    #[test]
    fn dollar_jumps_to_last_col() {
        let mut s = CopyMode::new(50, 10, 10, 0);
        let scr = screen(10, 80);
        handle(&ev(Modifiers::SHIFT, Key::Char('$')), &mut s, &scr);
        assert_eq!(s.cursor.1, 79);
    }

    #[test]
    fn shift_g_jumps_to_bottom_with_empty_mods() {
        // Legacy / modifyOtherKeys clients deliver Shift+g as a bare 'G' byte
        // with empty mods, so the motion must still fire (regression guard).
        let mut s = CopyMode::new(50, 10, 10, 5);
        let scr = screen(10, 80);
        handle(&ev(Modifiers::empty(), Key::Char('G')), &mut s, &scr);
        assert_eq!(s.cursor, (49, 0));
    }

    #[test]
    fn dollar_jumps_to_last_col_with_empty_mods() {
        let mut s = CopyMode::new(50, 10, 10, 0);
        let scr = screen(10, 80);
        handle(&ev(Modifiers::empty(), Key::Char('$')), &mut s, &scr);
        assert_eq!(s.cursor.1, 79);
    }

    #[test]
    fn capital_n_cycles_to_prev_match_with_wrap() {
        let scr = screen_with_lines(3, 30, &["foo", "foo bar foo", "foo baz"]);
        let mut s = CopyMode::new(3, 3, 0, 0);
        s.search.prompt_active = true;
        s.search.prompt_buf = "foo".into();
        handle(&ev(Modifiers::empty(), Key::Enter), &mut s, &scr);
        let len = s.search.matches.len();
        assert!(len >= 2, "need multiple matches, got {len}");
        // From the first match (index 0), previous wraps to the last match.
        // Also exercises the empty-mods 'N' wire shape legacy clients send.
        s.search.current = 0;
        handle(&ev(Modifiers::empty(), Key::Char('N')), &mut s, &scr);
        assert_eq!(s.search.current, len - 1);
        let last = &s.search.matches[len - 1];
        assert_eq!(s.cursor, (last.line_idx, last.col_start));
        // Stepping back once more lands on the second-to-last (no wrap).
        handle(&ev(Modifiers::empty(), Key::Char('N')), &mut s, &scr);
        assert_eq!(s.search.current, len - 2);
    }

    #[test]
    fn v_toggles_anchor() {
        let mut s = CopyMode::new(10, 5, 3, 2);
        let scr = screen(5, 80);
        handle(&ev(Modifiers::empty(), Key::Char('v')), &mut s, &scr);
        assert_eq!(s.anchor, Some((3, 2)));
        handle(&ev(Modifiers::empty(), Key::Char('v')), &mut s, &scr);
        assert_eq!(s.anchor, None);
    }

    #[test]
    fn y_with_selection_yanks_text() {
        let scr = screen_with_lines(2, 10, &["hello", "world"]);
        let mut s = CopyMode::new(2, 2, 0, 0);
        s.anchor = Some((0, 0));
        s.cursor = (0, 4);
        let action = handle(&ev(Modifiers::empty(), Key::Char('y')), &mut s, &scr);
        match action {
            CopyModeAction::Yank(text) => assert_eq!(text, "hello"),
            other => panic!("expected Yank, got {other:?}"),
        }
    }

    #[test]
    fn y_without_selection_yanks_current_line() {
        let scr = screen_with_lines(2, 10, &["hello", "world"]);
        let mut s = CopyMode::new(2, 2, 1, 0);
        let action = handle(&ev(Modifiers::empty(), Key::Char('y')), &mut s, &scr);
        match action {
            CopyModeAction::Yank(text) => assert!(text.contains("world"), "got: {text:?}"),
            other => panic!("expected Yank, got {other:?}"),
        }
    }

    #[test]
    fn slash_opens_search_prompt() {
        let mut s = CopyMode::new(10, 5, 0, 0);
        let scr = screen(5, 80);
        handle(&ev(Modifiers::empty(), Key::Char('/')), &mut s, &scr);
        assert!(s.search.prompt_active);
    }

    #[test]
    fn search_prompt_appends_typed_chars() {
        let mut s = CopyMode::new(10, 5, 0, 0);
        let scr = screen(5, 80);
        s.search.prompt_active = true;
        for c in ['f', 'o', 'o'] {
            handle(&ev(Modifiers::empty(), Key::Char(c)), &mut s, &scr);
        }
        assert_eq!(s.search.prompt_buf, "foo");
    }

    #[test]
    fn search_prompt_backspace_deletes() {
        let mut s = CopyMode::new(10, 5, 0, 0);
        let scr = screen(5, 80);
        s.search.prompt_active = true;
        s.search.prompt_buf = "foo".into();
        handle(&ev(Modifiers::empty(), Key::Backspace), &mut s, &scr);
        assert_eq!(s.search.prompt_buf, "fo");
    }

    #[test]
    fn search_submit_jumps_to_first_match_below_cursor() {
        let scr = screen_with_lines(3, 30, &["alpha", "beta passwd here", "gamma"]);
        let mut s = CopyMode::new(3, 3, 0, 0);
        s.search.prompt_active = true;
        s.search.prompt_buf = "passwd".into();
        handle(&ev(Modifiers::empty(), Key::Enter), &mut s, &scr);
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
        handle(&ev(Modifiers::empty(), Key::Enter), &mut s, &scr);
        assert!(s.search.matches.len() >= 2);
        let first_idx = s.search.current;
        handle(&ev(Modifiers::empty(), Key::Char('n')), &mut s, &scr);
        assert_ne!(s.search.current, first_idx);
    }

    #[test]
    fn search_empty_query_clears_state() {
        let scr = screen_with_lines(3, 30, &["alpha", "beta", "gamma"]);
        let mut s = CopyMode::new(3, 3, 0, 0);
        s.search.prompt_active = true;
        s.search.prompt_buf = String::new();
        handle(&ev(Modifiers::empty(), Key::Enter), &mut s, &scr);
        assert!(s.search.matches.is_empty());
        assert!(s.search.query.is_empty());
    }

    #[test]
    fn search_no_match_leaves_cursor() {
        let scr = screen_with_lines(3, 30, &["alpha", "beta", "gamma"]);
        let mut s = CopyMode::new(3, 3, 1, 0);
        s.search.prompt_active = true;
        s.search.prompt_buf = "zzzzz".into();
        handle(&ev(Modifiers::empty(), Key::Enter), &mut s, &scr);
        assert!(s.search.matches.is_empty());
        assert_eq!(s.cursor.0, 1);
    }

    #[test]
    fn escape_in_prompt_closes_prompt_only() {
        let mut s = CopyMode::new(10, 5, 0, 0);
        let scr = screen(5, 80);
        s.search.prompt_active = true;
        s.search.prompt_buf = "abc".into();
        let action = handle(&ev(Modifiers::empty(), Key::Escape), &mut s, &scr);
        assert!(matches!(action, CopyModeAction::Render));
        assert!(!s.search.prompt_active);
        assert!(s.search.prompt_buf.is_empty());
    }

    #[test]
    fn escape_with_selection_clears_selection() {
        let mut s = CopyMode::new(10, 5, 5, 3);
        s.anchor = Some((0, 0));
        let scr = screen(5, 80);
        let action = handle(&ev(Modifiers::empty(), Key::Escape), &mut s, &scr);
        assert!(matches!(action, CopyModeAction::Render));
        assert!(s.anchor.is_none());
    }

    #[test]
    fn escape_in_normal_mode_exits() {
        let mut s = CopyMode::new(10, 5, 5, 3);
        let scr = screen(5, 80);
        let action = handle(&ev(Modifiers::empty(), Key::Escape), &mut s, &scr);
        assert!(matches!(action, CopyModeAction::Exit));
    }

    /// Feed raw bytes (text + OSC 133 marks) through a real emulator; the
    /// trailing SGR-reset flushes the pending grapheme into the grid.
    fn screen_from_bytes(rows: u16, cols: u16, bytes: &[u8]) -> Screen {
        let mut e = Emulator::new(rows, cols);
        e.advance(bytes);
        e.advance(b"\x1b[m");
        e.screen().clone()
    }

    /// 8-row grid: A "$ one" line 0, C "hello" line 1, "world" line 2,
    /// A "$ two" line 3 (rest blank). No scrollback.
    fn marked_screen() -> Screen {
        screen_from_bytes(
            8,
            20,
            b"\x1b]133;A\x07$ one\r\n\x1b]133;C\x07hello\r\nworld\r\n\x1b]133;A\x07$ two",
        )
    }

    /// 3-row grid fed 6 lines: lines 0..2 scrolled into scrollback.
    /// Prompts at absolute lines 0 (scrollback) and 4 (grid).
    fn marked_screen_with_scrollback() -> Screen {
        let s = screen_from_bytes(
            3,
            20,
            b"\x1b]133;A\x07p1\r\no1\r\no2\r\no3\r\n\x1b]133;A\x07p2\r\nx",
        );
        assert_eq!(s.scrollback.rows().len(), 3, "setup: 3 rows scrolled");
        s
    }

    fn total(s: &Screen) -> u32 {
        s.scrollback.rows().len() as u32 + s.active.rows.len() as u32
    }

    #[test]
    fn open_bracket_jumps_to_prev_prompt_col_zero() {
        let scr = marked_screen();
        let mut s = CopyMode::new(total(&scr), 8, 6, 5);
        handle(&ev(Modifiers::empty(), Key::Char('[')), &mut s, &scr);
        assert_eq!(s.cursor, (3, 0));
        handle(&ev(Modifiers::empty(), Key::Char('[')), &mut s, &scr);
        assert_eq!(s.cursor, (0, 0));
    }

    #[test]
    fn open_bracket_at_oldest_prompt_is_a_noop() {
        let scr = marked_screen();
        let mut s = CopyMode::new(total(&scr), 8, 0, 4);
        handle(&ev(Modifiers::empty(), Key::Char('[')), &mut s, &scr);
        assert_eq!(s.cursor, (0, 4), "no wrap, cursor untouched");
    }

    #[test]
    fn close_bracket_jumps_to_next_prompt_col_zero() {
        let scr = marked_screen();
        let mut s = CopyMode::new(total(&scr), 8, 0, 5);
        handle(&ev(Modifiers::empty(), Key::Char(']')), &mut s, &scr);
        assert_eq!(s.cursor, (3, 0));
    }

    #[test]
    fn close_bracket_at_newest_prompt_is_a_noop() {
        let scr = marked_screen();
        let mut s = CopyMode::new(total(&scr), 8, 3, 2);
        handle(&ev(Modifiers::empty(), Key::Char(']')), &mut s, &scr);
        assert_eq!(s.cursor, (3, 2), "no wrap, cursor untouched");
    }

    #[test]
    fn brackets_cross_the_scrollback_boundary_and_scroll_the_viewport() {
        let scr = marked_screen_with_scrollback();
        let mut s = CopyMode::new(total(&scr), 3, 5, 0);
        assert_eq!(s.viewport_top, 3);
        // Grid prompt first, then the scrollback one.
        handle(&ev(Modifiers::empty(), Key::Char('[')), &mut s, &scr);
        assert_eq!(s.cursor, (4, 0));
        handle(&ev(Modifiers::empty(), Key::Char('[')), &mut s, &scr);
        assert_eq!(s.cursor, (0, 0), "prompt found in scrollback");
        assert_eq!(s.viewport_top, 0, "ensure_visible scrolled up");
        // And back down across the boundary.
        handle(&ev(Modifiers::empty(), Key::Char(']')), &mut s, &scr);
        assert_eq!(s.cursor, (4, 0));
        assert!(s.viewport_top + 2 >= 4, "ensure_visible scrolled down");
    }

    #[test]
    fn o_selects_the_output_region_of_the_current_block() {
        let scr = marked_screen();
        let mut s = CopyMode::new(total(&scr), 8, 2, 3);
        handle(&ev(Modifiers::empty(), Key::Char('o')), &mut s, &scr);
        // Output of block 1: C line 1 col 0 → block end line 2, last col.
        assert_eq!(s.anchor, Some((1, 0)));
        assert_eq!(s.cursor, (2, 19));
    }

    #[test]
    fn o_is_idempotent_on_repress() {
        let scr = marked_screen();
        let mut s = CopyMode::new(total(&scr), 8, 1, 0);
        handle(&ev(Modifiers::empty(), Key::Char('o')), &mut s, &scr);
        let (anchor, cursor) = (s.anchor, s.cursor);
        handle(&ev(Modifiers::empty(), Key::Char('o')), &mut s, &scr);
        assert_eq!((s.anchor, s.cursor), (anchor, cursor));
    }

    #[test]
    fn o_falls_back_to_the_prompt_line_without_output_start() {
        // No 133;C: selection starts at the prompt row itself.
        let scr = screen_from_bytes(6, 20, b"\x1b]133;A\x07$ a\r\nout\r\n\x1b]133;A\x07$ b");
        let mut s = CopyMode::new(total(&scr), 6, 1, 0);
        handle(&ev(Modifiers::empty(), Key::Char('o')), &mut s, &scr);
        assert_eq!(s.anchor, Some((0, 0)));
        assert_eq!(s.cursor, (1, 19));
    }

    #[test]
    fn o_is_a_noop_when_no_block_contains_the_cursor() {
        let scr = screen_from_bytes(6, 20, b"plain\r\n\x1b]133;A\x07$ a");
        let mut s = CopyMode::new(total(&scr), 6, 0, 2);
        handle(&ev(Modifiers::empty(), Key::Char('o')), &mut s, &scr);
        assert_eq!(s.anchor, None);
        assert_eq!(s.cursor, (0, 2));
    }

    #[test]
    fn o_then_y_yanks_the_block_output_text() {
        let scr = marked_screen();
        let mut s = CopyMode::new(total(&scr), 8, 2, 3);
        handle(&ev(Modifiers::empty(), Key::Char('o')), &mut s, &scr);
        let action =
            handle(&ev(Modifiers::empty(), Key::Char('y')), &mut s, &scr);
        match action {
            CopyModeAction::Yank(text) => assert_eq!(text, "hello\nworld"),
            other => panic!("expected Yank, got {other:?}"),
        }
    }

    #[test]
    fn o_then_y_spans_the_scrollback_boundary() {
        let scr = marked_screen_with_scrollback();
        // Cursor inside block 1 (lines 0..=3, no C → starts at the prompt).
        let mut s = CopyMode::new(total(&scr), 3, 2, 0);
        handle(&ev(Modifiers::empty(), Key::Char('o')), &mut s, &scr);
        assert_eq!(s.anchor, Some((0, 0)));
        assert_eq!(s.cursor, (3, 19));
        let action =
            handle(&ev(Modifiers::empty(), Key::Char('y')), &mut s, &scr);
        match action {
            CopyModeAction::Yank(text) => assert_eq!(text, "p1\no1\no2\no3"),
            other => panic!("expected Yank, got {other:?}"),
        }
    }

    #[test]
    fn q_in_normal_mode_exits() {
        let mut s = CopyMode::new(10, 5, 5, 3);
        let scr = screen(5, 80);
        let action = handle(&ev(Modifiers::empty(), Key::Char('q')), &mut s, &scr);
        assert!(matches!(action, CopyModeAction::Exit));
    }

    fn mouse_ev(kind: MouseKind, button: MouseButton, row: u16, col: u16) -> MouseEvent {
        MouseEvent {
            kind,
            button,
            modifiers: MouseModifiers::default(),
            row,
            col,
        }
    }

    #[test]
    fn j_moves_cursor_down_from_middle() {
        // Kills: 227:67 → false (j arm guard), 229:46 `+ → *` (`cursor + 1` → `cursor * 1`).
        // The clamping test uses cursor at max; moving from mid-range exposes the bug.
        let scr = screen(5, 80);
        let mut s = CopyMode::new(20, 5, 5, 0);
        handle(&ev(Modifiers::empty(), Key::Char('j')), &mut s, &scr);
        assert_eq!(s.cursor.0, 6);
    }

    #[test]
    fn page_down_jumps_by_pane_rows() {
        // Kills: 236:31 → false (PageDown guard), 238:46 `+ → -` / `+ → *`.
        let scr = screen(10, 80);
        let mut s = CopyMode::new(50, 10, 30, 0);
        handle(&ev(Modifiers::empty(), Key::PageDown), &mut s, &scr);
        assert_eq!(s.cursor.0, 40);
    }

    #[test]
    fn ctrl_u_jumps_by_half_pane() {
        // Kills: 247:32 → false / → != (Ctrl+U guard), 248:50 `/ → %` / `/ → *`.
        let scr = screen(10, 80);
        let mut s = CopyMode::new(50, 10, 30, 0);
        handle(&ev(Modifiers::CTRL, Key::Char('u')), &mut s, &scr);
        assert_eq!(s.cursor.0, 25);
    }

    #[test]
    fn j_scrolls_viewport_down_when_cursor_past_bottom() {
        // Kills: 322:37 `+ → *` in ensure_visible.
        // viewport_top=5, pane_rows=5 → real bottom=9.
        // Mutation computes bottom = 5 * 4 = 20; cursor=10 ≤ 20 → no scroll (stays 5).
        // Original: cursor=10 > bottom=9 → viewport_top adjusted to 6.
        let scr = screen(5, 80);
        let mut s = CopyMode::new(20, 5, 9, 0);
        s.viewport_top = 5;
        handle(&ev(Modifiers::empty(), Key::Char('j')), &mut s, &scr);
        assert_eq!(s.cursor.0, 10);
        assert_eq!(s.viewport_top, 6, "viewport must scroll down to keep cursor visible");
    }

    #[test]
    fn mouse_press_single_click_sets_cursor() {
        // Kills: 77:13 (delete Press arm), the Press arm is the only path that
        // sets cursor from a mouse event.
        let scr = screen(10, 80);
        let mut s = CopyMode::new(10, 10, 0, 0);
        s.viewport_top = 0;
        let me = mouse_ev(MouseKind::Press, MouseButton::Left, 3, 20);
        s.handle_mouse(&me, 1, &scr);
        assert_eq!(s.cursor, (3, 20));
        assert_eq!(s.anchor, Some((3, 20)));
    }

    #[test]
    fn mouse_move_updates_cursor_and_sets_anchor_when_none() {
        // Kills: 87:13 (delete Move arm).
        let scr = screen(10, 80);
        let mut s = CopyMode::new(10, 10, 0, 0);
        s.viewport_top = 0;
        let me = mouse_ev(MouseKind::Move, MouseButton::Left, 3, 15);
        s.handle_mouse(&me, 1, &scr);
        assert_eq!(s.cursor, (3, 15));
        assert!(s.anchor.is_some(), "Move with no anchor should set anchor");
    }

    #[test]
    fn mouse_press_double_click_selects_word() {
        // Kills: 77:13, 80:32 (>= → <), 82:39 (== → !=), 115:9, 118–143 (select_word).
        // "hello world": 'w' is at col 6. Double-click at col 6 expands to "world" (6–10).
        let scr = screen_with_lines(10, 30, &["hello world"]);
        let mut s = CopyMode::new(10, 10, 0, 0);
        s.viewport_top = 0;
        let me = mouse_ev(MouseKind::Press, MouseButton::Left, 0, 6);
        s.handle_mouse(&me, 2, &scr);
        assert_eq!(s.anchor, Some((0, 6)), "anchor at word start");
        assert_eq!(s.cursor.1, 10, "cursor at word end");
    }

    #[test]
    fn mouse_double_click_mid_word_walks_backward() {
        // Kills 131:21 `> → ==` and `> → <` survivors: those mutations disable the
        // backward walk so anchor stays at the click column instead of the word start.
        // Click at col 8 ('r' in "world"): the backward walk must reach col 6 ('w').
        let scr = screen_with_lines(10, 30, &["hello world"]);
        let mut s = CopyMode::new(10, 10, 0, 0);
        s.viewport_top = 0;
        let me = mouse_ev(MouseKind::Press, MouseButton::Left, 0, 8);
        s.handle_mouse(&me, 2, &scr);
        assert_eq!(s.anchor, Some((0, 6)), "anchor must walk back to word start");
        assert_eq!(s.cursor.1, 10, "cursor must reach word end");
    }

    #[test]
    fn mouse_double_click_on_wide_char_spacer_selects_word() {
        // "中文": col0='中', col1=spacer, col2='文', col3=spacer. A double-click
        // on col 1 (中's right half) must select the whole word, not nothing.
        let scr = screen_with_lines(10, 30, &["中文"]);
        let mut s = CopyMode::new(10, 10, 0, 0);
        s.viewport_top = 0;
        let me = mouse_ev(MouseKind::Press, MouseButton::Left, 0, 1);
        s.handle_mouse(&me, 2, &scr);
        assert_eq!(s.anchor, Some((0, 0)), "anchor at word start");
        assert_eq!(s.cursor.1, 3, "cursor at word end (文's trailing spacer)");
        // …and `y` yanks the non-empty word.
        let action = handle(&ev(Modifiers::empty(), Key::Char('y')), &mut s, &scr);
        match action {
            CopyModeAction::Yank(text) => assert_eq!(text, "中文"),
            other => panic!("expected Yank, got {other:?}"),
        }
    }

    #[test]
    fn mouse_double_click_on_wide_char_base_selects_word() {
        // Clicking the base half (col 2 = 文) must select the same word.
        let scr = screen_with_lines(10, 30, &["中文"]);
        let mut s = CopyMode::new(10, 10, 0, 0);
        s.viewport_top = 0;
        let me = mouse_ev(MouseKind::Press, MouseButton::Left, 0, 2);
        s.handle_mouse(&me, 2, &scr);
        assert_eq!(s.anchor, Some((0, 0)));
        assert_eq!(s.cursor.1, 3);
    }

    #[test]
    fn mouse_double_click_left_walk_crosses_wide_spacer() {
        // "ab中cd": a0 b1 中2 spacer3 c4 d5. Clicking 'd' (col 5) requires the
        // backward walk to STEP OVER 中's spacer (col 3) to reach the word start.
        let scr = screen_with_lines(10, 30, &["ab中cd"]);
        let mut s = CopyMode::new(10, 10, 0, 0);
        s.viewport_top = 0;
        let me = mouse_ev(MouseKind::Press, MouseButton::Left, 0, 5);
        s.handle_mouse(&me, 2, &scr);
        assert_eq!(s.anchor, Some((0, 0)), "left walk crosses 中's spacer to word start");
        assert_eq!(s.cursor.1, 5, "cursor at 'd'");
        let action = handle(&ev(Modifiers::empty(), Key::Char('y')), &mut s, &scr);
        match action {
            CopyModeAction::Yank(text) => assert_eq!(text, "ab中cd"),
            other => panic!("expected Yank, got {other:?}"),
        }
    }

    #[test]
    fn mouse_press_triple_click_selects_line() {
        // Kills: 77:13, 80:32 (>= → <), 160:9, 164:16 (delete ! in select_line).
        // "hello world": last non-blank is col 10 ('d').
        let scr = screen_with_lines(10, 30, &["hello world"]);
        let mut s = CopyMode::new(10, 10, 0, 0);
        s.viewport_top = 0;
        let me = mouse_ev(MouseKind::Press, MouseButton::Left, 0, 3);
        s.handle_mouse(&me, 3, &scr);
        assert_eq!(s.anchor, Some((0, 0)), "anchor at col 0");
        assert_eq!(s.cursor.1, 10, "cursor at last non-blank col");
    }

    #[test]
    fn mouse_wheel_up_scrolls_viewport_up() {
        // Kills: 97:13 (delete Wheel arm), 98:26 (> → ==/<), 104:60/63 (arithmetic).
        let scr = screen(10, 80);
        let mut s = CopyMode::new(20, 10, 0, 0);
        s.viewport_top = 10;
        let me = mouse_ev(
            MouseKind::Wheel { delta: 3, horizontal: false },
            MouseButton::None, 0, 0,
        );
        s.handle_mouse(&me, 1, &scr);
        assert_eq!(s.viewport_top, 7, "positive delta decreases viewport_top");
    }

    #[test]
    fn mouse_wheel_down_scrolls_viewport_down() {
        // Kills: 97:13 (delete Wheel arm), 98:26 (> → ==/<), 104:60/63 (arithmetic).
        let scr = screen(10, 80);
        let mut s = CopyMode::new(20, 10, 0, 0);
        s.viewport_top = 5;
        let me = mouse_ev(
            MouseKind::Wheel { delta: -3, horizontal: false },
            MouseButton::None, 0, 0,
        );
        s.handle_mouse(&me, 1, &scr);
        assert_eq!(s.viewport_top, 8, "negative delta increases viewport_top by |delta|");
    }

    #[test]
    fn extract_selection_respects_anchor_and_cursor_columns() {
        // Kills: 363:33 (== → !=), row_start becomes 0 instead of start.1,
        //   yielding "hello wo" instead of "wo".
        // Kills: 364:31 (== → !=), row_end becomes cols-1 instead of end.1,
        //   yielding "world" instead of "wo".
        let scr = screen_with_lines(2, 20, &["hello world", "more"]);
        let mut s = CopyMode::new(2, 2, 0, 0);
        s.anchor = Some((0, 6)); // 'w'
        s.cursor = (0, 7);       // 'o'
        let action = handle(&ev(Modifiers::empty(), Key::Char('y')), &mut s, &scr);
        match action {
            CopyModeAction::Yank(text) => assert_eq!(text, "wo"),
            other => panic!("expected Yank, got {other:?}"),
        }
    }

    // ── Modifier-guard tests ─────────────────────────────────────────────────────────
    // All copy-mode keys require empty modifiers (or SHIFT for some). If the
    // `m.is_empty()` guards were mutated to `true`, Ctrl-modified keys would
    // fire their action instead of falling through to `_ => {}` → `Render`.

    #[test]
    fn motion_modifier_guards_reject_ctrl() {
        // Kills the `m.is_empty() → true` mutants on the h/l/k/j/PageUp/PageDown/
        // g/0/v/y//n arms. Without the guard, Ctrl+h decrements cursor.1, etc.
        // With the guard in place, cursor and anchor are untouched.
        let scr = screen(10, 80);
        let mut s = CopyMode::new(50, 10, 20, 10);
        let start = s.cursor;

        // Ctrl+h must NOT decrement cursor.1
        handle(&ev(Modifiers::CTRL, Key::Char('h')), &mut s, &scr);
        assert_eq!(s.cursor, start, "Ctrl+h must not move cursor left");

        // Ctrl+l must NOT increment cursor.1
        handle(&ev(Modifiers::CTRL, Key::Char('l')), &mut s, &scr);
        assert_eq!(s.cursor, start, "Ctrl+l must not move cursor right");

        // Ctrl+k must NOT decrement cursor.0
        handle(&ev(Modifiers::CTRL, Key::Char('k')), &mut s, &scr);
        assert_eq!(s.cursor.0, start.0, "Ctrl+k must not move cursor up");

        // Ctrl+j must NOT increment cursor.0
        handle(&ev(Modifiers::CTRL, Key::Char('j')), &mut s, &scr);
        assert_eq!(s.cursor.0, start.0, "Ctrl+j must not move cursor down");

        // Ctrl+PageUp must NOT subtract pane_rows from cursor.0
        handle(&ev(Modifiers::CTRL, Key::PageUp), &mut s, &scr);
        assert_eq!(s.cursor.0, start.0, "Ctrl+PageUp must not page up");

        // Ctrl+PageDown must NOT add pane_rows to cursor.0
        handle(&ev(Modifiers::CTRL, Key::PageDown), &mut s, &scr);
        assert_eq!(s.cursor.0, start.0, "Ctrl+PageDown must not page down");

        // Ctrl+g must NOT jump to (0, 0)
        handle(&ev(Modifiers::CTRL, Key::Char('g')), &mut s, &scr);
        assert_eq!(s.cursor, start, "Ctrl+g must not jump to top");

        // Ctrl+0 must NOT set cursor.1 to 0
        handle(&ev(Modifiers::CTRL, Key::Char('0')), &mut s, &scr);
        assert_eq!(s.cursor.1, start.1, "Ctrl+0 must not jump to col 0");

        // Ctrl+v must NOT set anchor
        handle(&ev(Modifiers::CTRL, Key::Char('v')), &mut s, &scr);
        assert_eq!(s.anchor, None, "Ctrl+v must not toggle anchor");

        // Ctrl+y: guard is `m.is_empty()` so returns Render (not Yank)
        let action = handle(&ev(Modifiers::CTRL, Key::Char('y')), &mut s, &scr);
        assert_eq!(action, CopyModeAction::Render, "Ctrl+y must not yank");

        // Ctrl+/ must NOT open the search prompt
        handle(&ev(Modifiers::CTRL, Key::Char('/')), &mut s, &scr);
        assert!(!s.search.prompt_active, "Ctrl+/ must not open search prompt");

        // Ctrl+n with pre-set matches: must NOT advance search.current
        s.search.matches = vec![
            MatchSpan { line_idx: 5, col_start: 0, col_end: 2 },
            MatchSpan { line_idx: 15, col_start: 0, col_end: 2 },
        ];
        s.search.current = 0;
        handle(&ev(Modifiers::CTRL, Key::Char('n')), &mut s, &scr);
        assert_eq!(s.search.current, 0, "Ctrl+n must not advance to next match");
        assert_eq!(s.cursor, start, "Ctrl+n must not move cursor");
    }

    #[test]
    fn motion_shift_or_empty_guards_reject_ctrl() {
        // Kills the `m.is_empty() || m == SHIFT → true` mutants on G/$/ N.
        // With guard→true, Ctrl+G jumps cursor to bottom; Ctrl+$ to last col;
        // Ctrl+N cycles to the previous match. Real guard rejects CTRL.
        let scr = screen(10, 80);
        let mut s = CopyMode::new(50, 10, 20, 10);
        let start = s.cursor;

        // Ctrl+G must NOT jump to the last line
        handle(&ev(Modifiers::CTRL, Key::Char('G')), &mut s, &scr);
        assert_eq!(s.cursor, start, "Ctrl+G must not jump to bottom");

        // Ctrl+$ must NOT jump to the last column
        handle(&ev(Modifiers::CTRL, Key::Char('$')), &mut s, &scr);
        assert_eq!(s.cursor.1, start.1, "Ctrl+$ must not jump to last col");

        // Ctrl+N with pre-set matches: must NOT cycle to prev match
        s.search.matches = vec![
            MatchSpan { line_idx: 5, col_start: 0, col_end: 2 },
            MatchSpan { line_idx: 15, col_start: 0, col_end: 2 },
        ];
        s.search.current = 1;
        handle(&ev(Modifiers::CTRL, Key::Char('N')), &mut s, &scr);
        assert_eq!(s.search.current, 1, "Ctrl+N must not cycle to prev match");
        assert_eq!(s.cursor, start, "Ctrl+N must not move cursor");
    }

    #[test]
    fn motion_bracket_and_o_guards_reject_ctrl() {
        // Kills the `m.is_empty() → true` mutants on [ / ] / o: with the guard,
        // Ctrl+[ must not jump to prev prompt; Ctrl+] must not jump to next;
        // Ctrl+o must not set anchor/cursor to the block output range.
        let scr = marked_screen(); // has prompts at unified lines 0 and 3
        let total_lines = total(&scr);
        // Cursor at line 6, col 5, inside the second block's output.
        let mut s = CopyMode::new(total_lines, 8, 6, 5);
        let start = s.cursor;

        // Ctrl+[ must NOT jump to prev prompt (line 3)
        handle(&ev(Modifiers::CTRL, Key::Char('[')), &mut s, &scr);
        assert_eq!(s.cursor, start, "Ctrl+[ must not jump to prev prompt");

        // Ctrl+] must NOT jump to next prompt. From line 6 there is no prompt after line 3,
        // so it would be a no-op anyway; test from a position where a next prompt exists.
        let mut s2 = CopyMode::new(total_lines, 8, 1, 5);
        let start2 = s2.cursor;
        handle(&ev(Modifiers::CTRL, Key::Char(']')), &mut s2, &scr);
        assert_eq!(s2.cursor, start2, "Ctrl+] must not jump to next prompt");

        // Ctrl+o must NOT select the block output range
        let mut s3 = CopyMode::new(total_lines, 8, 2, 3);
        let start3 = s3.cursor;
        handle(&ev(Modifiers::CTRL, Key::Char('o')), &mut s3, &scr);
        assert_eq!(s3.anchor, None, "Ctrl+o must not set anchor");
        assert_eq!(s3.cursor, start3, "Ctrl+o must not move cursor");
    }

    #[test]
    fn ctrl_d_and_u_require_ctrl_modifier() {
        // Kills the `m == Modifiers::CTRL → true` mutants on d and u.
        // With guard→true, plain d does half-page-down; plain u does half-page-up.
        let scr = screen(10, 80);
        let mut s = CopyMode::new(50, 10, 20, 0);
        let start_line = s.cursor.0; // 20

        // Plain d (no mods): must NOT jump half-page-down
        let action = handle(&ev(Modifiers::empty(), Key::Char('d')), &mut s, &scr);
        assert_eq!(action, CopyModeAction::Render);
        assert_eq!(s.cursor.0, start_line, "plain d must not do half-page jump");

        // Plain u (no mods): must NOT jump half-page-up
        let action = handle(&ev(Modifiers::empty(), Key::Char('u')), &mut s, &scr);
        assert_eq!(action, CopyModeAction::Render);
        assert_eq!(s.cursor.0, start_line, "plain u must not do half-page jump");
    }

    #[test]
    fn search_prompt_ctrl_keys_are_ignored() {
        // Kills (Enter/Backspace/Char guard → true): Ctrl+Enter would commit the
        // search; Ctrl+Backspace would pop a char; Ctrl+Char would push.
        // Real guards (`m.is_empty()`) reject all Ctrl-modified events.
        let scr = screen(5, 80);
        let mut s = CopyMode::new(10, 5, 0, 0);
        s.search.prompt_active = true;
        s.search.prompt_buf = "abc".into();

        // Ctrl+Enter must NOT commit the query
        handle(&ev(Modifiers::CTRL, Key::Enter), &mut s, &scr);
        assert!(s.search.prompt_active, "Ctrl+Enter must not commit search");
        assert_eq!(s.search.prompt_buf, "abc", "prompt_buf unchanged after Ctrl+Enter");

        // Ctrl+Backspace must NOT pop a char
        handle(&ev(Modifiers::CTRL, Key::Backspace), &mut s, &scr);
        assert_eq!(s.search.prompt_buf, "abc", "Ctrl+Backspace must not pop from prompt");

        // Ctrl+Char must NOT push a char
        handle(&ev(Modifiers::CTRL, Key::Char('x')), &mut s, &scr);
        assert_eq!(s.search.prompt_buf, "abc", "Ctrl+Char must not push to prompt");
        assert!(s.search.prompt_active, "must still be in search prompt mode");
    }

    #[test]
    fn search_prompt_shift_char_accepted_ctrl_char_rejected() {
        // Kills the `m == Modifiers::SHIFT → m != Modifiers::SHIFT` mutant on the
        // Char arm guard (`m.is_empty() || m == SHIFT`). The wrong guard would
        // accept Ctrl+Char but reject Shift+Char, the opposite of the real behavior.
        let scr = screen(5, 80);
        let mut s = CopyMode::new(10, 5, 0, 0);
        s.search.prompt_active = true;

        // Shift+F: must push 'F' (capital letters arrive as Shift+char under Kitty)
        handle(&ev(Modifiers::SHIFT, Key::Char('F')), &mut s, &scr);
        assert_eq!(s.search.prompt_buf, "F", "Shift+Char must be accepted in search prompt");

        // Ctrl+x: must NOT push (CTRL rejected by the guard)
        handle(&ev(Modifiers::CTRL, Key::Char('x')), &mut s, &scr);
        assert_eq!(s.search.prompt_buf, "F", "Ctrl+Char must be rejected in search prompt");
    }

    #[test]
    fn search_jumps_to_first_match_at_or_after_cursor_row() {
        // Kills: 421:42 (>= → <): with <, `.position()` finds a match BEFORE
        // the cursor instead of AT or AFTER it.
        // Screen has "foo" at row 0 and row 2; cursor starts at row 1.
        // The match >= row 1 is row 2; with mutation (< row 1) the only candidate
        // is row 0 (index 0), and unwrap_or(0) also gives 0 → jumps to row 0 instead.
        let scr = screen_with_lines(4, 30, &["foo pre", "middle", "foo post", "end"]);
        let mut s = CopyMode::new(4, 4, 1, 0);
        s.search.prompt_active = true;
        s.search.prompt_buf = "foo".into();
        handle(&ev(Modifiers::empty(), Key::Enter), &mut s, &scr);
        assert_eq!(s.cursor.0, 2, "should jump to first match at/after cursor row (row 2 not row 0)");
    }
}
