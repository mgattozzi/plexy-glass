//! Property tests for `reflow`, the resize-time logical-line re-wrapper.
//!
//! These properties kill the arithmetic/boundary survivors that the example-based
//! tests leave uncovered. The strategy is to generate simple ASCII-only content
//! so the expected shape is easy to compute independently of the implementation.

use hegel::TestCase;
use hegel::generators as gs;
use plexy_glass_emulator::{Cell, Cursor, Grid, Row, RowMark, Scrollback, WrapOrigin};
use plexy_glass_emulator::reflow::reflow;
use smol_str::SmolStr;

// ──────────────────────────────────────────────────────────────────────────────
// Test helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Construct a single hard-origin row of ASCII cells (width-1 each).
fn ascii_row(s: &str, cols: u16) -> Row {
    let mut cells: Vec<Cell> = s
        .chars()
        .take(cols as usize)
        .map(|c| Cell {
            grapheme: SmolStr::new(c.to_string().as_str()),
            ..Cell::default()
        })
        .collect();
    // Pad to `cols` with blanks.
    while cells.len() < cols as usize {
        cells.push(Cell::default());
    }
    Row {
        cells,
        wrap_origin: WrapOrigin::Hard,
        mark: RowMark::default(),
    }
}

/// Extract the visible text of a row (no trailing blanks, no wide spacers).
fn row_text(r: &Row) -> String {
    r.cells
        .iter()
        .filter(|c| !c.is_wide_spacer())
        .map(|c| c.grapheme.as_str())
        .collect::<String>()
        .trim_end()
        .to_string()
}


/// Draw a small column count in [1, 40].
fn draw_cols(tc: &TestCase) -> u16 {
    tc.draw(gs::integers::<u16>().min_value(1).max_value(40))
}

/// Draw a small row count in [1, 20].
fn draw_rows(tc: &TestCase) -> u16 {
    tc.draw(gs::integers::<u16>().min_value(1).max_value(20))
}

// ──────────────────────────────────────────────────────────────────────────────
// Properties
// ──────────────────────────────────────────────────────────────────────────────

/// P1: After reflow, `active` has exactly `new_rows` rows and every row has
/// exactly `new_cols` cells. This kills the `push_row` padding mutation
/// (`< vs >` at line 223) as well as the top-level pad loop.
#[hegel::test(test_cases = 500)]
fn reflow_output_is_rectangular(tc: TestCase) {
    let init_rows = draw_rows(&tc);
    let init_cols = draw_cols(&tc);
    let new_rows = draw_rows(&tc);
    let new_cols = draw_cols(&tc);

    let mut active = Grid::new(init_rows, init_cols);
    let mut sb = Scrollback::with_cap(1000);
    let mut c = Cursor::default();

    reflow(&mut active, &mut sb, &mut c, new_rows, new_cols);

    assert_eq!(
        active.num_rows(),
        new_rows,
        "active must have exactly new_rows rows"
    );
    assert_eq!(
        active.num_cols(),
        new_cols,
        "active.cols must equal new_cols"
    );
    for (i, row) in active.rows.iter().enumerate() {
        assert_eq!(
            row.cells.len() as u16,
            new_cols,
            "row {i} must have exactly {new_cols} cells"
        );
    }
}

/// P2: Reflow then un-reflow (round-trip) of a single ASCII logical line
/// preserves the text content. This exercises the wrap loop, the `+` in cursor
/// offset computation (line 58), the `>` comparisons on lines 127/121, and the
/// cursor offset calculation (line 205, `-` vs `+`).
///
/// We constrain `new_rows` to be enough to hold all wrapped content, eliminating
/// scrollback overflow so the round-trip is exact.
#[hegel::test(test_cases = 500)]
fn single_line_round_trip_preserves_text(tc: TestCase) {
    let text_len = tc.draw(gs::integers::<u16>().min_value(1).max_value(20));
    let narrow_cols = tc.draw(gs::integers::<u16>().min_value(1).max_value(text_len));
    // Final width = text_len so the whole text fits on a single row after widening.
    let wide_cols = text_len;

    let text: String = (0..text_len)
        .map(|i| char::from(b'A' + (i % 26) as u8))
        .collect();

    // Initial grid: 1 hard row at wide_cols width.
    let mut active = Grid {
        cols: wide_cols,
        rows: vec![ascii_row(&text, wide_cols)],
    };
    let mut sb = Scrollback::with_cap(1000);
    let mut c = Cursor::default();

    // Use enough rows to hold all the wrapped content so we don't spill into
    // scrollback.
    let rows_needed = text_len.div_ceil(narrow_cols) + 2;
    tc.note(&format!("text={text:?} narrow={narrow_cols} wide={wide_cols} rows_needed={rows_needed}"));

    // Narrow: wraps into multiple rows.
    reflow(&mut active, &mut sb, &mut c, rows_needed, narrow_cols);
    // Widen back to text_len cols: everything should rejoin onto one row.
    reflow(&mut active, &mut sb, &mut c, rows_needed, wide_cols);

    // After widening to text_len cols, the first content hard-origin row must
    // hold the original text exactly.
    let first_content = active
        .rows
        .iter()
        .chain(sb.rows().iter())
        .find(|r| r.wrap_origin == WrapOrigin::Hard && !row_text(r).is_empty());

    if let Some(row) = first_content {
        assert_eq!(
            row_text(row),
            text,
            "round-trip narrow+wide must preserve the logical line's text"
        );
    } else if !text.is_empty() {
        panic!("expected to find a content row but none found");
    }
}

/// P3: Multiple independent hard logical lines survive reflow.
/// A grid of N hard rows (each a distinct short word) that all fit in `new_cols`
/// must produce N hard-origin rows in the result.
/// This kills the `|| vs &&` mutation at line 45.
#[hegel::test(test_cases = 500)]
fn multi_hard_line_count_preserved(tc: TestCase) {
    let n = tc.draw(gs::integers::<u8>().min_value(1).max_value(8)) as usize;
    // All lines ≤ 8 chars; new_cols ≥ 8 so no wrapping occurs.
    let new_cols: u16 = tc.draw(gs::integers::<u16>().min_value(8).max_value(40));
    let new_rows: u16 = tc.draw(gs::integers::<u16>().min_value(n as u16).max_value(20));

    // Build N hard rows with distinct 1-char content (so none is blank/trimmed away).
    let rows: Vec<Row> = (0..n)
        .map(|i| {
            let ch = char::from(b'a' + (i % 26) as u8).to_string();
            ascii_row(&ch, new_cols)
        })
        .collect();

    let mut active = Grid {
        cols: new_cols,
        rows,
    };
    let mut sb = Scrollback::with_cap(1000);
    let mut c = Cursor::default();

    reflow(&mut active, &mut sb, &mut c, new_rows, new_cols);

    // Count hard-origin rows in active + scrollback.
    // Count content hard rows (non-blank hard origin rows).
    // Blank padding rows also have Hard origin but carry no content.
    let content_hard = active
        .rows
        .iter()
        .chain(sb.rows().iter())
        .filter(|r| r.wrap_origin == WrapOrigin::Hard && !row_text(r).is_empty())
        .count();

    tc.note(&format!("n={n} new_cols={new_cols} new_rows={new_rows} content_hard={content_hard}"));

    assert_eq!(
        content_hard, n,
        "each input hard line must produce exactly one content hard-origin row"
    );
}

/// P4: After any reflow, the cursor position is within the grid bounds.
/// This kills the cursor-column calculation mutations (lines 101, 58, 205).
#[hegel::test(test_cases = 500)]
fn cursor_stays_in_bounds_after_reflow(tc: TestCase) {
    let init_rows = draw_rows(&tc);
    let init_cols = draw_cols(&tc);
    let new_rows = draw_rows(&tc);
    let new_cols = draw_cols(&tc);

    let mut active = Grid::new(init_rows, init_cols);
    // Place cursor somewhere valid.
    let mut c = Cursor {
        row: tc.draw(gs::integers::<u16>().min_value(0).max_value(init_rows - 1)),
        col: tc.draw(gs::integers::<u16>().min_value(0).max_value(init_cols - 1)),
        ..Cursor::default()
    };
    let mut sb = Scrollback::with_cap(1000);

    reflow(&mut active, &mut sb, &mut c, new_rows, new_cols);

    tc.note(&format!(
        "cursor after reflow: ({}, {}) grid: {}x{}",
        c.row, c.col, active.num_rows(), active.num_cols()
    ));

    assert!(
        c.row < active.num_rows(),
        "cursor.row={} must be < num_rows={}",
        c.row,
        active.num_rows()
    );
    assert!(
        c.col < active.num_cols(),
        "cursor.col={} must be < num_cols={}",
        c.col,
        active.num_cols()
    );
}

/// P5: After reflow, no row contains a wide-char spacer without a valid wide
/// char immediately before it. This kills the wide-char-wrap mutations
/// (lines 121, 122, 127, 152).
///
/// We create a row that starts with wide chars, then reflow to various widths.
#[hegel::test(test_cases = 500)]
fn wide_chars_are_never_split_across_rows(tc: TestCase) {
    let new_cols = draw_cols(&tc);
    let new_rows = draw_rows(&tc);
    let n_wide = tc.draw(gs::integers::<u8>().min_value(1).max_value(4)) as usize;

    // Build a row: n_wide × "あ" (CJK, width 2) each with a spacer.
    let init_cols = (n_wide * 2).max(1) as u16;
    let mut cells: Vec<Cell> = Vec::with_capacity(init_cols as usize);
    for _ in 0..n_wide {
        cells.push(Cell {
            grapheme: SmolStr::new("あ"),
            ..Cell::default()
        });
        cells.push(Cell::wide_spacer());
    }
    while cells.len() < init_cols as usize {
        cells.push(Cell::default());
    }

    let mut active = Grid {
        cols: init_cols,
        rows: vec![Row {
            cells,
            wrap_origin: WrapOrigin::Hard,
            mark: RowMark::default(),
        }],
    };
    let mut sb = Scrollback::with_cap(1000);
    let mut c = Cursor::default();

    reflow(&mut active, &mut sb, &mut c, new_rows, new_cols);

    // Check active rows.
    for row in active.rows.iter().chain(sb.rows().iter()) {
        for (col_idx, cell) in row.cells.iter().enumerate() {
            if cell.is_wide_spacer() {
                assert!(
                    col_idx > 0,
                    "wide spacer at column 0 is invalid (no preceding wide char)"
                );
                let prev = &row.cells[col_idx - 1];
                assert!(
                    !prev.is_wide_spacer() && !prev.is_blank(),
                    "wide spacer at col {col_idx} must be preceded by a wide grapheme, not {:?}",
                    prev.grapheme
                );
            }
        }
    }
}

/// P6: A mark on the first physical row of a logical line must remain on the
/// first physical row of that logical line after reflow.
/// This kills the mark-placement / row-reconstruction mutations.
#[hegel::test(test_cases = 500)]
fn mark_rides_first_row_of_logical_line(tc: TestCase) {
    let text_len: u16 = tc.draw(gs::integers::<u16>().min_value(4).max_value(20));
    let init_cols = text_len;
    let new_cols = draw_cols(&tc);
    let new_rows = draw_rows(&tc);

    let text: String = (0..text_len)
        .map(|i| char::from(b'A' + (i % 26) as u8))
        .collect();

    // Mark the single logical line with PROMPT_START.
    let mut row = ascii_row(&text, init_cols);
    row.mark.set(RowMark::PROMPT_START);

    let mut active = Grid {
        cols: init_cols,
        rows: vec![row],
    };
    let mut sb = Scrollback::with_cap(1000);
    let mut c = Cursor::default();

    reflow(&mut active, &mut sb, &mut c, new_rows, new_cols);

    // Find the first hard-origin row (first physical row of the logical line).
    let first_hard = active
        .rows
        .iter()
        .chain(sb.rows().iter())
        .find(|r| r.wrap_origin == WrapOrigin::Hard);

    tc.note(&format!(
        "text_len={text_len} new_cols={new_cols} new_rows={new_rows}"
    ));

    if let Some(first) = first_hard {
        assert!(
            first.mark.contains(RowMark::PROMPT_START),
            "PROMPT_START mark must be on the first hard-origin row after reflow"
        );
    }
    // Continuation rows must NOT carry the mark.
    for row in active.rows.iter().chain(sb.rows().iter()) {
        if matches!(row.wrap_origin, WrapOrigin::SoftFrom(_)) {
            assert!(
                !row.mark.contains(RowMark::PROMPT_START),
                "continuation row must not carry PROMPT_START"
            );
        }
    }
}

/// P7: For a single-char line with the cursor at col 0, reflow to any size
/// leaves the cursor at col 0 and in the grid. This verifies the cursor-column
/// tracking math doesn't accidentally multiply instead of adding (line 58).
#[hegel::test(test_cases = 500)]
fn cursor_at_start_of_single_char_line_stays_at_zero(tc: TestCase) {
    let new_cols = draw_cols(&tc);
    let new_rows = draw_rows(&tc);
    let init_cols: u16 = tc.draw(gs::integers::<u16>().min_value(1).max_value(40));

    // Place a single 'X' at col 0; cursor at (0, 0).
    let mut row = ascii_row("X", init_cols);
    row.wrap_origin = WrapOrigin::Hard;
    let mut active = Grid {
        cols: init_cols,
        rows: vec![row],
    };
    let mut sb = Scrollback::with_cap(1000);
    let mut c = Cursor {
        row: 0,
        col: 0,
        ..Cursor::default()
    };

    reflow(&mut active, &mut sb, &mut c, new_rows, new_cols);

    tc.note(&format!("cursor after = ({}, {})", c.row, c.col));

    assert!(c.row < active.num_rows(), "cursor.row out of bounds");
    assert!(c.col < active.num_cols(), "cursor.col out of bounds");
    // The 'X' is always the first cell of its row, so col should be 0.
    assert_eq!(c.col, 0, "cursor at col 0 of the first char must stay at col 0");
}

/// P8: Zero-width combining marks do not expand the column count.
/// After reflow, no row should be wider than `new_cols`.
/// This is a sanity check for the combining-mark attachment code (lines 157-162).
#[hegel::test(test_cases = 300)]
fn combining_marks_do_not_overflow_row_width(tc: TestCase) {
    let new_cols = draw_cols(&tc);
    let new_rows = draw_rows(&tc);
    let n_chars: u8 = tc.draw(gs::integers::<u8>().min_value(1).max_value(8));

    // A row with combining marks: "a" + U+0301 (width 0 combining accent)
    // stored in the same cell (a single grapheme cluster).
    let init_cols: u16 = n_chars as u16;
    let mut cells: Vec<Cell> = (0..n_chars)
        .map(|_| Cell {
            grapheme: SmolStr::new("a\u{0301}"), // 'á' as a combining sequence
            ..Cell::default()
        })
        .collect();
    while cells.len() < init_cols as usize {
        cells.push(Cell::default());
    }

    let mut active = Grid {
        cols: init_cols,
        rows: vec![Row {
            cells,
            wrap_origin: WrapOrigin::Hard,
            mark: RowMark::default(),
        }],
    };
    let mut sb = Scrollback::with_cap(1000);
    let mut c = Cursor::default();

    reflow(&mut active, &mut sb, &mut c, new_rows, new_cols);

    for (i, row) in active.rows.iter().enumerate() {
        assert_eq!(
            row.cells.len() as u16,
            new_cols,
            "row {i} must have exactly new_cols={new_cols} cells, not {}",
            row.cells.len()
        );
    }
}

/// P9: An empty grid reflowed to arbitrary dimensions stays rectangular and valid.
/// This exercises the padding path and the clamp-to-1 guards (min(1) on rows and cols).
#[hegel::test(test_cases = 500)]
fn empty_grid_reflow_is_valid(tc: TestCase) {
    let new_rows = draw_rows(&tc);
    let new_cols = draw_cols(&tc);

    let mut active = Grid::new(1, 1);
    let mut sb = Scrollback::with_cap(100);
    let mut c = Cursor::default();

    reflow(&mut active, &mut sb, &mut c, new_rows, new_cols);

    assert_eq!(active.num_rows(), new_rows);
    assert_eq!(active.num_cols(), new_cols);
    assert!(c.row < active.num_rows());
    assert!(c.col < active.num_cols());
}

/// P11: Several DISTINCT hard logical lines survive a narrow→wide round-trip with
/// their TEXT and relative ORDER intact (not just their count, as P3). This is the
/// roadmap's "resize round-trip preserves content" target generalized past one line.
#[hegel::test(test_cases = 500)]
fn multi_line_round_trip_preserves_content_and_order(tc: TestCase) {
    let n = tc.draw(gs::integers::<u8>().min_value(1).max_value(6)) as usize;
    // Each line ≤ 6 distinct chars; wide width holds the longest line on one row.
    let lines: Vec<String> = (0..n)
        .map(|i| {
            let len = tc.draw(gs::integers::<u16>().min_value(1).max_value(6));
            (0..len).map(|j| char::from(b'a' + ((i * 7 + j as usize) % 26) as u8)).collect()
        })
        .collect();
    let wide_cols: u16 = 8; // ≥ longest line, so each rejoins onto one row
    let narrow_cols = tc.draw(gs::integers::<u16>().min_value(1).max_value(wide_cols));

    let mut active = Grid {
        cols: wide_cols,
        rows: lines.iter().map(|l| ascii_row(l, wide_cols)).collect(),
    };
    let mut sb = Scrollback::with_cap(1000);
    let mut c = Cursor::default();
    // Enough rows that no line is evicted to scrollback after wrapping.
    let rows_needed: u16 = lines.iter().map(|l| (l.len() as u16).div_ceil(narrow_cols)).sum::<u16>() + 4;
    tc.note(&format!("lines={lines:?} narrow={narrow_cols} rows_needed={rows_needed}"));

    reflow(&mut active, &mut sb, &mut c, rows_needed, narrow_cols);
    reflow(&mut active, &mut sb, &mut c, rows_needed, wide_cols);

    // The content hard-origin rows, in order (scrollback then active), must be the
    // original lines in order.
    let got: Vec<String> = sb.rows().iter().chain(active.rows.iter())
        .filter(|r| r.wrap_origin == WrapOrigin::Hard && !row_text(r).is_empty())
        .map(row_text)
        .collect();
    assert_eq!(got, lines, "round-trip must preserve every logical line's text AND order");
}

/// P12: With N marked logical lines, each carrying a DISTINCT exit code, a
/// narrow→wide round-trip lands each mark on ITS OWN line's hard-origin row, not
/// a neighbor's. P6 has only one line and so can't catch a cross-line off-by-one
/// in mark redistribution; this can.
#[hegel::test(test_cases = 500)]
fn multi_mark_rides_correct_line(tc: TestCase) {
    let n = tc.draw(gs::integers::<u8>().min_value(2).max_value(6)) as usize;
    let wide_cols: u16 = 8;
    let narrow_cols = tc.draw(gs::integers::<u16>().min_value(1).max_value(wide_cols));

    // Line i gets unique text (so we can find its row later) AND a unique exit code i.
    let lines: Vec<String> = (0..n).map(|i| {
        let len = tc.draw(gs::integers::<u16>().min_value(1).max_value(6));
        (0..len).map(|j| char::from(b'a' + ((i * 5 + j as usize) % 26) as u8)).collect()
    }).collect();
    let rows: Vec<Row> = lines.iter().enumerate().map(|(i, l)| {
        let mut r = ascii_row(l, wide_cols);
        r.mark.set(RowMark::PROMPT_START);
        r.mark.set_exit(Some(i as i32));
        r
    }).collect();

    let mut active = Grid { cols: wide_cols, rows };
    let mut sb = Scrollback::with_cap(1000);
    let mut c = Cursor::default();
    let rows_needed: u16 = lines.iter().map(|l| (l.len() as u16).div_ceil(narrow_cols)).sum::<u16>() + 4;
    tc.note(&format!("lines={lines:?} narrow={narrow_cols}"));

    reflow(&mut active, &mut sb, &mut c, rows_needed, narrow_cols);
    reflow(&mut active, &mut sb, &mut c, rows_needed, wide_cols);

    // For each content hard row, the PROMPT_START + exit code must match the line's
    // index in `lines` (identified by its text). No mark may land on a soft row.
    for r in sb.rows().iter().chain(active.rows.iter()) {
        let t = row_text(r);
        if r.wrap_origin == WrapOrigin::Hard && !t.is_empty() {
            let idx = lines.iter().position(|l| *l == t).expect("row text must match a source line");
            assert!(r.mark.contains(RowMark::PROMPT_START), "line {idx:?} lost its PROMPT_START");
            assert_eq!(r.mark.exit(), Some(idx as i32), "line {t:?} carries the wrong exit code");
        } else {
            assert!(!r.mark.contains(RowMark::PROMPT_START), "a soft/blank row carries a mark: {t:?}");
        }
    }
}

/// P13: A mark's PAYLOAD (exit code, duration, prompt-end column) survives a
/// narrow→wide round-trip on the correct row. prop_grid round-trips these via
/// set/get in ISOLATION; this asserts they survive reflow's mark `merge`.
#[hegel::test(test_cases = 500)]
fn mark_payload_survives_reflow(tc: TestCase) {
    let len = tc.draw(gs::integers::<u16>().min_value(1).max_value(6));
    let text: String = (0..len).map(|j| char::from(b'a' + (j as usize % 26) as u8)).collect();
    let exit = tc.draw(gs::integers::<i32>().min_value(-1).max_value(255));
    let dur = tc.draw(gs::integers::<u32>().min_value(0).max_value(600_000));
    let pe = tc.draw(gs::integers::<u16>().min_value(0).max_value(len.saturating_sub(1)));

    let wide_cols: u16 = 8;
    let narrow_cols = tc.draw(gs::integers::<u16>().min_value(1).max_value(wide_cols));
    let mut row = ascii_row(&text, wide_cols);
    row.mark.set(RowMark::PROMPT_START);
    row.mark.set_exit(Some(exit));
    row.mark.set_duration(Some(dur));
    row.mark.set_prompt_end(pe);

    let mut active = Grid { cols: wide_cols, rows: vec![row] };
    let mut sb = Scrollback::with_cap(1000);
    let mut c = Cursor::default();
    let rows_needed = len.div_ceil(narrow_cols) + 4;
    tc.note(&format!("text={text:?} exit={exit} dur={dur} pe={pe} narrow={narrow_cols}"));

    reflow(&mut active, &mut sb, &mut c, rows_needed, narrow_cols);
    reflow(&mut active, &mut sb, &mut c, rows_needed, wide_cols);

    let row = sb.rows().iter().chain(active.rows.iter())
        .find(|r| r.wrap_origin == WrapOrigin::Hard && !row_text(r).is_empty())
        .expect("content row");
    assert_eq!(row.mark.exit(), Some(exit), "exit code lost in reflow");
    assert_eq!(row.mark.duration_ms(), Some(dur), "duration lost in reflow");
    assert_eq!(row.mark.prompt_end_col(), Some(pe), "prompt-end col lost in reflow");
}

/// P10: The wrap behavior is correct at the exact boundary: a line of exactly
/// `new_cols` chars must NOT be wrapped (no soft continuation row needed).
/// If the wrap trigger `col_in_row + cw > new_cols` used `>=`, a line that
/// exactly fills a row would wrap one char early.
#[hegel::test(test_cases = 400)]
fn line_fitting_exactly_in_cols_does_not_wrap(tc: TestCase) {
    let cols: u16 = tc.draw(gs::integers::<u16>().min_value(2).max_value(20));
    let new_rows = draw_rows(&tc);

    // A line with exactly `cols` ASCII chars, so it should fit in one row without wrapping.
    let text: String = (0..cols)
        .map(|i| char::from(b'A' + (i % 26) as u8))
        .collect();

    let mut active = Grid {
        cols,
        rows: vec![ascii_row(&text, cols)],
    };
    let mut sb = Scrollback::with_cap(100);
    let mut c = Cursor::default();

    reflow(&mut active, &mut sb, &mut c, new_rows, cols);

    // The single logical line of `cols` chars must occupy exactly ONE physical row
    // (no soft continuation).
    let soft_count = active
        .rows
        .iter()
        .chain(sb.rows().iter())
        .filter(|r| matches!(r.wrap_origin, WrapOrigin::SoftFrom(_)))
        .count();

    // Blank padding rows also have Hard origin, so don't assert on the total hard count.
    // The key invariant: no soft continuation rows should exist (the line fits exactly).
    assert_eq!(soft_count, 0, "exactly-fitting line must not generate soft continuation rows");
    // And there must be exactly one content hard row (the fitted line itself).
    let content_hard = active.rows.iter().chain(sb.rows().iter())
        .filter(|r| r.wrap_origin == WrapOrigin::Hard && !row_text(r).is_empty())
        .count();
    assert_eq!(content_hard, 1, "exactly-fitting line must produce exactly one content row");
}
