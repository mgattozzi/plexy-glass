//! Cursor state: position, current attrs for new cells, visibility, shape.

use crate::{attrs::Attrs, color::Color};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Underline,
    Bar,
}

#[derive(Debug, Clone)]
pub struct Cursor {
    pub row: u16,
    pub col: u16,
    /// SGR attributes that newly-written cells inherit.
    pub attrs: Attrs,
    pub fg: Color,
    pub bg: Color,
    pub hyperlink_id: Option<u16>,
    /// True when the next character should wrap to the next row. Set when the
    /// cursor advances past the last column with autowrap on.
    pub pending_wrap: bool,
    pub visible: bool,
    pub shape: CursorShape,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            row: 0,
            col: 0,
            attrs: Attrs::empty(),
            fg: Color::Default,
            bg: Color::Default,
            hyperlink_id: None,
            pending_wrap: false,
            visible: true,
            shape: CursorShape::Block,
        }
    }
}

impl Cursor {
    /// Move to an absolute (row, col), clamped into the grid.
    pub fn move_to(&mut self, row: u16, col: u16, max_rows: u16, max_cols: u16) {
        self.row = row.min(max_rows.saturating_sub(1));
        self.col = col.min(max_cols.saturating_sub(1));
        self.pending_wrap = false;
    }

    /// Move up by `n`, clamped to row 0.
    pub fn up(&mut self, n: u16) {
        self.row = self.row.saturating_sub(n);
        self.pending_wrap = false;
    }

    /// Move down by `n`, clamped to the last row.
    pub fn down(&mut self, n: u16, max_rows: u16) {
        self.row = self.row.saturating_add(n).min(max_rows.saturating_sub(1));
        self.pending_wrap = false;
    }

    /// Move left by `n`, clamped to column 0.
    pub fn left(&mut self, n: u16) {
        self.col = self.col.saturating_sub(n);
        self.pending_wrap = false;
    }

    /// Move right by `n`, clamped to the last column.
    pub fn right(&mut self, n: u16, max_cols: u16) {
        self.col = self.col.saturating_add(n).min(max_cols.saturating_sub(1));
        self.pending_wrap = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_at_home_visible_block() {
        let c = Cursor::default();
        assert_eq!((c.row, c.col), (0, 0));
        assert!(c.visible);
        assert_eq!(c.shape, CursorShape::Block);
        assert!(!c.pending_wrap);
    }

    #[test]
    fn move_to_clamps() {
        let mut c = Cursor::default();
        c.move_to(100, 100, 24, 80);
        assert_eq!((c.row, c.col), (23, 79));
    }

    #[test]
    fn up_saturates_at_zero() {
        let mut c = Cursor::default();
        c.row = 2;
        c.up(5);
        assert_eq!(c.row, 0);
    }

    #[test]
    fn down_clamps_to_max() {
        let mut c = Cursor::default();
        c.row = 20;
        c.down(10, 24);
        assert_eq!(c.row, 23);
    }

    #[test]
    fn motion_clears_pending_wrap() {
        let mut c = Cursor::default();
        c.pending_wrap = true;
        c.right(1, 80);
        assert!(!c.pending_wrap);
    }
}
