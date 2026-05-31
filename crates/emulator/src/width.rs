//! Unicode display-width and grapheme helpers, the single source of truth for
//! text measurement and layout across the workspace.
//!
//! Terminal layout is measured in *display columns*, not bytes or `char`s: a
//! CJK ideograph or most emoji occupy two columns, combining marks zero. Every
//! width / alignment / truncation / centering computation in plexy-glass must
//! go through these helpers so all crates measure identically. `char`/byte
//! counts (`s.len()`, `s.chars().count()`) are only correct for ASCII and must
//! not be used for layout.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Display width of a string in terminal columns (wide graphemes count 2,
/// combining marks 0). Clamped to `u16`, matching terminal dimensions.
pub fn display_width(s: &str) -> u16 {
    u16::try_from(UnicodeWidthStr::width(s)).unwrap_or(u16::MAX)
}

/// Display width of a single `char`: 0 (control / combining), 1, or 2.
pub fn char_width(c: char) -> u16 {
    UnicodeWidthChar::width(c).unwrap_or(0) as u16
}

/// Column advance for one grapheme cluster placed in the grid: its display
/// width, but at least 1 so a lone zero-width cluster still consumes a cell.
/// Mirrors the diff renderer's cursor advance.
pub fn grapheme_advance(g: &str) -> u16 {
    display_width(g).max(1)
}

/// Iterate `(grapheme, advance)` pairs, the building block for painting text
/// into a grid: place the grapheme at the running column, then a wide spacer
/// when `advance == 2`, then add `advance` to the column.
pub fn graphemes_with_width(s: &str) -> impl Iterator<Item = (&str, u16)> {
    s.graphemes(true).map(|g| (g, grapheme_advance(g)))
}

/// Longest prefix of `s` whose display width is `<= max`, never splitting a
/// grapheme cluster or a wide cell. Returns a sub-slice of `s`.
pub fn truncate_to_width(s: &str, max: u16) -> &str {
    let mut used = 0u16;
    let mut end = 0usize;
    for g in s.graphemes(true) {
        let w = grapheme_advance(g);
        if used + w > max {
            break;
        }
        used += w;
        end += g.len();
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_width_is_char_count() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width(""), 0);
    }

    #[test]
    fn cjk_and_emoji_are_two_wide() {
        assert_eq!(display_width("好"), 2);
        assert_eq!(display_width("👋"), 2);
        assert_eq!(display_width("a好b"), 4);
    }

    #[test]
    fn combining_mark_is_zero_width_but_advances_one() {
        // "e" + combining acute accent = one grapheme, width 1.
        assert_eq!(display_width("e\u{301}"), 1);
        assert_eq!(grapheme_advance("e\u{301}"), 1);
        // A lone combining mark is zero-width but still advances one cell.
        assert_eq!(char_width('\u{301}'), 0);
        assert_eq!(grapheme_advance("\u{301}"), 1);
    }

    #[test]
    fn char_widths() {
        assert_eq!(char_width('a'), 1);
        assert_eq!(char_width('好'), 2);
        assert_eq!(char_width('\u{0}'), 0);
    }

    #[test]
    fn graphemes_with_width_clusters_and_measures() {
        let v: Vec<(&str, u16)> = graphemes_with_width("a好").collect();
        assert_eq!(v, vec![("a", 1), ("好", 2)]);
    }

    #[test]
    fn truncate_never_splits_a_wide_cell() {
        // "好好" is 4 columns; a 3-column budget keeps only the first (2 cols).
        assert_eq!(truncate_to_width("好好", 4), "好好");
        assert_eq!(truncate_to_width("好好", 3), "好");
        assert_eq!(truncate_to_width("好好", 1), "");
        assert_eq!(truncate_to_width("abc", 2), "ab");
        assert_eq!(truncate_to_width("abc", 10), "abc");
    }
}
