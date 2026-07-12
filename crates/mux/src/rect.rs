//! Rectangle in (row, col) coordinates with (rows, cols) dimensions.
//!
//! Origin at top-left. All ops clamp; nothing panics.
//!
//! A `Rect` is four `u16`s, but they split into two very different kinds: a
//! grid *position* (`row`, `col`) and a *dimension* (`rows`, `cols`). Those
//! two pairs used to be four loose positional args to `Rect::new`, which meant
//! `Rect::new(rows, cols, row, col)` compiled into a subtly-wrong rect. So the
//! two axes are their own named-field types now — `Point` and `Size` — and
//! `Rect`'s fields are private: the only way to build one is
//! `Rect::new(Point, Size)`, which makes the transposition a type error instead
//! of a silent geometry bug.

use crate::direction::SplitDir;
use crate::layout::Ratio;

/// A grid position: row (from the top) and col (from the left).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Point {
    pub row: u16,
    pub col: u16,
}

impl Point {
    pub const fn new(row: u16, col: u16) -> Self {
        Self { row, col }
    }
}

/// A dimension: a count of rows and cols.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Size {
    pub rows: u16,
    pub cols: u16,
}

impl Size {
    pub const fn new(rows: u16, cols: u16) -> Self {
        Self { rows, cols }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    // Private so the only constructor is `Rect::new(Point, Size)`; a
    // struct-literal `Rect { rows: .., .. }` can't transpose the axes.
    row: u16,
    col: u16,
    rows: u16,
    cols: u16,
}

impl Rect {
    pub const fn new(origin: Point, size: Size) -> Self {
        Self {
            row: origin.row,
            col: origin.col,
            rows: size.rows,
            cols: size.cols,
        }
    }

    pub const fn row(self) -> u16 {
        self.row
    }

    pub const fn col(self) -> u16 {
        self.col
    }

    pub const fn rows(self) -> u16 {
        self.rows
    }

    pub const fn cols(self) -> u16 {
        self.cols
    }

    pub const fn origin(self) -> Point {
        Point::new(self.row, self.col)
    }

    pub const fn size(self) -> Size {
        Size::new(self.rows, self.cols)
    }

    pub const fn contains(self, p: Point) -> bool {
        p.row >= self.row
            && p.row < self.row.saturating_add(self.rows)
            && p.col >= self.col
            && p.col < self.col.saturating_add(self.cols)
    }

    pub const fn bottom_edge_row(self) -> u16 {
        self.row.saturating_add(self.rows).saturating_sub(1)
    }

    pub const fn right_edge_col(self) -> u16 {
        self.col.saturating_add(self.cols).saturating_sub(1)
    }

    /// Subdivide along `dir` at `ratio` (fraction of size that the first child
    /// gets). The `Ratio` type already guarantees `[0.1, 0.9]` non-NaN, so this
    /// just reads it. Children are sized so they sum to the original (minus a
    /// 1-cell separator between them) so callers can paint a border on the
    /// separator row/col.
    pub fn subdivide(self, dir: SplitDir, ratio: Ratio) -> (Self, Self) {
        let ratio = ratio.get();
        match dir {
            SplitDir::Horizontal => {
                if self.rows <= 1 {
                    // Too small to hold two children + a separator. The first
                    // child takes the row(s); the second is EMPTY, positioned at
                    // the boundary so it carries no cells. Crucially we do NOT
                    // invent a usable row here, a 0-row viewport must yield
                    // 0-row children, never a phantom 1-row rect that would sit
                    // outside the parent and desync rect_of from pane_at_coord.
                    let first = Self::new(
                        Point::new(self.row, self.col),
                        Size::new(self.rows, self.cols),
                    );
                    let second = Self::new(
                        Point::new(self.row.saturating_add(self.rows), self.col),
                        Size::new(0, self.cols),
                    );
                    return (first, second);
                }
                // Children stack top/bottom; one row reserved for the separator.
                let usable = self.rows - 1;
                let first_rows = (f32::from(usable) * ratio).round() as u16;
                let first_rows = first_rows.clamp(1, usable.saturating_sub(1).max(1));
                let second_rows = usable.saturating_sub(first_rows);
                let first = Self::new(
                    Point::new(self.row, self.col),
                    Size::new(first_rows, self.cols),
                );
                let second = Self::new(
                    Point::new(
                        self.row.saturating_add(first_rows).saturating_add(1),
                        self.col,
                    ),
                    Size::new(second_rows, self.cols),
                );
                (first, second)
            }
            SplitDir::Vertical => {
                if self.cols <= 1 {
                    // Mirror of the Horizontal degenerate case: too narrow for two
                    // children + a separator. First takes the col(s); second is
                    // empty at the boundary. No invented column → no phantom rect.
                    let first = Self::new(
                        Point::new(self.row, self.col),
                        Size::new(self.rows, self.cols),
                    );
                    let second = Self::new(
                        Point::new(self.row, self.col.saturating_add(self.cols)),
                        Size::new(self.rows, 0),
                    );
                    return (first, second);
                }
                // Children sit side by side; one col reserved for the separator.
                let usable = self.cols - 1;
                let first_cols = (f32::from(usable) * ratio).round() as u16;
                let first_cols = first_cols.clamp(1, usable.saturating_sub(1).max(1));
                let second_cols = usable.saturating_sub(first_cols);
                let first = Self::new(
                    Point::new(self.row, self.col),
                    Size::new(self.rows, first_cols),
                );
                let second = Self::new(
                    Point::new(
                        self.row,
                        self.col.saturating_add(first_cols).saturating_add(1),
                    ),
                    Size::new(self.rows, second_cols),
                );
                (first, second)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_inside_and_outside() {
        let r = Rect::new(Point::new(2, 3), Size::new(4, 5));
        assert!(r.contains(Point::new(2, 3)));
        assert!(r.contains(Point::new(5, 7)));
        assert!(!r.contains(Point::new(1, 3)));
        assert!(!r.contains(Point::new(2, 8)));
    }

    #[test]
    fn edges() {
        let r = Rect::new(Point::new(0, 0), Size::new(10, 20));
        assert_eq!(r.bottom_edge_row(), 9);
        assert_eq!(r.right_edge_col(), 19);
    }

    #[test]
    fn subdivide_vertical_splits_columns() {
        let r = Rect::new(Point::new(0, 0), Size::new(10, 21));
        let (a, b) = r.subdivide(SplitDir::Vertical, Ratio::new(0.5));
        assert_eq!(a, Rect::new(Point::new(0, 0), Size::new(10, 10)));
        assert_eq!(b, Rect::new(Point::new(0, 11), Size::new(10, 10)));
    }

    #[test]
    fn subdivide_horizontal_splits_rows() {
        let r = Rect::new(Point::new(0, 0), Size::new(11, 20));
        let (a, b) = r.subdivide(SplitDir::Horizontal, Ratio::new(0.5));
        assert_eq!(a, Rect::new(Point::new(0, 0), Size::new(5, 20)));
        assert_eq!(b, Rect::new(Point::new(6, 0), Size::new(5, 20)));
    }

    #[test]
    fn subdivide_clamps_ratio() {
        // `Ratio::new` clamps the out-of-range values before subdivide sees them.
        let r = Rect::new(Point::new(0, 0), Size::new(11, 21));
        let (a, _) = r.subdivide(SplitDir::Vertical, Ratio::new(0.001));
        assert!(a.cols() >= 1);
        let (_, b) = r.subdivide(SplitDir::Vertical, Ratio::new(0.999));
        assert!(b.cols() >= 1);
        // Horizontal mirrors the clamp on the row axis.
        let (a, _) = r.subdivide(SplitDir::Horizontal, Ratio::new(0.001));
        assert!(a.rows() >= 1);
        let (_, b) = r.subdivide(SplitDir::Horizontal, Ratio::new(0.999));
        assert!(b.rows() >= 1);
    }
}
