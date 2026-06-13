//! DEC private modes and ANSI modes as a `Modes` flags struct.

bitflags::bitflags! {
    /// ANSI + DEC-private terminal modes.
    ///
    /// Defaults match xterm: `AUTOWRAP` and `CURSOR_VISIBLE` on, everything else off.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Modes: u64 {
        const AUTOWRAP        = 1 << 0;  // DECAWM (?7)
        const ORIGIN          = 1 << 1;  // DECOM (?6)
        // bit 2 (IRM / ANSI mode 4) intentionally unused: insert mode is not
        // supported (no ICH/DCH/IL/DL/ECH) and DECRQM reports it unsupported.
        const ALT_SCREEN      = 1 << 3;  // ?1049
        const CURSOR_VISIBLE  = 1 << 4;  // ?25 (DECTCEM)
        const BRACKETED_PASTE = 1 << 5;  // ?2004
        const MOUSE_X10       = 1 << 6;  // ?9
        const MOUSE_BTN       = 1 << 7;  // ?1000
        const MOUSE_ANY       = 1 << 8;  // ?1003
        const MOUSE_SGR       = 1 << 9;  // ?1006
        const APP_CURSOR_KEYS = 1 << 10; // ?1 (DECCKM)
        const APP_KEYPAD      = 1 << 11; // DECKPAM/DECKPNM
        const MOUSE_BTN_EVENT      = 1 << 12; // ?1002 (button-event tracking)
        const FOCUS_EVENTS         = 1 << 13; // ?1004
        const COLOR_SCHEME_UPDATES = 1 << 14; // ?2031
    }
}

impl Default for Modes {
    fn default() -> Self {
        Modes::AUTOWRAP | Modes::CURSOR_VISIBLE
    }
}

impl Modes {
    /// Mask of every mouse-reporting `Modes` bit (?9, ?1000, ?1002, ?1003, ?1006).
    pub const MOUSE_REPORTING_BITS: Self = Self::empty()
        .union(Self::MOUSE_X10)
        .union(Self::MOUSE_BTN)
        .union(Self::MOUSE_BTN_EVENT) // ?1002 sends events like ?1000/?1003
        .union(Self::MOUSE_ANY)
        .union(Self::MOUSE_SGR);

    /// True if any of the DEC mouse-reporting modes (?9 / ?1000 / ?1002 / ?1003 /
    /// ?1006) is currently enabled, meaning the child wants raw mouse events.
    pub fn any_mouse_mode_active(self) -> bool {
        self.intersects(Self::MOUSE_REPORTING_BITS)
    }

    /// DECRQM `Pm` value for a queried private mode `ps`: 1 = set, 2 = reset,
    /// 0 = not recognized (still echoed by the caller). Consults the *specific*
    /// bit, not the mouse-gating mask.
    pub fn decrqm_state(self, ps: u16) -> u8 {
        let flag = match ps {
            1 => Self::APP_CURSOR_KEYS,
            7 => Self::AUTOWRAP,
            25 => Self::CURSOR_VISIBLE,
            9 => Self::MOUSE_X10,
            1000 => Self::MOUSE_BTN,
            1002 => Self::MOUSE_BTN_EVENT,
            1003 => Self::MOUSE_ANY,
            1006 => Self::MOUSE_SGR,
            1004 => Self::FOCUS_EVENTS,
            1049 => Self::ALT_SCREEN,
            2004 => Self::BRACKETED_PASTE,
            2031 => Self::COLOR_SCHEME_UPDATES,
            _ => return 0,
        };
        if self.contains(flag) { 1 } else { 2 }
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

    #[test]
    fn any_mouse_mode_active_reflects_a_mouse_bit() {
        // Default: no mouse reporting requested.
        let m = Modes::default();
        assert!(!m.any_mouse_mode_active());
        // Each mouse bit individually flips the helper to true.
        for bit in [
            Modes::MOUSE_X10,
            Modes::MOUSE_BTN,
            Modes::MOUSE_BTN_EVENT,
            Modes::MOUSE_ANY,
            Modes::MOUSE_SGR,
        ] {
            let m = bit;
            assert!(m.any_mouse_mode_active(), "{bit:?} should mark mouse reporting active");
        }
        // Non-mouse bits do not.
        let m = Modes::ALT_SCREEN | Modes::AUTOWRAP;
        assert!(!m.any_mouse_mode_active());
    }

    #[test]
    fn decrqm_state_reports_set_and_reset() {
        let mut m = Modes::default();
        m.insert(Modes::BRACKETED_PASTE);
        assert_eq!(m.decrqm_state(2004), 1);
        assert_eq!(m.decrqm_state(1004), 2, "focus events off → reset");
        assert_eq!(m.decrqm_state(9999), 0, "unknown mode");
    }

    #[test]
    fn decrqm_state_new_bits() {
        let mut m = Modes::default();
        m.insert(Modes::FOCUS_EVENTS);
        m.insert(Modes::COLOR_SCHEME_UPDATES);
        m.insert(Modes::MOUSE_BTN_EVENT);
        assert_eq!(m.decrqm_state(1004), 1);
        assert_eq!(m.decrqm_state(2031), 1);
        assert_eq!(m.decrqm_state(1002), 1);
    }
}
