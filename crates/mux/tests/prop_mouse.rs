//! Property tests for the mouse wire layer: an event encoded for a child and
//! parsed back must reproduce itself. Catches sign/axis/modifier/coordinate
//! drift between `encode_for_child` and `MouseParser` (the class of the
//! wheel-modifier and horizontal-wheel bugs).

use hegel::{TestCase, generators as gs};
use plexy_glass_mux::{
    MouseButton, MouseEncoding, MouseEvent, MouseKind, MouseModifiers, MouseParseAction,
    MouseParser, encode_for_child,
};

fn parse_one(bytes: &[u8]) -> Option<MouseEvent> {
    let mut p = MouseParser::new();
    let mut last = None;
    for &b in bytes {
        if let MouseParseAction::Event(e) = p.consume(b) {
            last = Some(e);
        }
    }
    last
}

/// A button that survives the wire round-trip on the SGR path. `None` is
/// excluded for press/release/move because it shares button code 0 with `Left`.
fn draw_button(tc: &TestCase) -> MouseButton {
    match tc.draw(gs::integers::<u8>().min_value(0).max_value(2)) {
        0 => MouseButton::Left,
        1 => MouseButton::Middle,
        _ => MouseButton::Right,
    }
}

fn draw_event(tc: &TestCase) -> MouseEvent {
    let modifiers = MouseModifiers {
        shift: tc.draw(gs::booleans()),
        alt: tc.draw(gs::booleans()),
        ctrl: tc.draw(gs::booleans()),
    };
    // Below the saturating boundary so the +1 (encode) / -1 (parse) round-trips.
    let row = tc.draw(gs::integers::<u16>().min_value(0).max_value(9999));
    let col = tc.draw(gs::integers::<u16>().min_value(0).max_value(9999));
    // The wheel delta magnitude the model ever produces is ±3 (build_event).
    let (kind, button) = match tc.draw(gs::integers::<u8>().min_value(0).max_value(4)) {
        0 => (MouseKind::Press, draw_button(tc)),
        1 => (MouseKind::Release, draw_button(tc)),
        2 => (MouseKind::Move, draw_button(tc)),
        3 => (
            MouseKind::Wheel {
                delta: 3,
                horizontal: tc.draw(gs::booleans()),
            },
            MouseButton::None,
        ),
        _ => (
            MouseKind::Wheel {
                delta: -3,
                horizontal: tc.draw(gs::booleans()),
            },
            MouseButton::None,
        ),
    };
    MouseEvent {
        kind,
        button,
        modifiers,
        row,
        col,
    }
}

/// `encode_for_child(_, Sgr)` followed by `MouseParser` must reproduce the event
/// exactly: button, modifiers, wheel axis/direction, and coordinates. (The
/// parser only decodes the SGR `\e[<` form, which is what plexy negotiates; the
/// legacy `\e[M` form is encode-only and has no decoder to round-trip against.)
#[hegel::test(test_cases = 500)]
fn sgr_round_trip(tc: TestCase) {
    let ev = draw_event(&tc);
    tc.note(&format!("event = {ev:?}"));
    let bytes = encode_for_child(ev, MouseEncoding::Sgr);
    let parsed = parse_one(&bytes).expect("SGR bytes must re-parse to an event");
    assert_eq!(parsed, ev, "SGR encode→parse must round-trip exactly");
}
