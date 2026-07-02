//! Resize-time reflow: re-wrap logical lines to a new column width.

use crate::{
    cell::Cell,
    cursor::Cursor,
    grid::{Grid, Row, RowMark, WrapOrigin},
    scrollback::Scrollback,
};
use unicode_width::UnicodeWidthStr;

/// Re-flow the active grid + scrollback to (`new_rows`, `new_cols`),
/// preserving logical lines and tracking the cursor.
pub fn reflow(
    active: &mut Grid,
    scrollback: &mut Scrollback,
    cursor: &mut Cursor,
    new_rows: u16,
    new_cols: u16,
) {
    let new_rows = new_rows.max(1);
    let new_cols = new_cols.max(1);

    // 1. Take ownership of every row (scrollback then active, in order).
    let mut all_rows: Vec<Row> = scrollback.rows_mut().drain(..).collect();
    let active_start_idx = all_rows.len();
    all_rows.append(&mut active.rows);

    let cursor_abs_row = active_start_idx + cursor.row as usize;
    let cursor_col_in_row = cursor.col as usize;

    // 2. Reconstruct logical lines. A line is a contiguous run of rows where
    //    the first has WrapOrigin::Hard and subsequent rows have
    //    WrapOrigin::SoftFrom(_).
    let mut logical_lines: Vec<Vec<Cell>> = Vec::new();
    // One merged RowMark per logical line (parallel to `logical_lines`).
    // Marks land at the cursor row, which CAN be a soft continuation row
    // (cursor mid-wrapped-line when the OSC arrives), so OR every physical
    // row's mark into the line rather than keeping only the first row's.
    let mut line_marks: Vec<RowMark> = Vec::new();
    let mut cursor_logical_line_idx: Option<usize> = None;
    let mut cursor_logical_col_in_line: Option<usize> = None;

    for (idx, row) in all_rows.iter().enumerate() {
        let is_continuation = matches!(row.wrap_origin, WrapOrigin::SoftFrom(_));
        if !is_continuation || logical_lines.is_empty() {
            logical_lines.push(Vec::with_capacity(row.cells.len()));
            line_marks.push(RowMark::default());
        }
        // invariant: we just pushed a line above if none existed
        let line = logical_lines.last_mut().expect("invariant: line started above");
        // invariant: line_marks is pushed in lockstep with logical_lines
        line_marks.last_mut().expect("invariant: mark pushed above").merge(row.mark);
        let col_offset_in_line = line.len();
        line.extend(row.cells.iter().cloned());

        if idx == cursor_abs_row {
            cursor_logical_line_idx = Some(logical_lines.len() - 1);
            cursor_logical_col_in_line = Some(col_offset_in_line + cursor_col_in_row);
        }
    }

    // 3. Trim trailing default blanks from each logical line. We want to drop
    //    row-padding spaces so reflow doesn't accumulate trailing whitespace.
    for line in &mut logical_lines {
        while line.last().is_some_and(Cell::is_blank) {
            line.pop();
        }
    }

    // If the cursor's logical column landed on a wide spacer (CUP/CHA clamp the
    // cursor only to cols-1, so it can sit on a spacer column), back it up to
    // the spacer's wide partner. Spacers are `continue`d in the re-wrap loop
    // before the per-cell cursor match, so a spacer-resident cursor would
    // otherwise never match and fall through to the stale-row clamp.
    if let (Some(li), Some(ci)) = (cursor_logical_line_idx, cursor_logical_col_in_line)
        && logical_lines
            .get(li)
            .and_then(|line| line.get(ci))
            .is_some_and(Cell::is_wide_spacer)
    {
        cursor_logical_col_in_line = Some(ci.saturating_sub(1));
    }

    // 4. Re-wrap each logical line at `new_cols`. Wide chars (width 2) must not
    //    split across a row boundary; if the next char doesn't fit, pad the
    //    current row with a blank and wrap.
    let mut new_rows_buf: Vec<Row> = Vec::new();
    let mut cursor_new_abs_row: Option<usize> = None;
    let mut cursor_new_col: u16 = 0;

    for (line_idx, line) in logical_lines.iter().enumerate() {
        let line_mark = line_marks[line_idx];
        if line.is_empty() {
            // Empty hard-newline line. Emit a single blank row with Hard
            // origin, keeping the line's mark (a 133;D can land on a row
            // that is blank after the trailing-blank trim above).
            let mut row = Row::blank(new_cols);
            row.mark = line_mark;
            // The cursor is on this empty logical line (any column maps to col 0
            // of the single blank row it becomes).
            if Some(line_idx) == cursor_logical_line_idx && cursor_logical_col_in_line.is_some() {
                cursor_new_abs_row = Some(new_rows_buf.len());
                cursor_new_col = 0;
            }
            new_rows_buf.push(row);
            continue;
        }

        let mut row_cells: Vec<Cell> = Vec::with_capacity(new_cols as usize);
        let mut col_in_row: u16 = 0;
        let mut first_row_of_line = true;

        for (cell_idx, cell) in line.iter().enumerate() {
            if cell.is_wide_spacer() {
                // Already consumed by its wide partner; never standalone.
                continue;
            }
            let cw = cell.grapheme.as_str().width() as u16;

            // Avoid splitting a wide char across the line edge.
            if cw == 2 && col_in_row + 2 > new_cols && col_in_row > 0 {
                // Equivalent note: `> 0` vs `< 0` on the last sub-expression is
                // unmeasurable: `col_in_row < 0` is always false for usize.
                while col_in_row < new_cols {
                    // Equivalent note: `< new_cols` vs `> new_cols`. `> new_cols` is always
                    // false here (we enter when col_in_row < new_cols).
                    row_cells.push(Cell::default());
                    col_in_row += 1;
                }
            }
            // Equivalent note: `cw > 0` vs `cw >= 0`. `cw >= 0` is always true for
            // u16, but in the `cw == 0` case `col_in_row + 0 > new_cols` is always
            // false (col never exceeds nc), so the flush body never fires regardless.
            if cw > 0 && col_in_row + cw > new_cols && col_in_row > 0 {
                // Flush row
                push_row(&mut new_rows_buf, row_cells, first_row_of_line, line_idx, new_cols, line_mark);
                row_cells = Vec::with_capacity(new_cols as usize);
                col_in_row = 0;
                first_row_of_line = false;
            }

            if Some(line_idx) == cursor_logical_line_idx
                && Some(cell_idx) == cursor_logical_col_in_line
            {
                cursor_new_abs_row = Some(new_rows_buf.len());
                cursor_new_col = col_in_row;
            }

            row_cells.push(cell.clone());
            col_in_row += 1;
            if cw == 2 {
                // Only emit the wide spacer if its column fits. In a 1-col grid
                // a leading wide char can't (the pad/flush guards above require
                // col_in_row > 0), so without this check we'd write a 2-cell row
                // into a 1-col grid. Drop the spacer and keep the grapheme as a
                // single clamped cell, mirroring `put_grapheme`'s degenerate case.
                if col_in_row < new_cols {
                    row_cells.push(Cell::wide_spacer());
                    col_in_row += 1;
                }
            } else if cw == 0 {
                // Zero-width char: attach it to the previous cell's grapheme, and if
                // there is no previous cell, drop it.
                let last = row_cells.len() - 1;
                if last > 0 {
                    let s = row_cells[last].grapheme.clone();
                    row_cells[last - 1].grapheme = format!("{}{}", row_cells[last - 1].grapheme, s).into();
                    row_cells.pop();
                    col_in_row = col_in_row.saturating_sub(1);
                }
            }
        }

        // Cursor at or past end-of-line (col >= line.len(), e.g. parked in the
        // trailing-blank padding that was trimmed away), so place it at the end
        // of the last emitted row rather than falling through to the stale-row
        // clamp.
        if Some(line_idx) == cursor_logical_line_idx
            && cursor_logical_col_in_line.is_some_and(|c| c >= line.len())
        {
            cursor_new_abs_row = Some(new_rows_buf.len());
            cursor_new_col = col_in_row;
        }

        // Push the final row of this logical line.
        push_row(&mut new_rows_buf, row_cells, first_row_of_line, line_idx, new_cols, line_mark);
    }

    // 5. Pad to at least `new_rows` total rows (blank rows at the bottom).
    while new_rows_buf.len() < new_rows as usize {
        new_rows_buf.push(Row::blank(new_cols));
    }

    // 6. Split: last `new_rows` rows become the active grid; rest goes to scrollback.
    let split_at = new_rows_buf.len().saturating_sub(new_rows as usize);
    let new_active: Vec<Row> = new_rows_buf.drain(split_at..).collect();
    let scroll_rows: Vec<Row> = new_rows_buf;

    for r in scroll_rows {
        scrollback.push(r);
    }
    active.rows = new_active;
    active.cols = new_cols;

    // 7. Resolve cursor.
    if let Some(abs) = cursor_new_abs_row {
        if abs < split_at {
            // Cursor's logical position is now in scrollback, so clamp to the top of
            // the active area.
            cursor.row = 0;
            cursor.col = 0;
        } else {
            cursor.row = (abs - split_at) as u16;
            cursor.col = cursor_new_col.min(new_cols.saturating_sub(1));
        }
    } else {
        cursor.row = cursor.row.min(new_rows.saturating_sub(1));
        cursor.col = cursor.col.min(new_cols.saturating_sub(1));
    }
    cursor.pending_wrap = false;
}

fn push_row(
    buf: &mut Vec<Row>,
    mut cells: Vec<Cell>,
    first_row_of_line: bool,
    line_idx: usize,
    cols: u16,
    line_mark: RowMark,
) {
    while (cells.len() as u16) < cols {
        cells.push(Cell::default());
    }
    let wrap_origin = if first_row_of_line {
        WrapOrigin::Hard
    } else {
        WrapOrigin::SoftFrom(line_idx as u32)
    };
    buf.push(Row {
        cells,
        wrap_origin,
        // The logical line's merged mark rides on its FIRST physical row only
        // (same placement rule as WrapOrigin::Hard); continuation rows are
        // markless so a block boundary exists exactly once per line.
        mark: if first_row_of_line { line_mark } else { RowMark::default() },
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use smol_str::SmolStr;

    fn cell(s: &str) -> Cell {
        Cell {
            grapheme: SmolStr::new(s),
            ..Cell::default()
        }
    }

    fn fill_row(cells: &[&str], origin: WrapOrigin) -> Row {
        Row {
            cells: cells.iter().map(|s| cell(s)).collect(),
            wrap_origin: origin,
            mark: RowMark::default(),
        }
    }

    fn row_text(r: &Row) -> String {
        r.cells
            .iter()
            .filter(|c| !c.is_wide_spacer())
            .map(|c| c.grapheme.as_str())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn narrow_to_wide_unwraps_soft_lines() {
        // Original: 4-col grid with "Hell" / "o!" (Hard then SoftFrom).
        let mut active = Grid {
            cols: 4,
            rows: vec![
                fill_row(&["H", "e", "l", "l"], WrapOrigin::Hard),
                fill_row(&["o", "!", " ", " "], WrapOrigin::SoftFrom(0)),
            ],
        };
        let mut sb = Scrollback::with_cap(100);
        let mut c = Cursor {
            row: 1,
            col: 2,
            ..Cursor::default()
        };

        reflow(&mut active, &mut sb, &mut c, 2, 8);

        assert_eq!(active.cols, 8);
        assert_eq!(active.rows.len(), 2);
        // "Hello!" all on row 0 now.
        assert_eq!(row_text(&active.rows[0]), "Hello!");
        assert_eq!(active.rows[0].wrap_origin, WrapOrigin::Hard);
        // Cursor was at logical col 6 (length of "Hello!"); on the new row at col 6.
        assert_eq!(c.row, 0);
        assert_eq!(c.col, 6);
    }

    #[test]
    fn wide_char_at_col0_in_one_col_grid_stays_rectangular() {
        // A leading wide grapheme reflowed into a 1-col grid must not emit a
        // 2-cell (wide + spacer) row, since that violates grid-rectangularity.
        let mut active = Grid {
            cols: 2,
            rows: vec![Row {
                cells: vec![cell("あ"), Cell::wide_spacer()],
                wrap_origin: WrapOrigin::Hard,
                mark: RowMark::default(),
            }],
        };
        let mut sb = Scrollback::with_cap(100);
        let mut c = Cursor::default();
        reflow(&mut active, &mut sb, &mut c, 4, 1);
        assert_eq!(active.cols, 1);
        for r in &active.rows {
            assert_eq!(r.cells.len(), 1, "every row must be exactly 1 cell wide");
        }
    }

    #[test]
    fn cursor_past_text_tracks_row_after_join() {
        // "Hello World" soft-wrapped across rows 0-1, then "ab" on row 2 with the
        // cursor parked in the trailing padding (col 5, past "ab"). Widening
        // joins rows 0-1, shifting "ab" up to new row 1, so the cursor must
        // follow to row 1, not stay on the stale row 2.
        let mut active = Grid {
            cols: 8,
            rows: vec![
                fill_row(&["H", "e", "l", "l", "o", " ", "W", "o"], WrapOrigin::Hard),
                fill_row(&["r", "l", "d", " ", " ", " ", " ", " "], WrapOrigin::SoftFrom(0)),
                fill_row(&["a", "b", " ", " ", " ", " ", " ", " "], WrapOrigin::Hard),
            ],
        };
        let mut sb = Scrollback::with_cap(100);
        let mut c = Cursor {
            row: 2,
            col: 5,
            ..Cursor::default()
        };
        reflow(&mut active, &mut sb, &mut c, 4, 16);
        assert_eq!(row_text(&active.rows[0]), "Hello World");
        assert_eq!(row_text(&active.rows[1]), "ab");
        assert_eq!(c.row, 1, "cursor must follow 'ab' to the joined-up row");
    }

    #[test]
    fn cursor_on_wide_spacer_normalizes_to_partner() {
        // Cursor parked on a wide char's spacer column (col 1). After reflow it
        // must resolve onto the wide grapheme (col 0), not fall through to the
        // stale-row clamp.
        let mut active = Grid {
            cols: 8,
            rows: vec![Row {
                cells: vec![
                    cell("あ"),
                    Cell::wide_spacer(),
                    cell("b"),
                    cell("c"),
                    Cell::default(),
                    Cell::default(),
                    Cell::default(),
                    Cell::default(),
                ],
                wrap_origin: WrapOrigin::Hard,
                mark: RowMark::default(),
            }],
        };
        let mut sb = Scrollback::with_cap(100);
        let mut c = Cursor {
            row: 0,
            col: 1,
            ..Cursor::default()
        };
        reflow(&mut active, &mut sb, &mut c, 4, 16);
        assert_eq!(c.row, 0);
        assert_eq!(c.col, 0, "spacer cursor must resolve to the wide grapheme");
    }

    #[test]
    fn wide_char_wraps_whole_not_split() {
        // "a好" in a 4-col grid reflowed to 2 cols: 好 (width 2) can't share row 0
        // with 'a', so it wraps WHOLE to the next row (row 0 padded), never split.
        let mut active = Grid {
            cols: 4,
            rows: vec![Row {
                cells: vec![cell("a"), cell("好"), Cell::wide_spacer(), Cell::default()],
                wrap_origin: WrapOrigin::Hard,
                mark: RowMark::default(),
            }],
        };
        let mut sb = Scrollback::with_cap(100);
        let mut c = Cursor::default();
        reflow(&mut active, &mut sb, &mut c, 4, 2);
        assert_eq!(active.cols, 2);
        assert_eq!(row_text(&active.rows[0]), "a");
        assert_eq!(row_text(&active.rows[1]), "好");
        // Row 1 holds the wide char + its spacer (rectangular, not split).
        assert_eq!(active.rows[1].cells.len(), 2);
        assert!(active.rows[1].cells[1].is_wide_spacer());
    }

    #[test]
    fn combining_mark_cell_survives_reflow() {
        // A precomposed combining-mark grapheme ("a" + U+0301) is a single
        // width-1 cell; reflow must carry it intact, not split or drop the mark.
        let mut active = Grid {
            cols: 4,
            rows: vec![fill_row(&["a\u{0301}", "b", " ", " "], WrapOrigin::Hard)],
        };
        let mut sb = Scrollback::with_cap(100);
        let mut c = Cursor::default();
        reflow(&mut active, &mut sb, &mut c, 4, 8);
        assert_eq!(active.cols, 8);
        assert_eq!(active.rows[0].cells[0].grapheme.as_str(), "a\u{0301}");
        assert_eq!(active.rows[0].cells[1].grapheme.as_str(), "b");
    }

    #[test]
    fn wide_to_narrow_re_wraps() {
        // 11-col line "Hello World"
        let mut active = Grid {
            cols: 11,
            rows: vec![fill_row(
                &["H", "e", "l", "l", "o", " ", "W", "o", "r", "l", "d"],
                WrapOrigin::Hard,
            )],
        };
        let mut sb = Scrollback::with_cap(100);
        let mut c = Cursor::default();

        reflow(&mut active, &mut sb, &mut c, 4, 6);

        // "Hello " on row 0, "World" on row 1, soft-continued.
        assert_eq!(active.cols, 6);
        assert!(active.rows.len() >= 2);
        assert_eq!(row_text(&active.rows[0]), "Hello");
        assert_eq!(row_text(&active.rows[1]), "World");
        assert!(matches!(active.rows[1].wrap_origin, WrapOrigin::SoftFrom(_)));
    }

    #[test]
    fn empty_hard_lines_preserved() {
        // Two empty hard-newline rows.
        let mut active = Grid {
            cols: 4,
            rows: vec![
                fill_row(&[" ", " ", " ", " "], WrapOrigin::Hard),
                fill_row(&[" ", " ", " ", " "], WrapOrigin::Hard),
            ],
        };
        let mut sb = Scrollback::with_cap(100);
        let mut c = Cursor::default();
        reflow(&mut active, &mut sb, &mut c, 2, 4);
        assert_eq!(active.rows.len(), 2);
        for r in &active.rows {
            assert!(r.cells.iter().all(super::super::cell::Cell::is_blank));
        }
    }

    fn with_mark(mut row: Row, flag: u8, exit: Option<i32>) -> Row {
        row.mark.set(flag);
        if exit.is_some() {
            row.mark.set_exit(exit);
        }
        row
    }

    #[test]
    fn narrower_reflow_puts_mark_on_lines_first_row_only() {
        use crate::grid::RowMark;
        // One marked 11-col hard line that will wrap into two rows at 6 cols.
        let mut active = Grid {
            cols: 11,
            rows: vec![with_mark(
                fill_row(
                    &["H", "e", "l", "l", "o", " ", "W", "o", "r", "l", "d"],
                    WrapOrigin::Hard,
                ),
                RowMark::PROMPT_START,
                Some(0),
            )],
        };
        let mut sb = Scrollback::with_cap(100);
        let mut c = Cursor::default();

        reflow(&mut active, &mut sb, &mut c, 4, 6);

        assert_eq!(row_text(&active.rows[0]), "Hello");
        assert_eq!(row_text(&active.rows[1]), "World");
        assert!(active.rows[0].mark.contains(RowMark::PROMPT_START));
        assert_eq!(active.rows[0].mark.exit(), Some(0));
        assert!(
            active.rows[1].mark.is_empty(),
            "continuation rows must be markless"
        );
    }

    #[test]
    fn wider_reflow_merges_marks_onto_joined_lines_first_row() {
        use crate::grid::RowMark;
        // A wrapped line whose first row is a prompt start and whose
        // continuation row carries a block end with an exit code (a 133;D that
        // arrived while the cursor sat mid-wrapped-line). Re-joining must merge
        // both onto the single resulting row.
        let mut active = Grid {
            cols: 4,
            rows: vec![
                with_mark(
                    fill_row(&["H", "e", "l", "l"], WrapOrigin::Hard),
                    RowMark::PROMPT_START,
                    None,
                ),
                with_mark(
                    fill_row(&["o", "!", " ", " "], WrapOrigin::SoftFrom(0)),
                    RowMark::BLOCK_END,
                    Some(3),
                ),
            ],
        };
        let mut sb = Scrollback::with_cap(100);
        let mut c = Cursor::default();

        reflow(&mut active, &mut sb, &mut c, 2, 8);

        assert_eq!(row_text(&active.rows[0]), "Hello!");
        let mark = active.rows[0].mark;
        assert!(mark.contains(RowMark::PROMPT_START));
        assert!(mark.contains(RowMark::BLOCK_END));
        assert_eq!(mark.exit(), Some(3));
    }

    #[test]
    fn reflow_round_trip_preserves_mark() {
        use crate::grid::RowMark;
        let mut active = Grid {
            cols: 11,
            rows: vec![with_mark(
                fill_row(
                    &["H", "e", "l", "l", "o", " ", "W", "o", "r", "l", "d"],
                    WrapOrigin::Hard,
                ),
                RowMark::OUTPUT_START,
                Some(1),
            )],
        };
        let mut sb = Scrollback::with_cap(100);
        let mut c = Cursor::default();

        reflow(&mut active, &mut sb, &mut c, 4, 6); // narrow: wraps to 2 rows
        reflow(&mut active, &mut sb, &mut c, 4, 11); // wide: re-joins

        assert_eq!(row_text(&active.rows[0]), "Hello World");
        assert!(active.rows[0].mark.contains(RowMark::OUTPUT_START));
        assert_eq!(active.rows[0].mark.exit(), Some(1));
        assert!(active.rows[1].mark.is_empty());
    }

    #[test]
    fn empty_marked_line_keeps_mark_through_reflow() {
        use crate::grid::RowMark;
        // A blank hard row can still carry a mark (e.g. a 133;D on an empty
        // line), so the all-blank trim must not lose it.
        let mut active = Grid {
            cols: 4,
            rows: vec![
                with_mark(
                    fill_row(&[" ", " ", " ", " "], WrapOrigin::Hard),
                    RowMark::BLOCK_END,
                    Some(0),
                ),
                fill_row(&["x", " ", " ", " "], WrapOrigin::Hard),
            ],
        };
        let mut sb = Scrollback::with_cap(100);
        let mut c = Cursor::default();

        reflow(&mut active, &mut sb, &mut c, 2, 8);

        assert!(active.rows[0].mark.contains(RowMark::BLOCK_END));
        assert_eq!(active.rows[0].mark.exit(), Some(0));
        assert!(active.rows[1].mark.is_empty());
    }

    #[test]
    fn shrink_to_zero_clamps_to_one() {
        let mut active = Grid::new(3, 3);
        let mut sb = Scrollback::with_cap(100);
        let mut c = Cursor::default();
        reflow(&mut active, &mut sb, &mut c, 0, 0);
        assert!(active.num_rows() >= 1);
        assert!(active.num_cols() >= 1);
    }

    // Helper: build a `Row` with `PROMPT_END` set at the given col.
    fn with_prompt_end(mut row: Row, col: u16) -> Row {
        row.mark.set_prompt_end(col);
        row
    }

    #[test]
    fn reflow_carries_prompt_end_to_logical_first_row() {
        use crate::grid::RowMark;
        // An 11-col hard line with PROMPT_END at col 4; it wraps to 6-col rows.
        // After reflow the PROMPT_END flag+col must be on the FIRST physical row.
        let mut active = Grid {
            cols: 11,
            rows: vec![with_prompt_end(
                fill_row(
                    &["$", " ", "c", "m", "d", " ", "a", "r", "g", " ", " "],
                    WrapOrigin::Hard,
                ),
                4, // B landed at col 4 (after "$ cmd")
            )],
        };
        let mut sb = Scrollback::with_cap(100);
        let mut c = Cursor::default();

        reflow(&mut active, &mut sb, &mut c, 4, 6);

        // Wrapped into two rows; PROMPT_END must be on the first row.
        assert!(
            active.rows[0].mark.contains(RowMark::PROMPT_END),
            "PROMPT_END must be on the first physical row after narrower reflow"
        );
        assert_eq!(
            active.rows[0].mark.prompt_end_col(),
            Some(4),
            "prompt_end_col must survive narrower reflow unchanged"
        );
        assert!(
            !active.rows[1].mark.contains(RowMark::PROMPT_END),
            "continuation row must not have PROMPT_END"
        );
    }

    #[test]
    fn reflow_merge_both_carry_prompt_end_other_col_wins() {
        use crate::grid::RowMark;
        // Two soft-wrapped rows that BOTH carry PROMPT_END (rare but defensive):
        // after join, the SECOND (other) row's col must win.
        let mut r0 = fill_row(&["a", "b", "c", "d"], WrapOrigin::Hard);
        r0.mark.set_prompt_end(2); // first row col 2

        let mut r1 = fill_row(&["e", "f", " ", " "], WrapOrigin::SoftFrom(0));
        r1.mark.set_prompt_end(6); // second (other) row col 6

        let mut active = Grid { cols: 4, rows: vec![r0, r1] };
        let mut sb = Scrollback::with_cap(100);
        let mut c = Cursor::default();

        reflow(&mut active, &mut sb, &mut c, 2, 8); // widen: joins

        let mark = active.rows[0].mark;
        assert!(mark.contains(RowMark::PROMPT_END));
        assert_eq!(
            mark.prompt_end_col(),
            Some(6),
            "other row's col wins when both carry PROMPT_END"
        );
    }

    // ── Targeted mutation-killer tests ──────────────────────────────────────

    /// Line 58: cursor_logical_col_in_line = col_offset + cursor_col (not *).
    /// With `+` mutated to `*` the cursor lands at col 0 instead of col 2.
    #[test]
    fn cursor_mid_soft_line_offset_uses_addition() {
        let mut active = Grid {
            cols: 2,
            rows: vec![
                fill_row(&["a", "b"], WrapOrigin::Hard),
                fill_row(&["c", "d"], WrapOrigin::SoftFrom(0)),
            ],
        };
        let mut sb = Scrollback::with_cap(100);
        let mut c = Cursor { row: 1, col: 0, ..Cursor::default() };
        reflow(&mut active, &mut sb, &mut c, 2, 4);
        assert_eq!(c.row, 0);
        assert_eq!(c.col, 2, "logical col = col_offset(2) + cursor_col(0) = 2, not 2*0=0");
    }

    /// Lines 101:31/58: empty-line cursor tracking must fire on the right logical
    /// line. With `==` mutated to `!=` the cursor is not tracked; with `&&` to
    /// `||` it fires for the wrong (later) empty line. Both mutations produce
    /// row 2, not row 1.
    #[test]
    fn cursor_on_empty_logical_line_follows_reflow() {
        let mut active = Grid {
            cols: 3,
            rows: vec![
                fill_row(&["a", "b", "c"], WrapOrigin::Hard),
                fill_row(&["d", "e", "f"], WrapOrigin::SoftFrom(0)),
                fill_row(&[" ", " ", " "], WrapOrigin::Hard), // cursor row (empty after trim)
                fill_row(&[" ", " ", " "], WrapOrigin::Hard),
            ],
        };
        let mut sb = Scrollback::with_cap(100);
        let mut c = Cursor { row: 2, col: 0, ..Cursor::default() };
        reflow(&mut active, &mut sb, &mut c, 4, 6);
        assert_eq!(c.row, 1, "empty logical line must follow join of 'abcdef' to row 1");
        assert_eq!(c.col, 0);
    }

    /// Lines 121:42: `col_in_row + 2 > new_cols` must use `>`, not `>=` / `<` / `==`.
    /// When col+2 == new_cols the wide char fits exactly, so padding must NOT fire.
    #[test]
    fn exact_fit_wide_char_stays_on_current_row() {
        // "a好": col 0='a', cols 1–2='好'+spacer in 3-col.  1+2=3 == nc=3: fits exactly.
        let mut active = Grid {
            cols: 3,
            rows: vec![Row {
                cells: vec![cell("a"), cell("好"), Cell::wide_spacer()],
                wrap_origin: WrapOrigin::Hard,
                mark: RowMark::default(),
            }],
        };
        let mut sb = Scrollback::with_cap(100);
        let mut c = Cursor::default();
        reflow(&mut active, &mut sb, &mut c, 2, 3);
        assert_eq!(row_text(&active.rows[0]), "a好",
            "col+2==nc means fits exactly; >= mutation wrongly wraps it to the next row");
    }

    /// Line 121:42: `col_in_row + 2 > new_cols`. The `<` mutation fires when the
    /// wide char FITS (col+2 < nc), spuriously padding and pushing it to the
    /// next row.
    #[test]
    fn fitting_wide_char_not_spuriously_wrapped() {
        // "a好" in nc=4: '好' at col=1, 1+2=3 < 4 (fits with room to spare).
        // The `< ` mutation sees 3<4=true and pads/wraps; original 3>4=false skips.
        let mut active = Grid {
            cols: 4,
            rows: vec![Row {
                cells: vec![cell("a"), cell("好"), Cell::wide_spacer(), Cell::default()],
                wrap_origin: WrapOrigin::Hard,
                mark: RowMark::default(),
            }],
        };
        let mut sb = Scrollback::with_cap(100);
        let mut c = Cursor::default();
        reflow(&mut active, &mut sb, &mut c, 2, 4);
        assert_eq!(row_text(&active.rows[0]), "a好",
            "col+2=3 < nc=4 so wide char fits; < mutation wrongly pads and wraps");
    }

    /// Line 121:38: overflow check uses addition `col + 2`, not multiplication.
    /// At col=3, nc=5: 3+2=5 (no overflow) but 3*2=6 (overflow), so the mutation
    /// wraps.
    #[test]
    fn wide_char_overflow_check_uses_addition() {
        let mut active = Grid {
            cols: 5,
            rows: vec![Row {
                cells: vec![cell("a"), cell("b"), cell("c"), cell("好"), Cell::wide_spacer()],
                wrap_origin: WrapOrigin::Hard,
                mark: RowMark::default(),
            }],
        };
        let mut sb = Scrollback::with_cap(100);
        let mut c = Cursor::default();
        reflow(&mut active, &mut sb, &mut c, 2, 5);
        assert_eq!(row_text(&active.rows[0]), "abc好",
            "3+2=5 (no overflow); * mutation gives 3*2=6>5 and spuriously wraps");
    }

    /// Lines 121:67 and 127:67: `col_in_row > 0` must be strict.
    /// With `>= 0` (always true) a wide char at col 0 triggers a spurious blank row.
    #[test]
    fn wide_char_at_col0_no_spurious_blank_row() {
        let mut active = Grid {
            cols: 2,
            rows: vec![Row {
                cells: vec![cell("好"), Cell::wide_spacer()],
                wrap_origin: WrapOrigin::Hard,
                mark: RowMark::default(),
            }],
        };
        let mut sb = Scrollback::with_cap(100);
        let mut c = Cursor::default();
        reflow(&mut active, &mut sb, &mut c, 4, 1);
        assert_eq!(active.rows[0].cells[0].grapheme.as_str(), "好",
            ">= mutation inserts a spurious blank row 0 before the wide char");
    }

    /// Line 122:34: pad loop is `while col < new_cols`, not `<=`.
    /// `<=` pushes one extra blank, making the row new_cols+1 wide.
    #[test]
    fn wide_char_pad_loop_does_not_overshoot() {
        // "ab好" in 5-col → nc=3: '好' at col=2 overflows (2+2=4>3), pad fires once.
        let mut active = Grid {
            cols: 5,
            rows: vec![Row {
                cells: vec![cell("a"), cell("b"), cell("好"), Cell::wide_spacer(), Cell::default()],
                wrap_origin: WrapOrigin::Hard,
                mark: RowMark::default(),
            }],
        };
        let mut sb = Scrollback::with_cap(100);
        let mut c = Cursor::default();
        reflow(&mut active, &mut sb, &mut c, 3, 3);
        for (i, row) in active.rows.iter().enumerate() {
            assert_eq!(row.cells.len(), 3,
                "row {i} must have exactly 3 cells; <= mutation adds an extra blank");
        }
    }

    /// Line 152: the spacer is counted by `col_in_row += 1`, not `-=` or `*=`.
    /// With `-=` the column reverts after each spacer, collapsing two wide chars
    /// onto the same columns, so the row ends up with 4 cells for nc=2.
    #[test]
    fn wide_char_spacer_col_counted() {
        let mut active = Grid {
            cols: 4,
            rows: vec![Row {
                cells: vec![cell("好"), Cell::wide_spacer(), cell("好"), Cell::wide_spacer()],
                wrap_origin: WrapOrigin::Hard,
                mark: RowMark::default(),
            }],
        };
        let mut sb = Scrollback::with_cap(100);
        let mut c = Cursor { row: 0, col: 2, ..Cursor::default() };
        reflow(&mut active, &mut sb, &mut c, 3, 2);
        assert_eq!(active.cols, 2);
        for (i, row) in active.rows.iter().enumerate() {
            assert_eq!(row.cells.len(), 2, "row {i} must have exactly 2 cells");
        }
        assert_eq!(row_text(&active.rows[0]), "好");
        assert_eq!(row_text(&active.rows[1]), "好");
        assert_eq!(c.row, 1, "cursor at col=2 of 4-col row must follow second '好' to row 1");
        assert_eq!(c.col, 0);
    }

    /// Lines 157/158/160: combining mark (cw=0) merges into the cell before it.
    /// Without the merge ('< 0' never fires) col is not decremented and 'b' wraps.
    /// Mutations at 157/160 that compute a wrong index cause an OOB panic.
    #[test]
    fn combining_mark_merges_into_prev_cell() {
        // ["a", "\u{0301}"(cw=0), "b"] in 3-col, reflow to nc=2.
        let mut active = Grid {
            cols: 3,
            rows: vec![Row {
                cells: vec![cell("a"), cell("\u{0301}"), cell("b")],
                wrap_origin: WrapOrigin::Hard,
                mark: RowMark::default(),
            }],
        };
        let mut sb = Scrollback::with_cap(100);
        let mut c = Cursor::default();
        reflow(&mut active, &mut sb, &mut c, 2, 2);
        assert_eq!(row_text(&active.rows[0]), "a\u{0301}b",
            "combining mark merges into 'a'; without merge 'b' col overflows to next row");
    }

    /// Line 158:25 `>= 0` mutation: when the combining mark is the FIRST cell in
    /// row_cells (last == 0), `>= 0` is true and `row_cells[last-1]` underflows
    /// and panics. The original `> 0` is false and correctly skips the merge
    /// body.
    #[test]
    fn lone_combining_mark_first_cell_does_not_panic() {
        let mut active = Grid {
            cols: 1,
            rows: vec![Row {
                cells: vec![cell("\u{0301}")],
                wrap_origin: WrapOrigin::Hard,
                mark: RowMark::default(),
            }],
        };
        let mut sb = Scrollback::with_cap(100);
        let mut c = Cursor::default();
        reflow(&mut active, &mut sb, &mut c, 2, 1);
        assert_eq!(active.cols, 1);
        for (i, row) in active.rows.iter().enumerate() {
            assert_eq!(row.cells.len(), 1, "row {i} must have 1 cell");
        }
    }
}
