//! Screen state composes the active grid, scrollback, cursor, modes, and
//! associated metadata, and provides the methods the parser dispatches into.

use crate::{
    cell::Cell,
    cursor::Cursor,
    grid::{Grid, WrapOrigin},
    hyperlinks::HyperlinkTable,
    modes::Modes,
    parser::ScreenOps,
    scrollback::Scrollback,
    tabs::TabStops,
};
use unicode_width::UnicodeWidthStr;

pub struct Screen {
    pub active: Grid,
    pub alt: Option<Grid>,
    pub scrollback: Scrollback,
    pub cursor: Cursor,
    pub saved_cursor: Option<Cursor>,
    pub modes: Modes,
    pub tabs: TabStops,
    pub title: String,
    pub icon_title: String,
    pub cwd: Option<String>,
    pub hyperlinks: HyperlinkTable,
    /// Scroll region (top, bottom), inclusive, in active-grid row coords.
    pub scroll_region: (u16, u16),
}

impl Screen {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            active: Grid::new(rows, cols),
            alt: None,
            scrollback: Scrollback::default(),
            cursor: Cursor::default(),
            saved_cursor: None,
            modes: Modes::default(),
            tabs: TabStops::new(cols),
            title: String::new(),
            icon_title: String::new(),
            cwd: None,
            hyperlinks: HyperlinkTable::default(),
            scroll_region: (0, rows.saturating_sub(1)),
        }
    }

    pub fn rows(&self) -> u16 {
        self.active.num_rows()
    }

    pub fn cols(&self) -> u16 {
        self.active.num_cols()
    }

    /// Place one grapheme at the cursor, respecting wide-char and autowrap.
    pub fn put_grapheme(&mut self, cluster: &str) {
        let w = cluster.width() as u16;

        // Zero-width: attach to the previous cell (left of cursor on this row,
        // or last cell of previous row if cursor is at col 0).
        if w == 0 {
            self.attach_zero_width(cluster);
            return;
        }

        if self.cursor.pending_wrap && self.modes.contains(Modes::AUTOWRAP) {
            self.advance_to_next_row(true);
            self.cursor.col = 0;
            self.cursor.pending_wrap = false;
        }

        // If a wide char doesn't fit, pad the last column and wrap.
        if w == 2 && self.cursor.col + 1 >= self.cols() {
            if self.modes.contains(Modes::AUTOWRAP) {
                if self.cursor.col < self.cols() {
                    self.put_cell_at_cursor(Cell::default());
                }
                self.advance_to_next_row(true);
                self.cursor.col = 0;
                self.cursor.pending_wrap = false;
            } else {
                // No autowrap: clamp the cursor and overwrite the last cell.
                self.cursor.col = self.cols().saturating_sub(2);
            }
        }

        let cell = Cell {
            grapheme: cluster.into(),
            fg: self.cursor.fg,
            bg: self.cursor.bg,
            attrs: self.cursor.attrs,
            hyperlink_id: self.cursor.hyperlink_id,
        };
        self.put_cell_at_cursor(cell);

        if w == 2 {
            self.cursor.col += 1;
            self.put_cell_at_cursor(Cell::wide_spacer());
        }

        if self.cursor.col + 1 >= self.cols() {
            self.cursor.pending_wrap = true;
        } else {
            self.cursor.col += 1;
        }
    }

    fn put_cell_at_cursor(&mut self, cell: Cell) {
        self.active.put_cell(self.cursor.row, self.cursor.col, cell);
    }

    fn attach_zero_width(&mut self, cluster: &str) {
        // Try the cell to the left on the same row.
        if self.cursor.col > 0 {
            if let Some(prev) = self.active.get_cell(self.cursor.row, self.cursor.col - 1) {
                let mut updated = prev.clone();
                let mut s = String::from(updated.grapheme.as_str());
                s.push_str(cluster);
                updated.grapheme = s.into();
                self.active.put_cell(self.cursor.row, self.cursor.col - 1, updated);
            }
        } else if self.cursor.row > 0 {
            // Append to the last cell of the previous row.
            let prev_col = self.cols().saturating_sub(1);
            if let Some(prev) = self.active.get_cell(self.cursor.row - 1, prev_col) {
                let mut updated = prev.clone();
                let mut s = String::from(updated.grapheme.as_str());
                s.push_str(cluster);
                updated.grapheme = s.into();
                self.active.put_cell(self.cursor.row - 1, prev_col, updated);
            }
        }
        // If there's no previous cell, drop the zero-width char.
    }

    /// Move to the next row. If the cursor is at the bottom of the scroll
    /// region, scroll up (pushing the top row into scrollback when on the
    /// active screen, or discarding when on the alt screen).
    pub fn advance_to_next_row(&mut self, soft_wrap: bool) {
        let (top, bottom) = self.scroll_region;
        if self.cursor.row >= bottom {
            // Need to scroll.
            let mut popped: Vec<crate::grid::Row> = Vec::new();
            let target = if self.alt.is_some() { None } else { Some(&mut popped) };
            self.active.scroll_up(top, bottom, 1, target);
            if self.alt.is_none() {
                for r in popped {
                    self.scrollback.push(r);
                }
            }
            // Stay at the bottom; new content goes there.
            self.cursor.row = bottom;
        } else {
            self.cursor.row += 1;
        }
        if soft_wrap {
            // Mark the new row as a soft continuation. The logical-line id is
            // approximate: reflow only needs to know "this row continues the
            // previous row", so we use the destination row's index.
            let dest = self.cursor.row;
            if let Some(row) = self.active.rows.get_mut(dest as usize) {
                row.wrap_origin = WrapOrigin::SoftFrom(u32::from(dest));
            }
        }
    }

    /// Stub handlers used by the parser. These get filled in by later tasks.
    pub fn handle_csi_stub(&mut self, _params: &vte::Params, _intermediates: &[u8], _action: char) {}
    pub fn handle_osc_stub(&mut self, _params: &[&[u8]]) {}
    pub fn handle_esc_stub(&mut self, _intermediates: &[u8], _byte: u8) {}
    pub fn execute_c0_stub(&mut self, _byte: u8) {}
}

impl ScreenOps for Screen {
    fn put_grapheme(&mut self, cluster: &str) {
        Screen::put_grapheme(self, cluster);
    }
    fn execute_c0(&mut self, byte: u8) {
        self.execute_c0_stub(byte);
    }
    fn handle_csi(&mut self, params: &vte::Params, intermediates: &[u8], action: char) {
        self.handle_csi_stub(params, intermediates, action);
    }
    fn handle_osc(&mut self, params: &[&[u8]]) {
        self.handle_osc_stub(params);
    }
    fn handle_esc(&mut self, intermediates: &[u8], byte: u8) {
        self.handle_esc_stub(intermediates, byte);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(input: &[u8]) -> Screen {
        let mut p = crate::parser::Parser::new();
        let mut s = Screen::new(4, 8);
        p.advance(&mut s, input);
        // Force-flush any cluster the parser retained as "possibly still
        // growing" by feeding a no-op C0 byte (NUL). Stub execute ignores it.
        p.advance(&mut s, b"\0");
        s
    }

    fn text_at(s: &Screen, row: u16) -> String {
        s.active.rows[row as usize]
            .cells
            .iter()
            .filter(|c| !c.is_wide_spacer())
            .map(|c| c.grapheme.as_str())
            .collect::<String>()
    }

    #[test]
    fn ascii_writes_left_to_right() {
        let s = drive(b"hello");
        assert!(text_at(&s, 0).starts_with("hello"));
        assert_eq!(s.cursor.row, 0);
        // After 5 chars at col 0..4, cursor at col 5.
        assert_eq!(s.cursor.col, 5);
    }

    #[test]
    fn autowrap_to_next_row() {
        let s = drive(b"abcdefghi"); // 9 chars on an 8-wide grid
        assert_eq!(text_at(&s, 0)[..8], *"abcdefgh");
        assert!(text_at(&s, 1).starts_with("i"));
    }

    #[test]
    fn wide_char_takes_two_columns_and_emits_spacer() {
        // "好" is width 2.
        let s = drive("好".as_bytes());
        let c0 = s.active.get_cell(0, 0).unwrap();
        let c1 = s.active.get_cell(0, 1).unwrap();
        assert_eq!(c0.grapheme.as_str(), "好");
        assert!(c1.is_wide_spacer());
        assert_eq!(s.cursor.col, 2);
    }

    #[test]
    fn cursor_advances_to_pending_wrap_at_end_of_row() {
        let s = drive(b"abcdefgh"); // exactly 8 chars
        assert_eq!(s.cursor.col, 7);
        assert!(s.cursor.pending_wrap);
    }
}
