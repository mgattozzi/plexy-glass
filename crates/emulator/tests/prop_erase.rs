//! Property: after any erase op (ED / EL / ECH), the active grid stays
//! WELL-FORMED with respect to wide graphemes: every width-2 grapheme is
//! immediately followed by its `wide_spacer`, and every spacer is immediately
//! preceded by a width-2 grapheme. Erasing part of a wide cell must erase the
//! whole cell (the orphaned half is blanked), so a partial erase can never leave
//! a dangling spacer or a half-wide grapheme behind.
//!
//! This guards the `clear_rect` wide-pair normalization that ED/EL/ECH route
//! through; without it, an erase whose boundary splits a wide grapheme would
//! orphan the other half.
//!
//! The companion `write_leaves_no_orphan_wide_pairs` asserts the same invariant
//! for the *print* path: overwriting one half of a wide cell (cursor-addressed)
//! must destroy the whole char, never leave a dangling spacer / half-wide
//! grapheme. Guards `clear_wide_straddle`.

use hegel::{TestCase, generators as gs};
use plexy_glass_emulator::Emulator;
use plexy_glass_emulator::width::display_width;

/// True if `row.cells` is well-formed: no orphaned wide grapheme / spacer.
fn well_formed(cells: &[plexy_glass_emulator::Cell]) -> Result<(), String> {
    let n = cells.len();
    for i in 0..n {
        let g = cells[i].grapheme.as_str();
        if cells[i].is_wide_spacer() {
            // A spacer must follow a width-2 grapheme.
            if i == 0 || display_width(cells[i - 1].grapheme.as_str()) != 2 {
                return Err(format!("orphaned wide-spacer at col {i}"));
            }
        } else if display_width(g) == 2 {
            // A width-2 grapheme must be followed by a spacer.
            if i + 1 >= n || !cells[i + 1].is_wide_spacer() {
                return Err(format!("wide grapheme {g:?} at col {i} missing its spacer"));
            }
        }
    }
    Ok(())
}

#[hegel::test(test_cases = 600)]
fn erase_leaves_no_orphan_wide_pairs(tc: TestCase) {
    let cols = tc.draw(gs::integers::<u16>().min_value(2).max_value(20));
    let rows = tc.draw(gs::integers::<u16>().min_value(1).max_value(4));
    let mut e = Emulator::new(rows, cols);

    // Fill with a random mix of ASCII and a wide CJK grapheme. The emulator keeps
    // the grid well-formed as it writes (wide chars wrap whole), so any orphan
    // after the erase below is the ERASE's doing.
    let len = tc.draw(
        gs::integers::<u16>()
            .min_value(0)
            .max_value(cols.saturating_mul(rows)),
    );
    let mut fill = String::new();
    for _ in 0..len {
        if tc.draw(gs::booleans()) {
            fill.push('好'); // width 2
        } else {
            fill.push('a'); // width 1
        }
    }
    e.advance(fill.as_bytes());

    // Move the cursor to a random cell (CUP clamps), then apply a random erase op.
    let r = tc.draw(gs::integers::<u16>().min_value(1).max_value(rows));
    let c = tc.draw(gs::integers::<u16>().min_value(1).max_value(cols));
    e.advance(format!("\x1b[{r};{c}H").as_bytes());
    let op: &[u8] = match tc.draw(gs::integers::<u8>().min_value(0).max_value(6)) {
        0 => b"\x1b[0K", // EL: cursor → end of line
        1 => b"\x1b[1K", // EL: start of line → cursor
        2 => b"\x1b[2K", // EL: whole line
        3 => b"\x1b[0J", // ED: cursor → end of screen
        4 => b"\x1b[1J", // ED: start of screen → cursor
        5 => b"\x1b[X",  // ECH 1
        _ => b"\x1b[3X", // ECH 3
    };
    e.advance(op);
    e.advance(b"\x1b[m"); // flush any pending grapheme into the grid

    tc.note(&format!(
        "cols={cols} rows={rows} fill={fill:?} cur=({r},{c}) op={op:?}"
    ));

    let screen = e.screen();
    for (ri, row) in screen.active.rows.iter().enumerate() {
        if let Err(why) = well_formed(&row.cells) {
            panic!("row {ri}: {why}");
        }
    }
}

#[hegel::test(test_cases = 600)]
fn write_leaves_no_orphan_wide_pairs(tc: TestCase) {
    let cols = tc.draw(gs::integers::<u16>().min_value(2).max_value(12));
    let rows = tc.draw(gs::integers::<u16>().min_value(1).max_value(4));
    let mut e = Emulator::new(rows, cols);

    // A run of cursor-address-then-write steps. Each write can land on a wide
    // grapheme or its spacer (overwriting half a wide cell) and can itself be
    // narrow or wide, the exact straddle that must blank the orphaned half.
    let steps = tc.draw(gs::integers::<u16>().min_value(1).max_value(20));
    let mut log = String::new();
    for _ in 0..steps {
        let r = tc.draw(gs::integers::<u16>().min_value(1).max_value(rows));
        let c = tc.draw(gs::integers::<u16>().min_value(1).max_value(cols));
        // c == cols deliberately reachable so a wide write at the last column
        // exercises the no-fit autowrap/pad path.
        let g = if tc.draw(gs::booleans()) { "好" } else { "a" };
        let step = format!("\x1b[{r};{c}H{g}");
        e.advance(step.as_bytes());
        log.push_str(&step);
    }
    e.advance(b"\x1b[m"); // flush any pending trailing grapheme

    tc.note(&format!(
        "cols={cols} rows={rows} steps={steps} log={log:?}"
    ));

    let screen = e.screen();
    for (ri, row) in screen.active.rows.iter().enumerate() {
        if let Err(why) = well_formed(&row.cells) {
            panic!("row {ri}: {why}");
        }
    }
}
