//! `KeyEvent` → legacy VT/xterm byte encoding.
//!
//! Used by tests and by features that need to synthesize key bytes for the
//! shell (e.g. click-to-position). Production pass-through preserves the
//! original bytes from the parser, so this is NOT in the hot path.

use plexy_glass_mux::{Direction, Key, KeyEvent, KeyEventKind, Modifiers};

fn legacy_bytes(event: KeyEvent) -> Vec<u8> {
    // Text-producing events (plain or shifted chars) type their text. This
    // covers down-converting a rich outer's `Shift+i` to "I" for a legacy
    // pane (previously the Shift case fell through and ATE the key).
    if let Some(text) = text_producing(&event) {
        return text.into_bytes();
    }
    match event.key {
        Key::Char(c) if event.mods == Modifiers::CTRL && c.is_ascii_alphabetic() => {
            // Ctrl+a..z -> 0x01..0x1a
            vec![(c.to_ascii_lowercase() as u8) - b'`']
        }
        // Ctrl+Space -> NUL, honoring `is_well_known_legacy`'s contract (emacs
        // set-mark, vim ^@). Without this it falls through to the catch-all and
        // is silently eaten.
        Key::Char(' ') if event.mods == Modifiers::CTRL => vec![0x00],
        Key::Char(_) if event.mods.contains(Modifiers::ALT) => {
            // Alt is the Meta/ESC prefix; emit ESC then the same key with Alt
            // removed. So Alt+x -> ESC x, Alt+Shift+a -> ESC 'A', Alt+Ctrl+a ->
            // ESC 0x01, degrading rather than eating the keystroke. Recursion
            // terminates: the inner event no longer has the Alt bit.
            let mut inner = event;
            inner.mods.remove(Modifiers::ALT);
            let mut out = vec![0x1b];
            out.extend_from_slice(&legacy_bytes(inner));
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
        // exact forms above have no faithful legacy encoding. Degrade rather
        // than eat the keystroke: Alt keeps its legacy ESC prefix (tmux/
        // readline behavior, Alt+Backspace is delete-word); other modifiers
        // strip to the base key.
        Key::Tab => base_with_alt_prefix(event.mods, 0x09),
        Key::Enter => base_with_alt_prefix(event.mods, 0x0d),
        Key::Backspace => base_with_alt_prefix(event.mods, 0x7f),
        Key::Escape => base_with_alt_prefix(event.mods, 0x1b),
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
    let m = mods_param(mods);
    format!("\x1b[1;{m}{}", csi_byte as char).into_bytes()
}

fn tilde(n: u32, mods: Modifiers) -> Vec<u8> {
    if mods.is_empty() {
        format!("\x1b[{n}~").into_bytes()
    } else {
        let m = mods_param(mods);
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
        let m = mods_param(mods);
        format!("\x1b[1;{m}{}", final_byte as char).into_bytes()
    }
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
/// `parser::decode_xterm_mods`.
pub fn mods_param(mods: Modifiers) -> u32 {
    1 + u32::from(mods.bits())
}

/// Re-encode a canonical key into the pane's negotiated protocol.
pub fn encode(event: &KeyEvent, target: KeyboardTarget, app_cursor: bool) -> Vec<u8> {
    // Drop Repeat/Release events for any target that did NOT ask for event types:
    // legacy and modifyOtherKeys have no concept of them, and a Kitty pane only
    // wants them when its `report_event_types` flag (0x02) is set. Without this,
    // a release re-encodes to the same bytes as the press and DOUBLES every
    // keystroke (the exact garble seen when an outer terminal with event-type
    // reporting feeds a legacy pane).
    let target_reports_events = matches!(target, KeyboardTarget::Kitty(f) if f & 0x02 != 0);
    if event.kind != KeyEventKind::Press && !target_reports_events {
        return Vec::new();
    }
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

/// The text a key event types, when it is a text-producing event per the
/// Kitty spec: a `Char` key whose modifiers are at most Shift and the lock
/// keys. Such events are delivered as their text (not escape codes) unless
/// the pane pushed "report all keys as escape codes" (0b1000). Preference
/// order: the event's own `text` (the outer terminal told us exactly what was
/// typed), the shifted alternate when Shift is held, best-effort uppercase,
/// the char itself. Ctrl/Alt/Super combos, Esc, Enter/Tab/Backspace produce
/// no text here, so they keep their escape encodings.
fn text_producing(event: &KeyEvent) -> Option<String> {
    let Key::Char(c) = event.key else { return None };
    let non_text_mods = event
        .mods
        .difference(Modifiers::SHIFT | Modifiers::CAPS_LOCK | Modifiers::NUM_LOCK);
    if !non_text_mods.is_empty() {
        return None;
    }
    if let Some(t) = event.text.as_ref().filter(|t| !t.is_empty()) {
        return Some(t.to_string());
    }
    if event.mods.contains(Modifiers::SHIFT) {
        if let Some(sh) = event.shifted {
            return Some(sh.to_string());
        }
        return Some(c.to_uppercase().to_string());
    }
    Some(c.to_string())
}

/// The legacy degrade for a modified Enter/Tab/Backspace/Escape: the base
/// byte, with the ESC prefix when Alt is held (the one modifier legacy
/// encodings CAN express).
fn base_with_alt_prefix(mods: Modifiers, base: u8) -> Vec<u8> {
    if mods.contains(Modifiers::ALT) {
        vec![0x1b, base]
    } else {
        vec![base]
    }
}

/// Whether this exact (key, mods) pair has a faithful legacy encoding, i.e.
/// one that preserves the modifier rather than a degraded base-key form.
/// Modified Enter/Backspace (e.g. Shift+Enter) have NONE: kitty/xterm send
/// them as escape codes, and routing them to legacy used to eat the keystroke.
fn legacy_exact_form(event: &KeyEvent) -> bool {
    match event.key {
        Key::Enter | Key::Backspace | Key::Escape => event.mods.is_empty(),
        Key::Tab => event.mods.is_empty() || event.mods == Modifiers::SHIFT, // \e[Z
        _ => false,
    }
}

/// Well-known legacy meanings that modifyOtherKeys level 1 preserves.
fn is_well_known_legacy(event: &KeyEvent) -> bool {
    match event.key {
        Key::Tab | Key::Backspace | Key::Enter | Key::Escape => legacy_exact_form(event),
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
    // Kitty's legacy exception for Enter/Tab/Backspace applies ONLY when no
    // modifiers (beyond locks) are held; verified against kitty's encoder
    // (key_encoding.c). Every MODIFIED form, including Shift+Tab (which has a
    // legacy \e[Z but kitty still CSI-u's it under disambiguate), goes
    // through the CSI-u encoder below.
    if !all_keys
        && matches!(event.key, Key::Enter | Key::Tab | Key::Backspace)
        && event
            .mods
            .difference(Modifiers::CAPS_LOCK | Modifiers::NUM_LOCK)
            .is_empty()
    {
        return legacy_with_cursor(event, app_cursor);
    }
    // Kitty spec: without "report all keys as escape codes", text-producing
    // events are delivered AS their text (kitty sends "I" for Shift+I at
    // helix's flags 5), and they have no release events at all, since only
    // escape-coded keys report event types.
    if !all_keys
        && let Some(text) = text_producing(event)
    {
        return match event.kind {
            KeyEventKind::Release => Vec::new(),
            KeyEventKind::Press | KeyEventKind::Repeat => text.into_bytes(),
        };
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
    use crate::parser::{KeyParseOutput, KeyParser, decode_xterm_mods};

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
    fn kitty_text_producing_keys_stay_text_without_all_keys_flag() {
        // The Kitty spec: unless "report all keys as escape codes" (0b1000) is
        // pushed, key events that produce text are delivered AS text. Helix
        // pushes flags 5 (disambiguate|alternates); kitty itself sends a plain
        // "I" for Shift+I at those flags. Regression: we used to CSI-u every
        // Char (and lowercased the codepoint) so hx received `\e[105u` (a
        // bare `i`) for Shift+I and entered plain insert instead of
        // insert-at-line-start.
        // The hx bug exactly: capital from a legacy/text outer, flags 5.
        let cap = KeyEvent::plain(Key::Char('I'));
        assert_eq!(encode(&cap, KeyboardTarget::Kitty(5), false), b"I");
        // Shift+i from a rich outer (shifted alternate populated).
        let mut shifted = KeyEvent::new(Key::Char('i'), Modifiers::SHIFT);
        shifted.shifted = Some('I');
        assert_eq!(encode(&shifted, KeyboardTarget::Kitty(5), false), b"I");
        // Plain lowercase at disambiguate-only.
        let a = KeyEvent::plain(Key::Char('a'));
        assert_eq!(encode(&a, KeyboardTarget::Kitty(1), false), b"a");
        // Ctrl combos produce no text: still CSI-u.
        let ctrl = KeyEvent::new(Key::Char('i'), Modifiers::CTRL);
        assert_eq!(encode(&ctrl, KeyboardTarget::Kitty(5), false), b"\x1b[105;5u");
        // With report-all-keys pushed, text keys DO become escape codes.
        let mut all = KeyEvent::new(Key::Char('i'), Modifiers::SHIFT);
        all.shifted = Some('I');
        assert_eq!(
            encode(&all, KeyboardTarget::Kitty(0b1101), false),
            b"\x1b[105:73;2u"
        );
    }

    #[test]
    fn kitty_text_key_release_is_silent_without_all_keys() {
        // Text-producing keys are not escape-coded at flags without 0b1000, so
        // they have no release events either, even when the pane asked for
        // event types (0b10). Only escape-coded keys report releases.
        let mut e = KeyEvent::plain(Key::Char('a'));
        e.kind = plexy_glass_mux::KeyEventKind::Release;
        assert!(encode(&e, KeyboardTarget::Kitty(1 | 2), false).is_empty());
    }

    #[test]
    fn legacy_shifted_char_types_its_text() {
        // Down-converting a rich outer's Shift+i for a legacy pane must type
        // "I". This used to fall through to `Vec::new()` and EAT the key.
        let mut e = KeyEvent::new(Key::Char('i'), Modifiers::SHIFT);
        e.shifted = Some('I');
        assert_eq!(encode(&e, KeyboardTarget::Legacy, false), b"I");
        // No shifted alternate supplied: best-effort uppercase.
        let bare = KeyEvent::new(Key::Char('i'), Modifiers::SHIFT);
        assert_eq!(encode(&bare, KeyboardTarget::Legacy, false), b"I");
        // The event's own text wins when the outer reported it.
        let mut texty = KeyEvent::new(Key::Char('7'), Modifiers::SHIFT);
        texty.text = Some("/".into());
        assert_eq!(encode(&texty, KeyboardTarget::Legacy, false), b"/");
    }

    #[test]
    fn modified_enter_tab_backspace_are_not_eaten() {
        // The Claude Code Shift+Enter bug: Shift+Enter has NO legacy byte
        // form, so a disambiguate-only outer (ours) receives `\e[13;2u` from
        // the terminal, and our re-encode used to route Enter/Tab/Backspace
        // through legacy unconditionally, where the modified forms encoded to
        // EMPTY: the key was eaten. Kitty sends the CSI-u form to panes that
        // negotiated any kitty flags; xterm's modifyOtherKeys sends the
        // 27-form; pure-legacy panes degrade to the base key (tmux behavior).
        let shift_enter = KeyEvent::new(Key::Enter, Modifiers::SHIFT);
        // Kitty pane (any flags): the CSI-u form.
        assert_eq!(encode(&shift_enter, KeyboardTarget::Kitty(1), false), b"\x1b[13;2u");
        assert_eq!(encode(&shift_enter, KeyboardTarget::Kitty(5), false), b"\x1b[13;2u");
        // Plain Enter at the same flags stays legacy (exact form exists).
        let enter = KeyEvent::plain(Key::Enter);
        assert_eq!(encode(&enter, KeyboardTarget::Kitty(1), false), vec![0x0d]);
        // Shift+Tab: kitty CSI-u's it under disambiguate (the legacy \e[Z is
        // used only in full legacy mode; verified against kitty's encoder).
        let shift_tab = KeyEvent::new(Key::Tab, Modifiers::SHIFT);
        assert_eq!(encode(&shift_tab, KeyboardTarget::Kitty(1), false), b"\x1b[9;2u");
        // At a LEGACY pane the same event degrades to the faithful \e[Z.
        assert_eq!(encode(&shift_tab, KeyboardTarget::Legacy, false), b"\x1b[Z");
        // Ctrl+Tab has no legacy form: CSI-u.
        let ctrl_tab = KeyEvent::new(Key::Tab, Modifiers::CTRL);
        assert_eq!(encode(&ctrl_tab, KeyboardTarget::Kitty(1), false), b"\x1b[9;5u");
        // modifyOtherKeys level 1: modified Enter gets the 27-form (it is NOT
        // a "well-known legacy" key once modified); plain Enter stays \r.
        assert_eq!(
            encode(&shift_enter, KeyboardTarget::ModifyOtherKeys(1), false),
            b"\x1b[27;2;13~"
        );
        assert_eq!(encode(&enter, KeyboardTarget::ModifyOtherKeys(1), false), vec![0x0d]);
        // Pure legacy pane: degrade to the base key (tmux strips the
        // modifier) rather than eating the keystroke.
        assert_eq!(encode(&shift_enter, KeyboardTarget::Legacy, false), vec![0x0d]);
        let ctrl_backspace = KeyEvent::new(Key::Backspace, Modifiers::CTRL);
        assert_eq!(encode(&ctrl_backspace, KeyboardTarget::Legacy, false), vec![0x7f]);
    }

    #[test]
    fn kitty_target_csi_u_basic() {
        // disambiguate(1): Ctrl+Shift+i -> \e[105;6u
        let e = KeyEvent::new(Key::Char('i'), Modifiers::CTRL | Modifiers::SHIFT);
        assert_eq!(encode(&e, KeyboardTarget::Kitty(1), false), b"\x1b[105;6u");
    }

    #[test]
    fn kitty_release_dropped_unless_event_types_flag() {
        let mut e = KeyEvent::new(Key::Char('a'), Modifiers::CTRL);
        e.kind = plexy_glass_mux::KeyEventKind::Release;
        // flags=1: no report_event_types -> the release is DROPPED entirely. A
        // pane that didn't request event types must never see a release, else the
        // re-encode would emit a second keystroke and double the input.
        assert!(encode(&e, KeyboardTarget::Kitty(1), false).is_empty());
        // flags 1|2: report_event_types -> the release is forwarded with `:3`.
        assert_eq!(encode(&e, KeyboardTarget::Kitty(1 | 2), false), b"\x1b[97;5:3u");
    }

    #[test]
    fn release_dropped_for_legacy_and_modkeys_targets() {
        let mut e = KeyEvent::new(Key::Char('a'), Modifiers::empty());
        e.kind = plexy_glass_mux::KeyEventKind::Release;
        assert!(encode(&e, KeyboardTarget::Legacy, false).is_empty());
        assert!(encode(&e, KeyboardTarget::ModifyOtherKeys(2), false).is_empty());
        // A Press of the same key is unaffected.
        let p = KeyEvent::new(Key::Char('a'), Modifiers::empty());
        assert_eq!(encode(&p, KeyboardTarget::Legacy, false), b"a");
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
            assert_eq!(decode_xterm_mods(mods_param(m)), m);
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

    #[test]
    fn ctrl_space_emits_nul() {
        let e = KeyEvent::new(Key::Char(' '), Modifiers::CTRL);
        // Legacy and modifyOtherKeys-level-1 panes both get NUL (was eaten).
        assert_eq!(encode(&e, KeyboardTarget::Legacy, false), vec![0x00]);
        assert_eq!(encode(&e, KeyboardTarget::ModifyOtherKeys(1), false), vec![0x00]);
        // Level 2 reports it in the canonical 27-form (codepoint 32, mods 5).
        assert_eq!(
            encode(&e, KeyboardTarget::ModifyOtherKeys(2), false),
            b"\x1b[27;5;32~"
        );
        // Round-trips: the parser decodes NUL back to Ctrl+Space.
        let mut p = KeyParser::new();
        let KeyParseOutput::Event { event: got, .. } = p.consume(0x00) else {
            panic!("decoded event")
        };
        assert_eq!(got.key, Key::Char(' '));
        assert_eq!(got.mods, Modifiers::CTRL);
    }

    #[test]
    fn alt_with_other_mods_degrades_via_esc_prefix() {
        // Alt+Shift+a degrades to ESC 'A' (was eaten by the catch-all).
        let alt_shift = KeyEvent::new(Key::Char('a'), Modifiers::ALT | Modifiers::SHIFT);
        assert_eq!(encode(&alt_shift, KeyboardTarget::Legacy, false), b"\x1bA");
        // Plain Alt+a is unchanged: ESC + lowercase.
        let plain = KeyEvent::new(Key::Char('a'), Modifiers::ALT);
        assert_eq!(encode(&plain, KeyboardTarget::Legacy, false), b"\x1ba");
        // Alt+Ctrl+a degrades to ESC + Ctrl-a (0x01) rather than being dropped.
        let ctrl_alt = KeyEvent::new(Key::Char('a'), Modifiers::ALT | Modifiers::CTRL);
        assert_eq!(
            encode(&ctrl_alt, KeyboardTarget::Legacy, false),
            vec![0x1b, 0x01]
        );
    }
}
