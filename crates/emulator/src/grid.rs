//! Rectangular cell grid with wrap-origin tracking on rows.

use crate::cell::Cell;

/// Per-row wrap origin. Used by reflow to reconstruct logical lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapOrigin {
    /// First row of a logical line (explicit newline or top of screen).
    Hard,
    /// Continuation of the logical line whose first row had this id.
    SoftFrom(u32),
}

/// OSC 133 block annotation carried by a row. Kept tiny (8 bytes, since rows
/// are cloned per frame): the exit code is stored inline with a presence bit
/// in `flags` instead of an `Option<i32>` (which has no niche and would pad
/// the struct to 12 bytes). `prompt_end_col` shares the padding gap between
/// the `u8` flags and the `i32` exit code (`u8` + `u16` + `i32` = 7 bytes,
/// 8 with alignment, so the size test is untouched).
///
/// Marks live ON the row, so they travel with it into scrollback, vanish on
/// eviction, and survive reflow by the same mechanism as `wrap_origin`.
/// `Default` is the empty (unmarked) state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RowMark {
    /// Bitwise OR of [`RowMark::PROMPT_START`], [`RowMark::OUTPUT_START`],
    /// [`RowMark::BLOCK_END`], [`RowMark::PROMPT_END`], the runtime
    /// [`RowMark::FOLDED`] bit, and the private exit-presence bit. 0 = unmarked.
    flags: u8,
    /// Column at which `OSC 133;B` (prompt end) landed. Valid only when
    /// [`RowMark::PROMPT_END`] is set; kept `0` otherwise so the derived
    /// `PartialEq` stays well-defined.
    prompt_end_col: u16,
    /// Exit code payload; meaningful only while the `HAS_EXIT` bit is set
    /// (kept 0 otherwise so the derived `PartialEq` stays well-defined).
    exit_code: i32,
}

impl RowMark {
    /// An `OSC 133;A` landed on this row: a prompt (block boundary) starts here.
    pub const PROMPT_START: u8 = 1;
    /// An `OSC 133;C` landed on this row: command output begins here.
    pub const OUTPUT_START: u8 = 2;
    /// An `OSC 133;D` landed on this row: a block completed here (see
    /// [`RowMark::exit`]; it may still be `None` when `D` carried no
    /// parseable code).
    pub const BLOCK_END: u8 = 4;
    /// An `OSC 133;B` landed on this row: prompt end / command input begins.
    /// `prompt_end_col` holds the cursor column at the time of the event.
    pub const PROMPT_END: u8 = 8;
    /// **Runtime** fold flag (not from OSC 133): set on a block's prompt row to
    /// mean "this block's output is collapsed in the viewport." Lives here so it
    /// rides reflow (merge ORs flags) and eviction (the row drops) like the 133
    /// marks; masked off when persisted (folds are runtime-only).
    pub const FOLDED: u8 = 16;
    /// Private presence bit for `exit_code`.
    const HAS_EXIT: u8 = 1 << 7;

    /// Set a flag (one of the public associated consts). `|=`, so re-marking
    /// the same row is idempotent (shells redraw prompts).
    pub fn set(&mut self, flag: u8) {
        self.flags |= flag;
    }

    /// True when `flag` (one of the public associated consts) is set.
    pub fn contains(self, flag: u8) -> bool {
        self.flags & flag != 0
    }

    /// Record the prompt-end column from `OSC 133;B`. Sets the `PROMPT_END`
    /// flag and stores `col`; re-calling updates `col` (idempotent re-mark).
    pub fn set_prompt_end(&mut self, col: u16) {
        self.flags |= Self::PROMPT_END;
        self.prompt_end_col = col;
    }

    /// The cursor column recorded by `OSC 133;B`, if any. `None` when the
    /// `PROMPT_END` flag is unset (kept `0` in that case so `PartialEq` is
    /// well-defined).
    pub fn prompt_end_col(self) -> Option<u16> {
        if self.flags & Self::PROMPT_END != 0 {
            Some(self.prompt_end_col)
        } else {
            None
        }
    }

    /// Record (or clear) the exit code from `OSC 133;D;code`.
    pub fn set_exit(&mut self, exit: Option<i32>) {
        match exit {
            Some(code) => {
                self.flags |= Self::HAS_EXIT;
                self.exit_code = code;
            }
            None => {
                self.flags &= !Self::HAS_EXIT;
                self.exit_code = 0;
            }
        }
    }

    /// The exit code recorded by `OSC 133;D;code`, if any. `None` when `D`
    /// arrived without a parseable code (the row is still a block end via
    /// [`RowMark::BLOCK_END`]).
    pub fn exit(self) -> Option<i32> {
        if self.flags & Self::HAS_EXIT != 0 {
            Some(self.exit_code)
        } else {
            None
        }
    }

    /// True when the row carries no block annotation at all.
    pub fn is_empty(self) -> bool {
        self.flags == 0
    }

    /// Set or clear the runtime [`RowMark::FOLDED`] flag. Unlike [`RowMark::set`]
    /// (which only ORs), this can clear, so a block can be unfolded.
    pub fn set_folded(&mut self, folded: bool) {
        if folded {
            self.flags |= Self::FOLDED;
        } else {
            self.flags &= !Self::FOLDED;
        }
    }

    /// True when this row's block is folded (output collapsed).
    pub fn is_folded(self) -> bool {
        self.flags & Self::FOLDED != 0
    }

    /// Fold another row's mark into this one: flags are OR-ed; an exit code on
    /// `other` wins; when `other` carries `PROMPT_END`, its col wins too.
    /// Used by reflow when a logical line's physical rows are merged. Today
    /// at most one row of a line carries a mark (133 marks land at the cursor
    /// row), but a mark CAN land on a soft continuation row (cursor
    /// mid-wrapped-line when the OSC arrives), so merge defensively.
    /// "Other wins" is the natural order: callers merge first→last row, so a
    /// later mark supersedes an earlier one.
    pub fn merge(&mut self, other: RowMark) {
        self.flags |= other.flags;
        if other.flags & Self::HAS_EXIT != 0 {
            self.exit_code = other.exit_code;
        }
        if other.flags & Self::PROMPT_END != 0 {
            self.prompt_end_col = other.prompt_end_col;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub wrap_origin: WrapOrigin,
    /// OSC 133 block annotations for this row, if any.
    pub mark: RowMark,
}

impl Row {
    pub fn blank(cols: u16) -> Self {
        Self {
            cells: vec![Cell::default(); cols as usize],
            wrap_origin: WrapOrigin::Hard,
            mark: RowMark::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Grid {
    pub rows: Vec<Row>,
    pub cols: u16,
}

impl Grid {
    pub fn new(rows: u16, cols: u16) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        Self {
            rows: vec![Row::blank(cols); rows as usize],
            cols,
        }
    }

    pub fn num_rows(&self) -> u16 {
        self.rows.len() as u16
    }

    pub fn num_cols(&self) -> u16 {
        self.cols
    }

    pub fn put_cell(&mut self, row: u16, col: u16, cell: Cell) {
        if let Some(r) = self.rows.get_mut(row as usize)
            && let Some(c) = r.cells.get_mut(col as usize)
        {
            *c = cell;
        }
    }

    pub fn get_cell(&self, row: u16, col: u16) -> Option<&Cell> {
        self.rows
            .get(row as usize)
            .and_then(|r| r.cells.get(col as usize))
    }

    /// Reset every cell to default.
    pub fn clear(&mut self) {
        for r in self.rows.iter_mut() {
            for c in r.cells.iter_mut() {
                *c = Cell::default();
            }
            r.wrap_origin = WrapOrigin::Hard;
            // Full-grid clear (ED 2/3J, e.g. ctrl-L) erases the blocks from
            // view; stale marks on now-blank rows would read as phantom blocks.
            r.mark = RowMark::default();
        }
    }

    /// Clear an inclusive rectangle (clamped to grid).
    ///
    /// Deliberately leaves `Row.mark` alone: partial erases (EL, ED 0/1)
    /// blank cells on a line without unmaking the command block whose
    /// boundary that line is. Only a full-grid [`Grid::clear`] wipes marks.
    pub fn clear_rect(&mut self, start_row: u16, start_col: u16, end_row: u16, end_col: u16) {
        let end_row = end_row.min(self.num_rows().saturating_sub(1));
        let end_col = end_col.min(self.cols.saturating_sub(1));
        if start_row > end_row || start_col > end_col {
            return;
        }
        for r in start_row..=end_row {
            if let Some(row) = self.rows.get_mut(r as usize) {
                for c in start_col..=end_col {
                    if let Some(cell) = row.cells.get_mut(c as usize) {
                        *cell = Cell::default();
                    }
                }
            }
        }
    }

    /// Scroll a region [top, bottom] (inclusive) up by `n`. If `popped` is
    /// provided, rows that fall off the top are appended to it; otherwise
    /// discarded. New blank rows are inserted at the bottom of the region.
    pub fn scroll_up(&mut self, top: u16, bottom: u16, n: u16, mut popped: Option<&mut Vec<Row>>) {
        let top = top as usize;
        let bottom = (bottom as usize).min(self.rows.len().saturating_sub(1));
        if top > bottom {
            return;
        }
        let region = bottom - top + 1;
        let n = (n as usize).min(region);
        for _ in 0..n {
            let r = self.rows.remove(top);
            if let Some(p) = popped.as_deref_mut() {
                p.push(r);
            }
            self.rows.insert(bottom, Row::blank(self.cols));
        }
    }

    /// Scroll region [top, bottom] (inclusive) down by `n`. Bottom rows are
    /// discarded; blank rows inserted at the top.
    pub fn scroll_down(&mut self, top: u16, bottom: u16, n: u16) {
        let top = top as usize;
        let bottom = (bottom as usize).min(self.rows.len().saturating_sub(1));
        if top > bottom {
            return;
        }
        let region = bottom - top + 1;
        let n = (n as usize).min(region);
        for _ in 0..n {
            self.rows.remove(bottom);
            self.rows.insert(top, Row::blank(self.cols));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smol_str::SmolStr;

    fn x_cell() -> Cell {
        Cell {
            grapheme: SmolStr::new("X"),
            ..Cell::default()
        }
    }

    #[test]
    fn row_mark_stays_small() {
        // Rows are cloned per frame, so the annotation must stay cheap.
        assert!(std::mem::size_of::<RowMark>() <= 8);
    }

    #[test]
    fn row_mark_default_is_empty() {
        let m = RowMark::default();
        assert!(m.is_empty());
        assert!(!m.contains(RowMark::PROMPT_START));
        assert!(!m.contains(RowMark::OUTPUT_START));
        assert!(!m.contains(RowMark::BLOCK_END));
        assert!(!m.contains(RowMark::PROMPT_END));
        assert_eq!(m.exit(), None);
        // Col is None when PROMPT_END is not set.
        assert_eq!(m.prompt_end_col(), None);
    }

    #[test]
    fn folded_flag_set_clear_and_compare() {
        let mut m = RowMark::default();
        assert!(!m.is_folded());
        m.set_folded(true);
        assert!(m.is_folded());
        // A folded mark differs from an unfolded one (drives the render diff).
        assert_ne!(m, RowMark::default());
        m.set_folded(false);
        assert!(!m.is_folded());
        assert_eq!(m, RowMark::default());
    }

    #[test]
    fn merge_ors_the_folded_flag() {
        // reflow merges first→last; a folded row keeps its fold through a merge.
        let mut base = RowMark::default();
        base.set(RowMark::PROMPT_START);
        base.set_folded(true);
        let mut other = RowMark::default();
        other.set(RowMark::OUTPUT_START);
        base.merge(other);
        assert!(base.is_folded(), "merge ORs FOLDED");
        assert!(base.contains(RowMark::OUTPUT_START));
        // And a fold on `other` propagates too.
        let mut a = RowMark::default();
        a.set(RowMark::PROMPT_START);
        let mut b = RowMark::default();
        b.set_folded(true);
        a.merge(b);
        assert!(a.is_folded());
    }

    #[test]
    fn folded_flag_does_not_collide_with_133_bits() {
        let mut m = RowMark::default();
        m.set(RowMark::PROMPT_START);
        m.set_folded(true);
        // Folding leaves the OSC 133 bits and exit untouched.
        assert!(m.contains(RowMark::PROMPT_START));
        assert!(!m.contains(RowMark::OUTPUT_START));
        assert_eq!(m.exit(), None);
        m.set_folded(false);
        assert!(m.contains(RowMark::PROMPT_START), "unfold leaves 133 marks");
    }

    #[test]
    fn prompt_end_col_round_trip() {
        let mut m = RowMark::default();
        // Unset → None.
        assert_eq!(m.prompt_end_col(), None);
        // Set col 5.
        m.set_prompt_end(5);
        assert!(m.contains(RowMark::PROMPT_END));
        assert_eq!(m.prompt_end_col(), Some(5));
        // Re-set (idempotent re-mark) updates the col.
        m.set_prompt_end(10);
        assert_eq!(m.prompt_end_col(), Some(10));
    }

    #[test]
    fn prompt_end_col_zeroed_when_flag_unset() {
        // A default RowMark has prompt_end_col == None even though the field
        // stores 0 internally (the flag gates the accessor).
        let m = RowMark::default();
        assert_eq!(m.prompt_end_col(), None, "no flag → None even if internal 0");
        // Two marks with PROMPT_END should not equal one without, even at col 0.
        let mut with_flag = RowMark::default();
        with_flag.set_prompt_end(0);
        assert_ne!(m, with_flag, "flag bit distinguishes the two states");
    }

    #[test]
    fn merge_prompt_end_col_other_wins() {
        // Merge rule: when OTHER carries PROMPT_END, its col wins.
        let mut base = RowMark::default();
        base.set_prompt_end(3); // base has col 3

        let mut other = RowMark::default();
        other.set_prompt_end(7); // other has col 7

        base.merge(other);
        assert!(base.contains(RowMark::PROMPT_END));
        assert_eq!(base.prompt_end_col(), Some(7), "other's col must win on merge");
    }

    #[test]
    fn merge_prompt_end_col_only_self_has_flag() {
        // When only self has PROMPT_END, self's col is unchanged after merge.
        let mut base = RowMark::default();
        base.set_prompt_end(3);

        let other = RowMark::default(); // no PROMPT_END

        base.merge(other);
        assert!(base.contains(RowMark::PROMPT_END));
        assert_eq!(base.prompt_end_col(), Some(3), "self col preserved when other has no flag");
    }

    #[test]
    fn merge_flags_or_ed() {
        // Flags are OR-ed: both sides' marks survive.
        let mut a = RowMark::default();
        a.set(RowMark::PROMPT_START);
        a.set_prompt_end(1);

        let mut b = RowMark::default();
        b.set(RowMark::BLOCK_END);
        b.set_exit(Some(0));

        a.merge(b);
        assert!(a.contains(RowMark::PROMPT_START));
        assert!(a.contains(RowMark::BLOCK_END));
        assert!(a.contains(RowMark::PROMPT_END));
        assert_eq!(a.exit(), Some(0));
        assert_eq!(a.prompt_end_col(), Some(1));
    }

    #[test]
    fn row_mark_set_and_exit_round_trip() {
        let mut m = RowMark::default();
        m.set(RowMark::BLOCK_END);
        m.set_exit(Some(0));
        assert!(m.contains(RowMark::BLOCK_END));
        assert_eq!(m.exit(), Some(0));
        // Idempotent re-set.
        m.set(RowMark::BLOCK_END);
        assert_eq!(m.exit(), Some(0));
        m.set_exit(None);
        assert_eq!(m.exit(), None);
        assert!(m.contains(RowMark::BLOCK_END), "clearing exit keeps the flag");
    }

    #[test]
    fn blank_rows_are_markless() {
        assert!(Row::blank(4).mark.is_empty());
        let g = Grid::new(2, 2);
        assert!(g.rows.iter().all(|r| r.mark.is_empty()));
    }

    #[test]
    fn new_grid_has_blank_rows() {
        let g = Grid::new(3, 4);
        assert_eq!(g.num_rows(), 3);
        assert_eq!(g.num_cols(), 4);
        assert!(g.get_cell(0, 0).unwrap().is_blank());
    }

    #[test]
    fn put_cell_oob_is_noop() {
        let mut g = Grid::new(2, 2);
        g.put_cell(99, 99, x_cell());
        for r in 0..2 {
            for c in 0..2 {
                assert!(g.get_cell(r, c).unwrap().is_blank());
            }
        }
    }

    #[test]
    fn clear_resets_row_marks() {
        let mut g = Grid::new(2, 2);
        g.rows[0].mark.set(RowMark::PROMPT_START);
        g.rows[0].mark.set_exit(Some(0));
        g.clear();
        assert!(g.rows.iter().all(|r| r.mark.is_empty()));
    }

    #[test]
    fn clear_rect_keeps_row_marks() {
        let mut g = Grid::new(2, 2);
        g.rows[0].mark.set(RowMark::PROMPT_START);
        g.clear_rect(0, 0, 1, 1);
        assert!(g.rows[0].mark.contains(RowMark::PROMPT_START));
    }

    #[test]
    fn clear_rect_clears_inclusive_range() {
        let mut g = Grid::new(3, 3);
        for r in 0..3 {
            for c in 0..3 {
                g.put_cell(r, c, x_cell());
            }
        }
        g.clear_rect(1, 1, 2, 2);
        assert_eq!(g.get_cell(0, 0).unwrap(), &x_cell());
        assert!(g.get_cell(1, 1).unwrap().is_blank());
        assert!(g.get_cell(2, 2).unwrap().is_blank());
    }

    #[test]
    fn scroll_up_moves_rows_and_blanks_bottom() {
        let mut g = Grid::new(3, 1);
        g.put_cell(0, 0, x_cell());
        g.scroll_up(0, 2, 1, None);
        assert!(g.get_cell(0, 0).unwrap().is_blank());
        assert!(g.get_cell(2, 0).unwrap().is_blank());
    }

    #[test]
    fn scroll_up_collects_popped() {
        let mut g = Grid::new(3, 1);
        g.put_cell(0, 0, x_cell());
        let mut out = Vec::new();
        g.scroll_up(0, 2, 1, Some(&mut out));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].cells[0], x_cell());
    }

    #[test]
    fn scroll_down_blanks_top_discards_bottom() {
        let mut g = Grid::new(3, 1);
        g.put_cell(2, 0, x_cell());
        g.scroll_down(0, 2, 1);
        assert!(g.get_cell(0, 0).unwrap().is_blank());
        assert!(g.get_cell(2, 0).unwrap().is_blank());
    }
}
