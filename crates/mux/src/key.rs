//! Typed keyboard events used by the keymap and input router.

use bitflags::bitflags;

use crate::Direction;

bitflags! {
    /// Keyboard modifiers, aligned to the wire convention so that
    /// `1 + (mods.bits() & 0xFF)` is the protocol modifier byte (kitty /
    /// modifyOtherKeys / xterm CSI-u share this).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct Modifiers: u8 {
        const SHIFT     = 1 << 0;
        const ALT       = 1 << 1;
        const CTRL      = 1 << 2;
        const SUPER     = 1 << 3;
        const HYPER     = 1 << 4;
        const META      = 1 << 5;
        const CAPS_LOCK = 1 << 6;
        const NUM_LOCK  = 1 << 7;
    }
}

impl Modifiers {
    /// `Meta` is a common alias for `Alt`. Accept it in user-facing parsing,
    /// always emit as `ALT` internally.
    pub fn alias_meta_as_alt(s: &str) -> Option<Self> {
        match s {
            "Shift" | "shift" | "SHIFT" => Some(Self::SHIFT),
            "Ctrl" | "ctrl" | "CTRL" | "Control" | "control" => Some(Self::CTRL),
            "Alt" | "alt" | "ALT" | "Meta" | "meta" | "META" => Some(Self::ALT),
            "Super" | "super" | "SUPER" | "Cmd" | "cmd" | "CMD" => Some(Self::SUPER),
            "Hyper" | "hyper" | "HYPER" => Some(Self::HYPER),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    /// Any printable scalar (or non-printable control char if it arrives raw).
    Char(char),
    Arrow(Direction),
    Function(u8), // 1..=12
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    Tab,
    Enter,
    Backspace,
    Escape,
    KeypadEnter,
}

/// Press / autorepeat / release. Decoded from the Kitty `:event` subparam;
/// legacy and modifyOtherKeys input is always `Press`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum KeyEventKind {
    #[default]
    Press,
    Repeat,
    Release,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeyEvent {
    pub key: Key,
    pub mods: Modifiers,
    /// Press / Repeat / Release (Kitty `report_event_types`).
    pub kind: KeyEventKind,
    /// Associated text (Kitty `report_associated_text`); carried, not composed.
    pub text: Option<smol_str::SmolStr>,
    /// Shifted alternate codepoint (Kitty `report_alternate_keys`).
    pub shifted: Option<char>,
    /// Base-layout alternate codepoint (Kitty `report_alternate_keys`).
    pub base_layout: Option<char>,
}

impl KeyEvent {
    pub const fn new(key: Key, mods: Modifiers) -> Self {
        Self {
            key,
            mods,
            kind: KeyEventKind::Press,
            text: None,
            shifted: None,
            base_layout: None,
        }
    }

    pub const fn plain(key: Key) -> Self {
        Self::new(key, Modifiers::empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifiers_parses_aliases() {
        assert_eq!(Modifiers::alias_meta_as_alt("Meta"), Some(Modifiers::ALT));
        assert_eq!(Modifiers::alias_meta_as_alt("Ctrl"), Some(Modifiers::CTRL));
        assert_eq!(Modifiers::alias_meta_as_alt("Cmd"), Some(Modifiers::SUPER));
        assert_eq!(Modifiers::alias_meta_as_alt("nonsense"), None);
    }

    #[test]
    fn plain_event_has_empty_mods() {
        let e = KeyEvent::plain(Key::Tab);
        assert_eq!(e.key, Key::Tab);
        assert!(e.mods.is_empty());
    }

    #[test]
    fn modifier_bits_align_to_wire_convention() {
        // Wire modifier param is `1 + bitset`: shift=1, alt=2, ctrl=4, super=8,
        // hyper=16, meta=32, caps_lock=64, num_lock=128.
        assert_eq!(Modifiers::SHIFT.bits(), 1 << 0);
        assert_eq!(Modifiers::ALT.bits(), 1 << 1);
        assert_eq!(Modifiers::CTRL.bits(), 1 << 2);
        assert_eq!(Modifiers::SUPER.bits(), 1 << 3);
        assert_eq!(Modifiers::HYPER.bits(), 1 << 4);
        assert_eq!(Modifiers::META.bits(), 1 << 5);
        assert_eq!(Modifiers::CAPS_LOCK.bits(), 1 << 6);
        assert_eq!(Modifiers::NUM_LOCK.bits(), 1 << 7);
        let m = Modifiers::CTRL | Modifiers::SHIFT;
        // `bits()` is already a u8, so `& 0xFF` is a no-op here; the wire byte
        // is simply `1 + bits`.
        assert_eq!(1 + u32::from(m.bits()), 6); // ctrl+shift -> param 6
    }

    #[test]
    fn alias_meta_still_maps_to_alt_for_keybindings() {
        // User-facing "Meta" alias maps to ALT (keybinding ergonomics); the
        // distinct META bit is set only from Kitty wire meta.
        assert_eq!(Modifiers::alias_meta_as_alt("Meta"), Some(Modifiers::ALT));
        assert_ne!(Modifiers::ALT, Modifiers::META);
    }

    #[test]
    fn new_and_plain_default_kind_press_and_none_fields() {
        let e = KeyEvent::new(Key::Char('a'), Modifiers::CTRL);
        assert_eq!(e.kind, KeyEventKind::Press);
        assert!(e.text.is_none());
        assert!(e.shifted.is_none());
        assert!(e.base_layout.is_none());
        assert_eq!(KeyEvent::plain(Key::Tab).kind, KeyEventKind::Press);
    }

    #[test]
    fn key_event_kind_default_is_press() {
        assert_eq!(KeyEventKind::default(), KeyEventKind::Press);
    }
}
