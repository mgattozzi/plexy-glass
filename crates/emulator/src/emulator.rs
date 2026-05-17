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
}
