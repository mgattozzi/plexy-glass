//! `KeyEvent` → legacy VT/xterm byte encoding.
//!
//! Used by tests and by features that need to synthesize key bytes for the
//! shell (e.g. click-to-position). Production pass-through preserves the
//! original bytes from the parser, so this is NOT in the hot path.

use plexy_glass_mux::{Direction, Key, KeyEvent, Modifiers};

pub fn legacy_bytes(event: KeyEvent) -> Vec<u8> {
    match event.key {
        Key::Char(c) if event.mods.is_empty() => {
            let mut buf = [0u8; 4];
            c.encode_utf8(&mut buf).as_bytes().to_vec()
        }
        Key::Char(c) if event.mods == Modifiers::CTRL && c.is_ascii_alphabetic() => {
            // Ctrl+a..z -> 0x01..0x1a
            vec![(c.to_ascii_lowercase() as u8) - b'`']
        }
        Key::Char(c) if event.mods == Modifiers::ALT => {
            let mut out = vec![0x1b];
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            out
        }
        Key::Tab if event.mods.is_empty() => vec![0x09],
        Key::Tab if event.mods == Modifiers::SHIFT => b"\x1b[Z".to_vec(),
        Key::Enter if event.mods.is_empty() => vec![0x0d],
        Key::Backspace if event.mods.is_empty() => vec![0x7f],
        Key::Escape if event.mods.is_empty() => vec![0x1b],
        Key::Arrow(dir) => arrow_bytes(dir, event.mods),
        Key::Home => with_mods(b'H', b'H', event.mods),
        Key::End => with_mods(b'F', b'F', event.mods),
        Key::Insert => tilde(2, event.mods),
        Key::Delete => tilde(3, event.mods),
        Key::PageUp => tilde(5, event.mods),
        Key::PageDown => tilde(6, event.mods),
        Key::Function(n) if (1..=4).contains(&n) => f1_to_f4(n, event.mods),
        Key::Function(5) => tilde(15, event.mods),
        Key::Function(6) => tilde(17, event.mods),
        Key::Function(7) => tilde(18, event.mods),
        Key::Function(8) => tilde(19, event.mods),
        Key::Function(9) => tilde(20, event.mods),
        Key::Function(10) => tilde(21, event.mods),
        Key::Function(11) => tilde(23, event.mods),
        Key::Function(12) => tilde(24, event.mods),
        // Modifier combinations for Tab/Enter/Backspace/Escape beyond the
        // simple cases above have no widely-agreed legacy encoding.
        Key::Tab | Key::Enter | Key::Backspace | Key::Escape => Vec::new(),
        Key::Function(_) | Key::KeypadEnter | Key::Char(_) => Vec::new(),
    }
}

fn arrow_bytes(dir: Direction, mods: Modifiers) -> Vec<u8> {
    let final_byte = match dir {
        Direction::Up => b'A',
        Direction::Down => b'B',
        Direction::Right => b'C',
        Direction::Left => b'D',
    };
    with_mods(final_byte, final_byte, mods)
}

fn with_mods(_ss3_byte: u8, csi_byte: u8, mods: Modifiers) -> Vec<u8> {
    if mods.is_empty() {
        return vec![0x1b, b'[', csi_byte];
    }
    let m = encode_xterm_mods(mods);
    format!("\x1b[1;{m}{}", csi_byte as char).into_bytes()
}

fn tilde(n: u32, mods: Modifiers) -> Vec<u8> {
    if mods.is_empty() {
        format!("\x1b[{n}~").into_bytes()
    } else {
        let m = encode_xterm_mods(mods);
        format!("\x1b[{n};{m}~").into_bytes()
    }
}

fn f1_to_f4(n: u8, mods: Modifiers) -> Vec<u8> {
    let final_byte = match n {
        1 => b'P',
        2 => b'Q',
        3 => b'R',
        4 => b'S',
        // invariant: callers only pass 1..=4 into this function
        _ => return Vec::new(),
    };
    if mods.is_empty() {
        vec![0x1b, b'O', final_byte]
    } else {
        let m = encode_xterm_mods(mods);
        format!("\x1b[1;{m}{}", final_byte as char).into_bytes()
    }
}

fn encode_xterm_mods(mods: Modifiers) -> u32 {
    // Inverse of `parser::decode_xterm_mods`: param = 1 + (bits & 0xFF).
    1 + u32::from(mods.bits())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{KeyParseOutput, KeyParser};

    fn parse_first_event(bytes: &[u8]) -> KeyEvent {
        let mut p = KeyParser::new();
        for &b in bytes {
            if let KeyParseOutput::Event { event, .. } = p.consume(b) {
                return event;
            }
        }
        panic!("no event for bytes {bytes:?}");
    }

    #[test]
    fn encodes_arrows() {
        assert_eq!(legacy_bytes(KeyEvent::plain(Key::Arrow(Direction::Up))), b"\x1b[A");
        assert_eq!(legacy_bytes(KeyEvent::plain(Key::Arrow(Direction::Down))), b"\x1b[B");
    }

    #[test]
    fn round_trip_arrow() {
        let bytes = legacy_bytes(KeyEvent::plain(Key::Arrow(Direction::Up)));
        let parsed = parse_first_event(&bytes);
        assert_eq!(parsed, KeyEvent::plain(Key::Arrow(Direction::Up)));
    }

    #[test]
    fn round_trip_ctrl_left() {
        let original = KeyEvent::new(Key::Arrow(Direction::Left), Modifiers::CTRL);
        let bytes = legacy_bytes(original.clone());
        let parsed = parse_first_event(&bytes);
        assert_eq!(parsed, original);
    }

    #[test]
    fn round_trip_f5() {
        let original = KeyEvent::plain(Key::Function(5));
        let bytes = legacy_bytes(original.clone());
        let parsed = parse_first_event(&bytes);
        assert_eq!(parsed, original);
    }
}
