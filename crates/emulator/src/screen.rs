//! Screen state composes the active grid, scrollback, cursor, modes, and
//! associated metadata, and provides the methods the parser dispatches into.

use crate::{
    cell::Cell,
    cursor::Cursor,
    grid::{Grid, WrapOrigin},
    hyperlinks::HyperlinkTable,
    modes::Modes,
    parser::ScreenOps,
    scrollback::Scrollback,
    tabs::TabStops,
};
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMarkKind {
    /// OSC 133 ; A (prompt start).
    PromptStart,
    /// OSC 133 ; B (prompt end / input start).
    PromptEnd,
    /// OSC 133 ; C (command submitted).
    CommandStart,
    /// OSC 133 ; D[;exit_code] (command finished).
    CommandEnd(Option<i32>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptMark {
    pub kind: PromptMarkKind,
    pub row: u16,
    pub col: u16,
}

#[derive(Clone)]
pub struct Screen {
    pub active: Grid,
    pub alt: Option<Grid>,
    pub scrollback: Scrollback,
    pub cursor: Cursor,
    pub saved_cursor: Option<Cursor>,
    pub modes: Modes,
    pub tabs: TabStops,
    pub title: String,
    pub icon_title: String,
    pub cwd: Option<String>,
    pub hyperlinks: HyperlinkTable,
    /// Scroll region (top, bottom), inclusive, in active-grid row coords.
    pub scroll_region: (u16, u16),
    /// Outbound replies the emulator owes the child (DSR, DA, …). Drained by
    /// the daemon's PTY reader thread after each `advance` and written back
    /// through the child's stdin so TUI line editors (reedline, fish, etc.)
    /// don't block on `ESC[6n`.
    pub replies: Vec<Vec<u8>>,
    /// OSC 133 prompt marks. Pruned as scrollback evicts the row they
    /// reference. Reflow recomputes positions on the active grid.
    pub prompt_marks: Vec<PromptMark>,
    /// OSC 52 clipboard payloads queued for the daemon to flush via
    /// `pbcopy` / `xclip`. Drained by `take_clipboard_writes`.
    pub clipboard_writes: Vec<Vec<u8>>,
}

impl Screen {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            active: Grid::new(rows, cols),
            alt: None,
            scrollback: Scrollback::default(),
            cursor: Cursor::default(),
            saved_cursor: None,
            modes: Modes::default(),
            tabs: TabStops::new(cols),
            title: String::new(),
            icon_title: String::new(),
            cwd: None,
            hyperlinks: HyperlinkTable::default(),
            scroll_region: (0, rows.saturating_sub(1)),
            replies: Vec::new(),
            prompt_marks: Vec::new(),
            clipboard_writes: Vec::new(),
        }
    }

    /// Drain queued replies. The daemon calls this after `Emulator::advance`
    /// and pipes the bytes back into the child's stdin.
    pub fn take_replies(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.replies)
    }

    /// Drain queued clipboard writes. The daemon calls this after
    /// `Emulator::advance` and flushes the payloads via `pbcopy` / `xclip`.
    pub fn take_clipboard_writes(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.clipboard_writes)
    }

    pub fn rows(&self) -> u16 {
        self.active.num_rows()
    }

    pub fn cols(&self) -> u16 {
        self.active.num_cols()
    }

    /// Place one grapheme at the cursor, respecting wide-char and autowrap.
    pub fn put_grapheme(&mut self, cluster: &str) {
        let w = cluster.width() as u16;

        // Zero-width: attach to the previous cell (left of cursor on this row,
        // or last cell of previous row if cursor is at col 0).
        if w == 0 {
            self.attach_zero_width(cluster);
            return;
        }

        if self.cursor.pending_wrap && self.modes.contains(Modes::AUTOWRAP) {
            self.advance_to_next_row(true);
            self.cursor.col = 0;
            self.cursor.pending_wrap = false;
        }

        // If a wide char doesn't fit, pad the last column and wrap.
        if w == 2 && self.cursor.col + 1 >= self.cols() {
            if self.modes.contains(Modes::AUTOWRAP) {
                if self.cursor.col < self.cols() {
                    self.put_cell_at_cursor(Cell::default());
                }
                self.advance_to_next_row(true);
                self.cursor.col = 0;
                self.cursor.pending_wrap = false;
            } else {
                // No autowrap: clamp the cursor and overwrite the last cell.
                self.cursor.col = self.cols().saturating_sub(2);
            }
        }

        let cell = Cell {
            grapheme: cluster.into(),
            fg: self.cursor.fg,
            bg: self.cursor.bg,
            attrs: self.cursor.attrs,
            hyperlink_id: self.cursor.hyperlink_id,
        };
        self.put_cell_at_cursor(cell);

        if w == 2 {
            self.cursor.col += 1;
            self.put_cell_at_cursor(Cell::wide_spacer());
        }

        if self.cursor.col + 1 >= self.cols() {
            self.cursor.pending_wrap = true;
        } else {
            self.cursor.col += 1;
        }
    }

    fn put_cell_at_cursor(&mut self, cell: Cell) {
        self.active.put_cell(self.cursor.row, self.cursor.col, cell);
    }

    fn attach_zero_width(&mut self, cluster: &str) {
        // Try the cell to the left on the same row.
        if self.cursor.col > 0 {
            if let Some(prev) = self.active.get_cell(self.cursor.row, self.cursor.col - 1) {
                let mut updated = prev.clone();
                let mut s = String::from(updated.grapheme.as_str());
                s.push_str(cluster);
                updated.grapheme = s.into();
                self.active.put_cell(self.cursor.row, self.cursor.col - 1, updated);
            }
        } else if self.cursor.row > 0 {
            // Append to the last cell of the previous row.
            let prev_col = self.cols().saturating_sub(1);
            if let Some(prev) = self.active.get_cell(self.cursor.row - 1, prev_col) {
                let mut updated = prev.clone();
                let mut s = String::from(updated.grapheme.as_str());
                s.push_str(cluster);
                updated.grapheme = s.into();
                self.active.put_cell(self.cursor.row - 1, prev_col, updated);
            }
        }
        // If there's no previous cell, drop the zero-width char.
    }

    /// Move to the next row. If the cursor is at the bottom of the scroll
    /// region, scroll up (pushing the top row into scrollback when on the
    /// active screen, or discarding when on the alt screen).
    pub fn advance_to_next_row(&mut self, soft_wrap: bool) {
        let (top, bottom) = self.scroll_region;
        if self.cursor.row >= bottom {
            // Need to scroll.
            let mut popped: Vec<crate::grid::Row> = Vec::new();
            let target = if self.alt.is_some() { None } else { Some(&mut popped) };
            self.active.scroll_up(top, bottom, 1, target);
            if self.alt.is_none() {
                for r in popped {
                    self.scrollback.push(r);
                }
            }
            // Stay at the bottom; new content goes there.
            self.cursor.row = bottom;
        } else {
            self.cursor.row += 1;
        }
        if soft_wrap {
            // Mark the new row as a soft continuation. The logical-line id is
            // approximate: reflow only needs to know "this row continues the
            // previous row", so we use the destination row's index.
            let dest = self.cursor.row;
            if let Some(row) = self.active.rows.get_mut(dest as usize) {
                row.wrap_origin = WrapOrigin::SoftFrom(u32::from(dest));
            }
        }
    }

    /// Handle C0 control characters: BEL, BS, HT, LF, VT, FF, CR.
    pub fn execute_c0(&mut self, byte: u8) {
        match byte {
            0x07 => { /* BEL: no audible bell in Phase 2 */ }
            0x08 => {
                // Backspace
                self.cursor.col = self.cursor.col.saturating_sub(1);
                self.cursor.pending_wrap = false;
            }
            0x09 => {
                // HT: horizontal tab to the next stop
                let next = self
                    .tabs
                    .next(self.cursor.col)
                    .unwrap_or(self.cols().saturating_sub(1));
                self.cursor.col = next.min(self.cols().saturating_sub(1));
                self.cursor.pending_wrap = false;
            }
            0x0A..=0x0C => {
                // LF / VT / FF: newline (no carriage return)
                self.advance_to_next_row(false);
                self.cursor.pending_wrap = false;
            }
            0x0D => {
                // CR
                self.cursor.col = 0;
                self.cursor.pending_wrap = false;
            }
            _ => {
                tracing::trace!(byte, "unhandled C0 control");
            }
        }
    }

    pub fn handle_csi(&mut self, params: &vte::Params, intermediates: &[u8], final_byte: char) {
        let mut iter = params.iter();
        let first = iter.next().and_then(|p| p.first().copied());
        let nth = |params: &vte::Params, idx: usize| -> Option<u16> {
            params.iter().nth(idx).and_then(|p| p.first().copied())
        };

        match final_byte {
            'A' => {
                let n = first.filter(|&n| n > 0).unwrap_or(1);
                self.cursor.up(n);
            }
            'B' => {
                let n = first.filter(|&n| n > 0).unwrap_or(1);
                let max = self.rows();
                self.cursor.down(n, max);
            }
            'C' => {
                let n = first.filter(|&n| n > 0).unwrap_or(1);
                let max = self.cols();
                self.cursor.right(n, max);
            }
            'D' => {
                let n = first.filter(|&n| n > 0).unwrap_or(1);
                self.cursor.left(n);
            }
            'G' => {
                let col = first.unwrap_or(1).saturating_sub(1);
                self.cursor.col = col.min(self.cols().saturating_sub(1));
                self.cursor.pending_wrap = false;
            }
            'H' | 'f' => {
                let row = first.unwrap_or(1).saturating_sub(1);
                let col = nth(params, 1).unwrap_or(1).saturating_sub(1);
                let max_rows = self.rows();
                let max_cols = self.cols();
                self.cursor.move_to(row, col, max_rows, max_cols);
            }
            'd' => {
                let row = first.unwrap_or(1).saturating_sub(1);
                self.cursor.row = row.min(self.rows().saturating_sub(1));
                self.cursor.pending_wrap = false;
            }
            'J' => {
                let mode = first.unwrap_or(0);
                let (r, c) = (self.cursor.row, self.cursor.col);
                let (last_r, last_c) = (self.rows() - 1, self.cols() - 1);
                match mode {
                    0 => {
                        self.active.clear_rect(r, c, r, last_c);
                        if r < last_r {
                            self.active.clear_rect(r + 1, 0, last_r, last_c);
                        }
                    }
                    1 => {
                        if r > 0 {
                            self.active.clear_rect(0, 0, r - 1, last_c);
                        }
                        self.active.clear_rect(r, 0, r, c);
                    }
                    2 | 3 => {
                        self.active.clear();
                    }
                    _ => {}
                }
                self.cursor.pending_wrap = false;
            }
            'K' => {
                let mode = first.unwrap_or(0);
                let (r, c) = (self.cursor.row, self.cursor.col);
                let last_c = self.cols() - 1;
                match mode {
                    0 => self.active.clear_rect(r, c, r, last_c),
                    1 => self.active.clear_rect(r, 0, r, c),
                    2 => self.active.clear_rect(r, 0, r, last_c),
                    _ => {}
                }
                self.cursor.pending_wrap = false;
            }
            'm' => {
                self.handle_sgr(params);
            }
            'h' => self.set_mode(params, intermediates, true),
            'l' => self.set_mode(params, intermediates, false),
            'r' => {
                let top = first.unwrap_or(1).saturating_sub(1);
                let bottom = nth(params, 1).unwrap_or(self.rows()).saturating_sub(1);
                let bottom = bottom.min(self.rows().saturating_sub(1));
                if top < bottom {
                    self.scroll_region = (top, bottom);
                } else {
                    self.scroll_region = (0, self.rows().saturating_sub(1));
                }
                let max_rows = self.rows();
                let max_cols = self.cols();
                self.cursor.move_to(0, 0, max_rows, max_cols);
            }
            'S' => {
                let n = first.filter(|&n| n > 0).unwrap_or(1);
                let (top, bottom) = self.scroll_region;
                let alt_active = self.alt.is_some();
                let mut popped = Vec::new();
                let target = if alt_active { None } else { Some(&mut popped) };
                self.active.scroll_up(top, bottom, n, target);
                if !alt_active {
                    for r in popped {
                        self.scrollback.push(r);
                    }
                }
            }
            'T' => {
                let n = first.filter(|&n| n > 0).unwrap_or(1);
                let (top, bottom) = self.scroll_region;
                self.active.scroll_down(top, bottom, n);
            }
            'g' => {
                let mode = first.unwrap_or(0);
                match mode {
                    0 => self.tabs.clear(self.cursor.col),
                    3 => self.tabs.clear_all(),
                    _ => {}
                }
            }
            'n' => {
                // DSR (Device Status Report). The child blocks waiting for
                // our reply, so this MUST be queued to be written back.
                let mode = first.unwrap_or(0);
                match mode {
                    5 => {
                        // Status report: "ready, no malfunction".
                        self.replies.push(b"\x1b[0n".to_vec());
                    }
                    6 => {
                        // Cursor Position Report: `ESC [ row ; col R`, 1-indexed.
                        let r = self.cursor.row.saturating_add(1);
                        let c = self.cursor.col.saturating_add(1);
                        self.replies.push(format!("\x1b[{r};{c}R").into_bytes());
                    }
                    _ => {}
                }
            }
            'c' => {
                // DA (Device Attributes): identify ourselves as a VT100 with
                // advanced video (the xterm-compatible answer most consumers expect).
                let is_secondary = intermediates.first() == Some(&b'>');
                if is_secondary {
                    // DA2: terminal id, firmware version, hardware. xterm answers
                    // `ESC [ > 0 ; 95 ; 0 c`; we mirror with a recognisable id.
                    self.replies.push(b"\x1b[>0;1;0c".to_vec());
                } else {
                    self.replies.push(b"\x1b[?1;2c".to_vec());
                }
            }
            _ => {
                tracing::trace!(?intermediates, ?final_byte, "unhandled CSI");
            }
        }
    }

    fn set_mode(&mut self, params: &vte::Params, intermediates: &[u8], on: bool) {
        let private = intermediates.first() == Some(&b'?');
        for p in params.iter() {
            let Some(&code) = p.first() else { continue };
            if private {
                self.set_dec_private_mode(code, on);
            } else {
                self.set_ansi_mode(code, on);
            }
        }
    }

    fn set_dec_private_mode(&mut self, code: u16, on: bool) {
        use crate::modes::Modes;
        let flag = match code {
            1 => Modes::APP_CURSOR_KEYS,
            7 => Modes::AUTOWRAP,
            25 => Modes::CURSOR_VISIBLE,
            1049 => {
                if on {
                    self.enter_alt_screen();
                } else {
                    self.leave_alt_screen();
                }
                return;
            }
            2004 => Modes::BRACKETED_PASTE,
            9 => Modes::MOUSE_X10,
            1000 => Modes::MOUSE_BTN,
            1003 => Modes::MOUSE_ANY,
            1006 => Modes::MOUSE_SGR,
            _ => {
                tracing::trace!(code, on, "unhandled DEC private mode");
                return;
            }
        };
        if on {
            self.modes.insert(flag);
        } else {
            self.modes.remove(flag);
        }
    }

    fn set_ansi_mode(&mut self, code: u16, on: bool) {
        use crate::modes::Modes;
        match code {
            4 => {
                if on {
                    self.modes.insert(Modes::INSERT);
                } else {
                    self.modes.remove(Modes::INSERT);
                }
            }
            _ => tracing::trace!(code, on, "unhandled ANSI mode"),
        }
    }

    fn enter_alt_screen(&mut self) {
        if self.alt.is_some() {
            return;
        }
        let (rows, cols) = (self.rows(), self.cols());
        let alt = std::mem::replace(&mut self.active, Grid::new(rows, cols));
        self.alt = Some(alt);
        self.saved_cursor = Some(self.cursor.clone());
        self.cursor = Cursor::default();
        self.modes.insert(crate::modes::Modes::ALT_SCREEN);
    }

    fn leave_alt_screen(&mut self) {
        if let Some(alt) = self.alt.take() {
            self.active = alt;
            if let Some(c) = self.saved_cursor.take() {
                self.cursor = c;
            }
            self.modes.remove(crate::modes::Modes::ALT_SCREEN);
        }
    }

    fn handle_sgr(&mut self, params: &vte::Params) {
        let codes: Vec<u16> = params.iter().flat_map(|p| p.iter().copied()).collect();
        let mut i = 0;
        while i < codes.len() {
            let n = codes[i];
            match n {
                0 => {
                    self.cursor.attrs = crate::attrs::Attrs::empty();
                    self.cursor.fg = crate::color::Color::Default;
                    self.cursor.bg = crate::color::Color::Default;
                }
                1 => self.cursor.attrs.insert(crate::attrs::Attrs::BOLD),
                2 => self.cursor.attrs.insert(crate::attrs::Attrs::DIM),
                3 => self.cursor.attrs.insert(crate::attrs::Attrs::ITALIC),
                4 => self.cursor.attrs.insert(crate::attrs::Attrs::UNDERLINE),
                5 => self.cursor.attrs.insert(crate::attrs::Attrs::BLINK),
                7 => self.cursor.attrs.insert(crate::attrs::Attrs::REVERSE),
                8 => self.cursor.attrs.insert(crate::attrs::Attrs::HIDDEN),
                9 => self.cursor.attrs.insert(crate::attrs::Attrs::STRIKETHROUGH),
                22 => {
                    self.cursor.attrs.remove(crate::attrs::Attrs::BOLD);
                    self.cursor.attrs.remove(crate::attrs::Attrs::DIM);
                }
                23 => self.cursor.attrs.remove(crate::attrs::Attrs::ITALIC),
                24 => self.cursor.attrs.remove(crate::attrs::Attrs::UNDERLINE),
                25 => self.cursor.attrs.remove(crate::attrs::Attrs::BLINK),
                27 => self.cursor.attrs.remove(crate::attrs::Attrs::REVERSE),
                28 => self.cursor.attrs.remove(crate::attrs::Attrs::HIDDEN),
                29 => self.cursor.attrs.remove(crate::attrs::Attrs::STRIKETHROUGH),
                30..=37 => self.cursor.fg = crate::color::Color::from_ansi_basic((n - 30) as u8),
                38 => {
                    let (color, consumed) = parse_extended_color(&codes[i + 1..]);
                    if let Some(c) = color {
                        self.cursor.fg = c;
                    }
                    i += consumed;
                }
                39 => self.cursor.fg = crate::color::Color::Default,
                40..=47 => self.cursor.bg = crate::color::Color::from_ansi_basic((n - 40) as u8),
                48 => {
                    let (color, consumed) = parse_extended_color(&codes[i + 1..]);
                    if let Some(c) = color {
                        self.cursor.bg = c;
                    }
                    i += consumed;
                }
                49 => self.cursor.bg = crate::color::Color::Default,
                90..=97 => self.cursor.fg = crate::color::Color::from_ansi_bright((n - 90) as u8),
                100..=107 => {
                    self.cursor.bg = crate::color::Color::from_ansi_bright((n - 100) as u8)
                }
                _ => {
                    tracing::trace!(code = n, "unhandled SGR");
                }
            }
            i += 1;
        }
    }

    pub fn handle_esc(&mut self, intermediates: &[u8], byte: u8) {
        if !intermediates.is_empty() {
            tracing::trace!(?intermediates, byte, "unhandled ESC intermediates");
            return;
        }
        match byte {
            b'7' => {
                self.saved_cursor = Some(self.cursor.clone());
            }
            b'8' => {
                if let Some(c) = self.saved_cursor.clone() {
                    self.cursor = c;
                    self.cursor.pending_wrap = false;
                }
            }
            b'H' => {
                self.tabs.set(self.cursor.col);
            }
            b'c' => {
                let (rows, cols) = (self.rows(), self.cols());
                *self = Screen::new(rows, cols);
            }
            b'M' => {
                let (top, _) = self.scroll_region;
                if self.cursor.row == top {
                    let (t, b) = self.scroll_region;
                    self.active.scroll_down(t, b, 1);
                } else {
                    self.cursor.up(1);
                }
            }
            _ => tracing::trace!(byte, "unhandled ESC"),
        }
    }

    pub fn handle_osc(&mut self, params: &[&[u8]]) {
        let Some(&cmd) = params.first() else {
            return;
        };
        let cmd_str = std::str::from_utf8(cmd).unwrap_or("");
        match cmd_str {
            "0" | "2" => {
                if let Some(arg) = params.get(1) {
                    self.title = String::from_utf8_lossy(arg).into_owned();
                }
            }
            "1" => {
                if let Some(arg) = params.get(1) {
                    self.icon_title = String::from_utf8_lossy(arg).into_owned();
                }
            }
            "7" => {
                if let Some(arg) = params.get(1) {
                    self.cwd = Some(String::from_utf8_lossy(arg).into_owned());
                }
            }
            "8" => {
                let url = params
                    .get(2)
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .unwrap_or_default();
                if url.is_empty() {
                    self.cursor.hyperlink_id = None;
                } else {
                    self.cursor.hyperlink_id = self.hyperlinks.intern(&url);
                }
            }
            "133" => self.handle_osc_133(params),
            other => {
                tracing::trace!(cmd = other, "unhandled OSC");
            }
        }
    }

    fn handle_osc_133(&mut self, params: &[&[u8]]) {
        // params[0] is "133", params[1] is the subcommand letter, optional
        // params[2..] carry sub-arguments (e.g. exit code for D).
        let Some(subcmd) = params.get(1).and_then(|p| p.first().copied()) else {
            return;
        };
        let kind = match subcmd {
            b'A' => PromptMarkKind::PromptStart,
            b'B' => PromptMarkKind::PromptEnd,
            b'C' => PromptMarkKind::CommandStart,
            b'D' => {
                let exit_code = params
                    .get(2)
                    .and_then(|p| std::str::from_utf8(p).ok())
                    .and_then(|s| s.parse::<i32>().ok());
                PromptMarkKind::CommandEnd(exit_code)
            }
            other => {
                tracing::trace!(subcmd = other, "unhandled OSC 133 subcommand");
                return;
            }
        };
        self.prompt_marks.push(PromptMark {
            kind,
            row: self.cursor.row,
            col: self.cursor.col,
        });
    }
}

fn parse_extended_color(rest: &[u16]) -> (Option<crate::color::Color>, usize) {
    if rest.is_empty() {
        return (None, 0);
    }
    match rest[0] {
        5 if rest.len() >= 2 => (Some(crate::color::Color::Indexed(rest[1] as u8)), 2),
        2 if rest.len() >= 4 => (
            Some(crate::color::Color::Rgb(
                rest[1] as u8,
                rest[2] as u8,
                rest[3] as u8,
            )),
            4,
        ),
        _ => (None, 0),
    }
}

impl ScreenOps for Screen {
    fn put_grapheme(&mut self, cluster: &str) {
        Screen::put_grapheme(self, cluster);
    }
    fn execute_c0(&mut self, byte: u8) {
        Screen::execute_c0(self, byte);
    }
    fn handle_csi(&mut self, params: &vte::Params, intermediates: &[u8], action: char) {
        Screen::handle_csi(self, params, intermediates, action);
    }
    fn handle_osc(&mut self, params: &[&[u8]]) {
        Screen::handle_osc(self, params);
    }
    fn handle_esc(&mut self, intermediates: &[u8], byte: u8) {
        Screen::handle_esc(self, intermediates, byte);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(input: &[u8]) -> Screen {
        let mut p = crate::parser::Parser::new();
        let mut s = Screen::new(4, 8);
        p.advance(&mut s, input);
        p.flush(&mut s);
        s
    }

    fn text_at(s: &Screen, row: u16) -> String {
        s.active.rows[row as usize]
            .cells
            .iter()
            .filter(|c| !c.is_wide_spacer())
            .map(|c| c.grapheme.as_str())
            .collect::<String>()
    }

    #[test]
    fn ascii_writes_left_to_right() {
        let s = drive(b"hello");
        assert!(text_at(&s, 0).starts_with("hello"));
        assert_eq!(s.cursor.row, 0);
        // After 5 chars at col 0..4, cursor at col 5.
        assert_eq!(s.cursor.col, 5);
    }

    #[test]
    fn autowrap_to_next_row() {
        let s = drive(b"abcdefghi"); // 9 chars on an 8-wide grid
        assert_eq!(text_at(&s, 0)[..8], *"abcdefgh");
        assert!(text_at(&s, 1).starts_with("i"));
    }

    #[test]
    fn wide_char_takes_two_columns_and_emits_spacer() {
        // "好" is width 2.
        let s = drive("好".as_bytes());
        let c0 = s.active.get_cell(0, 0).unwrap();
        let c1 = s.active.get_cell(0, 1).unwrap();
        assert_eq!(c0.grapheme.as_str(), "好");
        assert!(c1.is_wide_spacer());
        assert_eq!(s.cursor.col, 2);
    }

    #[test]
    fn cursor_advances_to_pending_wrap_at_end_of_row() {
        let s = drive(b"abcdefgh"); // exactly 8 chars
        assert_eq!(s.cursor.col, 7);
        assert!(s.cursor.pending_wrap);
    }

    #[test]
    fn cr_returns_to_column_zero() {
        let mut s = Screen::new(4, 8);
        s.cursor.col = 5;
        s.execute_c0(0x0D);
        assert_eq!(s.cursor.col, 0);
    }

    #[test]
    fn lf_moves_down_keeps_column() {
        let mut s = Screen::new(4, 8);
        s.cursor.row = 0;
        s.cursor.col = 3;
        s.execute_c0(0x0A);
        assert_eq!((s.cursor.row, s.cursor.col), (1, 3));
    }

    #[test]
    fn bs_moves_left_and_saturates() {
        let mut s = Screen::new(4, 8);
        s.cursor.col = 1;
        s.execute_c0(0x08);
        assert_eq!(s.cursor.col, 0);
        s.execute_c0(0x08);
        assert_eq!(s.cursor.col, 0);
    }

    #[test]
    fn ht_jumps_to_next_tab_stop() {
        let mut s = Screen::new(4, 24);
        s.cursor.col = 1;
        s.execute_c0(0x09);
        assert_eq!(s.cursor.col, 8);
        s.execute_c0(0x09);
        assert_eq!(s.cursor.col, 16);
    }

    #[test]
    fn lf_at_bottom_scrolls_into_scrollback() {
        let mut p = crate::parser::Parser::new();
        let mut s = Screen::new(2, 4);
        p.advance(&mut s, b"AAAA\nBBBB\nCCCC");
        // "AAAA" should have scrolled into scrollback.
        assert!(!s.scrollback.is_empty());
    }

    fn parse(input: &[u8]) -> Screen {
        let mut p = crate::parser::Parser::new();
        let mut s = Screen::new(8, 24);
        p.advance(&mut s, input);
        p.flush(&mut s);
        s
    }

    #[test]
    fn cup_homes_cursor() {
        let s = parse(b"abc\x1b[H");
        assert_eq!((s.cursor.row, s.cursor.col), (0, 0));
    }

    #[test]
    fn cup_with_params() {
        let s = parse(b"\x1b[3;5H");
        assert_eq!((s.cursor.row, s.cursor.col), (2, 4));
    }

    #[test]
    fn cuf_advances_columns() {
        let s = parse(b"\x1b[5C");
        assert_eq!(s.cursor.col, 5);
    }

    #[test]
    fn cuu_clamps_at_top() {
        let s = parse(b"\x1b[3;1H\x1b[10A");
        assert_eq!(s.cursor.row, 0);
    }

    #[test]
    fn cup_clamps_outside_grid() {
        let s = parse(b"\x1b[100;100H");
        assert_eq!(s.cursor.row, 7);
        assert_eq!(s.cursor.col, 23);
    }

    #[test]
    fn ed2_clears_screen() {
        let s = parse(b"hello\x1b[2J");
        for r in 0..8 {
            for c in 0..24 {
                assert!(s.active.get_cell(r, c).unwrap().is_blank());
            }
        }
    }

    #[test]
    fn el_clears_to_end_of_line() {
        let s = parse(b"abcdef\x1b[H\x1b[3C\x1b[K");
        assert_eq!(s.active.get_cell(0, 0).unwrap().grapheme.as_str(), "a");
        assert_eq!(s.active.get_cell(0, 2).unwrap().grapheme.as_str(), "c");
        assert!(s.active.get_cell(0, 3).unwrap().is_blank());
        assert!(s.active.get_cell(0, 5).unwrap().is_blank());
    }

    #[test]
    fn sgr_bold_red_then_reset() {
        use crate::{attrs::Attrs, color::Color};
        let s = parse(b"\x1b[1;31mhi\x1b[0mlo");
        let c0 = s.active.get_cell(0, 0).unwrap();
        assert!(c0.attrs.contains(Attrs::BOLD));
        assert_eq!(c0.fg, Color::Indexed(1));
        let c2 = s.active.get_cell(0, 2).unwrap();
        assert!(!c2.attrs.contains(Attrs::BOLD));
        assert_eq!(c2.fg, Color::Default);
    }

    #[test]
    fn sgr_rgb_truecolor() {
        use crate::color::Color;
        let s = parse(b"\x1b[38;2;10;20;30mX");
        let c0 = s.active.get_cell(0, 0).unwrap();
        assert_eq!(c0.fg, Color::Rgb(10, 20, 30));
    }

    #[test]
    fn sgr_indexed_256() {
        use crate::color::Color;
        let s = parse(b"\x1b[38;5;200mY");
        let c0 = s.active.get_cell(0, 0).unwrap();
        assert_eq!(c0.fg, Color::Indexed(200));
    }

    #[test]
    fn sgr_bright_bg() {
        use crate::color::Color;
        let s = parse(b"\x1b[101mZ");
        let c0 = s.active.get_cell(0, 0).unwrap();
        assert_eq!(c0.bg, Color::Indexed(9));
    }

    #[test]
    fn decset_alt_screen_save_and_restore() {
        let s = parse(b"main\x1b[?1049h");
        assert!(s.modes.contains(crate::modes::Modes::ALT_SCREEN));
        assert!(s.alt.is_some());
        assert!(s.active.get_cell(0, 0).unwrap().is_blank());

        let mut p = crate::parser::Parser::new();
        let mut s2 = Screen::new(8, 24);
        p.advance(&mut s2, b"main\x1b[?1049halt\x1b[?1049l");
        p.flush(&mut s2);
        assert!(!s2.modes.contains(crate::modes::Modes::ALT_SCREEN));
        assert!(s2.alt.is_none());
        assert_eq!(s2.active.get_cell(0, 0).unwrap().grapheme.as_str(), "m");
    }

    #[test]
    fn decset_25_toggles_cursor_visibility() {
        let mut p = crate::parser::Parser::new();
        let mut s = Screen::new(8, 24);
        p.advance(&mut s, b"\x1b[?25l");
        assert!(!s.modes.contains(crate::modes::Modes::CURSOR_VISIBLE));
        p.advance(&mut s, b"\x1b[?25h");
        assert!(s.modes.contains(crate::modes::Modes::CURSOR_VISIBLE));
    }

    #[test]
    fn decstbm_sets_scroll_region_and_homes_cursor() {
        let s = parse(b"\x1b[2;6r");
        assert_eq!(s.scroll_region, (1, 5));
        assert_eq!((s.cursor.row, s.cursor.col), (0, 0));
    }

    #[test]
    fn su_scrolls_within_region() {
        let mut p = crate::parser::Parser::new();
        let mut s = Screen::new(4, 4);
        p.advance(&mut s, b"AAAA\nBBBB\nCCCC\nDDDD");
        p.flush(&mut s);
        p.advance(&mut s, b"\x1b[H\x1b[S");
        assert!(s.active.get_cell(3, 0).unwrap().is_blank());
    }

    #[test]
    fn decsc_decrc_round_trip() {
        let s = parse(b"\x1b[3;5H\x1b7\x1b[1;1H\x1b8");
        assert_eq!((s.cursor.row, s.cursor.col), (2, 4));
    }

    #[test]
    fn ris_resets_screen() {
        let s = parse(b"hello\x1bc");
        assert!(s.active.get_cell(0, 0).unwrap().is_blank());
        assert_eq!((s.cursor.row, s.cursor.col), (0, 0));
    }

    #[test]
    fn ri_at_top_of_region_scrolls_down() {
        let mut p = crate::parser::Parser::new();
        let mut s = Screen::new(4, 4);
        p.advance(&mut s, b"AAAA\nBBBB\nCCCC\nDDDD\x1b[H");
        p.flush(&mut s);
        p.advance(&mut s, b"\x1bM");
        assert!(s.active.get_cell(0, 0).unwrap().is_blank());
    }

    #[test]
    fn hts_sets_tab_at_cursor() {
        let mut p = crate::parser::Parser::new();
        let mut s = Screen::new(8, 24);
        p.advance(&mut s, b"\x1b[1;4H\x1bH\x1b[1;1H\t");
        assert_eq!(s.cursor.col, 3);
    }

    #[test]
    fn osc_0_sets_title() {
        let s = parse(b"\x1b]0;my title\x07");
        assert_eq!(s.title, "my title");
    }

    #[test]
    fn osc_7_sets_cwd() {
        let s = parse(b"\x1b]7;file:///tmp\x07");
        assert_eq!(s.cwd.as_deref(), Some("file:///tmp"));
    }

    #[test]
    fn osc_8_assigns_then_clears_hyperlink_id() {
        let mut p = crate::parser::Parser::new();
        let mut s = Screen::new(4, 8);
        p.advance(
            &mut s,
            b"\x1b]8;;https://example.com\x07link\x1b]8;;\x07after",
        );
        p.flush(&mut s);
        let id = s.hyperlinks.intern("https://example.com");
        let l = s.active.get_cell(0, 0).unwrap();
        assert_eq!(l.hyperlink_id, id);
        let a = s.active.get_cell(0, 4).unwrap();
        assert_eq!(a.hyperlink_id, None);
    }

    #[test]
    fn new_screen_has_no_marks_or_clipboard() {
        let s = Screen::new(8, 24);
        assert!(s.prompt_marks.is_empty());
        assert!(s.clipboard_writes.is_empty());
    }

    #[test]
    fn take_clipboard_writes_drains() {
        let mut s = Screen::new(8, 24);
        s.clipboard_writes.push(b"hello".to_vec());
        s.clipboard_writes.push(b"world".to_vec());
        let drained = s.take_clipboard_writes();
        assert_eq!(drained, vec![b"hello".to_vec(), b"world".to_vec()]);
        assert!(s.clipboard_writes.is_empty());
    }

    #[test]
    fn osc_133_prompt_start_recorded() {
        let s = parse(b"\x1b]133;A\x07");
        assert_eq!(s.prompt_marks.len(), 1);
        assert_eq!(s.prompt_marks[0].kind, PromptMarkKind::PromptStart);
    }

    #[test]
    fn osc_133_prompt_end_records_position() {
        let s = parse(b"abc\x1b]133;B\x07");
        let mark = s.prompt_marks.iter().find(|m| m.kind == PromptMarkKind::PromptEnd);
        assert!(mark.is_some(), "expected a PromptEnd mark: {:?}", s.prompt_marks);
    }

    #[test]
    fn osc_133_command_end_carries_exit_code() {
        let s = parse(b"\x1b]133;D;0\x07");
        match s.prompt_marks.iter().find(|m| matches!(m.kind, PromptMarkKind::CommandEnd(_))) {
            Some(m) => assert_eq!(m.kind, PromptMarkKind::CommandEnd(Some(0))),
            None => panic!("expected CommandEnd mark; got {:?}", s.prompt_marks),
        }
    }

    #[test]
    fn osc_133_command_end_without_exit_code() {
        let s = parse(b"\x1b]133;D\x07");
        match s.prompt_marks.iter().find(|m| matches!(m.kind, PromptMarkKind::CommandEnd(_))) {
            Some(m) => assert_eq!(m.kind, PromptMarkKind::CommandEnd(None)),
            None => panic!("expected CommandEnd mark"),
        }
    }
}
