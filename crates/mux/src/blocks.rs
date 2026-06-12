//! Pure OSC 133 command-block scans over a [`Screen`]'s unified line space.
//!
//! Lines are absolute: scrollback index `i` is line `i`; active grid row `r`
//! is line `scrollback.len() + r`. Marks live on the rows themselves
//! ([`Row::mark`]), so every scan is a straight row walk and there is no index
//! to maintain or corrupt.
//!
//! A **block** is the rows from one `PROMPT_START` (inclusive) to the next
//! `PROMPT_START` (exclusive), or to the last line. Its **output range** runs
//! from its first `OUTPUT_START` row (falling back to the prompt row when
//! `133;C` never arrived) through the block's last row.

use plexy_glass_emulator::{Row, RowMark, Screen};

/// Row at absolute `line` (scrollback rows first, then the active grid).
fn row_at(screen: &Screen, line: u32) -> Option<&Row> {
    let scrollback = screen.scrollback.rows();
    let scrollback_len = scrollback.len() as u32;
    if line < scrollback_len {
        scrollback.get(line as usize)
    } else {
        screen.active.rows.get((line - scrollback_len) as usize)
    }
}

/// Total lines in the unified space (scrollback + active grid).
fn total_lines(screen: &Screen) -> u32 {
    screen.scrollback.rows().len() as u32 + screen.active.rows.len() as u32
}

fn is_prompt(screen: &Screen, line: u32) -> bool {
    row_at(screen, line).is_some_and(|r| r.mark.contains(RowMark::PROMPT_START))
}

/// Nearest `PROMPT_START` line strictly above `from`, scanning the unified
/// scrollback + grid space. `None` when no prompt exists above.
pub fn prev_prompt_line(screen: &Screen, from: u32) -> Option<u32> {
    let upper = from.min(total_lines(screen));
    (0..upper).rev().find(|&l| is_prompt(screen, l))
}

/// Nearest `PROMPT_START` line strictly below `from`. `None` when no prompt
/// exists below.
pub fn next_prompt_line(screen: &Screen, from: u32) -> Option<u32> {
    (from.saturating_add(1)..total_lines(screen)).find(|&l| is_prompt(screen, l))
}

/// Output range `(start, end)` of the block containing `line`.
///
/// The block's prompt is the nearest `PROMPT_START` at or above `line`; the
/// block ends on the line before the next `PROMPT_START` (or the last line).
/// `start` is the block's first `OUTPUT_START` line, falling back to the
/// prompt line itself when `133;C` never arrived. `None` when no block
/// contains `line` (no `PROMPT_START` at or above it).
pub fn block_output_range(screen: &Screen, line: u32) -> Option<(u32, u32)> {
    let total = total_lines(screen);
    if total == 0 {
        return None;
    }
    let line = line.min(total - 1);
    let prompt = if is_prompt(screen, line) {
        line
    } else {
        prev_prompt_line(screen, line)?
    };
    let end = next_prompt_line(screen, prompt).map_or(total - 1, |next| next - 1);
    let start = (prompt..=end)
        .find(|&l| row_at(screen, l).is_some_and(|r| r.mark.contains(RowMark::OUTPUT_START)))
        .unwrap_or(prompt);
    Some((start, end))
}

/// Output range of the most recent **completed** block, the newest block
/// closed by a `BLOCK_END` (`133;D`) row. `None` when no completed block
/// survives (no `D` seen yet, or its block's prompt was evicted).
///
/// Attribution nuance: in the common shell flow the `D` for a finished
/// command and the `A` of the NEXT prompt land on the same row (the shell
/// emits `D`, then `A`, then redraws the prompt). A `BLOCK_END` on a
/// `PROMPT_START` row therefore closes the block ABOVE that row, not the
/// block the row starts.
pub fn last_completed_block(screen: &Screen) -> Option<(u32, u32)> {
    let end_mark = (0..total_lines(screen))
        .rev()
        .find(|&l| row_at(screen, l).is_some_and(|r| r.mark.contains(RowMark::BLOCK_END)))?;
    let line_in_block = if is_prompt(screen, end_mark) {
        end_mark.checked_sub(1)?
    } else {
        end_mark
    };
    block_output_range(screen, line_in_block)
}

/// Per-visible-row block exit status for a viewport slice.
///
/// Values:
/// - `Ok`: the row's block completed with exit code 0.
/// - `Failed`: completed with a nonzero exit code.
///
/// `None` covers everything else: row before the first prompt, running block
/// (no `BLOCK_END` yet), or a `BLOCK_END` without an exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockLineStatus {
    Ok,
    Failed,
}

/// Status per visible viewport row.
///
/// `top` is the absolute line shown at viewport row 0 (the compositor's
/// effective-scroll mapping). `rows` is the viewport height. The returned
/// vector has exactly `rows as usize` elements.
///
/// - Alt screen (`screen.alt.is_some()`) → all `None`.
/// - Rows at or past the total line count → `None`.
/// - Rows before the first `PROMPT_START` → `None`.
/// - The whole block (prompt row through the row before the next prompt, or
///   the last row) takes the block's status.
///
/// Attribution rule: a `BLOCK_END` (`133;D`) on a prompt's own row is
/// attributed to the block **above** (the common shell flow emits `D` then `A`
/// on the same row). That row takes the status of the block it *starts*, not
/// the block it closes.
pub fn viewport_block_status(screen: &Screen, top: u32, rows: u16) -> Vec<Option<BlockLineStatus>> {
    let n = rows as usize;
    let mut result = vec![None; n];

    // Alt screen: all-None.
    if screen.alt.is_some() {
        return result;
    }

    let total = total_lines(screen);
    if total == 0 || n == 0 {
        return result;
    }

    // Find the governing prompt for `top`: at or above it (top may itself be a
    // prompt). If none exists, search forward into the viewport.
    let governing_prompt: Option<u32> = if top < total && is_prompt(screen, top) {
        Some(top)
    } else if top < total {
        prev_prompt_line(screen, top)
    } else {
        None // top is at or beyond total; all None
    };

    let start_prompt: u32 = match governing_prompt {
        Some(p) => p,
        None => {
            // No prompt at or above top, so find the first prompt in viewport.
            match (top..total).find(|&l| is_prompt(screen, l)) {
                Some(p) => p,
                None => return result, // no prompts anywhere in viewport
            }
        }
    };

    // Walk forward through blocks, filling `result`.
    let mut prompt = start_prompt;
    loop {
        // Block spans [prompt .. block_end_incl].
        let next_p = next_prompt_line(screen, prompt);
        let block_end_incl: u32 = next_p.map_or(total - 1, |np| np - 1);

        // Search for BLOCK_END in (prompt, search_end]: includes the next
        // prompt row so a shared D+A row closes this block.
        let search_end = block_end_incl.saturating_add(1);
        let status: Option<BlockLineStatus> = {
            // Find first BLOCK_END in (prompt, search_end] (strictly after prompt)
            let d_row = (prompt + 1..=search_end)
                .find(|&l| l < total && row_at(screen, l).is_some_and(|r| r.mark.contains(RowMark::BLOCK_END)));
            match d_row {
                None => None, // no BLOCK_END → running
                Some(d) => {
                    let exit = row_at(screen, d).and_then(|r| r.mark.exit());
                    match exit {
                        Some(0) => Some(BlockLineStatus::Ok),
                        Some(_) => Some(BlockLineStatus::Failed),
                        None => None, // D without exit code → unknown
                    }
                }
            }
        };

        // Fill viewport rows that belong to this block.
        // Viewport row r corresponds to absolute line top + r.
        // Block occupies absolute [prompt .. block_end_incl].
        // Overlap with viewport [top .. top + n - 1].
        let vp_end = top.saturating_add(n as u32 - 1);
        let overlap_start = prompt.max(top);
        let overlap_end = block_end_incl.min(vp_end);
        if overlap_start <= overlap_end {
            let r_start = (overlap_start - top) as usize;
            let r_end = (overlap_end - top) as usize;
            for slot in result[r_start..=r_end.min(n - 1)].iter_mut() {
                *slot = status;
            }
        }

        // Advance to next block.
        match next_p {
            None => break,
            Some(np) => {
                // If the next prompt is beyond the viewport, we're done.
                if np >= top.saturating_add(n as u32) {
                    break;
                }
                prompt = np;
            }
        }
    }

    result
}

/// Returns `true` when the pane's active shell is waiting at a prompt and
/// ready to accept a new command.
///
/// The rule: find the newest `PROMPT_START` line anywhere in the unified
/// scrollback + active-grid space; the pane is at a prompt iff no
/// `OUTPUT_START` mark exists on a line **strictly after** (i.e. with a higher
/// index than) that newest-prompt line.
///
/// Returns `false` when no `PROMPT_START` has been seen at all.
///
/// **Alt screen is deliberately not checked here.** The caller
/// (`connection.rs` `ExecCommand` arm) must test `screen.alt.is_some()`
/// separately and refuse before calling this function, since the alt-screen
/// pane belongs to a full-screen application, not the shell prompt cycle.
///
/// # Accepted edges (documented in the spec)
///
/// - **A-without-C integrations** (shell emits `133;A` but never `133;C`):
///   the pane looks at-prompt even mid-command. This is a *fails-open* trade-
///   off: full integration (A+C+D) avoids it; the alternative (fails-closed)
///   would permanently refuse panes with prompt-only integration.
/// - **C on the newest-prompt row itself** (e.g. `\x1b]133;A\x07$ \x1b]133;C\x07`
///   with no newline between): returns `true` because the `OUTPUT_START` is
///   not *strictly after* the prompt line. Chosen deliberately, because the
///   inclusive alternative would treat a shared C+D+A row as permanently busy.
pub fn pane_at_prompt(screen: &Screen) -> bool {
    let total = total_lines(screen);
    if total == 0 {
        return false;
    }
    // Find the newest PROMPT_START by scanning backwards.
    let newest_prompt = match (0..total).rev().find(|&l| is_prompt(screen, l)) {
        Some(l) => l,
        None => return false,
    };
    // The pane is at a prompt iff no OUTPUT_START exists strictly after it.
    let has_output_after = (newest_prompt + 1..total).any(|l| {
        row_at(screen, l).is_some_and(|r| r.mark.contains(RowMark::OUTPUT_START))
    });
    !has_output_after
}

/// Render the absolute-line range `(start, end)` (inclusive, scrollback rows
/// included) as plain text: one line per row, trailing whitespace trimmed,
/// trailing blank lines dropped (a block that ends at the bottom of the grid
/// would otherwise carry the unused rows below the output).
/// Wide-spacer cells carry an empty grapheme, so `push_str("")` skips them
/// naturally, same rendering rule as [`crate::selection::screen_text`].
pub fn block_text(screen: &Screen, (start, end): (u32, u32)) -> String {
    let mut lines: Vec<String> = Vec::with_capacity((end.saturating_sub(start) + 1) as usize);
    for line in start..=end {
        let Some(row) = row_at(screen, line) else { continue };
        let mut text = String::new();
        for cell in &row.cells {
            text.push_str(cell.grapheme.as_str());
        }
        let trimmed = text.trim_end();
        lines.push(trimmed.to_string());
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexy_glass_emulator::Emulator;

    /// Feed raw bytes (text + OSC 133 sequences) through a real emulator.
    /// A trailing SGR-reset flushes the pending grapheme into the grid.
    fn screen_from(rows: u16, cols: u16, bytes: &[u8]) -> Screen {
        let mut e = Emulator::new(rows, cols);
        e.advance(bytes);
        e.advance(b"\x1b[m");
        e.screen().clone()
    }

    /// 8-row grid, two blocks, D+A sharing line 3 (the common shell flow):
    ///   0: A "$ one"   1: C "out1"   2: "out2"
    ///   3: D;0 + A "$ two"           4: C "out3"   (5..7 blank)
    fn two_blocks() -> Screen {
        screen_from(
            8,
            20,
            b"\x1b]133;A\x07$ one\r\n\
              \x1b]133;C\x07out1\r\n\
              out2\r\n\
              \x1b]133;D;0\x07\x1b]133;A\x07$ two\r\n\
              \x1b]133;C\x07out3",
        )
    }

    /// 3-row grid fed 6 lines, so lines 0..2 live in scrollback.
    /// Prompts at absolute lines 0 (scrollback) and 4 (grid).
    fn across_boundary() -> Screen {
        let s = screen_from(
            3,
            20,
            b"\x1b]133;A\x07p1\r\no1\r\no2\r\no3\r\n\x1b]133;A\x07p2\r\nx",
        );
        assert_eq!(s.scrollback.rows().len(), 3, "setup: 3 rows scrolled");
        s
    }

    #[test]
    fn prev_prompt_finds_nearest_above() {
        let s = two_blocks();
        assert_eq!(prev_prompt_line(&s, 7), Some(3));
        assert_eq!(prev_prompt_line(&s, 3), Some(0));
        assert_eq!(prev_prompt_line(&s, 1), Some(0));
    }

    #[test]
    fn prev_prompt_is_strictly_above_and_none_at_oldest() {
        let s = two_blocks();
        // Line 0 IS a prompt; strictly-above finds nothing.
        assert_eq!(prev_prompt_line(&s, 0), None);
    }

    #[test]
    fn prev_prompt_from_beyond_total_scans_everything() {
        let s = two_blocks();
        assert_eq!(prev_prompt_line(&s, 1000), Some(3));
    }

    #[test]
    fn next_prompt_finds_nearest_below() {
        let s = two_blocks();
        assert_eq!(next_prompt_line(&s, 0), Some(3));
        assert_eq!(next_prompt_line(&s, 2), Some(3));
    }

    #[test]
    fn next_prompt_none_at_newest() {
        let s = two_blocks();
        assert_eq!(next_prompt_line(&s, 3), None);
        assert_eq!(next_prompt_line(&s, 7), None);
    }

    #[test]
    fn prompt_scan_crosses_scrollback_boundary() {
        let s = across_boundary();
        assert_eq!(prev_prompt_line(&s, 5), Some(4));
        assert_eq!(prev_prompt_line(&s, 4), Some(0), "prompt in scrollback");
        assert_eq!(next_prompt_line(&s, 0), Some(4), "prompt in grid");
    }

    #[test]
    fn block_output_range_uses_output_start() {
        let s = two_blocks();
        // Block 1 = lines 0..=2; C at line 1.
        assert_eq!(block_output_range(&s, 0), Some((1, 2)));
        assert_eq!(block_output_range(&s, 1), Some((1, 2)));
        assert_eq!(block_output_range(&s, 2), Some((1, 2)));
        // Block 2 = lines 3..=7 (last line); C at line 4.
        assert_eq!(block_output_range(&s, 3), Some((4, 7)));
        assert_eq!(block_output_range(&s, 6), Some((4, 7)));
    }

    #[test]
    fn block_output_range_falls_back_to_prompt_line() {
        // No 133;C anywhere: output start = the prompt line itself.
        let s = screen_from(6, 20, b"\x1b]133;A\x07$ a\r\nout\r\n\x1b]133;A\x07$ b");
        assert_eq!(block_output_range(&s, 1), Some((0, 1)));
    }

    #[test]
    fn block_output_range_none_above_first_prompt() {
        // Line 0 has no prompt at or above it.
        let s = screen_from(6, 20, b"plain\r\n\x1b]133;A\x07$ a");
        assert_eq!(block_output_range(&s, 0), None);
    }

    #[test]
    fn block_output_range_none_without_any_prompt() {
        let s = screen_from(4, 20, b"just\r\ntext");
        assert_eq!(block_output_range(&s, 1), None);
    }

    #[test]
    fn block_output_range_spans_scrollback_boundary() {
        let s = across_boundary();
        // Block 1 = lines 0..=3 (no C → start at the prompt line 0).
        assert_eq!(block_output_range(&s, 2), Some((0, 3)));
    }

    #[test]
    fn last_completed_block_attributes_shared_d_a_row_to_block_above() {
        let s = two_blocks();
        // D landed on line 3 together with block 2's A; it closes block 1.
        assert_eq!(last_completed_block(&s), Some((1, 2)));
    }

    #[test]
    fn last_completed_block_with_d_on_its_own_row() {
        // A line0, out line1, D alone line2, A line3.
        let s = screen_from(
            6,
            20,
            b"\x1b]133;A\x07$ a\r\nout\r\n\x1b]133;D;0\x07done\r\n\x1b]133;A\x07$ b",
        );
        // Block = 0..=2, no C → start falls back to the prompt line.
        assert_eq!(last_completed_block(&s), Some((0, 2)));
    }

    #[test]
    fn last_completed_block_none_without_block_end() {
        let s = screen_from(6, 20, b"\x1b]133;A\x07$ a\r\nrunning");
        assert_eq!(last_completed_block(&s), None);
    }

    #[test]
    fn block_text_renders_output_rows_trimmed() {
        let s = two_blocks();
        // invariant: two_blocks always has a completed block (D on line 3)
        let range = last_completed_block(&s).expect("completed block");
        assert_eq!(block_text(&s, range), "out1\nout2");
    }

    #[test]
    fn block_text_drops_trailing_blank_lines() {
        let s = two_blocks();
        // Block 2's range runs to the last grid line (4..=7); lines 5..7 are
        // the unused rows below "out3" and must not appear in the text.
        assert_eq!(block_output_range(&s, 4), Some((4, 7)));
        assert_eq!(block_text(&s, (4, 7)), "out3");
    }

    #[test]
    fn block_text_keeps_interior_blank_lines() {
        // Output with a blank line in the middle: only TRAILING blanks drop.
        let s = screen_from(
            8,
            20,
            b"\x1b]133;A\x07$ a\r\n\x1b]133;C\x07one\r\n\r\ntwo\r\n\x1b]133;A\x07$ b",
        );
        assert_eq!(block_text(&s, (1, 3)), "one\n\ntwo");
    }

    #[test]
    fn block_text_spans_the_scrollback_boundary() {
        let s = across_boundary();
        // Block 1 = lines 0..=3: line 0 in scrollback, line 3 in the grid.
        assert_eq!(block_text(&s, (0, 3)), "p1\no1\no2\no3");
    }

    #[test]
    fn block_text_emits_wide_graphemes_once() {
        let s = screen_from(4, 20, b"\x1b]133;A\x07$ a\r\n\x1b]133;C\x07\xe4\xb8\xad x");
        // 中 occupies two cells (grapheme + spacer); it must appear once.
        assert_eq!(block_text(&s, (1, 1)), "中 x");
    }

    #[test]
    fn last_completed_block_none_when_prompt_evicted() {
        // D + A on line 0 with no surviving prompt above: the closed block
        // is gone, so there's nothing to return.
        let s = screen_from(4, 20, b"\x1b]133;D;0\x07\x1b]133;A\x07$ a");
        assert_eq!(last_completed_block(&s), None);
    }

    // ── viewport_block_status tests ──────────────────────────────────────────

    /// Ok block: whole block (lines 0..=2) → Ok; line 3 (next block, running)
    /// → None. Uses `two_blocks()` which has D;0 on line 3 for block 1.
    #[test]
    fn vbs_ok_block() {
        // two_blocks: 8 rows
        //   0: A "$ one"   (prompt)
        //   1: C "out1"
        //   2: "out2"
        //   3: D;0 + A "$ two"   ← closes block 1 (Ok), starts block 2
        //   4: C "out3"
        //   5..7: blank (block 2 still running)
        let s = two_blocks();
        let status = viewport_block_status(&s, 0, 8);
        assert_eq!(status.len(), 8);
        // Block 1 (lines 0..=2): Ok
        assert_eq!(status[0], Some(BlockLineStatus::Ok), "prompt row of ok block");
        assert_eq!(status[1], Some(BlockLineStatus::Ok), "output row 1 of ok block");
        assert_eq!(status[2], Some(BlockLineStatus::Ok), "output row 2 of ok block");
        // Line 3: D;0+A, the next block's prompt row (block 2 running) → None
        assert_eq!(status[3], None, "shared D+A row shows NEXT block status (running)");
        // Block 2 (lines 4..=7): running → None
        assert_eq!(status[4], None, "output row of running block");
        assert_eq!(status[7], None, "last row of running block");
    }

    /// Failed block: D;1 closes block, whole block → Failed.
    #[test]
    fn vbs_failed_block() {
        // lines: 0=A, 1=C out, 2=D;1+A (next block, running)
        let s = screen_from(
            6,
            20,
            b"\x1b]133;A\x07$ fail\r\n\
              \x1b]133;C\x07error\r\n\
              \x1b]133;D;1\x07\x1b]133;A\x07$ next",
        );
        // Block 1: lines 0..=1 → Failed (D;1 on line 2 closes block 1)
        // Block 2: lines 2..=5 → None (running)
        let status = viewport_block_status(&s, 0, 6);
        assert_eq!(status.len(), 6);
        assert_eq!(status[0], Some(BlockLineStatus::Failed), "prompt row of failed block");
        assert_eq!(status[1], Some(BlockLineStatus::Failed), "output row of failed block");
        // Line 2 is the next block's prompt row (block 2, running) → None
        assert_eq!(status[2], None, "shared D+A row shows next block (running)");
        assert_eq!(status[3], None, "block 2 running");
    }

    /// Running block (A+C, no D): all None.
    #[test]
    fn vbs_running_block_all_none() {
        let s = screen_from(
            4,
            20,
            b"\x1b]133;A\x07$ run\r\n\x1b]133;C\x07working",
        );
        let status = viewport_block_status(&s, 0, 4);
        assert!(status.iter().all(|s| s.is_none()), "running block → all None");
    }

    /// D without exit code → None (completed but unknown).
    #[test]
    fn vbs_d_without_exit_code_is_none() {
        // 133;D without payload: BLOCK_END set, exit code absent.
        let s = screen_from(
            6,
            20,
            b"\x1b]133;A\x07$ cmd\r\n\
              \x1b]133;C\x07out\r\n\
              \x1b]133;D\x07\x1b]133;A\x07$ next",
        );
        let status = viewport_block_status(&s, 0, 6);
        // Block 1 (lines 0..=1): D on line 2 without code → None
        assert_eq!(status[0], None, "D without code → None for prompt row");
        assert_eq!(status[1], None, "D without code → None for output row");
    }

    /// Whole-block extent: every row of the block including the prompt row
    /// gets the status.
    #[test]
    fn vbs_whole_block_extent() {
        // Block 1: lines 0..=4 all → Ok (D;0 on line 5, which also has A for
        // block 2)
        let s = screen_from(
            8,
            20,
            b"\x1b]133;A\x07$ cmd\r\n\
              \x1b]133;C\x07line1\r\n\
              line2\r\n\
              line3\r\n\
              line4\r\n\
              \x1b]133;D;0\x07\x1b]133;A\x07$ next",
        );
        // Block 1: lines 0..=4 (D;0 on line 5 closes it)
        // Block 2: lines 5..=7 (running)
        let status = viewport_block_status(&s, 0, 8);
        for (i, s) in status.iter().enumerate().take(5) {
            assert_eq!(*s, Some(BlockLineStatus::Ok), "line {i} of ok block");
        }
        // Line 5 is block 2's prompt row (running) → None
        assert_eq!(status[5], None, "block 2 prompt row (running)");
    }

    /// Shared D+A row shows NEXT block's status, not the closed block's.
    #[test]
    fn vbs_shared_da_row_shows_next_block_status() {
        // two_blocks: line 3 has D;0+A. Block 1 is Ok (lines 0..=2). Block 2
        // starts on line 3 and is still running → line 3 → None.
        let s = two_blocks();
        let status = viewport_block_status(&s, 0, 8);
        assert_eq!(status[3], None, "shared D+A row belongs to next block (running)");
        // Block 1 rows (0..=2) should be Ok.
        assert_eq!(status[0], Some(BlockLineStatus::Ok));
        assert_eq!(status[1], Some(BlockLineStatus::Ok));
        assert_eq!(status[2], Some(BlockLineStatus::Ok));
    }

    /// Shared D+A row where block 2 also completes: the D+A row takes block
    /// 2's status.
    #[test]
    fn vbs_shared_da_row_takes_next_block_status_when_complete() {
        // Block 1 (lines 0..=2): D;0 on line 3, Ok
        // Block 2 (lines 3..=4): D;1 on line 5, Failed
        // Block 3 (lines 5..=7): running
        let s = screen_from(
            8,
            20,
            b"\x1b]133;A\x07$ one\r\n\
              \x1b]133;C\x07a\r\n\
              b\r\n\
              \x1b]133;D;0\x07\x1b]133;A\x07$ two\r\n\
              \x1b]133;C\x07c\r\n\
              \x1b]133;D;1\x07\x1b]133;A\x07$ three",
        );
        let status = viewport_block_status(&s, 0, 8);
        // Block 1: lines 0..=2 → Ok
        assert_eq!(status[0], Some(BlockLineStatus::Ok));
        assert_eq!(status[2], Some(BlockLineStatus::Ok));
        // Line 3: block 2 prompt row, block 2 is Failed → Failed
        assert_eq!(status[3], Some(BlockLineStatus::Failed), "block 2 prompt row takes block 2 status");
        // Line 4: block 2 output → Failed
        assert_eq!(status[4], Some(BlockLineStatus::Failed));
        // Line 5: block 3 prompt row (running) → None
        assert_eq!(status[5], None, "block 3 prompt row (running)");
    }

    /// Scrolled `top`: slice mid-block.
    #[test]
    fn vbs_scrolled_top_mid_block() {
        // two_blocks: 8 rows. Block 1 = lines 0..=2 (Ok). View from top=1.
        let s = two_blocks();
        // Viewport: lines 1..=4 (4 rows)
        let status = viewport_block_status(&s, 1, 4);
        assert_eq!(status.len(), 4);
        // row 0 → line 1 → block 1 → Ok
        assert_eq!(status[0], Some(BlockLineStatus::Ok), "mid-block row at line 1");
        // row 1 → line 2 → block 1 → Ok
        assert_eq!(status[1], Some(BlockLineStatus::Ok), "mid-block row at line 2");
        // row 2 → line 3 → block 2 prompt (running) → None
        assert_eq!(status[2], None, "block 2 prompt row");
        // row 3 → line 4 → block 2 output (running) → None
        assert_eq!(status[3], None, "block 2 output row");
    }

    /// Rows past end → None.
    #[test]
    fn vbs_rows_past_end_are_none() {
        let s = screen_from(
            4,
            20,
            b"\x1b]133;A\x07$ cmd\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ b",
        );
        // Request more rows than total lines (4).
        let status = viewport_block_status(&s, 0, 10);
        assert_eq!(status.len(), 10);
        // Rows at/past total → None
        for (i, s) in status.iter().enumerate().skip(4) {
            assert_eq!(*s, None, "row {i} past end → None");
        }
    }

    /// Alt screen → all None.
    #[test]
    fn vbs_alt_screen_all_none() {
        // Enter alt screen with \x1b[?1049h, then some content.
        let s = screen_from(
            4,
            20,
            b"\x1b]133;A\x07$ cmd\r\n\
              \x1b]133;D;0\x07done\r\n\
              \x1b[?1049h\
              \x1b]133;A\x07$ alt",
        );
        // Alt screen is active → all None regardless.
        let status = viewport_block_status(&s, 0, 4);
        assert!(status.iter().all(|s| s.is_none()), "alt screen → all None");
    }

    /// Rows before the first prompt → None.
    #[test]
    fn vbs_before_first_prompt_is_none() {
        // Lines 0..1 are plain text; prompt at line 2 onwards.
        let s = screen_from(
            6,
            20,
            b"plain1\r\nplain2\r\n\x1b]133;A\x07$ cmd\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ b",
        );
        let status = viewport_block_status(&s, 0, 6);
        // Lines 0..1: before first prompt → None
        assert_eq!(status[0], None, "line before first prompt");
        assert_eq!(status[1], None, "line before first prompt");
        // Line 2 is block 1's prompt, closed by D;0 on line 3 → Ok
        assert_eq!(status[2], Some(BlockLineStatus::Ok), "prompt row of ok block");
    }

    /// `top` beyond all marks → `None`.
    #[test]
    fn vbs_top_beyond_all_marks() {
        let s = two_blocks();
        // top = 1000 (beyond total_lines = 8)
        let status = viewport_block_status(&s, 1000, 4);
        assert!(status.iter().all(|s| s.is_none()), "top past total → all None");
    }

    /// D on the prompt's own row (D;0 + A on line 0, no surviving prior block):
    /// that D is attributed to the evicted block above, so this block has no
    /// BLOCK_END → running → all None.
    #[test]
    fn vbs_block_end_on_own_prompt_row_belongs_to_block_above() {
        // D;0 + A on line 0, but the D is from a previous (evicted) block, not this
        // one. Block 1 (prompt at line 0) has no D strictly after it → running → None.
        let s = screen_from(4, 20, b"\x1b]133;D;0\x07\x1b]133;A\x07$ a\r\nrunning");
        let status = viewport_block_status(&s, 0, 4);
        assert!(status.iter().all(|s| s.is_none()),
            "D on prompt's own row excluded from that block → all None");
    }

    /// `top` is exactly at `total_lines` → all `None`.
    #[test]
    fn vbs_top_at_total_lines() {
        let s = screen_from(4, 20, b"\x1b]133;A\x07$ cmd");
        // total_lines = 4; top = 4 → at end → all None
        let status = viewport_block_status(&s, 4, 4);
        assert!(status.iter().all(|s| s.is_none()), "top at total_lines → all None");
    }

    // ── pane_at_prompt tests ─────────────────────────────────────────────────

    /// Fresh prompt: A only, no C anywhere → at prompt → true.
    #[test]
    fn pap_fresh_prompt_a_only() {
        let s = screen_from(4, 20, b"\x1b]133;A\x07$ ");
        assert!(pane_at_prompt(&s), "fresh A-only prompt → true");
    }

    /// No prompts at all → false.
    #[test]
    fn pap_no_prompts_at_all() {
        let s = screen_from(4, 20, b"just some text");
        assert!(!pane_at_prompt(&s), "no prompts → false");
    }

    /// A then C (command running, no D yet) → false.
    #[test]
    fn pap_running_a_then_c() {
        let s = screen_from(4, 20, b"\x1b]133;A\x07$ cmd\r\n\x1b]133;C\x07output");
        assert!(!pane_at_prompt(&s), "A then C (running) → false");
    }

    /// Full cycle A, C, D, A, a completed block then a fresh prompt → true.
    #[test]
    fn pap_full_cycle_a_c_d_a() {
        // D on line 2, A (new prompt) on line 3: newest A is at line 3, no C after it.
        let s = screen_from(
            6,
            20,
            b"\x1b]133;A\x07$ first\r\n\
              \x1b]133;C\x07output\r\n\
              \x1b]133;D;0\x07\x1b]133;A\x07$ ",
        );
        assert!(pane_at_prompt(&s), "A,C,D,A full cycle → true");
    }

    /// Shared D+A row newest (A…C…D+A flow) → true.
    /// The newest PROMPT_START is on the D+A row; no C exists after it.
    #[test]
    fn pap_shared_da_row_newest() {
        // two_blocks: line 3 = D;0+A (block 2's prompt), line 4 = C (block 2 running)
        // But we want the state JUST after the D+A row, before the C.
        // Build: A line0, C line1, out line2, D+A line3 (no further C).
        let s = screen_from(
            6,
            20,
            b"\x1b]133;A\x07$ one\r\n\
              \x1b]133;C\x07out1\r\n\
              out2\r\n\
              \x1b]133;D;0\x07\x1b]133;A\x07$ two",
        );
        // Newest A is on line 3 (D+A). No C after line 3 → at prompt.
        assert!(pane_at_prompt(&s), "shared D+A row newest, no C after → true");
    }

    /// C on the SAME ROW as the newest A (no newline between A and C) → true.
    /// This pins the documented "fails-open" edge: strictly-after means
    /// C on the same row as A does NOT count as output-started.
    #[test]
    fn pap_c_on_same_row_as_newest_a() {
        // A and C on the same row, no newline → C is on the newest A's row, not strictly after.
        let s = screen_from(4, 20, b"\x1b]133;A\x07$ \x1b]133;C\x07x");
        assert!(
            pane_at_prompt(&s),
            "C on same row as newest A → true (fails-open edge)"
        );
    }
}
