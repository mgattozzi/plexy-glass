//! Line-space newtypes for the fold/scroll subsystem.
//!
//! The compositor juggles three distinct `u32` line-index spaces that must
//! never be mixed. Wrapping each in its own type turns crossing them into a
//! compile error instead of a silent off-by-a-space bug (the whole reason this
//! module exists — the fold/scroll math is where those three spaces meet):
//!
//! - [`UnifiedLine`] — absolute index into a pane's unified line space
//!   (scrollback rows first, then the active grid). This is what `Screen`'s row
//!   API and the `blocks` prompt/block helpers speak.
//! - [`VisibleLine`] — index in the FOLDED visible space (unified minus folded
//!   output ranges) that the viewport actually stacks.
//! - [`ScrollOffset`] — scrollback offset in visible-line space: how many
//!   visible lines the viewport is scrolled up from the live bottom.
//!
//! Conversions between spaces go through [`crate::blocks::FoldProjection`]
//! (`to_unified` / `from_unified`) and the scroll-geometry helpers
//! ([`crate::blocks::scroll_line_at`] etc.); nothing else crosses a boundary.
//! A *difference* of two same-space lines is a plain count, so `saturating_delta`
//! returns the primitive `u32` on purpose. `new`/`get` are the only raw hatches,
//! used at a genuine boundary (the screen row API, atomic storage, the wire) —
//! never to launder one space into another.

/// Define a `u32` line-index newtype with the shared arithmetic the fold/scroll
/// code needs: no bare `+`/`-` that would let two spaces mix, only same-space
/// `advance`/`retreat`/`saturating_delta`.
macro_rules! line_newtype {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(u32);

        impl $name {
            /// Wrap a raw index. Used only at a space boundary (the row API,
            /// atomic storage, the wire) — never to launder one space into another.
            #[must_use]
            pub const fn new(v: u32) -> Self {
                Self(v)
            }

            /// The raw index, for the row API / boundary arithmetic.
            #[must_use]
            pub const fn get(self) -> u32 {
                self.0
            }

            /// This line `n` positions later (saturating).
            #[must_use]
            pub const fn advance(self, n: u32) -> Self {
                Self(self.0.saturating_add(n))
            }

            /// This line `n` positions earlier (saturating).
            #[must_use]
            pub const fn retreat(self, n: u32) -> Self {
                Self(self.0.saturating_sub(n))
            }

            /// Count of lines from `base` up to `self` (saturating; `0` when
            /// `self <= base`). A difference of two same-space lines is a count,
            /// not a line, so this returns a plain `u32`.
            #[must_use]
            pub const fn saturating_delta(self, base: Self) -> u32 {
                self.0.saturating_sub(base.0)
            }
        }
    };
}

line_newtype! {
    /// Absolute index into a pane's unified line space: scrollback rows first,
    /// then the active grid. What `Screen`'s row API and the `blocks` helpers speak.
    UnifiedLine
}

line_newtype! {
    /// Index in the folded visible space (unified minus folded output ranges) that
    /// the viewport actually stacks. Maps to/from [`UnifiedLine`] via the fold
    /// projection.
    VisibleLine
}

line_newtype! {
    /// Scrollback offset in visible-line space: how many visible lines the viewport
    /// is scrolled up from the live bottom (`0` = live).
    ScrollOffset
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advance_retreat_saturate() {
        assert_eq!(UnifiedLine::new(3).advance(2), UnifiedLine::new(5));
        assert_eq!(VisibleLine::new(3).retreat(5), VisibleLine::new(0));
        assert_eq!(ScrollOffset::new(u32::MAX).advance(1).get(), u32::MAX);
    }

    #[test]
    fn saturating_delta_is_a_count() {
        assert_eq!(VisibleLine::new(7).saturating_delta(VisibleLine::new(4)), 3);
        // Below the base saturates to 0, never a negative/underflow.
        assert_eq!(VisibleLine::new(4).saturating_delta(VisibleLine::new(7)), 0);
    }

    #[test]
    fn ordering_follows_the_index() {
        assert!(UnifiedLine::new(2) < UnifiedLine::new(3));
        assert!(ScrollOffset::new(5) > ScrollOffset::new(1));
    }
}
