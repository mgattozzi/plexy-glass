//! SGR character attributes (bold, italic, …) as a bitflags struct.

/// The *style* dimension of an underline (SGR `4:0`..`4:5`), independent of the
/// underline *color* (SGR 58/59, stored separately on the cell). `Attrs::UNDERLINE`
/// remains the "any underline present" boolean; this records which kind so the diff
/// renderer can re-emit `4:N` to the outer terminal instead of flattening to `4`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnderlineStyle {
    /// No underline (SGR `4:0` / `24`).
    #[default]
    None,
    /// Straight single underline (SGR `4` / `4:1`).
    Single,
    /// Double underline (SGR `4:2`).
    Double,
    /// Curly / undercurl (SGR `4:3`).
    Curly,
    /// Dotted underline (SGR `4:4`).
    Dotted,
    /// Dashed underline (SGR `4:5`).
    Dashed,
}

impl UnderlineStyle {
    /// Map an SGR underline sub-parameter style code (`4:N`) to a style. Codes
    /// outside `0..=5` clamp to `Single` (any underline present, unknown shape).
    pub fn from_sgr_subparam(code: u16) -> Self {
        match code {
            0 => UnderlineStyle::None,
            1 => UnderlineStyle::Single,
            2 => UnderlineStyle::Double,
            3 => UnderlineStyle::Curly,
            4 => UnderlineStyle::Dotted,
            5 => UnderlineStyle::Dashed,
            _ => UnderlineStyle::Single,
        }
    }
}

bitflags::bitflags! {
    /// Per-cell character attributes from SGR.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct Attrs: u16 {
        const BOLD          = 1 << 0;
        const DIM           = 1 << 1;
        const ITALIC        = 1 << 2;
        const UNDERLINE     = 1 << 3;
        const BLINK         = 1 << 4;
        const REVERSE       = 1 << 5;
        const HIDDEN        = 1 << 6;
        const STRIKETHROUGH = 1 << 7;
        /// Search-match highlight (copy-mode). The renderer should paint a
        /// distinctive background (yellow) so matches stand out from
        /// REVERSE (which copy-mode selection uses).
        const HIGHLIGHT     = 1 << 8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_default() {
        assert_eq!(Attrs::default(), Attrs::empty());
    }

    #[test]
    fn flags_compose() {
        let combo = Attrs::BOLD | Attrs::UNDERLINE;
        assert!(combo.contains(Attrs::BOLD));
        assert!(combo.contains(Attrs::UNDERLINE));
        assert!(!combo.contains(Attrs::ITALIC));
    }

    #[test]
    fn insert_remove_idempotent() {
        let mut a = Attrs::empty();
        a.insert(Attrs::ITALIC);
        a.insert(Attrs::ITALIC);
        assert!(a.contains(Attrs::ITALIC));
        a.remove(Attrs::ITALIC);
        a.remove(Attrs::ITALIC);
        assert!(!a.contains(Attrs::ITALIC));
    }
}
