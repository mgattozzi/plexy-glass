//! Typed keyboard events used by the keymap and input router.

use crate::Direction;
use bitflags::bitflags;

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct Modifiers: u8 {
        const SHIFT = 1 << 0;
        const CTRL  = 1 << 1;
        const ALT   = 1 << 2;
        const SUPER = 1 << 3;
        const HYPER = 1 << 4;
    }
}

impl Modifiers {
    /// `Meta` is a common alias for `Alt`. Accept it in user-facing parsing,
    /// always emit as `ALT` internally.
    pub fn alias_meta_as_alt(s: &str) -> Option<Self> {
        match s {
            "Shift" | "shift" | "SHIFT" => Some(Self::SHIFT),
            "Ctrl"  | "ctrl"  | "CTRL"  | "Control" | "control" => Some(Self::CTRL),
            "Alt"   | "alt"   | "ALT"   | "Meta"    | "meta" | "META" => Some(Self::ALT),
            "Super" | "super" | "SUPER" | "Cmd"     | "cmd"  | "CMD"  => Some(Self::SUPER),
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
    Function(u8),    // 1..=12
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyEvent {
    pub key: Key,
    pub mods: Modifiers,
}

impl KeyEvent {
    pub fn new(key: Key, mods: Modifiers) -> Self {
        Self { key, mods }
    }

    pub fn plain(key: Key) -> Self {
        Self { key, mods: Modifiers::empty() }
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
}
