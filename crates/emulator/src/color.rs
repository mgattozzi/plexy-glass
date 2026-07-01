//! Cell foreground/background color.

/// Color representation matching the SGR vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    /// The terminal's default fg/bg (rendered by the host TTY).
    #[default]
    Default,
    /// 256-color palette index (0..=15 are the named ANSI colors,
    /// 16..=231 are the 6×6×6 cube, 232..=255 are grayscale).
    Indexed(u8),
    /// True-color RGB triple.
    Rgb(u8, u8, u8),
}

impl Color {
    /// Map an ANSI 30-37 / 40-47 code to an indexed color.
    pub const fn from_ansi_basic(n: u8) -> Self {
        Self::Indexed(n)
    }

    /// Map an ANSI 90-97 / 100-107 bright code to an indexed color in 8..=15.
    pub const fn from_ansi_bright(n: u8) -> Self {
        Self::Indexed(n + 8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_default_variant() {
        assert_eq!(Color::default(), Color::Default);
    }

    #[test]
    fn indexed_roundtrips() {
        let c = Color::Indexed(42);
        assert_eq!(c, Color::Indexed(42));
        assert_ne!(c, Color::Default);
    }

    #[test]
    fn rgb_roundtrips() {
        let c = Color::Rgb(10, 20, 30);
        assert_eq!(c, Color::Rgb(10, 20, 30));
        assert_ne!(c, Color::Rgb(10, 20, 31));
    }

    #[test]
    fn from_ansi_basic_uses_index() {
        assert_eq!(Color::from_ansi_basic(1), Color::Indexed(1));
        assert_eq!(Color::from_ansi_basic(7), Color::Indexed(7));
    }

    #[test]
    fn from_ansi_bright_offsets_by_eight() {
        assert_eq!(Color::from_ansi_bright(0), Color::Indexed(8));
        assert_eq!(Color::from_ansi_bright(7), Color::Indexed(15));
    }
}
