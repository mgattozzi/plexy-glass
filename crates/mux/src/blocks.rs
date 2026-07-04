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

use plexy_glass_emulator::{Row, RowMark, Screen, WrapOrigin};

/// Row at absolute `line` (scrollback rows first, then the active grid).
pub(crate) fn row_at(screen: &Screen, line: u32) -> Option<&Row> {
    let scrollback = screen.scrollback.rows();
    let scrollback_len = scrollback.len() as u32;
    if line < scrollback_len {
        scrollback.get(line as usize)
    } else {
        screen.active.rows.get((line - scrollback_len) as usize)
    }
}

/// Total lines in the unified space (scrollback + active grid).
pub(crate) fn total_lines(screen: &Screen) -> u32 {
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
    // Equivalent note (60:31 `- → +` and `- → /`): clamping only differs for
    // out-of-bounds `line`; prev_prompt_line scans the same range and finds
    // the same governing prompt regardless.
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

/// First `PROMPT_START` line at or after line 0, the oldest block's prompt.
/// `None` when no prompt exists anywhere.
pub fn first_prompt_line(screen: &Screen) -> Option<u32> {
    (0..total_lines(screen)).find(|&l| is_prompt(screen, l))
}

/// Newest `PROMPT_START` line, the last block's prompt. `None` when no prompt
/// exists anywhere.
pub fn last_prompt_line(screen: &Screen) -> Option<u32> {
    prev_prompt_line(screen, total_lines(screen))
}

/// Every `PROMPT_START` line, ascending: the full block set in display order.
pub fn all_prompt_lines(screen: &Screen) -> Vec<u32> {
    (0..total_lines(screen))
        .filter(|&l| is_prompt(screen, l))
        .collect()
}

/// The governing `PROMPT_START` at or above `line` (the block that contains
/// `line`). `None` when no prompt exists at or above `line`.
pub fn prompt_at_or_above(screen: &Screen, line: u32) -> Option<u32> {
    let total = total_lines(screen);
    if total == 0 {
        return None;
    }
    // Equivalent note (100:31 `- → +` and `- → /`): same as block_output_range
    // line 60, the clamp only differs for out-of-bounds input; prev_prompt_line
    // then scans 0..total and finds the same result.
    let line = line.min(total - 1);
    if is_prompt(screen, line) {
        Some(line)
    } else {
        prev_prompt_line(screen, line)
    }
}

/// Whole-block line range `(prompt_line, end)` (inclusive): the prompt row
/// through the row before the next prompt (or the last line). Unlike
/// [`block_output_range`], `start` is always the prompt row, so this is the
/// extent the block-mode bracket spans and the "whole block" yank renders.
pub fn block_extent(screen: &Screen, prompt_line: u32) -> (u32, u32) {
    let total = total_lines(screen);
    let end =
        next_prompt_line(screen, prompt_line).map_or_else(|| total.saturating_sub(1), |n| n - 1);
    (prompt_line, end)
}

/// First `BLOCK_END` strictly after `prompt_line`, up to and including the
/// next prompt row (attribution rule: a `BLOCK_END` on the NEXT block's
/// `PROMPT_START` row still closes this block). Returns `None` when no such
/// row exists (running block or a `D` that would be attributed to a different
/// block).
///
/// This is the canonical location of the attribution rule, and
/// `viewport_block_status` and `last_completed_block` / `closing_exit` all
/// delegate to it.
fn closing_block_end_line(screen: &Screen, prompt_line: u32) -> Option<u32> {
    let total = total_lines(screen);
    let next_p = next_prompt_line(screen, prompt_line);
    // Search range: (prompt_line, next_p], including the next prompt row so a
    // shared D+A row still counts.
    let last = total.saturating_sub(1);
    let search_end = next_p.map_or(last, |np| np.min(last));
    (prompt_line + 1..=search_end)
        .find(|&l| row_at(screen, l).is_some_and(|r| r.mark.contains(RowMark::BLOCK_END)))
}

/// Prompt line of the most recent **completed** block (internal helper).
///
/// The newest `BLOCK_END` row is located; if it falls on a `PROMPT_START` row
/// (shared D+A), the block above is the one being closed (attributed above).
/// Returns the governing `PROMPT_START` line for that block, or `None` when no
/// completed block survives (no `D` seen, or its prompt was evicted).
pub fn last_completed_prompt(screen: &Screen) -> Option<u32> {
    let end_mark = (0..total_lines(screen))
        .rev()
        .find(|&l| row_at(screen, l).is_some_and(|r| r.mark.contains(RowMark::BLOCK_END)))?;
    // Attribution: D on a PROMPT_START row closes the block ABOVE it.
    let line_in_block = if is_prompt(screen, end_mark) {
        end_mark.checked_sub(1)?
    } else {
        end_mark
    };
    // Find the governing PROMPT_START at or above `line_in_block`.
    if is_prompt(screen, line_in_block) {
        Some(line_in_block)
    } else {
        prev_prompt_line(screen, line_in_block)
    }
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

/// Exit code of the `BLOCK_END` (`133;D`) that closes the block anchored at
/// `prompt_line`, or `None` when the block is still running or its `D` row
/// carried no parseable code.
///
/// Uses `closing_block_end_line` for attribution so that the exit always
/// matches the specific block identified by `prompt_line`, which diverges from
/// `Screen::last_block_exit` when the newest block's rows have been evicted
/// from scrollback.
pub fn closing_exit(screen: &Screen, prompt_line: u32) -> Option<i32> {
    closing_block_end_line(screen, prompt_line).and_then(|d| row_at(screen, d)?.mark.exit())
}

/// True when `line` is the row a block's typed command sits on: the `OSC 133;B`
/// (prompt-end) row, or (for shells that emit no `B`) the `OSC 133;A`
/// (prompt-start) row itself. This is where command-row annotations (duration /
/// fold summary) anchor, so they land on the command line rather than the top of
/// a multi-line prompt (e.g. starship's two-line prompt puts `A` a row above `B`).
pub fn is_command_row(screen: &Screen, line: u32) -> bool {
    let Some(mark) = row_at(screen, line).map(|r| r.mark) else {
        return false;
    };
    if mark.contains(RowMark::PROMPT_END) {
        return true;
    }
    if !mark.contains(RowMark::PROMPT_START) {
        return false;
    }
    // A prompt-start row is the command row only when its block has no `B` row.
    let end = next_prompt_line(screen, line).unwrap_or_else(|| total_lines(screen));
    !(line..end).any(|l| row_at(screen, l).is_some_and(|r| r.mark.contains(RowMark::PROMPT_END)))
}

/// Wall-clock duration (millis) of the block anchored at `prompt_line`, if its
/// closing `OSC 133;D` recorded one. `None` when the block is unclosed, the row
/// was evicted, or `C` never preceded `D`. Mirrors [`closing_exit`].
pub fn closing_duration(screen: &Screen, prompt_line: u32) -> Option<u32> {
    closing_block_end_line(screen, prompt_line).and_then(|d| row_at(screen, d)?.mark.duration_ms())
}

/// Lowercased "command\noutput" for the block at `prompt_line`, output soft-capped
/// to ~`cap` bytes, the history-palette search haystack. Output is the block's
/// rows from output-start through block end (the same region `o`/copy-output use).
pub fn block_search_text(screen: &Screen, prompt_line: u32, cap: usize) -> String {
    let mut s = block_command_line(screen, prompt_line).unwrap_or_default();
    if let Some((start, end)) = block_output_range(screen, prompt_line) {
        let mut out = String::new();
        'rows: for line in start..=end {
            let Some(row) = row_at(screen, line) else {
                continue;
            };
            for cell in &row.cells {
                out.push_str(cell.grapheme.as_str());
                if out.len() >= cap {
                    break 'rows;
                }
            }
            out.push('\n');
            if out.len() >= cap {
                break;
            }
        }
        // Do NOT `out.truncate(cap)`: the per-grapheme break already soft-bounds
        // `out` (overshoot ≤ one grapheme), and a raw byte truncate at `cap`
        // panics when it lands mid-grapheme (CJK / emoji / accented).
        s.push('\n');
        s.push_str(&out);
    }
    s.to_lowercase()
}

/// Among blocks whose command line equals `command`, the prompt line minimizing
/// `|line - near|`. `None` if none match. Disambiguates a repeated command at
/// jump time when scrollback has drifted since the palette was built.
pub fn find_block_by_command(screen: &Screen, command: &str, near: u32) -> Option<u32> {
    all_prompt_lines(screen)
        .into_iter()
        .filter(|&l| block_command_line(screen, l).as_deref() == Some(command))
        .min_by_key(|&l| l.abs_diff(near))
}

/// Human-compact duration: `340ms` / `2.3s` / `45s` / `2m05s`.
pub fn format_duration(ms: u32) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 9_950 {
        // Tenths, e.g. `2.3s`. Capped below 9.95s so `{:.1}` rounding can never
        // produce `10.0s` (the whole-seconds branch renders that as `10s`).
        format!("{:.1}s", f64::from(ms) / 1_000.0)
    } else {
        // Whole seconds, rounded (saturating_add avoids debug overflow at u32::MAX).
        let secs = ms.saturating_add(500) / 1_000;
        if secs < 60 {
            format!("{secs}s")
        } else {
            format!("{}m{:02}s", secs / 60, secs % 60)
        }
    }
}

/// Text of the command line typed at `prompt_line`.
///
/// The command line is the text between the prompt-end mark (`OSC 133;B`) and
/// the output-start mark (`OSC 133;C`) for the block anchored at `prompt_line`.
///
/// Returns `None` when:
/// - The block has no `PROMPT_END` row (no `133;B` emitted).
/// - The block has no `OUTPUT_START` row (no `133;C` emitted).
/// - The `PROMPT_END` and `OUTPUT_START` rows are the same (command and output
///   on the same physical row, indistinguishable; honest null beats a guess).
/// - The extracted text is empty after trimming.
///
/// **Wrap-aware join**: a row whose SUCCESSOR carries
/// `WrapOrigin::SoftFrom(_)` was broken by the terminal at the right margin
/// (the user typed one long line). Such rows are joined WITHOUT a `\n`. Hard
/// row boundaries (the successor is `WrapOrigin::Hard`) join WITH `\n`,
/// representing a real newline the user pressed (e.g. a here-doc continuation).
///
/// **Cell-index == display-column invariant**: each cell in `row.cells`
/// occupies exactly one display column (wide characters are stored as a
/// grapheme cell followed by a `Cell::wide_spacer()` in the next column).
/// `PROMPT_END`'s col is therefore a direct cell-vector index, and slicing at
/// that index gives the cells from the command-start column onward.
///
/// Trailing whitespace is trimmed per physical row before joining. A typed
/// space at a soft-wrap boundary may be lost, which is rare and documented.
pub fn block_command_line(screen: &Screen, prompt_line: u32) -> Option<String> {
    // Use block_output_range only for the block boundary (block_end), then find
    // the true OUTPUT_START row (C) ourselves with a mark scan. block_output_range
    // falls back to the prompt line when no C exists, so we can't trust the start
    // it returns.
    let (_fallback_start, block_end) = block_output_range(screen, prompt_line)?;

    // Find the OUTPUT_START row (C): first row with the flag at-or-after
    // prompt_line within the block. Returns None when no C mark exists.
    let c_row = (prompt_line..=block_end)
        .find(|&l| row_at(screen, l).is_some_and(|r| r.mark.contains(RowMark::OUTPUT_START)))?;

    // Find the PROMPT_END row (B): first row at-or-after prompt_line with the
    // flag, strictly before the C row. The range excludes c_row, so b_row < c_row.
    let b_row = (prompt_line..c_row)
        .find(|&l| row_at(screen, l).is_some_and(|r| r.mark.contains(RowMark::PROMPT_END)))?;

    // B col: the cell index at which command text begins on the B row.
    // Cell index == display column (invariant: each cell occupies one column,
    // wide chars are grapheme cell + spacer cell, so `cells[col]` == column col).
    let b_col = row_at(screen, b_row)
        .and_then(|r| r.mark.prompt_end_col())
        .unwrap_or(0) as usize;

    // Collect rows from `b_row` through `c_row - 1`.
    let mut parts: Vec<String> = Vec::new();
    for line in b_row..c_row {
        let Some(row) = row_at(screen, line) else {
            continue;
        };

        // Render this row's cells as text, starting from `b_col` on the first
        // row (offset 0 on continuation rows, so the full row content).
        let cell_start = if line == b_row { b_col } else { 0 };
        let mut text = String::new();
        for cell in row.cells.iter().skip(cell_start) {
            // Wide spacers have an empty grapheme, so `push_str("")` is a no-op and
            // they get skipped naturally without any special-casing.
            text.push_str(cell.grapheme.as_str());
        }
        parts.push(text.trim_end().to_string());
    }

    // Join rows: check if each row's SUCCESSOR is a soft continuation
    // (WrapOrigin::SoftFrom). If the successor is SoftFrom, omit the `\n`,
    // since the row break is just the terminal's margin wrap, not a real newline.
    let mut result = String::new();
    let n = parts.len();
    for (i, part) in parts.into_iter().enumerate() {
        result.push_str(&part);
        if i + 1 < n {
            // Equivalent note (359:18 `< → <=`, 359:14 `+ → *`): the mutation
            // adds a separator after the LAST element (i = n-1), but the outer
            // `result.trim()` call always strips any trailing '\n'. No observable
            // difference.
            // Look at the successor row's wrap_origin.
            // Equivalent note (361:40 `+ → -`): for all existing tests b_row=0
            // so `b_row - i = 0` for the only executed i=0; equivalent.
            let successor_line = b_row + i as u32 + 1;
            let is_soft = row_at(screen, successor_line)
                .is_some_and(|r| matches!(r.wrap_origin, WrapOrigin::SoftFrom(_)));
            if !is_soft {
                result.push('\n');
            }
            // Soft wrap: no separator (the long command continues on the next row).
        }
    }

    let trimmed = result.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
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
    // Equivalent note (421:19 `|| → &&`): `&& mutation` only fires when n==0
    // (would reach arithmetic with `n as u32 - 1` = underflow), an input
    // never passed by callers. For all n>0 callers the guard is equivalent.
    if total == 0 || n == 0 {
        return result;
    }

    // Find the governing prompt for `top`: at or above it (top may itself be a
    // prompt). If none exists, search forward into the viewport.
    // Equivalent note (427:48 `< → ==`, `< → >`, `< → <=`; 427:56 `&& → ||`;
    // 429:19 `< → <=`): mutations that misidentify `top` as a prompt or extend
    // the `else if` to top==total still find the correct `start_prompt` via the
    // forward scan in the None branch. The overlap computation then produces
    // identical results for all tested viewports.
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
        // Equivalent note (451:55 `total - 1 → + / /`; 451:68 `np - 1 → + / /`):
        // over-extending block_end_incl beyond total is clamped by vp_end in the
        // overlap calculation; including the next block's prompt row is overwritten
        // by that block's own forward fill on the next iteration.
        let block_end_incl: u32 = next_p.map_or(total - 1, |np| np - 1);

        // Delegate to closing_block_end_line for the attribution rule (first
        // BLOCK_END strictly after prompt, up to and including the next prompt row).
        let status: Option<BlockLineStatus> = {
            match closing_block_end_line(screen, prompt) {
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
        // Equivalent note (473:50 `n - 1 → +n / /n`): vp_end + 1 or + n would
        // extend the overlap bound, but r_end is capped by `.min(n - 1)` below;
        // the extra slot never gets written. Equivalent note (478:38 `- → +`):
        // r_end = overlap_end + top would be too large, but the next block's
        // forward fill overwrites any over-filled rows. Equivalent note
        // (479:54 `n - 1 → +n / /n`): since r_end = overlap_end - top ≤ n - 1
        // always, `r_end.min(n - 1)` = r_end = `r_end.min(n)`. Equivalent.
        let vp_end = top.saturating_add(n as u32 - 1);
        let overlap_start = prompt.max(top);
        let overlap_end = block_end_incl.min(vp_end);
        if overlap_start <= overlap_end {
            let r_start = (overlap_start - top) as usize;
            let r_end = (overlap_end - top) as usize;
            for slot in &mut result[r_start..=r_end.min(n - 1)] {
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

/// Block exit status for a single unified `line`, the fold-aware analogue of
/// [`viewport_block_status`], called per *display* row (each mapped through the
/// fold projection to its unified line). Same attribution rule: a prompt row
/// takes the status of the block it *starts*.
pub fn block_status_at(screen: &Screen, line: u32) -> Option<BlockLineStatus> {
    if screen.alt.is_some() {
        return None;
    }
    let prompt = prompt_at_or_above(screen, line)?;
    let d = closing_block_end_line(screen, prompt)?;
    match row_at(screen, d).and_then(|r| r.mark.exit()) {
        Some(0) => Some(BlockLineStatus::Ok),
        Some(_) => Some(BlockLineStatus::Failed),
        None => None, // D without a parseable exit code → unknown
    }
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
    let Some(newest_prompt) = (0..total).rev().find(|&l| is_prompt(screen, l)) else {
        return false;
    };
    // The pane is at a prompt iff no OUTPUT_START exists strictly after it.
    let has_output_after = (newest_prompt + 1..total)
        .any(|l| row_at(screen, l).is_some_and(|r| r.mark.contains(RowMark::OUTPUT_START)));
    !has_output_after
}

/// The command line of the block currently executing, or `None` when the pane
/// is sitting at a prompt awaiting input (or has no integration at all).
///
/// "Running" means the newest `PROMPT_START` has output started after it
/// (the inverse of [`pane_at_prompt`]); the running command is that newest
/// prompt's command line ([`block_command_line`]). Note that this returns the
/// FULL command line, and the caller takes the first token / basename for
/// window naming.
pub fn running_command(screen: &Screen) -> Option<String> {
    if pane_at_prompt(screen) {
        return None;
    }
    // The most recent prompt at/above the end of the line space owns the
    // in-flight command. `prev_prompt_line` from beyond total scans everything.
    let prompt = prev_prompt_line(screen, total_lines(screen))?;
    block_command_line(screen, prompt)
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
        let Some(row) = row_at(screen, line) else {
            continue;
        };
        let mut text = String::new();
        for cell in &row.cells {
            text.push_str(cell.grapheme.as_str());
        }
        let trimmed = text.trim_end();
        lines.push(trimmed.to_string());
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines.join("\n")
}

// ── Folding ────────────────────────────────────────────────────────────────
//
// A fold is a runtime `RowMark::FOLDED` bit on a block's prompt row (set here,
// rendered by the compositor via the fold projection below). Only *completed*
// blocks with a real output region fold; the command line stays visible.

/// Row at absolute `line`, mutable (scrollback first, then the active grid).
pub(crate) fn row_at_mut(screen: &mut Screen, line: u32) -> Option<&mut Row> {
    let sb_len = screen.scrollback.rows().len() as u32;
    if line < sb_len {
        screen.scrollback.rows_mut().get_mut(line as usize)
    } else {
        screen.active.rows.get_mut((line - sb_len) as usize)
    }
}

/// True when `line` is a visible **command** row of a folded block (the prompt
/// row through the row before its hidden output). Used to dim folded command
/// rows so a fold reads as folded, not as a command with no output.
pub fn is_folded_command_line(screen: &Screen, line: u32) -> bool {
    prompt_at_or_above(screen, line)
        .filter(|&p| row_at(screen, p).is_some_and(|r| r.mark.is_folded()))
        .and_then(|p| foldable_output(screen, p))
        .is_some_and(|(start, _)| line < start)
}

/// The hidden output range `(start, end)` (inclusive) of a *foldable* block at
/// `prompt_line`, or `None`. Foldable means: it's a prompt row, the block is
/// completed (a later prompt exists, so never the active/running block), and it
/// has a real output region below the command (`OUTPUT_START` strictly below the
/// prompt). The command line itself is never hidden.
pub fn foldable_output(screen: &Screen, prompt_line: u32) -> Option<(u32, u32)> {
    if !is_prompt(screen, prompt_line) {
        return None;
    }
    next_prompt_line(screen, prompt_line)?; // completed only
    let (start, end) = block_output_range(screen, prompt_line)?;
    (start > prompt_line).then_some((start, end))
}

/// Set or clear the fold on the block at `prompt_line`. Folding is a no-op
/// unless [`foldable_output`] allows it; unfolding always clears the bit on a
/// prompt row.
pub fn set_block_folded(screen: &mut Screen, prompt_line: u32, folded: bool) {
    if folded && foldable_output(screen, prompt_line).is_none() {
        return;
    }
    if let Some(row) = row_at_mut(screen, prompt_line)
        && row.mark.contains(RowMark::PROMPT_START)
    {
        row.mark.set_folded(folded);
    }
}

/// Toggle the fold on the block at `prompt_line`.
pub fn toggle_block_fold(screen: &mut Screen, prompt_line: u32) {
    let folded = row_at(screen, prompt_line).is_some_and(|r| r.mark.is_folded());
    set_block_folded(screen, prompt_line, !folded);
}

/// Fold every completed block (all but the active/last, where you're typing).
pub fn fold_all_completed(screen: &mut Screen) {
    let last = last_prompt_line(screen);
    for line in all_prompt_lines(screen) {
        if Some(line) != last {
            set_block_folded(screen, line, true);
        }
    }
}

/// Clear all folds.
pub fn unfold_all(screen: &mut Screen) {
    for line in all_prompt_lines(screen) {
        set_block_folded(screen, line, false);
    }
}

/// A fold-aware projection between the **unified** line space (scrollback ++
/// active) and the **visible** line space (unified minus folded blocks' output
/// ranges). Built once per frame; every viewport consumer maps through it so a
/// folded block occupies zero display rows everywhere. O(#folds) per query.
#[derive(Debug, Clone, Default)]
pub struct FoldProjection {
    /// Hidden output ranges (inclusive), sorted and disjoint.
    hidden: Vec<(u32, u32)>,
    total: u32,
}

impl FoldProjection {
    /// An identity projection over `total` lines (nothing folded), used for panes
    /// where folds don't apply (copy mode, block mode).
    pub const fn identity(total: u32) -> Self {
        Self {
            hidden: Vec::new(),
            total,
        }
    }

    /// Build from a screen's folded prompt rows.
    pub fn build(screen: &Screen) -> Self {
        let total = total_lines(screen);
        let mut hidden: Vec<(u32, u32)> = all_prompt_lines(screen)
            .into_iter()
            .filter(|&p| row_at(screen, p).is_some_and(|r| r.mark.is_folded()))
            .filter_map(|p| foldable_output(screen, p))
            .collect();
        hidden.sort_unstable();
        // Blocks are disjoint, so ranges shouldn't overlap, but we coalesce
        // defensively anyway.
        // Equivalent note (726:38 guard → false): without coalescing, adjacent
        // block ranges remain as separate entries. Since block outputs are disjoint
        // by construction, the coalesce path is never taken; the guard being false
        // produces identical merged output. to_unified and from_unified give the
        // same result whether adjacent entries are merged or kept separate.
        let mut merged: Vec<(u32, u32)> = Vec::with_capacity(hidden.len());
        for (s, e) in hidden {
            match merged.last_mut() {
                Some((_, pe)) if s <= pe.saturating_add(1) => *pe = (*pe).max(e),
                _ => merged.push((s, e)),
            }
        }
        Self {
            hidden: merged,
            total,
        }
    }

    /// True when nothing is folded (callers can take the cheap 1:1 path).
    pub const fn is_identity(&self) -> bool {
        self.hidden.is_empty()
    }

    /// Count of visible unified lines (`total − Σ hidden`).
    pub fn visible_total(&self) -> u32 {
        let hidden: u32 = self.hidden.iter().map(|&(s, e)| e - s + 1).sum();
        self.total.saturating_sub(hidden)
    }

    /// The unified line shown at visible position `visible_idx`, clamped to the
    /// last line when `visible_idx` is past the end.
    pub fn to_unified(&self, visible_idx: u32) -> u32 {
        let mut u = visible_idx;
        for &(s, e) in &self.hidden {
            if s <= u {
                u += e - s + 1;
            } else {
                break;
            }
        }
        // Equivalent note (755:58 `total - 1 → + / /`): clamping only differs
        // when u > total (visible_idx past visible_total), which never occurs
        // for valid callers, since u after the loop is always within [0, total-1].
        if self.total == 0 {
            0
        } else {
            u.min(self.total - 1)
        }
    }

    /// The visible index of `unified`, or `None` when it falls inside a fold.
    pub fn from_unified(&self, unified: u32) -> Option<u32> {
        let mut vis = unified;
        for &(s, e) in &self.hidden {
            if e < unified {
                vis -= e - s + 1;
            } else if s <= unified {
                return None; // hidden inside a fold
            } else {
                break;
            }
        }
        Some(vis)
    }
}

// ── Visible-space scroll geometry ────────────────────────────────────────────
//
// `scroll_offset` is kept (by the daemon) in VISIBLE-line space: lines scrolled
// up from the live bottom, with folded output skipped. These helpers let the
// daemon's wheel / prompt-jump / click-to-jump produce fold-exact offsets that
// the compositor consumes directly (`top_visible = visible_total - rows - off`).

/// Max scroll offset for a pane of `rows` rows: scrolled all the way up, the
/// oldest visible line sits at the top.
pub fn max_scroll_offset(screen: &Screen, rows: u16) -> u32 {
    FoldProjection::build(screen)
        .visible_total()
        .saturating_sub(u32::from(rows))
}

/// The unified line shown at display `row` for a pane of `rows` rows scrolled
/// `offset` visible lines up. (`row` 0 = the top visible line.)
pub fn scroll_line_at(screen: &Screen, rows: u16, offset: u32, row: u16) -> u32 {
    let p = FoldProjection::build(screen);
    let top = p
        .visible_total()
        .saturating_sub(u32::from(rows))
        .saturating_sub(offset);
    p.to_unified(top + u32::from(row))
}

/// The visible scroll offset that puts `target_unified` at the viewport top of a
/// pane of `rows` rows. Saturates to 0 (live) when the target sits within the
/// bottom `rows` visible lines.
pub fn scroll_offset_for_top(screen: &Screen, rows: u16, target_unified: u32) -> u32 {
    let p = FoldProjection::build(screen);
    let max = p.visible_total().saturating_sub(u32::from(rows));
    let target_visible = p.from_unified(target_unified).unwrap_or(0);
    max.saturating_sub(target_visible)
}

#[cfg(test)]
mod tests {
    use plexy_glass_emulator::Emulator;

    use super::*;

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
    fn is_command_row_picks_b_row_or_a_when_no_b() {
        // Multi-line prompt: 133;A on row 0, 133;B on row 1 (the command row).
        let s = screen_from(
            8,
            40,
            b"\x1b]133;A\x07p\r\n\x1b]133;B\x07ls\r\n\x1b]133;C\x07o\r\nx",
        );
        assert!(
            !is_command_row(&s, 0),
            "prompt-start row isn't the command row when a B exists"
        );
        assert!(is_command_row(&s, 1), "the 133;B row is the command row");
        assert!(!is_command_row(&s, 2), "an output row is not a command row");
        // Single-line prompt with no B: the 133;A row IS the command row.
        let s2 = screen_from(8, 40, b"\x1b]133;A\x07$ ls\r\n\x1b]133;C\x07o\r\nx");
        assert!(
            is_command_row(&s2, 0),
            "a no-B prompt row is the command row"
        );
    }

    #[test]
    fn closing_duration_reads_end_row() {
        let mut s = two_blocks();
        // Block 1's closing D is on line 3; stamp a known duration there.
        s.active.rows[3].mark.set_duration(Some(2300));
        assert_eq!(closing_duration(&s, 0), Some(2300));
    }

    #[test]
    fn closing_duration_none_when_unclosed() {
        let s = two_blocks();
        // Block 2 (prompt at line 3) has no closing D in the grid.
        assert_eq!(closing_duration(&s, 3), None);
    }

    #[test]
    fn block_search_text_includes_command_and_output_lowercased() {
        // Full A/B/C block so block_command_line yields the command.
        let s = screen_from(
            8,
            40,
            b"\x1b]133;A\x07$ \x1b]133;B\x07One\r\n\x1b]133;C\x07Out1\r\nOut2\r\nx",
        );
        let t = block_search_text(&s, 0, 4096);
        assert!(t.contains("one"), "command, lowercased: {t:?}");
        assert!(
            t.contains("out1") && t.contains("out2"),
            "output, lowercased: {t:?}"
        );
        assert_eq!(t, t.to_lowercase());
    }

    #[test]
    fn block_search_text_multibyte_at_cap_does_not_panic() {
        // Output is wide CJK; a cap landing mid-grapheme must not panic (the old
        // `out.truncate(cap)` did). The block's full A/B/C marks give a command.
        let s = screen_from(
            8,
            40,
            "\u{1b}]133;A\u{07}$ \u{1b}]133;B\u{07}c\r\n\u{1b}]133;C\u{07}\u{4e2d}\u{4e2d}\u{4e2d}\r\nx".as_bytes(),
        );
        // cap 5 lands inside a 3-byte grapheme run, and it must return, not panic.
        let t = block_search_text(&s, 0, 5);
        assert!(t.contains('\u{4e2d}'), "kept some output: {t:?}");
    }

    #[test]
    fn block_search_text_caps_output_bytes() {
        let s = screen_from(
            8,
            40,
            b"\x1b]133;A\x07$ \x1b]133;B\x07c\r\n\x1b]133;C\x07aaaaaaaaaa\r\nbbbbbbbbbb\r\nx",
        );
        let t = block_search_text(&s, 0, 8);
        // command "c" + '\n' + at most 8 output bytes.
        assert!(t.len() <= "c\n".len() + 8, "output capped: {t:?}");
        assert!(t.starts_with("c\n"));
    }

    #[test]
    fn find_block_by_command_matches_nearest() {
        let s = screen_from(
            8,
            40,
            b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07a\r\n\
              \x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07b\r\nx",
        );
        let lines = all_prompt_lines(&s);
        assert_eq!(lines.len(), 2, "two prompts");
        let (first, second) = (lines[0], lines[1]);
        assert_eq!(find_block_by_command(&s, "ls", second), Some(second));
        assert_eq!(find_block_by_command(&s, "ls", first), Some(first));
        assert_eq!(find_block_by_command(&s, "nope", first), None);
    }

    #[test]
    fn format_duration_units() {
        assert_eq!(format_duration(340), "340ms");
        assert_eq!(format_duration(2300), "2.3s");
        assert_eq!(format_duration(45_000), "45s");
        assert_eq!(format_duration(125_000), "2m05s");
        // Boundary: 9.95s..9.999s reads "10s", never "10.0s".
        assert_eq!(format_duration(9_949), "9.9s");
        assert_eq!(format_duration(9_999), "10s");
        assert_eq!(format_duration(10_000), "10s");
        // u32::MAX must not panic (saturating round).
        let _ = format_duration(u32::MAX);
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
        assert_eq!(
            status[0],
            Some(BlockLineStatus::Ok),
            "prompt row of ok block"
        );
        assert_eq!(
            status[1],
            Some(BlockLineStatus::Ok),
            "output row 1 of ok block"
        );
        assert_eq!(
            status[2],
            Some(BlockLineStatus::Ok),
            "output row 2 of ok block"
        );
        // Line 3: D;0+A, the next block's prompt row (block 2 running) → None
        assert_eq!(
            status[3], None,
            "shared D+A row shows NEXT block status (running)"
        );
        // Block 2 (lines 4..=7): running → None
        assert_eq!(status[4], None, "output row of running block");
        assert_eq!(status[7], None, "last row of running block");
    }

    #[test]
    fn vbs_completed_block_straddles_scrollback_grid_boundary() {
        // 3-row grid: a completed block's prompt + first output scroll into
        // scrollback while its closing D lands in the active grid. A viewport
        // whose top is in scrollback and which crosses into the grid must show
        // the block's Ok status on BOTH sides of the boundary.
        let s = screen_from(
            3,
            20,
            b"\x1b]133;A\x07p1\r\no1\r\no2\r\no3\r\n\x1b]133;D;0\x07done",
        );
        let sb = s.scrollback.rows().len();
        assert_eq!(sb, 2, "setup: prompt p1 + o1 scrolled into scrollback");
        // Unified lines: 0=p1(A), 1=o1 [scrollback]; 2=o2, 3=o3, 4=done(D;0) [grid].
        // Output rows o1 (scrollback) .. o3 (grid) are all inside the Ok block.
        let status = viewport_block_status(&s, 1, 3);
        assert_eq!(
            status,
            vec![
                Some(BlockLineStatus::Ok),
                Some(BlockLineStatus::Ok),
                Some(BlockLineStatus::Ok),
            ],
            "completed-block status must span the scrollback->grid boundary"
        );
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
        assert_eq!(
            status[0],
            Some(BlockLineStatus::Failed),
            "prompt row of failed block"
        );
        assert_eq!(
            status[1],
            Some(BlockLineStatus::Failed),
            "output row of failed block"
        );
        // Line 2 is the next block's prompt row (block 2, running) → None
        assert_eq!(status[2], None, "shared D+A row shows next block (running)");
        assert_eq!(status[3], None, "block 2 running");
    }

    /// Running block (A+C, no D): all None.
    #[test]
    fn vbs_running_block_all_none() {
        let s = screen_from(4, 20, b"\x1b]133;A\x07$ run\r\n\x1b]133;C\x07working");
        let status = viewport_block_status(&s, 0, 4);
        assert!(
            status.iter().all(Option::is_none),
            "running block → all None"
        );
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
        assert_eq!(
            status[3], None,
            "shared D+A row belongs to next block (running)"
        );
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
        assert_eq!(
            status[3],
            Some(BlockLineStatus::Failed),
            "block 2 prompt row takes block 2 status"
        );
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
        assert_eq!(
            status[0],
            Some(BlockLineStatus::Ok),
            "mid-block row at line 1"
        );
        // row 1 → line 2 → block 1 → Ok
        assert_eq!(
            status[1],
            Some(BlockLineStatus::Ok),
            "mid-block row at line 2"
        );
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
        assert!(status.iter().all(Option::is_none), "alt screen → all None");
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
        assert_eq!(
            status[2],
            Some(BlockLineStatus::Ok),
            "prompt row of ok block"
        );
    }

    /// `top` beyond all marks → `None`.
    #[test]
    fn vbs_top_beyond_all_marks() {
        let s = two_blocks();
        // top = 1000 (beyond total_lines = 8)
        let status = viewport_block_status(&s, 1000, 4);
        assert!(
            status.iter().all(Option::is_none),
            "top past total → all None"
        );
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
        assert!(
            status.iter().all(Option::is_none),
            "D on prompt's own row excluded from that block → all None"
        );
    }

    /// `top` is exactly at `total_lines` → all `None`.
    #[test]
    fn vbs_top_at_total_lines() {
        let s = screen_from(4, 20, b"\x1b]133;A\x07$ cmd");
        // total_lines = 4; top = 4 → at end → all None
        let status = viewport_block_status(&s, 4, 4);
        assert!(
            status.iter().all(Option::is_none),
            "top at total_lines → all None"
        );
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
        assert!(
            pane_at_prompt(&s),
            "shared D+A row newest, no C after → true"
        );
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

    // ── block_command_line tests ─────────────────────────────────────────────

    /// Simple: A "$ " B "cargo test" \r\n C out → Some("cargo test").
    /// Prompt prefix "$ " is excluded (text starts at B col = 2).
    #[test]
    fn bcl_simple_command() {
        // Line 0: A, "$ " prompt, B (cursor now at col 2), "cargo test"
        // Line 1: C, output
        let s = screen_from(
            4,
            20,
            b"\x1b]133;A\x07$ \x1b]133;B\x07cargo test\r\n\x1b]133;C\x07output",
        );
        assert_eq!(block_command_line(&s, 0), Some("cargo test".to_string()));
    }

    /// Prompt prefix is excluded: text before the B col is not included.
    #[test]
    fn bcl_prompt_prefix_excluded() {
        // ">>>" then B at col 3, then "cmd"
        let s = screen_from(
            4,
            20,
            b"\x1b]133;A\x07>>>\x1b]133;B\x07cmd\r\n\x1b]133;C\x07out",
        );
        let result = block_command_line(&s, 0);
        assert_eq!(
            result,
            Some("cmd".to_string()),
            "prefix '>>>' must be excluded"
        );
    }

    /// Soft-wrapped long command in a narrow screen → joined WITHOUT \\n.
    #[test]
    fn bcl_soft_wrapped_command_no_newline() {
        // Screen 10 cols wide; "$ " then B (col 2), then a 16-char command
        // that wraps at col 10 onto the next physical row.
        // Command: "abcdefghijklmnop" (16 chars); first row has "abcdefgh" (8 chars,
        // from col 2 to col 9), second row has "ijklmnop".
        let s = screen_from(
            4,
            10,
            b"\x1b]133;A\x07$ \x1b]133;B\x07abcdefghijklmnop\r\n\x1b]133;C\x07out",
        );
        // The command wraps: row 0 has "$ abcdefgh" (10 cols), row 1 has "ijklmnop".
        // block_command_line should join them without \n.
        let result = block_command_line(&s, 0);
        assert!(result.is_some(), "should extract wrapped command");
        let text = result.unwrap();
        assert!(
            !text.contains('\n'),
            "soft-wrapped command must not contain newline: {text:?}"
        );
        assert!(text.contains("abcdefgh"), "first segment present");
        assert!(text.contains("ijklmnop"), "second segment present");
    }

    /// Hard multi-row command (real newline between B and C) → joined WITH \\n.
    /// We emit \r\n between two command rows to force a Hard wrap boundary.
    #[test]
    fn bcl_hard_multirow_joined_with_newline() {
        // Line 0: A, "$ " B, "line1"
        // Line 1: "line2" (hard newline from the \r\n above)
        // Line 2: C, output
        let s = screen_from(
            6,
            20,
            b"\x1b]133;A\x07$ \x1b]133;B\x07line1\r\nline2\r\n\x1b]133;C\x07out",
        );
        let result = block_command_line(&s, 0);
        assert_eq!(
            result,
            Some("line1\nline2".to_string()),
            "hard rows must be joined with newline"
        );
    }

    /// No B row → None.
    #[test]
    fn bcl_no_b_row_is_none() {
        // A and C but no B: shell did not emit 133;B.
        let s = screen_from(4, 20, b"\x1b]133;A\x07$ cmd\r\n\x1b]133;C\x07out");
        assert_eq!(block_command_line(&s, 0), None, "no B → None");
    }

    /// No C row → None.
    #[test]
    fn bcl_no_c_row_is_none() {
        // A and B but no C.
        let s = screen_from(4, 20, b"\x1b]133;A\x07$ \x1b]133;B\x07cmd");
        assert_eq!(block_command_line(&s, 0), None, "no C → None");
    }

    /// B and C on the same row → None (command and output indistinguishable).
    ///
    /// This is the degenerate case where 133;B and 133;C land on the same physical
    /// line; the normal tests emit a newline before C to put them on different rows,
    /// but here we test the same-row edge.
    /// Note that `block_command_line` searches for B in `prompt_line..c_row`, and
    /// c_row is found first, so a B that appears after C on the same row would
    /// never be found. If 133;B and 133;C are both on row 0, `block_output_range`
    /// finds C at prompt_line = row 0 and the B search over `0..0` is empty, so it
    /// returns None via the B search.
    /// To force B == C we need B on the same row as C, with C strictly after the
    /// prompt. Build: A on row 0, newline, B then C on row 1.
    #[test]
    fn bcl_b_and_c_same_row_is_none() {
        // Row 0: A "prompt"
        // Row 1: B immediately followed by C (same physical row)
        let s = screen_from(
            4,
            20,
            b"\x1b]133;A\x07prompt\r\n\x1b]133;B\x07\x1b]133;C\x07out",
        );
        // B row == C row == 1 → None
        assert_eq!(block_command_line(&s, 0), None, "B and C same row → None");
    }

    /// Block in scrollback → still extracted.
    #[test]
    fn bcl_block_in_scrollback() {
        // 3-row screen, feed 6 rows so the first block scrolls into scrollback.
        // Block 1: A row 0 (scrollback), B at col 2, "cmd1", C at row 1, output.
        // Then enough newlines to push block 1 into scrollback.
        let s = screen_from(
            3,
            20,
            b"\x1b]133;A\x07$ \x1b]133;B\x07cmd1\r\n\x1b]133;C\x07out1\r\n\
              \x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07cmd2\r\n\x1b]133;C\x07out2",
        );
        // Block 1's prompt should be in scrollback.
        assert!(!s.scrollback.rows().is_empty(), "setup: rows in scrollback");
        // Find the prompt line for block 1 (should be line 0 in scrollback).
        assert_eq!(
            block_command_line(&s, 0),
            Some("cmd1".to_string()),
            "command in scrollback must be extracted"
        );
    }

    /// Wide grapheme before the `B` col: CJK in the prompt occupies 2 cells,
    /// so col slicing at the cell index must still be correct.
    #[test]
    fn bcl_wide_grapheme_before_b_col() {
        // "中" is a CJK wide char occupying 2 cells (grapheme + spacer).
        // Prompt: "中 " = 3 cells (wide grapheme, spacer, space), then B at col 3.
        // Command: "hello"
        // The cell-index == display-column invariant means `cells[3]` = 'h'.
        let s = screen_from(
            4,
            20,
            b"\x1b]133;A\x07\xe4\xb8\xad \x1b]133;B\x07hello\r\n\x1b]133;C\x07out",
        );
        let result = block_command_line(&s, 0);
        assert_eq!(
            result,
            Some("hello".to_string()),
            "wide grapheme in prompt must not corrupt col slicing"
        );
    }

    // ── closing_exit tests ───────────────────────────────────────────────────

    /// D on its own row (not shared with A): closing_exit returns the code.
    #[test]
    fn ce_own_row_d() {
        // A line0, C line1, D;42 line2, A line3.
        let s = screen_from(
            6,
            20,
            b"\x1b]133;A\x07$ cmd\r\n\x1b]133;C\x07out\r\n\x1b]133;D;42\x07\r\n\x1b]133;A\x07$ b",
        );
        assert_eq!(closing_exit(&s, 0), Some(42));
    }

    /// Shared D+A row: D on a PROMPT_START row still closes the block ABOVE.
    #[test]
    fn ce_shared_da_row() {
        // two_blocks: D;0 on line 3 which also has A for block 2.
        // closing_exit for block 1 (prompt_line=0) should find D on line 3.
        let s = two_blocks();
        assert_eq!(closing_exit(&s, 0), Some(0));
        // Block 2 (prompt_line=3) has no D yet → None.
        assert_eq!(closing_exit(&s, 3), None);
    }

    /// No D → None.
    #[test]
    fn ce_no_d_is_none() {
        let s = screen_from(4, 20, b"\x1b]133;A\x07$ cmd\r\n\x1b]133;C\x07out");
        assert_eq!(closing_exit(&s, 0), None);
    }

    /// Divergence case: two blocks; the FIRST block's D has a DIFFERENT exit
    /// code from the second block's D. `closing_exit(first_prompt)` returns the
    /// first block's exit, NOT the `last_block_exit`.
    #[test]
    fn ce_divergence_from_last_block_exit() {
        // Block 1: A(0), C(1), D;7(2), A(3)
        // Block 2: A(3), C(4), D;0(5), A(6)
        let s = screen_from(
            8,
            20,
            b"\x1b]133;A\x07$ one\r\n\
              \x1b]133;C\x07out1\r\n\
              \x1b]133;D;7\x07\x1b]133;A\x07$ two\r\n\
              \x1b]133;C\x07out2\r\n\
              \x1b]133;D;0\x07\x1b]133;A\x07$ three",
        );
        // Block 1's prompt is at line 0, D;7 is on line 2 (shared with block 2's A).
        assert_eq!(closing_exit(&s, 0), Some(7), "block 1 exit must be 7");
        // Block 2's prompt is at line 2 (the D+A row), D;0 is on line 4.
        assert_eq!(closing_exit(&s, 2), Some(0), "block 2 exit must be 0");
    }

    // ── last_completed_prompt tests ──────────────────────────────────────────

    /// `last_completed_prompt` returns the prompt line of the newest completed block.
    #[test]
    fn lcp_returns_prompt_of_newest_completed() {
        // two_blocks: D on line 3 (shared with A for block 2) → block 1's prompt = 0.
        let s = two_blocks();
        assert_eq!(last_completed_prompt(&s), Some(0));
    }

    /// `last_completed_prompt` with `D` on its own row: the prompt is the
    /// `PROMPT_START` at or above the `D` row.
    #[test]
    fn lcp_d_on_own_row() {
        // A line0, C line1, D line2, A line3.
        let s = screen_from(
            6,
            20,
            b"\x1b]133;A\x07$ a\r\nout\r\n\x1b]133;D;0\x07done\r\n\x1b]133;A\x07$ b",
        );
        assert_eq!(last_completed_prompt(&s), Some(0));
    }

    /// `last_completed_prompt` returns `None` when no `D` has been seen.
    #[test]
    fn lcp_none_without_block_end() {
        let s = screen_from(4, 20, b"\x1b]133;A\x07$ running");
        assert_eq!(last_completed_prompt(&s), None);
    }

    // ── running_command tests ────────────────────────────────────────────────

    /// A, B "cargo build", C (output started), no D → command is running.
    #[test]
    fn running_command_reports_in_flight_command() {
        // Line 0: A, "$ " prompt, B (col 2), "cargo build"
        // Line 1: C, output (command started, not yet finished)
        let s = screen_from(
            4,
            20,
            b"\x1b]133;A\x07$ \x1b]133;B\x07cargo build\r\n\x1b]133;C\x07building",
        );
        assert_eq!(running_command(&s), Some("cargo build".to_string()));
    }

    /// Fresh prompt (A,C,D,A awaiting input) → None.
    #[test]
    fn running_command_none_when_at_prompt() {
        // Full cycle then a fresh A: pane is at a prompt, nothing running.
        let s = screen_from(
            6,
            20,
            b"\x1b]133;A\x07$ first\r\n\
              \x1b]133;C\x07output\r\n\
              \x1b]133;D;0\x07\x1b]133;A\x07$ ",
        );
        assert_eq!(running_command(&s), None);
    }

    /// No OSC 133 integration at all → None (`pane_at_prompt` is false, but no
    /// prompt exists to extract a command from).
    #[test]
    fn running_command_none_without_integration() {
        let s = screen_from(4, 20, b"just plain output");
        assert_eq!(running_command(&s), None);
    }

    #[test]
    fn foldable_only_for_completed_blocks_with_output() {
        let s = two_blocks();
        // block 0: completed (next prompt at 3), output rows (1,2) → foldable.
        assert_eq!(foldable_output(&s, 0), Some((1, 2)));
        // block 3: active/running (no next prompt) → not foldable.
        assert_eq!(foldable_output(&s, 3), None);
        // a non-prompt line → not foldable.
        assert_eq!(foldable_output(&s, 1), None);
    }

    #[test]
    fn set_block_folded_respects_foldability() {
        let mut s = two_blocks();
        set_block_folded(&mut s, 0, true);
        assert!(row_at(&s, 0).unwrap().mark.is_folded());
        // Active block can't be folded.
        set_block_folded(&mut s, 3, true);
        assert!(!row_at(&s, 3).unwrap().mark.is_folded());
        // Unfold clears it.
        set_block_folded(&mut s, 0, false);
        assert!(!row_at(&s, 0).unwrap().mark.is_folded());
    }

    #[test]
    fn toggle_fold_all_and_unfold_all() {
        let mut s = two_blocks();
        toggle_block_fold(&mut s, 0);
        assert!(row_at(&s, 0).unwrap().mark.is_folded());
        toggle_block_fold(&mut s, 0);
        assert!(!row_at(&s, 0).unwrap().mark.is_folded());

        fold_all_completed(&mut s);
        assert!(
            row_at(&s, 0).unwrap().mark.is_folded(),
            "completed block folded"
        );
        assert!(
            !row_at(&s, 3).unwrap().mark.is_folded(),
            "active block left open"
        );

        unfold_all(&mut s);
        assert!(!row_at(&s, 0).unwrap().mark.is_folded());
    }

    #[test]
    fn zero_output_block_is_not_foldable() {
        // Two prompts back-to-back, no output between → nothing to fold.
        let mut s = screen_from(6, 20, b"\x1b]133;A\x07$ x\r\n\x1b]133;A\x07$ y\r\nz");
        assert_eq!(foldable_output(&s, 0), None);
        set_block_folded(&mut s, 0, true);
        assert!(!row_at(&s, 0).unwrap().mark.is_folded(), "nothing to fold");
    }

    #[test]
    fn fold_projection_identity_when_nothing_folded() {
        let s = two_blocks();
        let p = FoldProjection::build(&s);
        assert!(p.is_identity());
        assert_eq!(p.visible_total(), total_lines(&s));
        for i in 0..total_lines(&s) {
            assert_eq!(p.to_unified(i), i);
            assert_eq!(p.from_unified(i), Some(i));
        }
    }

    #[test]
    fn fold_projection_skips_a_folded_output_range() {
        // Fold block 0 (output rows 1..=2 hidden); total 8 → 6 visible: 0,3,4,5,6,7.
        let mut s = two_blocks();
        set_block_folded(&mut s, 0, true);
        let p = FoldProjection::build(&s);
        assert!(!p.is_identity());
        assert_eq!(p.visible_total(), 6);
        // Command row stays; output hidden; next prompt follows directly.
        assert_eq!(p.to_unified(0), 0, "command row visible");
        assert_eq!(p.to_unified(1), 3, "output 1,2 skipped → next prompt");
        assert_eq!(p.to_unified(2), 4);
        // Hidden lines map to None; visible lines round-trip.
        assert_eq!(p.from_unified(0), Some(0));
        assert_eq!(p.from_unified(1), None, "inside the fold");
        assert_eq!(p.from_unified(2), None);
        assert_eq!(p.from_unified(3), Some(1));
        assert_eq!(p.from_unified(4), Some(2));
    }

    #[test]
    fn visible_space_scroll_helpers_are_fold_exact() {
        // two_blocks: 8 unified lines, fold block 0 (hides output 1,2) → visible 6.
        let mut s = two_blocks();
        set_block_folded(&mut s, 0, true);
        let rows = 4u16;
        // Max scroll = visible_total(6) - rows(4) = 2.
        assert_eq!(max_scroll_offset(&s, rows), 2);
        // At offset 0 the top display row is visible_total-rows = 2 → unified 4
        // (visible seq 0,3,4,5,6,7 → index 2 is unified 4).
        assert_eq!(scroll_line_at(&s, rows, 0, 0), 4);
        // Scrolled to the top (offset 2): top display row is unified 0 (the $a
        // prompt), display row 1 skips the fold → unified 3 ($two prompt).
        assert_eq!(scroll_line_at(&s, rows, 2, 0), 0);
        assert_eq!(scroll_line_at(&s, rows, 2, 1), 3);
        // Offset that lands the $two prompt (unified 3) at the top: from_unified(3)
        // = 1, max(2) - 1 = 1.
        assert_eq!(scroll_offset_for_top(&s, rows, 3), 1);
        assert_eq!(
            scroll_line_at(&s, rows, 1, 0),
            3,
            "the prompt is at the top"
        );
        // A target within the bottom `rows` visible lines snaps to live (0).
        assert_eq!(scroll_offset_for_top(&s, rows, 7), 0);
    }

    #[test]
    fn fold_projection_accumulates_multiple_folds() {
        // 4 single-output blocks across 8 lines:
        //   0:A 1:C  2:A 3:C  4:A 5:C  6:A 7:C(running)
        let mut s = screen_from(
            8,
            20,
            b"\x1b]133;A\x07$a\r\n\x1b]133;C\x07o\r\n\
              \x1b]133;A\x07$b\r\n\x1b]133;C\x07o\r\n\
              \x1b]133;A\x07$c\r\n\x1b]133;C\x07o\r\n\
              \x1b]133;A\x07$d\r\n\x1b]133;C\x07o",
        );
        // Fold blocks at prompts 0 and 2 (each hides one output row: 1 and 3).
        set_block_folded(&mut s, 0, true);
        set_block_folded(&mut s, 2, true);
        let p = FoldProjection::build(&s);
        assert_eq!(p.visible_total(), 6, "8 − 2 hidden rows");
        // Visible unified sequence: 0,2,4,5,6,7.
        assert_eq!(p.to_unified(0), 0);
        assert_eq!(p.to_unified(1), 2);
        assert_eq!(p.to_unified(2), 4);
        assert_eq!(p.from_unified(1), None);
        assert_eq!(p.from_unified(3), None);
        assert_eq!(p.from_unified(4), Some(2));
    }

    // ── Targeted mutation-kill tests ─────────────────────────────────────────

    #[test]
    fn first_prompt_line_finds_oldest_and_none_when_absent() {
        // Kills: 76:5 replace first_prompt_line -> Option<u32> with None
        let s = two_blocks();
        assert_eq!(first_prompt_line(&s), Some(0), "oldest prompt is at line 0");
        // Ensure None on a screen with no OSC 133 marks.
        let no_marks = screen_from(4, 20, b"just text");
        assert_eq!(first_prompt_line(&no_marks), None);
    }

    #[test]
    fn format_duration_boundaries() {
        // Kills: 260:11 < → <= (ms=1000: without boundary test, "1000ms" vs "1.0s")
        assert_eq!(
            format_duration(1_000),
            "1.0s",
            "1000ms crosses the ms→s boundary"
        );
        // Kills: 262:18 < → <= (ms=9950: "10.0s" via tenths vs "10s" via whole-secs)
        assert_eq!(
            format_duration(9_950),
            "10s",
            "9950ms crosses the tenths→whole boundary"
        );
        // Kills: 269:17 < → <= (secs=60: "60s" vs "1m00s")
        assert_eq!(
            format_duration(60_000),
            "1m00s",
            "60s crosses the secs→minutes boundary"
        );
    }

    #[test]
    fn bcl_b_on_second_row_soft_wrapped_to_third() {
        // Kills: 361:40 replace first + with * (b_row*i+1 = 1*0+1=1 instead of b_row+i+1=2).
        // b_row=1 (B is on the row after A), command wraps from row 1 to row 2,
        // C on row 3. For i=0: mutation checks row 1 (Hard wrap) instead of row 2
        // (SoftFrom) → incorrectly inserts '\n' between the soft-wrapped segments.
        //
        // Screen: 10 cols, "$ \r\n" then B at col 0 on row 1, "abcdefghijk" (11 chars
        // → fills row 1 and spills 1 char onto row 2 as SoftFrom), "\r\nC".
        let s = screen_from(
            6,
            10,
            b"\x1b]133;A\x07$ \r\n\x1b]133;B\x07abcdefghijk\r\n\x1b]133;C\x07out",
        );
        let result = block_command_line(&s, 0);
        assert!(result.is_some(), "command must be extracted");
        let text = result.unwrap();
        // The soft-wrap between row 1 and row 2 must NOT become a '\n'.
        assert!(
            !text.contains('\n'),
            "soft-wrapped rows must not have newline: {text:?}"
        );
        assert!(text.starts_with("abcdefghij"), "first segment present");
    }

    #[test]
    fn row_at_mut_active_grid_index_with_scrollback() {
        // Kills: 630:42 replace - with + in row_at_mut
        // With sb_len=3 and line=3 (first active-grid line):
        //   original: active.rows.get_mut(3 - 3) = active.rows.get_mut(0) → Some
        //   mutation: active.rows.get_mut(3 + 3) = active.rows.get_mut(6) → None
        let mut s = across_boundary(); // sb_len=3, active has 3 rows (lines 3,4,5)
        let row = row_at_mut(&mut s, 3);
        assert!(
            row.is_some(),
            "line 3 (active[0]) must be reachable via row_at_mut with sb_len=3"
        );
    }

    #[test]
    fn is_folded_command_line_false_on_output_start_row() {
        // Kills: 641:40 replace < with <= in is_folded_command_line
        // (line < start → line <= start treats the OUTPUT_START row itself as a
        // folded command line, but it's the first hidden output row, not a command row).
        let mut s = two_blocks();
        set_block_folded(&mut s, 0, true);
        // Block 0: prompt row=0, output rows=1..=2 (start=1, end=2, folded).
        assert!(
            is_folded_command_line(&s, 0),
            "prompt row is a folded command line"
        );
        // Row 1 IS the output_start row: original says line(1) < start(1) → false.
        // Mutation says 1 <= 1 → true (wrong).
        assert!(
            !is_folded_command_line(&s, 1),
            "output-start row must NOT be classified as a folded command line"
        );
    }
}
