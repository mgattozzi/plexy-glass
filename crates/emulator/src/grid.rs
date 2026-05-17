//! Rectangular cell grid with wrap-origin tracking on rows.

use crate::cell::Cell;

/// Per-row wrap origin. Used by reflow to reconstruct logical lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapOrigin {
    /// First row of a logical line (explicit newline or top of screen).
    Hard,
    /// Continuation of the logical line whose first row had this id.
    SoftFrom(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub wrap_origin: WrapOrigin,
}

impl Row {
    pub fn blank(cols: u16) -> Self {
        Self {
            cells: vec![Cell::default(); cols as usize],
            wrap_origin: WrapOrigin::Hard,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Grid {
    pub rows: Vec<Row>,
    pub cols: u16,
}

impl Grid {
    pub fn new(rows: u16, cols: u16) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        Self {
            rows: vec![Row::blank(cols); rows as usize],
            cols,
        }
    }

    pub fn num_rows(&self) -> u16 {
        self.rows.len() as u16
    }

    pub fn num_cols(&self) -> u16 {
        self.cols
    }

    pub fn put_cell(&mut self, row: u16, col: u16, cell: Cell) {
        if let Some(r) = self.rows.get_mut(row as usize) {
            if let Some(c) = r.cells.get_mut(col as usize) {
                *c = cell;
            }
        }
    }

    pub fn get_cell(&self, row: u16, col: u16) -> Option<&Cell> {
        self.rows
            .get(row as usize)
            .and_then(|r| r.cells.get(col as usize))
    }

    /// Reset every cell to default.
    pub fn clear(&mut self) {
        for r in self.rows.iter_mut() {
            for c in r.cells.iter_mut() {
                *c = Cell::default();
            }
            r.wrap_origin = WrapOrigin::Hard;
        }
    }

    /// Clear an inclusive rectangle (clamped to grid).
    pub fn clear_rect(&mut self, start_row: u16, start_col: u16, end_row: u16, end_col: u16) {
        let end_row = end_row.min(self.num_rows().saturating_sub(1));
        let end_col = end_col.min(self.cols.saturating_sub(1));
        if start_row > end_row || start_col > end_col {
            return;
        }
        for r in start_row..=end_row {
            if let Some(row) = self.rows.get_mut(r as usize) {
                for c in start_col..=end_col {
                    if let Some(cell) = row.cells.get_mut(c as usize) {
                        *cell = Cell::default();
                    }
                }
            }
        }
    }

    /// Scroll a region [top, bottom] (inclusive) up by `n`. If `popped` is
    /// provided, rows that fall off the top are appended to it; otherwise
    /// discarded. New blank rows are inserted at the bottom of the region.
    pub fn scroll_up(&mut self, top: u16, bottom: u16, n: u16, mut popped: Option<&mut Vec<Row>>) {
        let top = top as usize;
        let bottom = (bottom as usize).min(self.rows.len().saturating_sub(1));
        if top > bottom {
            return;
        }
        let region = bottom - top + 1;
        let n = (n as usize).min(region);
        for _ in 0..n {
            let r = self.rows.remove(top);
            if let Some(p) = popped.as_deref_mut() {
                p.push(r);
            }
            self.rows.insert(bottom, Row::blank(self.cols));
        }
    }

    /// Scroll region [top, bottom] (inclusive) down by `n`. Bottom rows are
    /// discarded; blank rows inserted at the top.
    pub fn scroll_down(&mut self, top: u16, bottom: u16, n: u16) {
        let top = top as usize;
        let bottom = (bottom as usize).min(self.rows.len().saturating_sub(1));
        if top > bottom {
            return;
        }
        let region = bottom - top + 1;
        let n = (n as usize).min(region);
        for _ in 0..n {
            self.rows.remove(bottom);
            self.rows.insert(top, Row::blank(self.cols));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smol_str::SmolStr;

    fn x_cell() -> Cell {
        let mut c = Cell::default();
        c.grapheme = SmolStr::new("X");
        c
    }

    #[test]
    fn new_grid_has_blank_rows() {
        let g = Grid::new(3, 4);
        assert_eq!(g.num_rows(), 3);
        assert_eq!(g.num_cols(), 4);
        assert!(g.get_cell(0, 0).unwrap().is_blank());
    }

    #[test]
    fn put_cell_oob_is_noop() {
        let mut g = Grid::new(2, 2);
        g.put_cell(99, 99, x_cell());
        for r in 0..2 {
            for c in 0..2 {
                assert!(g.get_cell(r, c).unwrap().is_blank());
            }
        }
    }

    #[test]
    fn clear_rect_clears_inclusive_range() {
        let mut g = Grid::new(3, 3);
        for r in 0..3 {
            for c in 0..3 {
                g.put_cell(r, c, x_cell());
            }
        }
        g.clear_rect(1, 1, 2, 2);
        assert_eq!(g.get_cell(0, 0).unwrap(), &x_cell());
        assert!(g.get_cell(1, 1).unwrap().is_blank());
        assert!(g.get_cell(2, 2).unwrap().is_blank());
    }

    #[test]
    fn scroll_up_moves_rows_and_blanks_bottom() {
        let mut g = Grid::new(3, 1);
        g.put_cell(0, 0, x_cell());
        g.scroll_up(0, 2, 1, None);
        assert!(g.get_cell(0, 0).unwrap().is_blank());
        assert!(g.get_cell(2, 0).unwrap().is_blank());
    }

    #[test]
    fn scroll_up_collects_popped() {
        let mut g = Grid::new(3, 1);
        g.put_cell(0, 0, x_cell());
        let mut out = Vec::new();
        g.scroll_up(0, 2, 1, Some(&mut out));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].cells[0], x_cell());
    }

    #[test]
    fn scroll_down_blanks_top_discards_bottom() {
        let mut g = Grid::new(3, 1);
        g.put_cell(2, 0, x_cell());
        g.scroll_down(0, 2, 1);
        assert!(g.get_cell(0, 0).unwrap().is_blank());
        assert!(g.get_cell(2, 0).unwrap().is_blank());
    }
}
