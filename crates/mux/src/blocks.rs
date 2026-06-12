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

/// Render the absolute-line range `(start, end)` (inclusive, scrollback rows
/// included) as plain text: one line per row, trailing whitespace trimmed.
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
}
