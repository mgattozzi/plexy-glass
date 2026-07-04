//! Property test: decode∘encode == id on the round-trippable subset.
//! Under ModifyOtherKeys(2), codepoint keys (non-empty mods) take the symmetric
//! 27-form and functional keys round-trip via legacy CSI/SS3/tilde.

use hegel::{TestCase, generators as gs};
use plexy_glass_keys::{KeyParseOutput, KeyParser, KeyboardProtocol, KeyboardTarget, encode};
use plexy_glass_mux::{Direction, Key, KeyEvent, Modifiers};

/// Drive bytes through the parser and return the last decoded event.
fn decode(bytes: &[u8], proto: KeyboardProtocol) -> Option<KeyEvent> {
    let mut p = KeyParser::new().with_protocol(proto);
    let mut last = None;
    for &b in bytes {
        if let KeyParseOutput::Event { event, .. } = p.consume(b) {
            last = Some(event);
        }
    }
    last
}

fn draw_dir(tc: &TestCase) -> Direction {
    match tc.draw(gs::integers::<u8>().min_value(0).max_value(3)) {
        0 => Direction::Up,
        1 => Direction::Down,
        2 => Direction::Left,
        _ => Direction::Right,
    }
}

/// A non-empty modifier bitset (full u8: SUPER/HYPER/META/locks all round-trip,
/// since `mods_param=1+bits` and `decode_xterm_mods` are exact u8 inverses).
fn draw_mods_nonempty(tc: &TestCase) -> Modifiers {
    let bits = tc.draw(gs::integers::<u8>().min_value(1).max_value(255));
    Modifiers::from_bits_truncate(bits)
}

/// Draw a `KeyEvent` from the round-trippable subset: codepoint keys (Char/Enter/
/// Tab/Backspace/Escape, which need non-empty mods to take the 27-form) and
/// functional keys (Arrow/Function/Home/End/Insert/Delete/PageUp/PageDown, any
/// mods, legacy form).
fn draw_event(tc: &TestCase) -> KeyEvent {
    let mods = draw_mods_nonempty(tc);
    let key = match tc.draw(gs::integers::<u8>().min_value(0).max_value(13)) {
        // codepoint keys (lowercase ASCII letter avoids uppercase + control codepoints)
        0 => Key::Char(tc.draw(gs::integers::<u8>().min_value(b'a').max_value(b'z')) as char),
        1 => Key::Char(tc.draw(gs::integers::<u8>().min_value(b'0').max_value(b'9')) as char),
        2 => Key::Enter,
        3 => Key::Tab,
        4 => Key::Backspace,
        5 => Key::Escape,
        // functional keys
        6 => Key::Arrow(draw_dir(tc)),
        7 => Key::Function(tc.draw(gs::integers::<u8>().min_value(1).max_value(12))),
        8 => Key::Home,
        9 => Key::End,
        10 => Key::Insert,
        11 => Key::Delete,
        12 => Key::PageUp,
        _ => Key::PageDown,
    };
    KeyEvent::new(key, mods) // kind=Press, text/shifted/base_layout=None
}

#[hegel::test(test_cases = 1000)]
fn mok2_decode_encode_round_trips(tc: TestCase) {
    let ev = draw_event(&tc);
    tc.note(&format!("event = {ev:?}"));
    let bytes = encode(
        &ev,
        KeyboardTarget::ModifyOtherKeys(2),
        /*app_cursor=*/ false,
    );
    assert!(
        !bytes.is_empty(),
        "round-trippable event must encode to non-empty bytes"
    );
    let got = decode(&bytes, KeyboardProtocol::ModifyOtherKeys)
        .unwrap_or_else(|| panic!("{ev:?} encoded to {bytes:?} but did not re-decode"));
    assert_eq!(
        got, ev,
        "decode∘encode must be the identity on the round-trippable subset"
    );
}
