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
    for line in logical_lines.iter_mut() {
        while line.last().is_some_and(Cell::is_blank) {
            line.pop();
        }
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
            if Some(line_idx) == cursor_logical_line_idx && Some(0) == cursor_logical_col_in_line {
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
                while col_in_row < new_cols {
                    row_cells.push(Cell::default());
                    col_in_row += 1;
                }
            }
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
                row_cells.push(Cell::wide_spacer());
                col_in_row += 1;
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

        // Cursor at end-of-line (col == line.len()), so place it at the end of
        // the last row.
        if Some(line_idx) == cursor_logical_line_idx
            && cursor_logical_col_in_line == Some(line.len())
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
            mark: crate::grid::RowMark::default(),
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
            assert!(r.cells.iter().all(|c| c.is_blank()));
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
}
