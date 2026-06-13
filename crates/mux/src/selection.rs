//! Click-and-drag selection state for one pane. Constrained at the pane's
//! rect; extension past the border clamps.

use crate::{pane_id::PaneId, rect::Rect};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionKind {
    Char,
    Word,
    Line,
}

#[derive(Debug, Clone)]
pub struct Selection {
    pub source_pane: PaneId,
    /// (row, col) within the source pane's local coords. Anchor is the
    /// click-down point; head is the current end (moves while dragging).
    pub anchor: (u16, u16),
    pub head: (u16, u16),
    pub kind: SelectionKind,
}

impl Selection {
    pub fn start(source_pane: PaneId, row: u16, col: u16, kind: SelectionKind) -> Self {
        Self {
            source_pane,
            anchor: (row, col),
            head: (row, col),
            kind,
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

/// Return a `Word`-kind `Selection` covering the word at (row, col), or
/// `None` if the click is on a non-word cell. Walks outward through
/// graphemes on the same row.
pub fn word_at(
    source_pane: PaneId,
    screen: &plexy_glass_emulator::Screen,
    row: u16,
    col: u16,
) -> Option<Selection> {
    let cols = screen.active.num_cols();
    if col >= cols {
        return None;
    }
    let cell = screen.active.get_cell(row, col)?;
    if !is_word_char(cell.grapheme.as_str()) {
        return None;
    }
    let mut start = col;
    while start > 0 {
        let prev = start - 1;
        if screen
            .active
            .get_cell(row, prev)
            .map(|c| is_word_char(c.grapheme.as_str()))
            .unwrap_or(false)
        {
            start = prev;
        } else {
            break;
        }
    }
    let mut end = col;
    while end + 1 < cols {
        let next = end + 1;
        if screen
            .active
            .get_cell(row, next)
            .map(|c| is_word_char(c.grapheme.as_str()))
            .unwrap_or(false)
        {
            end = next;
        } else {
            break;
        }
    }
    Some(Selection {
        source_pane,
        anchor: (row, start),
        head: (row, end),
        kind: SelectionKind::Word,
    })
}

/// Return a `Line`-kind `Selection` covering the row from col 0 to the last
/// non-blank cell. Returns `None` if the row is entirely blank.
pub fn line_at(
    source_pane: PaneId,
    screen: &plexy_glass_emulator::Screen,
    row: u16,
) -> Option<Selection> {
    let cols = screen.active.num_cols();
    let mut last = None;
    for c in 0..cols {
        if let Some(cell) = screen.active.get_cell(row, c)
            && !cell.is_blank()
        {
            last = Some(c);
        }
    }
    let end = last?;
    Some(Selection {
        source_pane,
        anchor: (row, 0),
        head: (row, end),
        kind: SelectionKind::Line,
    })
}

/// Pull the selected text out of `screen`. Walks the cells in selection
/// order; inserts `\n` at row boundaries. Trailing default-blank cells on
/// each row are trimmed so empty space at the right edge of the pane
/// doesn't bloat the copied string. Wide-spacer cells are skipped.
pub fn extract_text(selection: &Selection, screen: &plexy_glass_emulator::Screen) -> String {
    let (start, end) = selection.normalized();
    let cols = screen.active.num_cols();
    let mut out = String::new();
    for r in start.0..=end.0 {
        let row_start = if r == start.0 { start.1 } else { 0 };
        let row_end = if r == end.0 {
            end.1
        } else {
            cols.saturating_sub(1)
        };
        let mut last_significant = row_start;
        for c in row_start..=row_end {
            if let Some(cell) = screen.active.get_cell(r, c)
                && !cell.is_blank()
            {
                last_significant = c;
            }
        }
        for c in row_start..=last_significant {
            if let Some(cell) = screen.active.get_cell(r, c) {
                if cell.is_wide_spacer() {
                    continue;
                }
                out.push_str(cell.grapheme.as_str());
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
        let mut s = Selection::start(PaneId(0), 1, 2, SelectionKind::Char);
        s.extend(3, 5, Rect::new(0, 0, 10, 10));
        assert_eq!(s.head, (3, 5));
    }

    #[test]
    fn extend_clamps_to_rect() {
        let mut s = Selection::start(PaneId(0), 1, 2, SelectionKind::Char);
        s.extend(99, 99, Rect::new(0, 0, 10, 10));
        assert_eq!(s.head, (9, 9));
    }

    #[test]
    fn normalized_orders_anchor_before_head() {
        let mut s = Selection::start(PaneId(0), 5, 5, SelectionKind::Char);
        s.extend(2, 3, Rect::new(0, 0, 10, 10));
        let (a, b) = s.normalized();
        assert_eq!(a, (2, 3));
        assert_eq!(b, (5, 5));
    }

    #[test]
    fn cells_walks_inclusive_left_to_right_top_to_bottom() {
        let mut s = Selection::start(PaneId(0), 0, 0, SelectionKind::Char);
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
        let mut s = Selection::start(PaneId(0), 2, 1, SelectionKind::Char);
        s.extend(2, 4, Rect::new(0, 0, 10, 10));
        let cells: Vec<_> = s.cells(10).collect();
        assert_eq!(cells, vec![(2, 1), (2, 2), (2, 3), (2, 4)]);
    }

    #[test]
    fn empty_selection_when_anchor_equals_head() {
        let s = Selection::start(PaneId(0), 0, 0, SelectionKind::Char);
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
        let mut s = Selection::start(PaneId(0), 0, 0, SelectionKind::Char);
        s.extend(0, 4, Rect::new(0, 0, 2, 10));
        assert_eq!(extract_text(&s, &screen), "hello");
    }

    #[test]
    fn extract_across_rows_joins_with_newline() {
        let screen = screen_from(2, 10, &["abc", "def"]);
        let mut s = Selection::start(PaneId(0), 0, 0, SelectionKind::Char);
        s.extend(1, 2, Rect::new(0, 0, 2, 10));
        let txt = extract_text(&s, &screen);
        assert!(txt.starts_with("abc"));
        assert!(txt.contains('\n'));
        assert!(txt.ends_with("def"));
    }

    #[test]
    fn word_at_returns_word_range() {
        let screen = screen_from(1, 20, &["hello world.foo"]);
        let s = word_at(PaneId(0), &screen, 0, 2).expect("on 'hello'");
        assert_eq!(s.kind, SelectionKind::Word);
        assert_eq!(s.anchor, (0, 0));
        assert_eq!(s.head, (0, 4));
    }

    #[test]
    fn word_at_on_whitespace_returns_none() {
        let screen = screen_from(1, 10, &["foo  bar"]);
        assert!(word_at(PaneId(0), &screen, 0, 3).is_none());
    }

    #[test]
    fn word_at_on_punctuation_returns_none() {
        let screen = screen_from(1, 10, &["foo,bar"]);
        assert!(word_at(PaneId(0), &screen, 0, 3).is_none());
    }

    #[test]
    fn word_at_includes_underscore_and_dash() {
        // '=' breaks the word; underscore + dash do not.
        let screen = screen_from(1, 20, &["foo_bar-baz=junk"]);
        let s = word_at(PaneId(0), &screen, 0, 2).expect("on 'foo_bar-baz'");
        assert_eq!(s.anchor, (0, 0));
        assert_eq!(s.head, (0, 10));
    }

    #[test]
    fn word_at_clamps_at_row_edge() {
        let screen = screen_from(1, 5, &["hello"]);
        let s = word_at(PaneId(0), &screen, 0, 4).expect("on last 'o'");
        assert_eq!(s.anchor, (0, 0));
        assert_eq!(s.head, (0, 4));
    }

    #[test]
    fn line_at_trims_trailing_blanks() {
        let screen = screen_from(1, 20, &["hello"]);
        let s = line_at(PaneId(0), &screen, 0).expect("non-blank row");
        assert_eq!(s.kind, SelectionKind::Line);
        assert_eq!(s.anchor, (0, 0));
        assert_eq!(s.head, (0, 4));
    }

    #[test]
    fn line_at_on_blank_row_returns_none() {
        let screen = screen_from(2, 10, &["hello", ""]);
        assert!(line_at(PaneId(0), &screen, 1).is_none());
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
