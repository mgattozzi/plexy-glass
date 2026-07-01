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

#[test]
fn conformance_erase_clears_whole_wide_char() {
    // Erasing part of a wide grapheme erases the WHOLE cell: a clear that splits a
    // wide grapheme from its spacer blanks the orphaned half too, keeping the row
    // well-formed (no dangling spacer, no half-wide grapheme).
    check(&[
        // ECH on the grapheme cell clears its now-orphaned spacer. "好x" (好@0-1,
        // x@2), cursor col 0, ECH 1 → both halves of 好 blank, x survives.
        Case { name: "ech_clears_orphan_spacer", rows: 1, cols: 4, input: "好x\x1b[1;1H\x1b[X".as_bytes(),
            cursor: Some((0, 0)),
            cells: &[(0, 0, Expect::Blank), (0, 1, Expect::Blank), (0, 2, Expect::Text("x"))], ..BASE },
        // ECH on the SPACER (cursor parked on the right half) clears the orphaned
        // grapheme too. "好x", cursor col 1, ECH 1.
        Case { name: "ech_clears_orphan_grapheme", rows: 1, cols: 4, input: "好x\x1b[1;2H\x1b[X".as_bytes(),
            cursor: Some((0, 1)),
            cells: &[(0, 0, Expect::Blank), (0, 1, Expect::Blank), (0, 2, Expect::Text("x"))], ..BASE },
        // EL 1 (start→cursor inclusive) ending ON a wide grapheme clears its spacer
        // too. "x好" (x@0, 好@1-2), cursor col 1, CSI 1K erases cols 0..=1 → the
        // spacer at col 2 is orphaned → blanked.
        Case { name: "el1_clears_orphan_spacer", rows: 1, cols: 4, input: "x好\x1b[1;2H\x1b[1K".as_bytes(),
            cursor: Some((0, 1)),
            cells: &[(0, 0, Expect::Blank), (0, 1, Expect::Blank), (0, 2, Expect::Blank)], ..BASE },
    ]);
}

#[test]
fn conformance_rep() {
    check(&[
        // REP (CSI Ps b): repeat the last printed graphic Ps times. "a" + CSI 4 b
        // → five 'a's (the original + 4 repeats), cursor after the last.
        Case { name: "rep_4", input: b"a\x1b[4b", cursor: Some((0, 5)),
            cells: &[(0, 0, Expect::Text("a")), (0, 4, Expect::Text("a")), (0, 5, Expect::Blank)], ..BASE },
        // REP default count is 1.
        Case { name: "rep_default_1", input: b"z\x1b[b", cursor: Some((0, 2)),
            cells: &[(0, 0, Expect::Text("z")), (0, 1, Expect::Text("z")), (0, 2, Expect::Blank)], ..BASE },
        // REP repeats a wide grapheme whole (spacer included): 好 + CSI 2 b → three 好.
        Case { name: "rep_wide", input: "好\x1b[2b".as_bytes(), cursor: Some((0, 6)),
            cells: &[(0, 0, Expect::Text("好")), (0, 1, Expect::Spacer), (0, 2, Expect::Text("好")),
                     (0, 4, Expect::Text("好")), (0, 5, Expect::Spacer)], ..BASE },
        // A control (CR/LF) between the char and REP clears the target → REP is a
        // no-op (ECMA-48: REP after a control is undefined). Row 1 stays blank.
        Case { name: "rep_cleared_by_newline", input: b"a\r\n\x1b[3b", cursor: Some((1, 0)),
            cells: &[(0, 0, Expect::Text("a")), (1, 0, Expect::Blank)], ..BASE },
    ]);
}

#[test]
fn conformance_cht_cbt() {
    check(&[
        // CHT (CSI Ps I): advance Ps tab stops. 40-wide stops at 0,8,16,24,32.
        Case { name: "cht_3", rows: 8, cols: 40, input: b"\x1b[3I", cursor: Some((0, 24)), ..BASE },
        Case { name: "cht_default_1", rows: 8, cols: 40, input: b"\x1b[I", cursor: Some((0, 8)), ..BASE },
        // Runs out of stops → clamps to the last column.
        Case { name: "cht_clamps_last", rows: 8, cols: 40, input: b"\x1b[99I", cursor: Some((0, 39)), ..BASE },
        // CBT (CSI Ps Z): retreat Ps tab stops. From col 20 → 16.
        Case { name: "cbt_1", rows: 8, cols: 40, input: b"\x1b[1;21H\x1b[Z", cursor: Some((0, 16)), ..BASE },
        Case { name: "cbt_3", rows: 8, cols: 40, input: b"\x1b[1;21H\x1b[3Z", cursor: Some((0, 0)), ..BASE },
        // CBT saturates at col 0.
        Case { name: "cbt_saturates", rows: 8, cols: 40, input: b"\x1b[1;3H\x1b[Z", cursor: Some((0, 0)), ..BASE },
        // CHT then CBT round-trips back to home (stop-aligned).
        Case { name: "cht_cbt_round_trip", rows: 8, cols: 40, input: b"\x1b[3I\x1b[3Z", cursor: Some((0, 0)), ..BASE },
    ]);
}

#[test]
fn conformance_cnl_cpl() {
    check(&[
        // CNL (CSI Ps E): move down Ps lines to column 0. "Hello" then CNL 2 puts
        // the cursor at row 2 col 0; 'x' lands there while "Hello" stays on row 0.
        Case { name: "cnl_2", input: b"Hello\x1b[2Ex", cursor: Some((2, 1)),
            cells: &[(0, 0, Expect::Text("H")), (0, 4, Expect::Text("o")),
                     (1, 0, Expect::Blank), (2, 0, Expect::Text("x"))], ..BASE },
        // CNL default 1.
        Case { name: "cnl_default_1", input: b"\x1b[Ex", cursor: Some((1, 1)),
            cells: &[(1, 0, Expect::Text("x"))], ..BASE },
        // CNL clamps at the bottom row.
        Case { name: "cnl_clamps_bottom", input: b"\x1b[99Ex", cursor: Some((7, 1)),
            cells: &[(7, 0, Expect::Text("x"))], ..BASE },
        // CPL (CSI Ps F): move up Ps lines to column 0. Start row 5 col 9, CPL 2 →
        // row 3 col 0.
        Case { name: "cpl_2", input: b"\x1b[6;10H\x1b[2Fx", cursor: Some((3, 1)),
            cells: &[(3, 0, Expect::Text("x"))], ..BASE },
    ]);
}

#[test]
fn conformance_decaln() {
    check(&[
        // DECALN (ESC # 8): fill the whole grid with 'E', cursor home.
        Case { name: "decaln_fills_e", input: b"\x1b#8", cursor: Some((0, 0)),
            cells: &[(0, 0, Expect::Text("E")), (0, 23, Expect::Text("E")),
                     (3, 12, Expect::Text("E")), (7, 0, Expect::Text("E")),
                     (7, 23, Expect::Text("E"))], ..BASE },
    ]);
}

#[test]
fn conformance_attach_zero_width() {
    check(&[
        // A combining mark after a WIDE grapheme must attach to the wide base, not
        // its spacer. CUP col 10, print 好 (cols 10-11), then U+0301 → "好\u{0301}"
        // at col 10, spacer at col 11 preserved. (SGR reset separates the two
        // print calls so the mark takes the zero-width attach path.)
        Case { name: "zwj_after_wide", input: b"\x1b[1;11H\xe5\xa5\xbd\x1b[m\xcc\x81\x1b[m",
            cells: &[(0, 10, Expect::Text("好\u{0301}")), (0, 11, Expect::Spacer)], ..BASE },
        // A combining mark after a width-1 char written at the LAST column (pending
        // wrap latched, cursor did not advance) attaches to that char, not the cell
        // to its left. 'a' at col 23 → "a\u{0301}".
        Case { name: "zwj_at_last_col_pending_wrap", input: b"\x1b[1;24Ha\x1b[m\xcc\x81\x1b[m",
            cursor: Some((0, 23)),
            cells: &[(0, 22, Expect::Blank), (0, 23, Expect::Text("a\u{0301}"))], ..BASE },
    ]);
}

#[test]
fn ed3_clears_scrollback() {
    // Feed more than `rows` lines so scrollback is non-empty, then CSI 3 J (ED 3,
    // erase saved lines) must clear the scrollback buffer.
    let mut s = Screen::new(4, 8);
    let mut p = Parser::new();
    for i in 0..20u16 {
        p.advance(&mut s, format!("line{i}\r\n").as_bytes());
    }
    p.flush(&mut s);
    assert!(!s.scrollback.is_empty(), "scrollback should be populated before ED 3");
    p.advance(&mut s, b"\x1b[3J");
    p.flush(&mut s);
    assert!(s.scrollback.is_empty(), "ED 3 must clear the scrollback buffer");
}

#[test]
fn decscusr_sets_cursor_shape() {
    use plexy_glass_emulator::CursorShape;
    // DECSCUSR (CSI Ps SP q): 0/1/2 block, 3/4 underline, 5/6 bar. Each case
    // starts from a different shape to prove the sequence actually changes it.
    let cases: &[(&[u8], CursorShape)] = &[
        (b"\x1b[6 q", CursorShape::Bar),          // from default Block
        (b"\x1b[4 q", CursorShape::Underline),
        (b"\x1b[6 q\x1b[2 q", CursorShape::Block), // Bar → Block
        (b"\x1b[6 q\x1b[0 q", CursorShape::Block),
        (b"\x1b[6 q\x1b[3 q", CursorShape::Underline),
    ];
    for (input, want) in cases {
        let s = run(4, 8, input);
        assert_eq!(s.cursor.shape, *want, "DECSCUSR {input:?}");
    }
}

#[test]
fn conformance_dec_special_graphics() {
    check(&[
        // ESC ( 0 designates G0 = DEC Special Graphics; l q q q k → box border.
        // ESC ( B restores ASCII. `run` flushes the trailing grapheme.
        Case {
            name: "esc_open_0_box_drawing",
            input: b"\x1b(0lqqqk\x1b(B",
            cells: &[
                (0, 0, Expect::Text("┌")),
                (0, 1, Expect::Text("─")),
                (0, 2, Expect::Text("─")),
                (0, 3, Expect::Text("─")),
                (0, 4, Expect::Text("┐")),
            ],
            ..BASE
        },
        // ESC ( B returns G0 to ASCII: the first `l` (special) is ┌, the second
        // (after ESC ( B) is a literal 'l'.
        Case {
            name: "esc_open_B_restores_ascii",
            input: b"\x1b(0l\x1b(Bl",
            cells: &[(0, 0, Expect::Text("┌")), (0, 1, Expect::Text("l"))],
            ..BASE
        },
        // SI/SO round-trip via G1: ESC ) 0 designates G1 = special graphics; SO
        // (0x0E) shifts GL→G1 so `lqk` draws box glyphs; SI (0x0F) shifts GL→G0
        // (ASCII) so the following `lqk` prints literally.
        Case {
            name: "si_so_round_trip_via_g1",
            input: b"\x1b)0\x0elqk\x0flqk",
            cells: &[
                (0, 0, Expect::Text("┌")),
                (0, 1, Expect::Text("─")),
                (0, 2, Expect::Text("┐")),
                (0, 3, Expect::Text("l")),
                (0, 4, Expect::Text("q")),
                (0, 5, Expect::Text("k")),
            ],
            ..BASE
        },
        // ASCII outside 0x60..=0x7e passes through unchanged under special
        // graphics (digits/uppercase are not in the line-drawing range).
        Case {
            name: "ascii_below_range_passes_through",
            input: b"\x1b(0A1 \x1b(B",
            cells: &[
                (0, 0, Expect::Text("A")),
                (0, 1, Expect::Text("1")),
                (0, 2, Expect::Text(" ")),
            ],
            ..BASE
        },
        // RIS (ESC c) resets the charset to ASCII: after RIS, `l` prints literally.
        Case {
            name: "ris_resets_charset",
            input: b"\x1b(0\x1bcl",
            cells: &[(0, 0, Expect::Text("l"))],
            ..BASE
        },
    ]);
}
