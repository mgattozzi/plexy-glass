//! DEC private modes and ANSI modes as a `Modes` flags struct.

bitflags::bitflags! {
    /// ANSI + DEC-private terminal modes.
    ///
    /// Defaults match xterm: `AUTOWRAP` and `CURSOR_VISIBLE` on, everything else off.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Modes: u64 {
        const AUTOWRAP        = 1 << 0;  // DECAWM (?7)
        const ORIGIN          = 1 << 1;  // DECOM (?6)
        const INSERT          = 1 << 2;  // IRM
        const ALT_SCREEN      = 1 << 3;  // ?1049
        const CURSOR_VISIBLE  = 1 << 4;  // ?25 (DECTCEM)
        const BRACKETED_PASTE = 1 << 5;  // ?2004
        const MOUSE_X10       = 1 << 6;  // ?9
        const MOUSE_BTN       = 1 << 7;  // ?1000
        const MOUSE_ANY       = 1 << 8;  // ?1003
        const MOUSE_SGR       = 1 << 9;  // ?1006
        const APP_CURSOR_KEYS = 1 << 10; // ?1 (DECCKM)
        const APP_KEYPAD      = 1 << 11; // DECKPAM/DECKPNM
    }
}

impl Default for Modes {
    fn default() -> Self {
        Modes::AUTOWRAP | Modes::CURSOR_VISIBLE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_has_autowrap_and_cursor() {
        let m = Modes::default();
        assert!(m.contains(Modes::AUTOWRAP));
        assert!(m.contains(Modes::CURSOR_VISIBLE));
        assert!(!m.contains(Modes::ALT_SCREEN));
    }

    #[test]
    fn set_clear_alt_screen() {
        let mut m = Modes::default();
        m.insert(Modes::ALT_SCREEN);
        assert!(m.contains(Modes::ALT_SCREEN));
        m.remove(Modes::ALT_SCREEN);
        assert!(!m.contains(Modes::ALT_SCREEN));
    }
}
