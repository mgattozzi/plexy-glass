//! Property tests for the DEC Special Graphics line-drawing charset
//! (`ESC ( 0` / `ESC ) 0` + SI/SO). Two invariants:
//!   1. Under the DEFAULT (ASCII) charset, printing any printable ASCII byte is
//!      identity, the cell grapheme is the byte itself (no translation).
//!   2. Under DEC Special Graphics, ASCII bytes OUTSIDE 0x60..=0x7e pass through
//!      unchanged; bytes INSIDE the range are translated to a single non-ASCII
//!      glyph (never equal to the source byte).

use hegel::TestCase;
use hegel::generators as gs;
use plexy_glass_emulator::parser::Parser;
use plexy_glass_emulator::Screen;

/// Print one raw byte after an optional charset-setup prefix; return the (0,0) cell text.
fn cell0_after(prefix: &[u8], byte: u8) -> String {
    let mut p = Parser::new();
    let mut s = Screen::new(4, 8);
    p.advance(&mut s, prefix);
    p.advance(&mut s, &[byte]);
    p.flush(&mut s); // commit the trailing grapheme
    s.active
        .get_cell(0, 0)
        .map(|c| c.grapheme.to_string())
        .unwrap_or_default()
}

/// P1: default charset is ASCII → printing a printable ASCII byte is identity.
#[hegel::test(test_cases = 300)]
fn default_charset_is_identity(tc: TestCase) {
    let byte = tc.draw(gs::integers::<u8>().min_value(0x20).max_value(0x7e)) as u8;
    let got = cell0_after(b"", byte);
    tc.note(&format!("byte=0x{byte:02x} got={got:?}"));
    assert_eq!(got, (byte as char).to_string(), "ASCII charset must not translate");
}

/// P2: under DEC Special Graphics, only 0x60..=0x7e is translated; every other
/// printable ASCII byte passes through identically, and every in-range byte
/// maps to a single non-ASCII glyph.
#[hegel::test(test_cases = 300)]
fn special_graphics_translates_only_the_line_drawing_range(tc: TestCase) {
    let byte = tc.draw(gs::integers::<u8>().min_value(0x20).max_value(0x7e)) as u8;
    let got = cell0_after(b"\x1b(0", byte); // ESC ( 0 → G0 = DEC special graphics
    tc.note(&format!("byte=0x{byte:02x} got={got:?}"));

    if (0x60..=0x7e).contains(&byte) {
        assert_ne!(got, (byte as char).to_string(), "in-range byte must be translated");
        assert!(!got.is_ascii(), "translated glyph must be non-ASCII, got {got:?}");
        assert_eq!(got.chars().count(), 1, "translated glyph is a single char");
    } else {
        assert_eq!(got, (byte as char).to_string(), "out-of-range byte passes through");
    }
}
