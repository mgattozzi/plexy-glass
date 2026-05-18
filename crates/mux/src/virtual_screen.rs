//! In-memory composite output grid. The compositor builds one; the
//! diff-renderer compares two to produce ANSI bytes.

use plexy_glass_emulator::Cell;

#[derive(Debug, Clone)]
pub struct VirtualScreen {
    pub cells: Vec<Cell>,
    pub cursor: Option<(u16, u16)>,
    pub cursor_visible: bool,
    pub rows: u16,
    pub cols: u16,
}

impl VirtualScreen {
    pub fn blank(rows: u16, cols: u16) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        Self {
            cells: vec![Cell::default(); rows as usize * cols as usize],
            cursor: None,
            cursor_visible: false,
            rows,
            cols,
        }
    }

    pub fn cell(&self, r: u16, c: u16) -> Option<&Cell> {
        if r >= self.rows || c >= self.cols {
            return None;
        }
        self.cells.get(r as usize * self.cols as usize + c as usize)
    }

    pub fn cell_mut(&mut self, r: u16, c: u16) -> Option<&mut Cell> {
        if r >= self.rows || c >= self.cols {
            return None;
        }
        let cols = self.cols as usize;
        self.cells.get_mut(r as usize * cols + c as usize)
    }

    pub fn put(&mut self, r: u16, c: u16, cell: Cell) {
        if let Some(slot) = self.cell_mut(r, c) {
            *slot = cell;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smol_str::SmolStr;

    #[test]
    fn blank_dimensions() {
        let v = VirtualScreen::blank(4, 6);
        assert_eq!(v.rows, 4);
        assert_eq!(v.cols, 6);
        assert_eq!(v.cells.len(), 24);
        assert!(v.cells.iter().all(|c| c.is_blank()));
    }

    #[test]
    fn put_then_get_roundtrips() {
        let mut v = VirtualScreen::blank(2, 2);
        let c = Cell {
            grapheme: SmolStr::new("Z"),
            ..Cell::default()
        };
        v.put(0, 1, c.clone());
        assert_eq!(v.cell(0, 1), Some(&c));
        assert!(v.cell(0, 0).unwrap().is_blank());
    }

    #[test]
    fn put_oob_is_noop() {
        let mut v = VirtualScreen::blank(2, 2);
        let c = Cell {
            grapheme: SmolStr::new("X"),
            ..Cell::default()
        };
        v.put(99, 99, c);
        assert!(v.cells.iter().all(|c| c.is_blank()));
    }
}
