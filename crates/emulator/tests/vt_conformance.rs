//! Table-driven VT conformance corpus.
//!
//! Each case feeds escape-sequence bytes to a fresh `Screen` (driven via the
//! public `Parser`, byte-exact-flushed) and asserts grid/cursor/mode state
//! against spec-correct expected values (DEC VT510 manual, xterm ctlseqs,
//! esctest). A failing case is a real conformance bug.

use plexy_glass_emulator::parser::Parser;
use plexy_glass_emulator::{Modes, Screen};

/// Drive a fresh rows×cols screen with `input`, flushing the trailing grapheme.
fn run(rows: u16, cols: u16, input: &[u8]) -> Screen {
    let mut s = Screen::new(rows, cols);
    let mut p = Parser::new();
    p.advance(&mut s, input);
    p.flush(&mut s);
    s
}

/// Visible text of a row: cell graphemes with wide-spacers omitted (a wide char
/// renders once), blanks rendered as their space grapheme. NOT trimmed.
fn row(s: &Screen, r: u16) -> String {
    s.active.rows[r as usize]
        .cells
        .iter()
        .filter(|c| !c.is_wide_spacer())
        .map(|c| c.grapheme.as_str())
        .collect()
}

/// Per-cell expectation.
// The variants below aren't constructed in this first batch of cases, but
// the later tasks (tab-stops, wide-char, erase/insert ops) will use them.
#[allow(dead_code)]
#[derive(Clone, Copy)]
enum Expect {
    Text(&'static str),
    Blank,
    Spacer,
}

#[derive(Clone, Copy)]
struct Case {
    name: &'static str,
    rows: u16,
    cols: u16,
    input: &'static [u8],
    /// (row, col) 0-based, if asserted.
    cursor: Option<(u16, u16)>,
    /// (top, bottom) 0-based inclusive, if asserted.
    scroll_region: Option<(u16, u16)>,
    /// DECOM origin mode, if asserted.
    origin: Option<bool>,
    /// (row, exact visible text) pairs.
    rows_text: &'static [(u16, &'static str)],
    /// (row, col, expectation) cells.
    cells: &'static [(u16, u16, Expect)],
}

const BASE: Case = Case {
    name: "",
    rows: 8,
    cols: 24,
    input: b"",
    cursor: None,
    scroll_region: None,
    origin: None,
    rows_text: &[],
    cells: &[],
};

fn check(cases: &[Case]) {
    for c in cases {
        let s = run(c.rows, c.cols, c.input);
        if let Some((r, col)) = c.cursor {
            assert_eq!((s.cursor.row, s.cursor.col), (r, col), "{}: cursor", c.name);
        }
        if let Some(sr) = c.scroll_region {
            assert_eq!(s.scroll_region, sr, "{}: scroll_region", c.name);
        }
        if let Some(o) = c.origin {
            assert_eq!(s.modes.contains(Modes::ORIGIN), o, "{}: origin mode", c.name);
        }
        for (r, txt) in c.rows_text {
            assert_eq!(&row(&s, *r), txt, "{}: row {}", c.name, r);
        }
        for (r, col, e) in c.cells {
            let cell = s
                .active
                .get_cell(*r, *col)
                .unwrap_or_else(|| panic!("{}: cell ({},{}) out of bounds", c.name, r, col));
            match e {
                Expect::Text(t) => {
                    assert_eq!(cell.grapheme.as_str(), *t, "{}: cell ({},{}) text", c.name, r, col)
                }
                Expect::Blank => assert!(cell.is_blank(), "{}: cell ({},{}) not blank", c.name, r, col),
                Expect::Spacer => {
                    assert!(cell.is_wide_spacer(), "{}: cell ({},{}) not spacer", c.name, r, col)
                }
            }
        }
    }
}

#[test]
fn conformance_cursor_basic() {
    check(&[
        // CUP: 1-based → 0-based; defaults to home.
        Case { name: "cup_home", input: b"abc\x1b[H", cursor: Some((0, 0)), ..BASE },
        Case { name: "cup_params", input: b"\x1b[3;5H", cursor: Some((2, 4)), ..BASE },
        Case { name: "cup_zero_is_one", input: b"\x1b[0;0H", cursor: Some((0, 0)), ..BASE },
        // CUF/CUB column moves; CUB saturates at col 0.
        Case { name: "cuf", input: b"\x1b[5C", cursor: Some((0, 5)), ..BASE },
        Case { name: "cub", input: b"\x1b[1;6H\x1b[3D", cursor: Some((0, 2)), ..BASE },
        Case { name: "cub_saturates", input: b"\x1b[1;2H\x1b[9D", cursor: Some((0, 0)), ..BASE },
        // Grid-edge clamps (8×24).
        Case { name: "cuu_clamps_top", input: b"\x1b[3;1H\x1b[10A", cursor: Some((0, 0)), ..BASE },
        Case { name: "cup_clamps_outside", input: b"\x1b[100;100H", cursor: Some((7, 23)), ..BASE },
        Case { name: "cuf_clamps_right", input: b"\x1b[99C", cursor: Some((0, 23)), ..BASE },
    ]);
}

#[test]
fn conformance_cursor_margins() {
    check(&[
        // CUU stops at the TOP margin when starting inside the region.
        Case { name: "cuu_stops_at_top_margin", input: b"\x1b[3;7r\x1b[6;1H\x1b[10A", cursor: Some((2, 0)), ..BASE },
        // CUD stops at the BOTTOM margin when starting inside the region.
        Case { name: "cud_stops_at_bottom_margin", input: b"\x1b[3;7r\x1b[4;1H\x1b[10B", cursor: Some((6, 0)), ..BASE },
        // CUU started ABOVE the region clamps to the screen top, not the margin.
        Case { name: "cuu_above_region_clamps_screen_top", input: b"\x1b[4;5r\x1b[3;1H\x1b[10A", cursor: Some((0, 0)), ..BASE },
        // CUD started BELOW the region clamps to the screen bottom.
        Case { name: "cud_below_region_clamps_screen_bottom", input: b"\x1b[4;5r\x1b[7;1H\x1b[10B", cursor: Some((7, 0)), ..BASE },
    ]);
}

#[test]
fn conformance_decstbm() {
    check(&[
        // Set region (1-based inclusive) + home the cursor.
        Case { name: "decstbm_sets_and_homes", input: b"\x1b[2;6r", scroll_region: Some((1, 5)), cursor: Some((0, 0)), ..BASE },
        // Bare CSI r resets to full screen (8 rows → (0,7)).
        Case { name: "decstbm_reset_full", input: b"\x1b[2;6r\x1b[r", scroll_region: Some((0, 7)), ..BASE },
        // Inverted / equal margins (Pt >= Pb) are IGNORED → full screen.
        Case { name: "decstbm_equal_ignored", input: b"\x1b[3;3r", scroll_region: Some((0, 7)), ..BASE },
        Case { name: "decstbm_inverted_ignored", input: b"\x1b[6;2r", scroll_region: Some((0, 7)), ..BASE },
        // Over-large bottom clamps to the page height.
        Case { name: "decstbm_overlarge_clamps", input: b"\x1b[2;99r", scroll_region: Some((1, 7)), cursor: Some((0, 0)), ..BASE },
    ]);
}

#[test]
fn conformance_decom() {
    check(&[
        // ?6h homes the cursor to the region top.
        Case { name: "decom_homes_to_region_top", input: b"\x1b[5;8r\x1b[?6h", origin: Some(true), cursor: Some((4, 0)), ..BASE },
        // CUP rows become region-relative: row 3 in origin mode = top(4)+2.
        Case { name: "decom_cup_region_relative", input: b"\x1b[5;8r\x1b[?6h\x1b[3;1H", cursor: Some((6, 0)), ..BASE },
        // Reset (`?6l`) returns to absolute addressing.
        Case { name: "decom_reset_absolute", input: b"\x1b[5;8r\x1b[?6h\x1b[?6l\x1b[3;1H", origin: Some(false), cursor: Some((2, 0)), ..BASE },
        // A row past the region bottom clamps to the region bottom (NOT the grid
        // bottom) under origin mode. Region rows 5..8 (top=4,bottom=7); CUP 99 →
        // region bottom = grid row 7. Use a region whose bottom is < grid bottom
        // to make the clamp observable:
        Case { name: "decom_clamps_to_region_bottom", input: b"\x1b[2;4r\x1b[?6h\x1b[99;1H", cursor: Some((3, 0)), ..BASE },
    ]);
}

#[test]
fn conformance_tabs() {
    check(&[
        // Default stops every 8 cols (1-based 9,17,…). On 24-wide: col0 →HT→ 8 →HT→ 16.
        Case { name: "ht_default_stops", input: b"\t\t", cursor: Some((0, 16)), ..BASE },
        // HT at/after the last stop stops at the LAST column (24-wide → col 23).
        Case { name: "ht_stops_at_last_col", input: b"\t\t\t\t", cursor: Some((0, 23)), ..BASE },
        // HTS sets a stop at the cursor column; a later HT from home lands on it.
        Case { name: "hts_sets_stop", input: b"\x1b[1;4H\x1bH\x1b[1;1H\t", cursor: Some((0, 3)), ..BASE },
        // TBC 3 clears ALL stops → HT runs to the last column.
        Case { name: "tbc_clear_all", input: b"\x1b[3g\t", cursor: Some((0, 23)), ..BASE },
        // TBC 0 clears the stop at the cursor col → next HT skips it.
        // Stand on col 8 (the first default stop), clear it, home, HT → col 16.
        Case { name: "tbc_clear_at_cursor", input: b"\t\x1b[0g\x1b[1;1H\t", cursor: Some((0, 16)), ..BASE },
    ]);
}

#[test]
fn conformance_wide_char_wrap() {
    // 4-col line, autowrap on (default). Fill cols 0..2 with "abc" (cursor at col
    // 3), then a wide char "好" cannot fit at col 3 → pad col 3 blank, wrap, place
    // 好 at row1 col0 (+ spacer col1). cursor ends row1 col2.
    check(&[Case {
        name: "wide_char_wraps_whole_not_split",
        rows: 2,
        cols: 4,
        input: "abc好".as_bytes(),
        cursor: Some((1, 2)),
        cells: &[
            (0, 0, Expect::Text("a")),
            (0, 3, Expect::Blank),       // pad cell, 好 did not split here
            (1, 0, Expect::Text("好")),
            (1, 1, Expect::Spacer),
        ],
        ..BASE
    }]);
    // A wide char that fits exactly at the last two columns does NOT wrap.
    check(&[Case {
        name: "wide_char_exact_fit_no_wrap",
        rows: 2,
        cols: 4,
        input: "ab好".as_bytes(),
        cursor: Some((0, 3)),  // pending-wrap latched at the right edge; row unchanged
        cells: &[(0, 2, Expect::Text("好")), (0, 3, Expect::Spacer)],
        ..BASE
    }]);
}

#[test]
fn conformance_ed_el() {
    check(&[
        // EL 0 (cursor→eol): "0123456789" on a 10-wide line, cursor col4 → "0123" + blanks.
        Case { name: "el_0_to_eol", rows: 2, cols: 10, input: b"0123456789\x1b[1;5H\x1b[0K",
            cursor: Some((0, 4)), rows_text: &[(0, "0123      ")], ..BASE },
        // EL 1 (sol→cursor inclusive): cursor col4 → cols 0..4 blank, "56789" kept.
        Case { name: "el_1_to_cursor", rows: 2, cols: 10, input: b"0123456789\x1b[1;5H\x1b[1K",
            cursor: Some((0, 4)), rows_text: &[(0, "     56789")], ..BASE },
        // EL 2 (whole line).
        Case { name: "el_2_whole_line", rows: 2, cols: 10, input: b"0123456789\x1b[1;5H\x1b[2K",
            cursor: Some((0, 4)), rows_text: &[(0, "          ")], ..BASE },
        // ED 0 (cursor→end of screen): clears rest of line + lines below.
        Case { name: "ed_0_to_end", rows: 2, cols: 4, input: b"AAAA\r\nBBBB\x1b[1;3H\x1b[0J",
            cursor: Some((0, 2)), rows_text: &[(0, "AA  "), (1, "    ")], ..BASE },
        // ED 1 (start→cursor inclusive): clears lines above + line start..cursor.
        Case { name: "ed_1_to_cursor", rows: 2, cols: 4, input: b"AAAA\r\nBBBB\x1b[2;3H\x1b[1J",
            cursor: Some((1, 2)), rows_text: &[(0, "    "), (1, "   B")], ..BASE },
        // ED 2 (whole screen); cursor unchanged.
        Case { name: "ed_2_whole_screen", rows: 2, cols: 4, input: b"AAAA\r\nBBBB\x1b[2;2H\x1b[2J",
            cursor: Some((1, 1)), rows_text: &[(0, "    "), (1, "    ")], ..BASE },
    ]);
}

#[test]
fn conformance_insert_delete_erase() {
    check(&[
        // ICH default (1): "abcdefg" 8-wide, cursor col1 → "a bcdefg".
        Case { name: "ich_default", rows: 1, cols: 8, input: b"abcdefg\x1b[1;2H\x1b[@",
            cursor: Some((0, 1)), rows_text: &[(0, "a bcdefg")], ..BASE },
        // ICH explicit (2) with overflow lost: "ABCDEFGH" cursor col2 → "AB  CDEF".
        Case { name: "ich_explicit_overflow_lost", rows: 1, cols: 8, input: b"ABCDEFGH\x1b[1;3H\x1b[2@",
            cursor: Some((0, 2)), rows_text: &[(0, "AB  CDEF")], ..BASE },
        // DCH default (1): "abcd" cursor col1 → "acd " (+ blank).
        Case { name: "dch_default", rows: 1, cols: 4, input: b"abcd\x1b[1;2H\x1b[P",
            cursor: Some((0, 1)), rows_text: &[(0, "acd ")], ..BASE },
        // DCH explicit (2): "ABCDEFGH" cursor col2 → "ABEFGH  ".
        Case { name: "dch_explicit", rows: 1, cols: 8, input: b"ABCDEFGH\x1b[1;3H\x1b[2P",
            cursor: Some((0, 2)), rows_text: &[(0, "ABEFGH  ")], ..BASE },
        // ECH (2): erase 2 at cursor, no shift: "ABCDEFGH" cursor col2 → "AB  EFGH".
        Case { name: "ech_2", rows: 1, cols: 8, input: b"ABCDEFGH\x1b[1;3H\x1b[2X",
            cursor: Some((0, 2)), rows_text: &[(0, "AB  EFGH")], ..BASE },
        // ICH inserted BETWEEN a wide char and its spacer would SPLIT 好. The
        // orphaned grapheme (col 0) and orphaned spacer are both blanked so the
        // row stays well-formed (no half-wide cell), while 'x' survives at col 3.
        // (Cursor at col 1 = the spacer; ICH does not move it.)
        Case { name: "ich_does_not_split_wide", rows: 1, cols: 4, input: "好x\x1b[1;2H\x1b[@".as_bytes(),
            cursor: Some((0, 1)), cells: &[(0, 0, Expect::Blank), (0, 3, Expect::Text("x"))], ..BASE },
    ]);
}

#[test]
fn conformance_insert_delete_line() {
    check(&[
        // IL: 4-row screen "AAAA/BBBB/CCCC/DDDD", cursor row1 col2, IL 1 → blank row1,
        // B/C shift down, D lost. Cursor homes to col 0.
        Case { name: "il_1", rows: 4, cols: 4, input: b"AAAA\r\nBBBB\r\nCCCC\r\nDDDD\x1b[2;3H\x1b[L",
            cursor: Some((1, 0)), rows_text: &[(0, "AAAA"), (1, "    "), (2, "BBBB"), (3, "CCCC")], ..BASE },
        // DL: same fill, cursor row1 col2, DL 1 → row1 deleted, C/D shift up, blank bottom.
        Case { name: "dl_1", rows: 4, cols: 4, input: b"AAAA\r\nBBBB\r\nCCCC\r\nDDDD\x1b[2;3H\x1b[M",
            cursor: Some((1, 0)), rows_text: &[(0, "AAAA"), (1, "CCCC"), (2, "DDDD"), (3, "    ")], ..BASE },
        // IL is a no-op when the cursor is OUTSIDE the scroll region.
        Case { name: "il_noop_outside_region", rows: 4, cols: 4, input: b"AAAA\r\nBBBB\r\nCCCC\r\nDDDD\x1b[1;3r\x1b[4;1H\x1b[L",
            rows_text: &[(0, "AAAA"), (1, "BBBB"), (2, "CCCC"), (3, "DDDD")], ..BASE },
    ]);
}
