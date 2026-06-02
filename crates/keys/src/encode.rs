//! `KeyEvent` → legacy VT/xterm byte encoding.
//!
//! Used by tests and by features that need to synthesize key bytes for the
//! shell (e.g. click-to-position). Production pass-through preserves the
//! original bytes from the parser, so this is NOT in the hot path.

use plexy_glass_mux::{Direction, Key, KeyEvent, KeyEventKind, Modifiers};

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
    // Shares the one body with the public `mods_param` so they cannot drift.
    mods_param(mods)
}

/// Where a canonical `KeyEvent` is being re-encoded to. The `u8` is the
/// modifyOtherKeys level (0/1/2) or the Kitty flag set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardTarget {
    Legacy,
    ModifyOtherKeys(u8),
    Kitty(u8),
}

/// Wire modifier param = `1 + (bits & 0xFF)`. Inverse of
/// `parser::KeyboardProtocol::decode_mods_param`.
pub fn mods_param(mods: Modifiers) -> u32 {
    1 + u32::from(mods.bits())
}

/// Re-encode a canonical key into the pane's negotiated protocol.
pub fn encode(event: &KeyEvent, target: KeyboardTarget, app_cursor: bool) -> Vec<u8> {
    match target {
        KeyboardTarget::Legacy => legacy_with_cursor(event, app_cursor),
        KeyboardTarget::ModifyOtherKeys(level) => modify_other_keys_bytes(event, level, app_cursor),
        KeyboardTarget::Kitty(flags) => kitty_bytes(event, flags, app_cursor),
    }
}

/// `legacy_bytes`, but honoring DECCKM for unmodified arrows (SS3 vs CSI).
fn legacy_with_cursor(event: &KeyEvent, app_cursor: bool) -> Vec<u8> {
    if let Key::Arrow(dir) = event.key
        && event.mods.is_empty()
        && app_cursor
    {
        let b = match dir {
            Direction::Up => b'A',
            Direction::Down => b'B',
            Direction::Right => b'C',
            Direction::Left => b'D',
        };
        return vec![0x1b, b'O', b];
    }
    legacy_bytes(event.clone())
}

/// The base (unshifted/lowercase) codepoint a key reports in CSI-u / 27-form.
/// Functional keys (arrows, F-keys, nav) have none, so they keep CSI forms.
fn key_codepoint(key: Key) -> Option<u32> {
    match key {
        Key::Char(c) => Some(u32::from(c.to_ascii_lowercase())),
        Key::Enter => Some(13),
        Key::Tab => Some(9),
        Key::Backspace => Some(127),
        Key::Escape => Some(27),
        _ => None,
    }
}

/// Well-known legacy meanings that modifyOtherKeys level 1 preserves.
fn is_well_known_legacy(event: &KeyEvent) -> bool {
    match event.key {
        Key::Tab | Key::Backspace | Key::Enter | Key::Escape => true,
        Key::Char(' ') if event.mods == Modifiers::CTRL => true, // Ctrl+Space = NUL
        Key::Char(c) if event.mods == Modifiers::CTRL && c.is_ascii_alphabetic() => true,
        _ => false,
    }
}

/// modifyOtherKeys 27-form: `\e[27;<mods>;<code>~` when the level encodes this
/// key; otherwise legacy. Unmodified keys are always legacy.
fn modify_other_keys_bytes(event: &KeyEvent, level: u8, app_cursor: bool) -> Vec<u8> {
    if level == 0 || event.mods.is_empty() {
        return legacy_with_cursor(event, app_cursor);
    }
    let Some(code) = key_codepoint(event.key) else {
        return legacy_with_cursor(event, app_cursor);
    };
    if level == 1 && is_well_known_legacy(event) {
        return legacy_with_cursor(event, app_cursor);
    }
    let m = mods_param(event.mods);
    format!("\x1b[27;{m};{code}~").into_bytes()
}

/// Kitty CSI-u, down-converted to the pane's `flags`:
/// - 0x02 report_event_types: include `:<event>` only on Repeat/Release
/// - 0x04 report_alternate_keys: include `:<shifted>[:<base>]`
/// - 0x08 report_all_keys: Enter/Tab/Backspace stay legacy unless set
/// - 0x10 report_associated_text: include the text param only if set
fn kitty_bytes(event: &KeyEvent, flags: u8, app_cursor: bool) -> Vec<u8> {
    let Some(code) = key_codepoint(event.key) else {
        // Arrows / F-keys / nav have no CSI-u code: keep their legacy CSI forms.
        return legacy_with_cursor(event, app_cursor);
    };
    let all_keys = flags & 0b1000 != 0;
    if !all_keys && matches!(event.key, Key::Enter | Key::Tab | Key::Backspace) {
        return legacy_with_cursor(event, app_cursor);
    }
    let report_events = flags & 0b0010 != 0;
    let report_alts = flags & 0b0100 != 0;
    let report_text = flags & 0b1_0000 != 0;

    // Param 1: code[:shifted[:base]]
    let mut key_field = code.to_string();
    if report_alts && (event.shifted.is_some() || event.base_layout.is_some()) {
        let sh = event.shifted.map(u32::from);
        let base = event.base_layout.map(u32::from);
        key_field = match (sh, base) {
            (Some(s), Some(b)) => format!("{code}:{s}:{b}"),
            (Some(s), None) => format!("{code}:{s}"),
            (None, Some(b)) => format!("{code}::{b}"),
            (None, None) => key_field,
        };
    }

    // Param 2: mods[:event]
    let m = mods_param(event.mods);
    let include_event = report_events && event.kind != KeyEventKind::Press;
    let event_code = match event.kind {
        KeyEventKind::Press => 1u32,
        KeyEventKind::Repeat => 2,
        KeyEventKind::Release => 3,
    };

    // Param 3: associated text codepoints, only if `report_associated_text`.
    if report_text
        && let Some(t) = event.text.as_ref()
        && !t.is_empty()
    {
        // A 3rd param requires param 2 to be present (even if mods == 1).
        let mods_field = if include_event {
            format!(";{m}:{event_code}")
        } else {
            format!(";{m}")
        };
        let cps: Vec<String> = t.chars().map(|c| u32::from(c).to_string()).collect();
        return format!("\x1b[{key_field}{mods_field};{}u", cps.join(":")).into_bytes();
    }

    let mods_field = if m != 1 || include_event {
        if include_event {
            format!(";{m}:{event_code}")
        } else {
            format!(";{m}")
        }
    } else {
        String::new()
    };
    format!("\x1b[{key_field}{mods_field}u").into_bytes()
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

    use crate::encode::{encode, mods_param, KeyboardTarget};
    use crate::parser::KeyboardProtocol;

    #[test]
    fn legacy_target_ctrl_a() {
        let e = KeyEvent::new(Key::Char('a'), Modifiers::CTRL);
        assert_eq!(encode(&e, KeyboardTarget::Legacy, false), vec![0x01]);
    }

    #[test]
    fn legacy_target_arrow_app_cursor_uses_ss3() {
        let e = KeyEvent::plain(Key::Arrow(Direction::Up));
        assert_eq!(encode(&e, KeyboardTarget::Legacy, false), b"\x1b[A");
        assert_eq!(encode(&e, KeyboardTarget::Legacy, true), b"\x1bOA");
    }

    #[test]
    fn modify_other_keys_27_form() {
        // Ctrl+Shift+i (code 105, mods 6) at level 2 -> 27-form.
        let e = KeyEvent::new(Key::Char('i'), Modifiers::CTRL | Modifiers::SHIFT);
        assert_eq!(encode(&e, KeyboardTarget::ModifyOtherKeys(2), false), b"\x1b[27;6;105~");
    }

    #[test]
    fn modify_other_keys_level1_leaves_tab_legacy() {
        let e = KeyEvent::plain(Key::Tab);
        assert_eq!(encode(&e, KeyboardTarget::ModifyOtherKeys(1), false), vec![0x09]);
    }

    #[test]
    fn modify_other_keys_unmodified_char_is_legacy() {
        let e = KeyEvent::plain(Key::Char('a'));
        assert_eq!(encode(&e, KeyboardTarget::ModifyOtherKeys(2), false), b"a");
    }

    #[test]
    fn kitty_target_csi_u_basic() {
        // disambiguate(1): Ctrl+Shift+i -> \e[105;6u
        let e = KeyEvent::new(Key::Char('i'), Modifiers::CTRL | Modifiers::SHIFT);
        assert_eq!(encode(&e, KeyboardTarget::Kitty(1), false), b"\x1b[105;6u");
    }

    #[test]
    fn kitty_omits_event_type_unless_bit2() {
        let mut e = KeyEvent::new(Key::Char('a'), Modifiers::CTRL);
        e.kind = plexy_glass_mux::KeyEventKind::Release;
        // flags=1: no report_event_types -> event sub-param omitted.
        assert_eq!(encode(&e, KeyboardTarget::Kitty(1), false), b"\x1b[97;5u");
        // flags 1|2: report_event_types -> :3 present.
        assert_eq!(encode(&e, KeyboardTarget::Kitty(1 | 2), false), b"\x1b[97;5:3u");
    }

    #[test]
    fn kitty_keeps_enter_legacy_unless_bit8() {
        let e = KeyEvent::plain(Key::Enter);
        assert_eq!(encode(&e, KeyboardTarget::Kitty(1), false), vec![0x0d]);
        assert_eq!(encode(&e, KeyboardTarget::Kitty(1 | 8), false), b"\x1b[13u");
    }

    #[test]
    fn mods_param_round_trips_with_parser() {
        for m in [
            Modifiers::empty(),
            Modifiers::CTRL,
            Modifiers::CTRL | Modifiers::SHIFT,
            Modifiers::ALT | Modifiers::SUPER,
        ] {
            assert_eq!(KeyboardProtocol::decode_mods_param(mods_param(m)), m);
        }
    }

    #[test]
    fn round_trip_kitty_ctrl_i() {
        let original = KeyEvent::new(Key::Char('i'), Modifiers::CTRL);
        let bytes = encode(&original, KeyboardTarget::Kitty(1), false);
        let mut p = KeyParser::new().with_protocol(KeyboardProtocol::Kitty);
        let mut got = None;
        for &b in &bytes {
            if let KeyParseOutput::Event { event, .. } = p.consume(b) {
                got = Some(event);
            }
        }
        let got = got.expect("decoded event");
        assert_eq!(got.key, original.key);
        assert_eq!(got.mods, original.mods);
    }
}
