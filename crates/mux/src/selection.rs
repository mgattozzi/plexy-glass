//! Click-and-drag selection state for one pane. Constrained at the pane's
//! rect; extension past the border clamps.

use crate::{pane_id::PaneId, rect::Rect};

#[derive(Debug, Clone)]
pub struct Selection {
    pub source_pane: PaneId,
    /// (row, col) within the source pane's local coords. Anchor is the
    /// click-down point; head is the current end (moves while dragging).
    pub anchor: (u16, u16),
    pub head: (u16, u16),
}

impl Selection {
    pub fn start(source_pane: PaneId, row: u16, col: u16) -> Self {
        Self {
            source_pane,
            anchor: (row, col),
            head: (row, col),
        }
    }

    /// Move the head; clamps to the pane's rect (`rect` in viewport coords;
    /// caller must pre-clamp to local coords).
    pub fn extend(&mut self, row: u16, col: u16, pane_rect: Rect) {
        let max_row = pane_rect.rows.saturating_sub(1);
        let max_col = pane_rect.cols.saturating_sub(1);
        self.head = (row.min(max_row), col.min(max_col));
    }

    /// Iterate cells in selection order (left-to-right, top-to-bottom) given
    /// the normalized rectangle the selection covers.
    pub fn cells(&self, max_cols: u16) -> impl Iterator<Item = (u16, u16)> + '_ {
        let (start, end) = self.normalized();
        SelectionCells {
            cur: start,
            end,
            max_cols,
        }
    }

    /// Normalized (anchor, head) so anchor <= head in lexicographic order.
    pub fn normalized(&self) -> ((u16, u16), (u16, u16)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// True when the gesture never left a one-cell dead-zone of the click-down
    /// point (same row, at most one column of drift), i.e. a click, not a
    /// drag. Mouse reporting is cell-granular, so a firm trackpad press easily
    /// nudges a cell or two; treating that as a click (rather than a
    /// one-character selection) keeps click-to-reposition from degrading into a
    /// stray single-character copy on imprecise hardware.
    pub fn is_click(&self) -> bool {
        self.anchor.0 == self.head.0 && self.anchor.1.abs_diff(self.head.1) <= 1
    }
}

struct SelectionCells {
    cur: (u16, u16),
    end: (u16, u16),
    max_cols: u16,
}

impl Iterator for SelectionCells {
    type Item = (u16, u16);

    fn next(&mut self) -> Option<Self::Item> {
        if self.cur > self.end {
            return None;
        }
        let here = self.cur;
        // Advance: col + 1, or row + 1 + col 0 if we'd run past max_cols or
        // past the end-row's column.
        let on_end_row = here.0 == self.end.0;
        let last_col = if on_end_row {
            self.end.1
        } else {
            self.max_cols.saturating_sub(1)
        };
        if here.1 >= last_col {
            self.cur = (here.0.saturating_add(1), 0);
        } else {
            self.cur = (here.0, here.1 + 1);
        }
        Some(here)
    }
}

/// Hardcoded word-char predicate for double-click word selection. Matches
/// alphanumeric, `_`, `.`, `-`, `/`, `~`. Multi-codepoint graphemes (emoji,
/// combining marks) are treated as word chars. A configurable `[mouse]
/// word_chars` knob is a future follow-up.
pub(crate) fn is_word_char(g: &str) -> bool {
    let mut chars = g.chars();
    let Some(ch) = chars.next() else { return false };
    if chars.next().is_some() {
        return true;
    }
    ch.is_alphanumeric() || matches!(ch, '_' | '.' | '-' | '/' | '~')
}

/// The content row (scrollback or grid) shown at viewport row `vrow`, given the
/// pane height and scroll position, the same mapping the compositor uses to
/// place content. `None` when `vrow` is past the visible content. Every mouse
/// selection helper reads through this so a click made while scrolled back
/// targets the scrollback, not the live grid underneath it.
pub fn viewport_content_row(
    screen: &plexy_glass_emulator::Screen,
    pane_rows: u16,
    scroll_offset: u32,
    vrow: u16,
) -> Option<&plexy_glass_emulator::Row> {
    let proj = crate::blocks::FoldProjection::build(screen);
    let visible_total = proj.visible_total();
    let top = visible_total
        .saturating_sub(u32::from(pane_rows))
        .saturating_sub(scroll_offset);
    let idx = top + u32::from(vrow);
    (idx < visible_total)
        .then(|| proj.to_unified(idx))
        .and_then(|u| crate::blocks::row_at(screen, u))
}

/// Return a `Word`-kind `Selection` covering the word at viewport (row, col), or
/// `None` if the click is on a non-word cell. Walks outward through graphemes on
/// the same (scroll-mapped) content row. `row`/`col` stay viewport-relative.
pub fn word_at(
    source_pane: PaneId,
    screen: &plexy_glass_emulator::Screen,
    pane_rows: u16,
    scroll_offset: u32,
    row: u16,
    col: u16,
) -> Option<Selection> {
    let cols = screen.active.num_cols();
    if col >= cols {
        return None;
    }
    let content = viewport_content_row(screen, pane_rows, scroll_offset, row)?;
    let is_word = |c: u16| {
        content
            .cells
            .get(c as usize)
            .is_some_and(|cell| is_word_char(cell.grapheme.as_str()))
    };
    let is_spacer = |c: u16| {
        content
            .cells
            .get(c as usize)
            .is_some_and(|cell| cell.is_wide_spacer())
    };
    // A wide (CJK/emoji) grapheme occupies its cell plus a wide-spacer in the
    // next column. A click on that spacer (the glyph's right half) targets the
    // owning grapheme, and the outward walk must STEP OVER spacers, since
    // treating a spacer as a non-word cell would truncate the word at the
    // first wide glyph.
    let col = if col > 0 && is_spacer(col) { col - 1 } else { col };
    if !is_word(col) {
        return None;
    }
    let mut start = col;
    while start > 0 {
        // The cell left of `start` may be a spacer; its grapheme is one further.
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
    Some(Selection {
        source_pane,
        anchor: (row, start),
        head: (row, end),
    })
}

/// Return a `Line`-kind `Selection` covering viewport `row` from col 0 to the
/// last non-blank cell of its (scroll-mapped) content row. `None` if blank.
pub fn line_at(
    source_pane: PaneId,
    screen: &plexy_glass_emulator::Screen,
    pane_rows: u16,
    scroll_offset: u32,
    row: u16,
) -> Option<Selection> {
    let cols = screen.active.num_cols();
    let content = viewport_content_row(screen, pane_rows, scroll_offset, row)?;
    let mut last = None;
    for c in 0..cols {
        if content.cells.get(c as usize).is_some_and(|cell| !cell.is_blank()) {
            last = Some(c);
        }
    }
    let end = last?;
    Some(Selection {
        source_pane,
        anchor: (row, 0),
        head: (row, end),
    })
}

/// Pull the selected text out of `screen`. Walks the cells in selection order;
/// inserts `\n` at row boundaries. Trailing default-blank cells on each row are
/// trimmed so empty space at the right edge doesn't bloat the copied string;
/// wide-spacer cells are skipped.
///
/// The selection's rows are *viewport-relative* (0 = top of the pane). They're
/// mapped to the actual content row (scrollback or grid) through the SAME
/// scroll-back + fold projection the compositor uses to place content, so a
/// selection made while scrolled back copies what's highlighted, not the live
/// grid underneath. `pane_rows` is the pane's visible height, `scroll_offset` its
/// current wheel-scroll position (both `0`/full when at the live bottom).
pub fn extract_text(
    selection: &Selection,
    screen: &plexy_glass_emulator::Screen,
    pane_rows: u16,
    scroll_offset: u32,
) -> String {
    let proj = crate::blocks::FoldProjection::build(screen);
    let visible_total = proj.visible_total();
    // Top visible line, matching the compositor's live-pane FoldCtx.
    let top_visible = visible_total
        .saturating_sub(u32::from(pane_rows))
        .saturating_sub(scroll_offset);
    let (start, end) = selection.normalized();
    let cols = screen.active.num_cols();
    let mut out = String::new();
    for r in start.0..=end.0 {
        // Viewport row r → unified content line (scrollback ++ grid), via the
        // fold projection at the current scroll position.
        let visible_idx = top_visible + u32::from(r);
        let row = (visible_idx < visible_total)
            .then(|| proj.to_unified(visible_idx))
            .and_then(|u| crate::blocks::row_at(screen, u));
        if let Some(row) = row {
            let mut row_start = if r == start.0 { start.1 } else { 0 };
            // If a drag anchor landed on a wide grapheme's spacer half, back up
            // to the owning grapheme cell so the leading glyph isn't dropped.
            if r == start.0
                && row_start > 0
                && row.cells.get(row_start as usize).is_some_and(|c| c.is_wide_spacer())
            {
                row_start -= 1;
            }
            let row_end = if r == end.0 { end.1 } else { cols.saturating_sub(1) };
            let mut last_significant = row_start;
            for c in row_start..=row_end {
                if let Some(cell) = row.cells.get(c as usize)
                    && !cell.is_blank()
                {
                    last_significant = c;
                }
            }
            for c in row_start..=last_significant {
                if let Some(cell) = row.cells.get(c as usize) {
                    if cell.is_wide_spacer() {
                        continue;
                    }
                    out.push_str(cell.grapheme.as_str());
                }
            }
        }
        if r < end.0 {
            out.push('\n');
        }
    }
    out
}

/// The visible grid as plain text: one `String` line per grid row, wide-glyph
/// spacer cells skipped, per-line trailing whitespace trimmed, trailing blank
/// lines dropped. The CLI `capture` verb's renderer.
///
/// Wide-spacer cells have an empty grapheme (`""`), so `push_str("")` is a
/// no-op and they are naturally skipped without an explicit `is_wide_spacer()`
/// check. Blank cells carry a space grapheme (`" "`); `trim_end` removes them.
pub fn screen_text(screen: &plexy_glass_emulator::Screen) -> String {
    let mut lines: Vec<String> = Vec::with_capacity(screen.active.num_rows() as usize);
    for row in &screen.active.rows {
        let mut line = String::new();
        for cell in &row.cells {
            line.push_str(cell.grapheme.as_str());
        }
        let trimmed = line.trim_end();
        lines.push(trimmed.to_string());
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_then_extend_within_bounds() {
        let mut s = Selection::start(PaneId(0), 1, 2);
        s.extend(3, 5, Rect::new(0, 0, 10, 10));
        assert_eq!(s.head, (3, 5));
    }

    #[test]
    fn word_at_spans_wide_graphemes() {
        use plexy_glass_emulator::Emulator;
        let mut emu = Emulator::new(5, 20);
        emu.advance("ab中cd ".as_bytes()); // trailing space flushes the last grapheme
        let s = emu.screen();
        // Double-click 'a' (col 0): the word must not truncate at 中's spacer.
        let sel = word_at(PaneId(0), s, 5, 0, 0, 0).expect("word");
        assert_eq!(extract_text(&sel, s, 5, 0), "ab中cd");
        // Clicking the spacer half of 中 (col 3) targets the same word.
        let sel2 = word_at(PaneId(0), s, 5, 0, 0, 3).expect("word from spacer");
        assert_eq!(extract_text(&sel2, s, 5, 0), "ab中cd");
    }

    #[test]
    fn extract_text_keeps_a_leading_wide_grapheme_from_a_spacer_anchor() {
        use plexy_glass_emulator::Emulator;
        let mut emu = Emulator::new(5, 20);
        emu.advance("中文ab ".as_bytes());
        let s = emu.screen();
        // Drag anchor on 中's spacer (col 1) → head at 'b' (col 5): 中 must survive.
        let sel = Selection { source_pane: PaneId(0), anchor: (0, 1), head: (0, 5) };
        assert_eq!(extract_text(&sel, s, 5, 0), "中文ab");
    }

    #[test]
    fn is_click_holds_within_a_one_cell_dead_zone() {
        let mut s = Selection::start(PaneId(0), 5, 5);
        assert!(s.is_click(), "no drift is a click");
        for head in [(5, 4), (5, 6)] {
            s.head = head;
            assert!(s.is_click(), "{head:?}: one-cell drift is still a click");
        }
        for head in [(5, 7), (5, 3), (6, 5)] {
            s.head = head;
            assert!(!s.is_click(), "{head:?}: a real drag is not a click");
        }
    }

    #[test]
    fn extend_clamps_to_rect() {
        let mut s = Selection::start(PaneId(0), 1, 2);
        s.extend(99, 99, Rect::new(0, 0, 10, 10));
        assert_eq!(s.head, (9, 9));
    }

    #[test]
    fn normalized_orders_anchor_before_head() {
        let mut s = Selection::start(PaneId(0), 5, 5);
        s.extend(2, 3, Rect::new(0, 0, 10, 10));
        let (a, b) = s.normalized();
        assert_eq!(a, (2, 3));
        assert_eq!(b, (5, 5));
    }

    #[test]
    fn cells_walks_inclusive_left_to_right_top_to_bottom() {
        let mut s = Selection::start(PaneId(0), 0, 0);
        s.extend(1, 2, Rect::new(0, 0, 3, 3));
        let cells: Vec<_> = s.cells(3).collect();
        // Row 0: (0,0), (0,1), (0,2); Row 1: (1,0), (1,1), (1,2).
        assert_eq!(cells, vec![
            (0, 0), (0, 1), (0, 2),
            (1, 0), (1, 1), (1, 2),
        ]);
    }

    #[test]
    fn cells_on_single_row_is_inclusive() {
        let mut s = Selection::start(PaneId(0), 2, 1);
        s.extend(2, 4, Rect::new(0, 0, 10, 10));
        let cells: Vec<_> = s.cells(10).collect();
        assert_eq!(cells, vec![(2, 1), (2, 2), (2, 3), (2, 4)]);
    }

    #[test]
    fn empty_selection_when_anchor_equals_head() {
        let s = Selection::start(PaneId(0), 0, 0);
        assert!(s.is_empty());
    }

    use plexy_glass_emulator::Emulator;

    fn screen_from(rows: u16, cols: u16, lines: &[&str]) -> plexy_glass_emulator::Screen {
        let mut e = Emulator::new(rows, cols);
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                e.advance(b"\r\n");
            }
            e.advance(line.as_bytes());
        }
        // A no-op SGR flushes the parser's pending grapheme buffer so the
        // final grapheme is committed to the screen before we clone it.
        e.advance(b"\x1b[m");
        e.screen().clone()
    }

    #[test]
    fn extract_simple_word() {
        let screen = screen_from(2, 10, &["hello", ""]);
        let mut s = Selection::start(PaneId(0), 0, 0);
        s.extend(0, 4, Rect::new(0, 0, 2, 10));
        assert_eq!(extract_text(&s, &screen, 2, 0), "hello");
    }

    #[test]
    fn extract_across_rows_joins_with_newline() {
        let screen = screen_from(2, 10, &["abc", "def"]);
        let mut s = Selection::start(PaneId(0), 0, 0);
        s.extend(1, 2, Rect::new(0, 0, 2, 10));
        let txt = extract_text(&s, &screen, 2, 0);
        assert!(txt.starts_with("abc"));
        assert!(txt.contains('\n'));
        assert!(txt.ends_with("def"));
    }

    #[test]
    fn extract_scrolled_back_reads_scrollback_not_grid() {
        // 2-row pane fed 4 lines, so the top 2 (sb0/sb1) live in scrollback.
        // Scrolled back by 2, the viewport shows sb0/sb1; a row-0 selection must
        // copy "sb0", NOT the live grid's row 0 ("grid0").
        let screen = screen_from(2, 10, &["sb0", "sb1", "grid0", "grid1"]);
        assert_eq!(screen.scrollback.rows().len(), 2, "setup: 2 rows scrolled off");
        let mut s = Selection::start(PaneId(0), 0, 0);
        s.extend(0, 2, Rect::new(0, 0, 2, 10)); // row 0, cols 0..=2
        // Live (scroll_offset 0): viewport row 0 is the grid's row 0 → "gri".
        assert_eq!(extract_text(&s, &screen, 2, 0), "gri");
        // Scrolled back 2: viewport row 0 is scrollback's "sb0" (the bug).
        assert_eq!(extract_text(&s, &screen, 2, 2), "sb0");
    }

    #[test]
    fn word_at_returns_word_range() {
        let screen = screen_from(1, 20, &["hello world.foo"]);
        let s = word_at(PaneId(0), &screen, screen.active.num_rows(), 0, 0, 2).expect("on 'hello'");
        assert_eq!(s.anchor, (0, 0));
        assert_eq!(s.head, (0, 4));
    }

    #[test]
    fn word_at_on_whitespace_returns_none() {
        let screen = screen_from(1, 10, &["foo  bar"]);
        assert!(word_at(PaneId(0), &screen, screen.active.num_rows(), 0, 0, 3).is_none());
    }

    #[test]
    fn word_at_on_punctuation_returns_none() {
        let screen = screen_from(1, 10, &["foo,bar"]);
        assert!(word_at(PaneId(0), &screen, screen.active.num_rows(), 0, 0, 3).is_none());
    }

    #[test]
    fn word_at_includes_underscore_and_dash() {
        // '=' breaks the word; underscore + dash do not.
        let screen = screen_from(1, 20, &["foo_bar-baz=junk"]);
        let s = word_at(PaneId(0), &screen, screen.active.num_rows(), 0, 0, 2).expect("on 'foo_bar-baz'");
        assert_eq!(s.anchor, (0, 0));
        assert_eq!(s.head, (0, 10));
    }

    #[test]
    fn word_at_clamps_at_row_edge() {
        let screen = screen_from(1, 5, &["hello"]);
        let s = word_at(PaneId(0), &screen, screen.active.num_rows(), 0, 0, 4).expect("on last 'o'");
        assert_eq!(s.anchor, (0, 0));
        assert_eq!(s.head, (0, 4));
    }

    #[test]
    fn line_at_trims_trailing_blanks() {
        let screen = screen_from(1, 20, &["hello"]);
        let s = line_at(PaneId(0), &screen, screen.active.num_rows(), 0, 0).expect("non-blank row");
        assert_eq!(s.anchor, (0, 0));
        assert_eq!(s.head, (0, 4));
    }

    #[test]
    fn line_at_on_blank_row_returns_none() {
        let screen = screen_from(2, 10, &["hello", ""]);
        assert!(line_at(PaneId(0), &screen, screen.active.num_rows(), 0, 1).is_none());
    }

    #[test]
    fn word_and_line_at_scrolled_read_scrollback_not_grid() {
        // 2-row pane, 4 lines fed → top two ("sb0one"/"sb1") in scrollback.
        // Scrolled back 2, viewport row 0 is "sb0one": double/triple-click there
        // must select from scrollback, not the live grid's "grid0" underneath.
        let screen = screen_from(2, 10, &["sb0one", "sb1", "grid0", "grid1"]);
        assert_eq!(screen.scrollback.rows().len(), 2, "setup: 2 rows scrolled off");

        // Live (offset 0): viewport row 0 = grid's "grid0".
        let w = word_at(PaneId(0), &screen, 2, 0, 0, 1).expect("word on grid0");
        assert_eq!((w.anchor, w.head), ((0, 0), (0, 4)));
        // Scrolled back 2: viewport row 0 = scrollback's "sb0one".
        let w = word_at(PaneId(0), &screen, 2, 2, 0, 1).expect("word on sb0one");
        assert_eq!((w.anchor, w.head), ((0, 0), (0, 5)));
        let l = line_at(PaneId(0), &screen, 2, 2, 0).expect("line on sb0one");
        assert_eq!((l.anchor, l.head), ((0, 0), (0, 5)));
    }

    // ── screen_text tests ────────────────────────────────────────────────────

    #[test]
    fn screen_text_plain_two_lines() {
        let screen = screen_from(3, 20, &["hello", "world", ""]);
        let txt = super::screen_text(&screen);
        assert_eq!(txt, "hello\nworld");
    }

    #[test]
    fn screen_text_wide_grapheme_appears_once() {
        // "世" is a wide (2-col) CJK character; the emulator places it at col 0
        // and inserts a spacer at col 1. screen_text must emit "世" once.
        let screen = screen_from(2, 20, &["世", ""]);
        let txt = super::screen_text(&screen);
        assert_eq!(txt, "世");
    }

    #[test]
    fn screen_text_trailing_spaces_trimmed() {
        // Content "ab" in a 10-col grid; the remaining 8 cols are blank spaces.
        let screen = screen_from(2, 10, &["ab", ""]);
        let txt = super::screen_text(&screen);
        assert_eq!(txt, "ab");
    }

    #[test]
    fn screen_text_trailing_blank_lines_dropped() {
        // Content only on row 0 of a 5-row grid; rows 1-4 must not appear.
        let screen = screen_from(5, 20, &["content", "", "", "", ""]);
        let txt = super::screen_text(&screen);
        assert_eq!(txt, "content");
    }
}
