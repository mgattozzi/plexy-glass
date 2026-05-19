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
        } else {
            reflow(
                &mut self.screen.active,
                &mut self.screen.scrollback,
                &mut self.screen.cursor,
                rows,
                cols,
            );
        }
        // Also resize tab stops.
        self.screen.tabs.resize(cols);
        // Reset scroll region to full screen.
        self.screen.scroll_region = (0, rows.saturating_sub(1));
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
        assert_eq!(replies, vec![b"\x1b[>0;1;0c".to_vec()]);
    }

    #[test]
    fn take_clipboard_writes_drains_after_osc52() {
        let mut e = Emulator::new(4, 8);
        e.advance(b"\x1b]52;c;aGVsbG8=\x07");
        let drained = e.take_clipboard_writes();
        assert_eq!(drained, vec![b"hello".to_vec()]);
        assert!(e.take_clipboard_writes().is_empty());
    }
}
