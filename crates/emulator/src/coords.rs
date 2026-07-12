//! Grid coordinates: [`Row`] (a row index) and [`Col`] (a column index).
//!
//! Both wrap a `u16`, but they are distinct types so the grid API and the
//! cursor can't be called with the axes swapped — `put_cell(row, col)` won't
//! type-check if you hand it a column where the row goes. The convention is
//! `(row, col)` order everywhere.
//!
//! The wrappers guard the *axis*, not the *role*: two [`Row`]s (a start and an
//! end) can still be swapped among themselves, and dimensions reuse the same
//! types (a `max_rows: Row` bound is a `Row`, not a separate `RowCount`). The
//! grid's internal storage stays `usize`/`u16`; these are the public API layer,
//! converted at the method boundary with [`Row::get`] / [`Col::get`].

/// A 0-based grid row index. See the [module docs](self) for why this is a
/// newtype and not a bare `u16`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Row(u16);

/// A 0-based grid column index. See the [module docs](self).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Col(u16);

impl Row {
    /// Row 0 (top of the grid). Saves spelling `Row::new(0)` at the many
    /// home/carriage-return sites.
    pub const ZERO: Self = Self(0);

    /// Wrap a raw row index.
    pub const fn new(v: u16) -> Self {
        Self(v)
    }

    /// The raw row index, for indexing into internal `usize` storage or feeding
    /// arithmetic the newtype deliberately doesn't expose.
    pub const fn get(self) -> u16 {
        self.0
    }

    /// Move down `n` rows, saturating at `u16::MAX` (the grid clamps to the last
    /// real row separately). The additive counterpart of [`Row::retreat`].
    #[must_use]
    pub const fn advance(self, n: u16) -> Self {
        Self(self.0.saturating_add(n))
    }

    /// Move up `n` rows, saturating at row 0.
    #[must_use]
    pub const fn retreat(self, n: u16) -> Self {
        Self(self.0.saturating_sub(n))
    }
}

impl Col {
    /// Column 0 (left edge). See [`Row::ZERO`].
    pub const ZERO: Self = Self(0);

    /// Wrap a raw column index.
    pub const fn new(v: u16) -> Self {
        Self(v)
    }

    /// The raw column index. See [`Row::get`].
    pub const fn get(self) -> u16 {
        self.0
    }

    /// Move right `n` columns, saturating at `u16::MAX`.
    #[must_use]
    pub const fn advance(self, n: u16) -> Self {
        Self(self.0.saturating_add(n))
    }

    /// Move left `n` columns, saturating at column 0.
    #[must_use]
    pub const fn retreat(self, n: u16) -> Self {
        Self(self.0.saturating_sub(n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_get_round_trip() {
        assert_eq!(Row::new(7).get(), 7);
        assert_eq!(Col::new(42).get(), 42);
        assert_eq!(Row::ZERO.get(), 0);
        assert_eq!(Col::ZERO.get(), 0);
    }

    #[test]
    fn advance_retreat_saturate() {
        assert_eq!(Row::new(3).advance(2), Row::new(5));
        assert_eq!(Row::new(3).retreat(2), Row::new(1));
        assert_eq!(Row::new(1).retreat(5), Row::ZERO, "retreat saturates at 0");
        assert_eq!(
            Col::new(u16::MAX).advance(5),
            Col::new(u16::MAX),
            "advance saturates at u16::MAX"
        );
    }

    #[test]
    fn ord_is_by_index() {
        assert!(Row::new(2) < Row::new(3));
        assert_eq!(Row::new(2).max(Row::new(5)), Row::new(5));
        assert_eq!(Col::new(2).min(Col::new(5)), Col::new(2));
    }
}
