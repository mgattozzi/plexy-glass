//! SGR character attributes (bold, italic, …) as a bitflags struct.

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
