//! Cursor state: position, current attrs for new cells, visibility, shape.

use crate::attrs::{Attrs, UnderlineStyle};
use crate::color::Color;
use crate::coords::{Col, Row};
use crate::hyperlinks::HyperlinkId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    /// No explicit DECSCUSR — the outer terminal's own default cursor.
    Default,
    Block,
    Underline,
    Bar,
}

#[derive(Debug, Clone)]
pub struct Cursor {
    pub row: Row,
    pub col: Col,
    /// SGR attributes that newly-written cells inherit.
    pub attrs: Attrs,
    pub fg: Color,
    pub bg: Color,
    /// Underline color pen (SGR 58/59). `Color::Default` = follow text fg.
    pub underline_color: Color,
    /// Underline style pen (SGR `4:0`..`4:5`). Mirrors `Attrs::UNDERLINE` (the
    /// any-underline boolean) with the specific kind for the diff renderer.
    pub underline_style: UnderlineStyle,
    pub hyperlink_id: Option<HyperlinkId>,
    /// True when the next character should wrap to the next row. Set when the
    /// cursor advances past the last column with autowrap on.
    pub pending_wrap: bool,
    pub visible: bool,
    pub shape: CursorShape,
    /// DECSCUSR blink bit (Ps 0/odd = blink, even = steady).
    pub blink: bool,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            row: Row::ZERO,
            col: Col::ZERO,
            attrs: Attrs::empty(),
            fg: Color::Default,
            bg: Color::Default,
            underline_color: Color::Default,
            underline_style: UnderlineStyle::None,
            hyperlink_id: None,
            pending_wrap: false,
            visible: true,
            shape: CursorShape::Default,
            blink: true,
        }
    }
}

impl Cursor {
    /// Move to an absolute (row, col), clamped into the grid.
    pub fn move_to(&mut self, row: Row, col: Col, max_rows: Row, max_cols: Col) {
        self.row = row.min(max_rows.retreat(1));
        self.col = col.min(max_cols.retreat(1));
        self.pending_wrap = false;
    }

    /// Move up by `n`, clamped to row 0.
    pub const fn up(&mut self, n: u16) {
        self.row = self.row.retreat(n);
        self.pending_wrap = false;
    }

    /// Move down by `n`, clamped to the last row.
    pub fn down(&mut self, n: u16, max_rows: Row) {
        self.row = self.row.advance(n).min(max_rows.retreat(1));
        self.pending_wrap = false;
    }

    /// Move left by `n`, clamped to column 0.
    pub const fn left(&mut self, n: u16) {
        self.col = self.col.retreat(n);
        self.pending_wrap = false;
    }

    /// Move right by `n`, clamped to the last column.
    pub fn right(&mut self, n: u16, max_cols: Col) {
        self.col = self.col.advance(n).min(max_cols.retreat(1));
        self.pending_wrap = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_at_home_visible_default_shape() {
        let c = Cursor::default();
        assert_eq!((c.row, c.col), (Row::ZERO, Col::ZERO));
        assert!(c.visible);
        assert_eq!(c.shape, CursorShape::Default);
        assert!(!c.pending_wrap);
    }

    #[test]
    fn move_to_clamps() {
        let mut c = Cursor::default();
        c.move_to(Row::new(100), Col::new(100), Row::new(24), Col::new(80));
        assert_eq!((c.row, c.col), (Row::new(23), Col::new(79)));
    }

    #[test]
    fn up_saturates_at_zero() {
        let mut c = Cursor {
            row: Row::new(2),
            ..Cursor::default()
        };
        c.up(5);
        assert_eq!(c.row, Row::ZERO);
    }

    #[test]
    fn down_clamps_to_max() {
        let mut c = Cursor {
            row: Row::new(20),
            ..Cursor::default()
        };
        c.down(10, Row::new(24));
        assert_eq!(c.row, Row::new(23));
    }

    #[test]
    fn left_decrements_col() {
        let mut c = Cursor {
            col: Col::new(10),
            ..Cursor::default()
        };
        c.left(3);
        assert_eq!(c.col, Col::new(7));
    }

    #[test]
    fn left_saturates_at_zero() {
        let mut c = Cursor {
            col: Col::new(2),
            ..Cursor::default()
        };
        c.left(5);
        assert_eq!(c.col, Col::ZERO);
    }

    #[test]
    fn right_increments_col() {
        let mut c = Cursor {
            col: Col::new(5),
            ..Cursor::default()
        };
        c.right(3, Col::new(80));
        assert_eq!(c.col, Col::new(8));
    }

    #[test]
    fn motion_clears_pending_wrap() {
        let mut c = Cursor {
            pending_wrap: true,
            ..Cursor::default()
        };
        c.right(1, Col::new(80));
        assert!(!c.pending_wrap);
    }

    #[test]
    fn left_clears_pending_wrap() {
        let mut c = Cursor {
            col: Col::new(5),
            pending_wrap: true,
            ..Cursor::default()
        };
        c.left(1);
        assert!(!c.pending_wrap);
        assert_eq!(c.col, Col::new(4));
    }
}
