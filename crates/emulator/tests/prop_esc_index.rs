//! Property tests for IND (`ESC D`) and NEL (`ESC E`), the C1 index controls
//! that must mirror LF's downward motion exactly (IND = LF's index semantics,
//! NEL = CR then IND). Regression coverage for the "IND/NEL silently dropped"
//! defect lives in `crates/emulator/src/emulator.rs`; these assert the general
//! invariants across arbitrary cursor positions and grid sizes.

use hegel::{TestCase, generators as gs};
use plexy_glass_emulator::parser::Parser;
use plexy_glass_emulator::{Emulator, Screen};

/// Build a screen positioned via CUP at (row, col) (0-based), then feed `rest`.
fn screen_after(rows: u16, cols: u16, row: u16, col: u16, rest: &[u8]) -> Screen {
    let mut p = Parser::new();
    let mut s = Screen::new(rows, cols);
    let cup = format!("\x1b[{};{}H", row + 1, col + 1);
    p.advance(&mut s, cup.as_bytes());
    p.advance(&mut s, rest);
    p.flush(&mut s);
    s
}

/// P1: IND is positionally equivalent to LF. From an arbitrary in-bounds cursor
/// NOT at the bottom scroll margin, IND must land the cursor at the same
/// (row, col) as a single LF: row+1, column unchanged (no carriage return).
#[hegel::test(test_cases = 500)]
fn ind_matches_lf_position_away_from_bottom_margin(tc: TestCase) {
    let rows = tc.draw(gs::integers::<u16>().min_value(2).max_value(30));
    let cols = tc.draw(gs::integers::<u16>().min_value(1).max_value(30));
    // row < rows-1 so the cursor starts strictly above the bottom margin, and
    // both LF and IND then take the plain "row += 1" branch, not the scroll
    // branch (which is covered separately by the bottom-margin regression test).
    let row = tc.draw(gs::integers::<u16>().min_value(0).max_value(rows - 2));
    let col = tc.draw(gs::integers::<u16>().min_value(0).max_value(cols - 1));

    let lf_screen = screen_after(rows, cols, row, col, b"\n");
    let ind_screen = screen_after(rows, cols, row, col, b"\x1bD");

    tc.note(&format!("rows={rows} cols={cols} start=({row},{col})"));

    assert_eq!(
        (lf_screen.cursor.row, lf_screen.cursor.col),
        (ind_screen.cursor.row, ind_screen.cursor.col),
        "IND must land at the same cursor position as a single LF"
    );
    assert_eq!(
        ind_screen.cursor.row,
        row + 1,
        "IND must move exactly one row down"
    );
    assert_eq!(
        ind_screen.cursor.col, col,
        "IND must preserve the column (no carriage return)"
    );
}

/// P2: NEL is equivalent to CR then IND, so the column resets to 0 and the row
/// advances the same way IND's does, over arbitrary starting positions
/// (including at the bottom margin, so the scroll behavior is exercised too).
#[hegel::test(test_cases = 500)]
fn nel_matches_cr_then_ind(tc: TestCase) {
    let rows = tc.draw(gs::integers::<u16>().min_value(1).max_value(30));
    let cols = tc.draw(gs::integers::<u16>().min_value(1).max_value(30));
    let row = tc.draw(gs::integers::<u16>().min_value(0).max_value(rows - 1));
    let col = tc.draw(gs::integers::<u16>().min_value(0).max_value(cols - 1));

    let cr_then_ind = screen_after(rows, cols, row, col, b"\r\x1bD");
    let nel = screen_after(rows, cols, row, col, b"\x1bE");

    tc.note(&format!("rows={rows} cols={cols} start=({row},{col})"));

    assert_eq!(
        (cr_then_ind.cursor.row, cr_then_ind.cursor.col),
        (nel.cursor.row, nel.cursor.col),
        "NEL must land at the same cursor position as CR followed by IND"
    );
    assert_eq!(nel.cursor.col, 0, "NEL must reset the column to 0");
}

/// P3: Under any sequence of {CUU, CUD, CR, LF, IND, NEL}, the cursor row never
/// leaves `[0, rows)`. This is the general safety net for the scroll-region
/// clamping shared by all six controls (`advance_to_next_row`, CUU/CUD margin
/// clamps).
#[hegel::test(test_cases = 500)]
fn cursor_row_stays_in_bounds_under_vertical_motion_sequence(tc: TestCase) {
    let rows = tc.draw(gs::integers::<u16>().min_value(1).max_value(20));
    let cols = tc.draw(gs::integers::<u16>().min_value(1).max_value(20));
    let mut e = Emulator::new(rows, cols);

    let steps = tc.draw(gs::integers::<u16>().min_value(1).max_value(40));
    let mut log = String::new();
    for _ in 0..steps {
        let n = tc.draw(gs::integers::<u16>().min_value(1).max_value(5));
        let op = match tc.draw(gs::integers::<u8>().min_value(0).max_value(5)) {
            0 => format!("\x1b[{n}A"), // CUU
            1 => format!("\x1b[{n}B"), // CUD
            2 => "\r".to_string(),     // CR
            3 => "\n".to_string(),     // LF
            4 => "\x1bD".to_string(),  // IND
            _ => "\x1bE".to_string(),  // NEL
        };
        e.advance(op.as_bytes());
        log.push_str(&op);
    }
    e.advance(b"\x1b[m"); // flush any pending trailing grapheme

    tc.note(&format!(
        "rows={rows} cols={cols} steps={steps} log={log:?}"
    ));

    let row = e.screen().cursor.row;
    assert!(
        row < rows,
        "cursor row {row} must stay within [0, {rows}) after {log:?}"
    );
}
