//! Top-level `Emulator`: composes `Parser` + `Screen` behind a small public API.

use crate::{
    parser::Parser,
    reflow::reflow,
    screen::Screen,
};

pub struct Emulator {
    parser: Parser,
    screen: Screen,
}

impl Emulator {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: Parser::new(),
            screen: Screen::new(rows, cols),
        }
    }

    pub fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.screen, bytes);
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        // Reflow the active screen (and scrollback unless we're in alt-screen mode).
        if self.screen.alt.is_some() {
            // Alt-screen is reflowed independently; scrollback untouched.
            let mut empty_sb = crate::scrollback::Scrollback::with_cap(0);
            reflow(
                &mut self.screen.active,
                &mut empty_sb,
                &mut self.screen.cursor,
                rows,
                cols,
            );
            // Also reflow the PARKED main grid (held in `self.screen.alt`). Without
            // this, leaving alt-screen after a resize (vim/less + resize + :q)
            // restores the main grid at its stale pre-resize dimensions while the
            // cursor/scroll_region reflect the new size. Its cursor lives in
            // `saved_cursor`; reflow it too so the restored cursor stays in-bounds.
            let mut parked_sb = crate::scrollback::Scrollback::with_cap(0);
            let mut throwaway = crate::cursor::Cursor::default();
            let parked_cursor = self.screen.saved_cursor.as_mut().unwrap_or(&mut throwaway);
            if let Some(parked) = self.screen.alt.as_mut() {
                reflow(parked, &mut parked_sb, parked_cursor, rows, cols);
            }
        } else {
            reflow(
                &mut self.screen.active,
                &mut self.screen.scrollback,
                &mut self.screen.cursor,
                rows,
                cols,
            );
        }
        // Row-resident marks (including `PROMPT_END`) travel with their rows
        // through reflow automatically, so no housekeeping is needed here.
        // Also resize tab stops.
        self.screen.tabs.resize(cols);
        // Reset scroll region to full screen.
        self.screen.scroll_region = (0, rows.saturating_sub(1));
    }

    /// Seed restored scrollback into the screen (session restore).
    /// Forwards to [`Screen::preseed_scrollback`]; see its docs for the cap and
    /// counter-defaults rules. Called once, before any `advance`.
    pub fn preseed_scrollback(&mut self, rows: Vec<crate::grid::Row>) {
        self.screen.preseed_scrollback(rows);
    }

    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    pub fn screen_mut(&mut self) -> &mut Screen {
        &mut self.screen
    }

    /// Drain any replies the child is waiting on (DSR cursor reports, DA, …).
    /// Call after `advance` and write the returned bytes back through the
    /// child's stdin. Empty most of the time; non-empty only when the child
    /// has issued a query.
    pub fn take_replies(&mut self) -> Vec<Vec<u8>> {
        self.screen.take_replies()
    }

    /// Drain queued OSC 52 clipboard payloads. The daemon calls this from
    /// the PTY reader thread (same place it drains `take_replies`).
    pub fn take_clipboard_writes(&mut self) -> Vec<Vec<u8>> {
        self.screen.take_clipboard_writes()
    }

    /// Drain queued OSC 10/11/12 color queries. The daemon calls this from
    /// the PTY reader thread (same place it drains `take_replies`) and replies
    /// with the current palette colors.
    pub fn take_color_queries(&mut self) -> Vec<crate::screen::ColorQuery> {
        self.screen.take_color_queries()
    }

    /// Drain the standalone-BEL flag (set when the child emitted `0x07`). The
    /// daemon calls this from the PTY reader thread for per-window bell
    /// monitoring.
    pub fn take_bell(&mut self) -> bool {
        self.screen.take_bell()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advance_writes_to_screen() {
        let mut e = Emulator::new(4, 8);
        e.advance(b"hi");
        // Force flush so we can observe the trailing grapheme.
        e.parser.flush(&mut e.screen);
        let s = e.screen();
        assert_eq!(s.active.get_cell(0, 0).unwrap().grapheme.as_str(), "h");
        assert_eq!(s.active.get_cell(0, 1).unwrap().grapheme.as_str(), "i");
    }

    #[test]
    fn resize_grows_grid() {
        let mut e = Emulator::new(4, 8);
        e.advance(b"hello");
        e.parser.flush(&mut e.screen);
        e.resize(4, 16);
        assert_eq!(e.screen().active.num_cols(), 16);
        assert_eq!(e.screen().active.get_cell(0, 0).unwrap().grapheme.as_str(), "h");
    }

    #[test]
    fn resize_carries_prompt_end_mark_via_reflow() {
        // Row-resident `PROMPT_END` marks travel with their row through reflow,
        // so no side-list housekeeping is needed. The shell still re-emits 133;B
        // on its post-SIGWINCH redraw, but reflow does not destroy the mark.
        let mut e = Emulator::new(4, 8);
        e.advance(b"\x1b]133;B\x07"); // cursor col 0, row 0
        assert!(
            e.screen().active.rows[0]
                .mark
                .contains(crate::grid::RowMark::PROMPT_END),
            "PROMPT_END must be set on row 0 before resize"
        );
        e.resize(4, 16);
        // After reflow the logical line maps to the first physical row; the
        // mark is still there.
        assert!(
            e.screen().active.rows[0]
                .mark
                .contains(crate::grid::RowMark::PROMPT_END),
            "PROMPT_END must survive reflow"
        );
    }

    #[test]
    fn leaving_alt_screen_after_resize_restores_new_dimensions() {
        // Enter alt-screen, resize, then leave: the restored main grid must be
        // at the NEW size, not the stale pre-resize dimensions. (vim/less +
        // resize + :q.)
        let mut e = Emulator::new(4, 8);
        e.advance(b"main");
        e.parser.flush(&mut e.screen);
        e.advance(b"\x1b[?1049h"); // enter alt-screen (parks the 4x8 main grid)
        e.advance(b"alt");
        e.parser.flush(&mut e.screen);
        e.resize(6, 20); // resize while on the alt-screen
        assert_eq!(e.screen().active.num_cols(), 20);
        assert_eq!(e.screen().active.num_rows(), 6);
        e.advance(b"\x1b[?1049l"); // leave alt-screen -> restore the main grid
        assert_eq!(
            e.screen().active.num_cols(),
            20,
            "restored main grid must be at the new width"
        );
        assert_eq!(
            e.screen().active.num_rows(),
            6,
            "restored main grid must be at the new height"
        );
        assert_eq!(e.screen().cols(), 20);
        assert_eq!(e.screen().rows(), 6);
        // Content survived the parked-grid reflow.
        assert_eq!(
            e.screen().active.get_cell(0, 0).unwrap().grapheme.as_str(),
            "m"
        );
    }

    #[test]
    fn dsr_cursor_position_report_queued() {
        let mut e = Emulator::new(8, 24);
        // Move cursor to (3, 5) (0-indexed) then ask for cursor position.
        e.advance(b"\x1b[4;6H\x1b[6n");
        let replies = e.take_replies();
        assert_eq!(replies.len(), 1);
        // CPR is 1-indexed: row 4, col 6.
        assert_eq!(replies[0], b"\x1b[4;6R");
        // Subsequent take drains.
        assert!(e.take_replies().is_empty());
    }

    #[test]
    fn dsr_status_report_queued() {
        let mut e = Emulator::new(4, 8);
        e.advance(b"\x1b[5n");
        let replies = e.take_replies();
        assert_eq!(replies, vec![b"\x1b[0n".to_vec()]);
    }

    #[test]
    fn primary_da_queued() {
        let mut e = Emulator::new(4, 8);
        e.advance(b"\x1b[c");
        let replies = e.take_replies();
        assert_eq!(replies, vec![b"\x1b[?1;2c".to_vec()]);
    }

    #[test]
    fn secondary_da_queued() {
        let mut e = Emulator::new(4, 8);
        e.advance(b"\x1b[>c");
        let replies = e.take_replies();
        // DA2 now packs the crate version (0.1.0 -> 100) instead of a literal 1.
        let ver = crate::screen::pack_da2_version();
        assert_eq!(replies, vec![format!("\x1b[>0;{ver};0c").into_bytes()]);
    }

    #[test]
    fn take_clipboard_writes_drains_after_osc52() {
        let mut e = Emulator::new(4, 8);
        e.advance(b"\x1b]52;c;aGVsbG8=\x07");
        let drained = e.take_clipboard_writes();
        assert_eq!(drained, vec![b"hello".to_vec()]);
        assert!(e.take_clipboard_writes().is_empty());
    }

    #[test]
    fn take_color_queries_drains_after_osc11() {
        let mut e = Emulator::new(4, 8);
        e.advance(b"\x1b]11;?\x07");
        let drained = e.take_color_queries();
        assert_eq!(drained, vec![crate::screen::ColorQuery::Background]);
        assert!(e.take_color_queries().is_empty());
    }

    fn seed_row(text: &str, cols: u16) -> crate::grid::Row {
        let mut r = crate::grid::Row::blank(cols);
        for (i, ch) in text.chars().enumerate() {
            if (i as u16) < cols {
                r.cells[i].grapheme = ch.to_string().into();
            }
        }
        r
    }

    #[test]
    fn preseed_pushes_rows_into_scrollback_active_blank() {
        let mut e = Emulator::new(4, 8);
        let rows = vec![seed_row("hist1", 8), seed_row("hist2", 8)];
        e.preseed_scrollback(rows);
        let s = e.screen();
        assert_eq!(s.scrollback.len(), 2, "both seeded rows land in scrollback");
        let texts: Vec<String> = s
            .scrollback
            .iter()
            .map(|r| r.cells.iter().map(|c| c.grapheme.as_str()).collect::<String>().trim_end().to_string())
            .collect();
        assert_eq!(texts, vec!["hist1".to_string(), "hist2".to_string()]);
        // Active grid stays blank.
        assert!(s.active.rows.iter().all(|r| r.cells.iter().all(|c| c.is_blank())));
        // Counters untouched.
        assert_eq!(s.blocks_completed, 0);
        assert_eq!(s.last_block_exit, None);
    }

    #[test]
    fn preseed_marks_ride_into_scrollback() {
        let mut e = Emulator::new(4, 8);
        let mut prompt = seed_row("$ ls", 8);
        prompt.mark.set(crate::grid::RowMark::PROMPT_START);
        let mut out = seed_row("file", 8);
        out.mark.set(crate::grid::RowMark::OUTPUT_START);
        e.preseed_scrollback(vec![prompt, out]);
        let s = e.screen();
        assert!(s.scrollback.rows()[0].mark.contains(crate::grid::RowMark::PROMPT_START));
        assert!(s.scrollback.rows()[1].mark.contains(crate::grid::RowMark::OUTPUT_START));
    }

    #[test]
    fn preseed_over_cap_keeps_newest() {
        // Tiny scrollback so we can overflow it deterministically.
        let mut e = Emulator::new(2, 4);
        e.screen_mut().scrollback = crate::scrollback::Scrollback::with_cap(2);
        let rows = vec![seed_row("A", 4), seed_row("B", 4), seed_row("C", 4)];
        e.preseed_scrollback(rows);
        let s = e.screen();
        assert_eq!(s.scrollback.len(), 2, "cap holds only 2 rows");
        let texts: Vec<String> = s
            .scrollback
            .iter()
            .map(|r| r.cells[0].grapheme.as_str().to_string())
            .collect();
        assert_eq!(texts, vec!["B".to_string(), "C".to_string()], "newest rows kept");
    }

    #[test]
    fn preseed_then_advance_output_goes_below_history() {
        let mut e = Emulator::new(4, 8);
        e.preseed_scrollback(vec![seed_row("OLD", 8)]);
        // Child draws into the grid; history stays in scrollback above it.
        e.advance(b"NEW");
        e.parser.flush(&mut e.screen);
        let s = e.screen();
        assert_eq!(s.scrollback.len(), 1, "history still in scrollback");
        assert_eq!(s.scrollback.rows()[0].cells[0].grapheme.as_str(), "O");
        // The new bytes land in the active grid (row 0).
        let row0: String = s.active.rows[0].cells.iter().map(|c| c.grapheme.as_str()).collect();
        assert!(row0.starts_with("NEW"), "child output is in the live grid: {row0:?}");
    }
}
