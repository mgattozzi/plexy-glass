//! A single grid cell: one grapheme cluster plus its colors and attributes.

use crate::{attrs::Attrs, color::Color};
use smol_str::SmolStr;

/// One screen cell.
///
/// Wide characters (CJK, most emoji) occupy two grid columns. The first column
/// holds a `Cell` with the grapheme and width 2, and the second holds a
/// "wide spacer" cell (empty grapheme) so the grid stays rectangular. Use
/// `Cell::wide_spacer()` to construct one and `Cell::is_wide_spacer()` to check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// Grapheme cluster contents. Empty string = wide spacer.
    pub grapheme: SmolStr,
    pub fg: Color,
    pub bg: Color,
    /// Underline color (SGR 58/59). `Color::Default` = follow text fg.
    /// Independent of whether `Attrs::UNDERLINE` is set.
    pub underline_color: Color,
    pub attrs: Attrs,
    /// Index into the screen's `HyperlinkTable`, if this cell is part of a
    /// hyperlinked region (OSC 8).
    pub hyperlink_id: Option<u16>,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            grapheme: SmolStr::new(" "),
            fg: Color::Default,
            bg: Color::Default,
            underline_color: Color::Default,
            attrs: Attrs::empty(),
            hyperlink_id: None,
        }
    }
}

impl Cell {
    /// A blank cell (single space, default attrs).
    pub fn blank() -> Self {
        Self::default()
    }

    /// The right half of a wide character. Carries no grapheme of its own.
    pub fn wide_spacer() -> Self {
        Self {
            grapheme: SmolStr::default(),
            fg: Color::Default,
            bg: Color::Default,
            underline_color: Color::Default,
            attrs: Attrs::empty(),
            hyperlink_id: None,
        }
    }

    pub fn is_wide_spacer(&self) -> bool {
        self.grapheme.is_empty()
    }

    pub fn is_blank(&self) -> bool {
        self.grapheme.as_str() == " "
            && self.fg == Color::Default
            && self.bg == Color::Default
            && self.underline_color == Color::Default
            && self.attrs.is_empty()
            && self.hyperlink_id.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_cell_is_blank_space() {
        let c = Cell::default();
        assert_eq!(c.grapheme.as_str(), " ");
        assert!(c.is_blank());
        assert!(!c.is_wide_spacer());
    }

    #[test]
    fn wide_spacer_is_recognizable() {
        let s = Cell::wide_spacer();
        assert!(s.is_wide_spacer());
        assert!(!s.is_blank());
    }

    #[test]
    fn blank_returns_default() {
        assert_eq!(Cell::blank(), Cell::default());
    }
}
