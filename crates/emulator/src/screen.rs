//! Screen state composes the active grid, scrollback, cursor, modes, and
//! associated metadata, and provides the methods the parser dispatches into.

use crate::{
    cell::Cell,
    cursor::Cursor,
    grid::{Grid, WrapOrigin},
    hyperlinks::HyperlinkTable,
    keyboard::KeyboardState,
    modes::Modes,
    parser::ScreenOps,
    scrollback::Scrollback,
    tabs::TabStops,
};
use unicode_width::UnicodeWidthStr;

/// Terminal color queries from inner apps (OSC 10/11/12 with `?` parameter).
/// Daemon drains and replies with the configured palette colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorQuery {
    /// OSC 10 ; ? (foreground color query).
    Foreground,
    /// OSC 11 ; ? (background color query).
    Background,
    /// OSC 12 ; ? (cursor color query).
    Cursor,
}

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
    /// OSC 10/11/12 color queries from the child. Drained by
    /// `take_color_queries` and answered by the daemon with palette colors.
    pub color_queries: Vec<ColorQuery>,
    /// Set when the child emits a standalone BEL (`0x07`); drained by
    /// `take_bell`. (A BEL that terminates an OSC string is routed to
    /// `osc_dispatch`, not here, so this flags only genuine bells.) Used by the
    /// daemon for per-window bell monitoring.
    pub bell_pending: bool,
    /// Per-pane keyboard-protocol negotiation state (modifyOtherKeys level +
    /// Kitty flag stacks). Read by the daemon's key re-encode stage.
    pub kbd: KeyboardState,
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
            color_queries: Vec::new(),
            bell_pending: false,
            kbd: KeyboardState::default(),
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

    /// Drain queued color queries. The daemon calls this after
    /// `Emulator::advance` and writes back the palette color replies to the
    /// child's stdin.
    pub fn take_color_queries(&mut self) -> Vec<ColorQuery> {
        std::mem::take(&mut self.color_queries)
    }

    /// Drain the standalone-BEL flag. The daemon calls this after
    /// `Emulator::advance` to detect a bell for per-window monitoring.
    pub fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.bell_pending)
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
            underline_color: self.cursor.underline_color,
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
            0x07 => self.bell_pending = true, // BEL → flag for per-window monitoring
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
            'm' => match intermediates.first() {
                // CSI > Ps m is XTMODKEYS (modifyOtherKeys), NOT SGR. Claude Code
                // emits `\e[>4;2m` during keyboard setup, and routing it through
                // handle_sgr misreads it as underline+dim (the whole-frame
                // underline bug). Sets the per-pane modifyOtherKeys level.
                Some(b'>') => self.handle_xtmodkeys(params),
                // CSI ? Ps m is the XTQMODKEYS query: we reply \e[>4;<level>m.
                Some(b'?') => self.xtqmodkeys_report(params),
                _ => self.handle_sgr(params),
            },
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
                    // DA2: terminal id, firmware version (packed crate version),
                    // hardware id. xterm answers `ESC [ > 0 ; 95 ; 0 c`.
                    let ver = pack_da2_version();
                    self.replies.push(format!("\x1b[>0;{ver};0c").into_bytes());
                } else {
                    self.replies.push(b"\x1b[?1;2c".to_vec());
                }
            }
            'q' => {
                // \e[>q (= \e[>0q) is XTVERSION. Reply with a DCS naming us.
                if intermediates.first() == Some(&b'>') {
                    let reply = format!("\x1bP>|plexy-glass({})\x1b\\", env!("CARGO_PKG_VERSION"));
                    self.replies.push(reply.into_bytes());
                } else {
                    tracing::trace!(?intermediates, "unhandled CSI q");
                }
            }
            'p' => {
                if intermediates.first() == Some(&b'!') {
                    // DECSTR: soft reset.
                    self.handle_decstr();
                } else {
                    tracing::trace!(?intermediates, "unhandled CSI p");
                }
            }
            'u' => self.handle_kitty_kbd(params, intermediates),
            _ => {
                tracing::trace!(?intermediates, ?final_byte, "unhandled CSI");
            }
        }
    }

    /// XTMODKEYS (`CSI > Ps ; Pv m`). `\e[>4;<Pv>m` sets the modifyOtherKeys
    /// level; `\e[>4m` / `\e[>4;m` (Pv omitted) resets it to 0. The leading
    /// param must be `4`; any other resource selector is ignored (we only
    /// implement modifyOtherKeys).
    fn handle_xtmodkeys(&mut self, params: &vte::Params) {
        let mut iter = params.iter();
        let resource = iter.next().and_then(|p| p.first().copied());
        if resource != Some(4) {
            tracing::trace!(?resource, "unhandled XTMODKEYS resource");
            return;
        }
        match iter.next().and_then(|p| p.first().copied()) {
            Some(level) => self.kbd.set_modify_other_keys(level),
            None => self.kbd.reset_modify_other_keys(),
        }
    }

    /// `\e[?4m` is XTQMODKEYS: report the current modifyOtherKeys level as
    /// `\e[>4;<level>m`. Only resource `4` is answered.
    fn xtqmodkeys_report(&mut self, params: &vte::Params) {
        let resource = params.iter().next().and_then(|p| p.first().copied());
        if resource != Some(4) {
            tracing::trace!(?resource, "unhandled XTQMODKEYS resource");
            return;
        }
        let level = self.kbd.modify_other_keys();
        self.replies.push(format!("\x1b[>4;{level}m").into_bytes());
    }

    /// Kitty keyboard-protocol negotiation (final byte `u`), dispatched on the
    /// intermediate prefix. Operates on the active screen's flag stack.
    ///
    /// `?` query → reply `\e[?<flags>u`; `=` set-in-place (mode 1/2/3);
    /// `>` push (default 0); `<` pop (default 1, empty-stack resets to 0).
    fn handle_kitty_kbd(&mut self, params: &vte::Params, intermediates: &[u8]) {
        let alt = self.modes.contains(crate::modes::Modes::ALT_SCREEN);
        fn first_param(p: &vte::Params) -> Option<u16> {
            p.iter().next().and_then(|g| g.first().copied())
        }
        fn second_param(p: &vte::Params) -> Option<u16> {
            p.iter().nth(1).and_then(|g| g.first().copied())
        }
        match intermediates.first() {
            Some(b'?') => {
                let flags = self.kbd.kitty_flags(alt);
                self.replies.push(format!("\x1b[?{flags}u").into_bytes());
            }
            Some(b'=') => {
                // invariant: Kitty flags are a 5-bit mask (max 31) and mode is
                // 1..=3; any value >255 is malformed and truncating to the low
                // byte still yields a valid (if unusual) flag set.
                let flags = first_param(params).unwrap_or(0) as u8;
                let mode = second_param(params).unwrap_or(1) as u8;
                self.kbd.kitty_set(alt, flags, mode);
            }
            Some(b'>') => {
                // invariant: see the `=` arm, flags truncate to the low byte.
                let flags = first_param(params).unwrap_or(0) as u8;
                self.kbd.kitty_push(alt, flags);
            }
            Some(b'<') => {
                let n = first_param(params).unwrap_or(1);
                self.kbd.kitty_pop(alt, n);
            }
            _ => {
                tracing::trace!(?intermediates, "unhandled CSI u");
            }
        }
    }

    /// DECSTR (`\e[!p`): soft terminal reset. Clears keyboard negotiation
    /// state (modifyOtherKeys level + Kitty stacks) along with the cursor's
    /// rendition. Hard reset (RIS, `\ec`) is handled in `handle_esc`.
    fn handle_decstr(&mut self) {
        self.kbd.reset();
        self.cursor.attrs = crate::attrs::Attrs::empty();
        self.cursor.fg = crate::color::Color::Default;
        self.cursor.bg = crate::color::Color::Default;
        self.cursor.underline_color = crate::color::Color::Default;
        self.cursor.pending_wrap = false;
        self.saved_cursor = None;
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
        // Normalize colon-subparameter groups before flattening. vte groups
        // colon-separated subparams into one param: `4:3` -> [4, 3] (curly
        // underline), `4:0` -> [4, 0] (styled underline OFF), `38:2:r:g:b` ->
        // [38, 2, r, g, b] (truecolor). Blindly flattening would turn `4:3`
        // into "underline; italic" and `4:0` into "underline; full-reset", so
        // styled underlines (which tmux-256color advertises and TUIs emit)
        // render wrong. Collapse styled underline to plain 4 / 24, canonicalize
        // the extended-color colon groups (38:/48:/58:) by dropping the ISO
        // colorspace-id slot, and flatten the rest. Semicolon forms like
        // `38;2;r;g;b` arrive as separate single-element groups and flow through
        // unchanged; colon forms are canonicalized here so parse_extended_color
        // (shared with the semicolon path) sees an unambiguous element count.
        // (Semicolon forms like `4;3` arrive as separate single-element groups
        // and are intentionally left as "underline; italic".)
        let mut codes: Vec<u16> = Vec::new();
        for g in params.iter() {
            match g {
                // Styled underline `4:x` -> plain 4 / 24.
                [4, style, ..] => codes.push(if *style == 0 { 24 } else { 4 }),
                // Colon-form RGB extended color (38:2:.. / 48:2:.. / 58:2:..):
                // canonicalize to [sel, 2, r, g, b] by dropping the optional ISO
                // colorspace-id slot, so the linear parse_extended_color (shared
                // with the semicolon form) reads an unambiguous element count.
                [sel @ (38 | 48 | 58), 2, rest @ ..] => {
                    codes.push(*sel);
                    codes.push(2);
                    match rest {
                        [r, g, b] => codes.extend_from_slice(&[*r, *g, *b]), // kitty form
                        [_cs, r, g, b] => codes.extend_from_slice(&[*r, *g, *b]), // ISO: drop cs
                        _ => codes.extend_from_slice(rest), // malformed -> parser Nones it
                    }
                }
                // Colon-form indexed extended color (38:5:n / 48:5:n / 58:5:n).
                [sel @ (38 | 48 | 58), 5, rest @ ..] => {
                    codes.push(*sel);
                    codes.push(5);
                    codes.extend_from_slice(rest);
                }
                // Semicolon forms arrive as single-element groups and flow through
                // unchanged; colon 38:/48:/58: are canonicalized above.
                other => codes.extend_from_slice(other),
            }
        }
        let mut i = 0;
        while i < codes.len() {
            let n = codes[i];
            match n {
                0 => {
                    self.cursor.attrs = crate::attrs::Attrs::empty();
                    self.cursor.fg = crate::color::Color::Default;
                    self.cursor.bg = crate::color::Color::Default;
                    self.cursor.underline_color = crate::color::Color::Default;
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
                58 => {
                    let (color, consumed) = parse_extended_color(&codes[i + 1..]);
                    if let Some(c) = color {
                        self.cursor.underline_color = c;
                    }
                    i += consumed;
                }
                59 => self.cursor.underline_color = crate::color::Color::Default,
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
            "10" => self.handle_osc_color_query(params, ColorQuery::Foreground),
            "11" => self.handle_osc_color_query(params, ColorQuery::Background),
            "12" => self.handle_osc_color_query(params, ColorQuery::Cursor),
            "133" => self.handle_osc_133(params),
            "52" => self.handle_osc_52(params),
            other => {
                tracing::trace!(cmd = other, "unhandled OSC");
            }
        }
    }

    fn handle_osc_color_query(&mut self, params: &[&[u8]], query: ColorQuery) {
        // params[0] = "10"/"11"/"12", params[1] = payload.
        // Query form: payload is exactly "?".
        // Set form (e.g. payload = "#1d1c19"): ignored, the palette is daemon-controlled.
        let Some(payload) = params.get(1) else { return };
        if *payload == b"?" {
            self.color_queries.push(query);
        } else {
            tracing::trace!(?query, "OSC color set form ignored (palette is daemon-controlled)");
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

    const OSC52_MAX_BYTES: usize = 4 * 1024 * 1024;

    fn handle_osc_52(&mut self, params: &[&[u8]]) {
        use base64::Engine as _;
        // params[0] = "52", params[1] = selection chars ("c", "s", "p", ...),
        // params[2] = base64 payload OR "?".
        let Some(payload) = params.get(2) else { return };
        if *payload == b"?" {
            return; // Phase 4 is set-only.
        }
        let selection = params.get(1).and_then(|p| p.first().copied()).unwrap_or(b'c');
        if !matches!(selection, b'c' | b's') {
            return;
        }
        let decoded = match base64::engine::general_purpose::STANDARD.decode(payload) {
            Ok(d) => d,
            Err(_) => {
                tracing::trace!("OSC 52 base64 decode failed; ignoring");
                return;
            }
        };
        if decoded.len() > Self::OSC52_MAX_BYTES {
            tracing::warn!(bytes = decoded.len(), "OSC 52 payload exceeds cap; dropping");
            return;
        }
        self.clipboard_writes.push(decoded);
    }
}

/// Pack `CARGO_PKG_VERSION` (major.minor.patch) into xterm's DA2 firmware-
/// version convention: `major*10000 + minor*100 + patch`. For 0.1.0 → 100.
pub(crate) fn pack_da2_version() -> u32 {
    let mut parts = env!("CARGO_PKG_VERSION").split('.');
    let major: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let minor: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let patch: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    major * 10000 + minor * 100 + patch
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
    use base64::Engine as _;

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
    fn bare_reset_clears_underline() {
        use crate::attrs::Attrs;
        // \e[m (no params) must behave as \e[0m (reset). If it doesn't, an underline
        // started with \e[4m sticks on everything after, which is the reported bug.
        let s = parse(b"\x1b[4mX\x1b[mY");
        assert!(s.active.get_cell(0, 0).unwrap().attrs.contains(Attrs::UNDERLINE), "X underlined");
        assert!(
            !s.active.get_cell(0, 1).unwrap().attrs.contains(Attrs::UNDERLINE),
            "Y must NOT be underlined after a bare \\e[m reset"
        );
    }

    #[test]
    fn styled_underline_colon_subparams() {
        use crate::attrs::Attrs;
        // \e[4:3m = curly underline ON; \e[4:0m = underline OFF (styled forms that
        // tmux-256color advertises and TUIs like claude-code emit).
        let s = parse(b"\x1b[4:3mX\x1b[4:0mY");
        assert!(s.active.get_cell(0, 0).unwrap().attrs.contains(Attrs::UNDERLINE), "X underlined (curly)");
        assert!(
            !s.active.get_cell(0, 0).unwrap().attrs.contains(Attrs::ITALIC),
            "X must NOT be italic — 4:3 is a curly-underline style, not SGR 3"
        );
        assert!(
            !s.active.get_cell(0, 1).unwrap().attrs.contains(Attrs::UNDERLINE),
            "Y must NOT be underlined after \\e[4:0m"
        );
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

    #[test]
    fn osc_52_clipboard_set_decodes_base64() {
        let s = parse(b"\x1b]52;c;aGVsbG8=\x07");
        assert_eq!(s.clipboard_writes.len(), 1);
        assert_eq!(s.clipboard_writes[0], b"hello");
    }

    #[test]
    fn osc_52_clipboard_set_with_s_selection() {
        let s = parse(b"\x1b]52;s;d29ybGQ=\x07");
        assert_eq!(s.clipboard_writes.len(), 1);
        assert_eq!(s.clipboard_writes[0], b"world");
    }

    #[test]
    fn osc_52_oversized_payload_dropped() {
        // 5 MiB of 'a' base64-encoded.
        let big = "a".repeat(5 * 1024 * 1024);
        let encoded = base64::engine::general_purpose::STANDARD.encode(big.as_bytes());
        let sequence = format!("\x1b]52;c;{encoded}\x07");
        let s = parse(sequence.as_bytes());
        assert!(s.clipboard_writes.is_empty(), "expected oversized payload to be dropped");
    }

    #[test]
    fn osc_52_read_request_ignored() {
        // The selection char is followed by `?`, which is a READ request.
        // Phase 4 set-only: drop these silently.
        let s = parse(b"\x1b]52;c;?\x07");
        assert!(s.clipboard_writes.is_empty());
    }

    #[test]
    fn osc_11_query_pushes_background_color_query() {
        let s = parse(b"\x1b]11;?\x07");
        assert_eq!(s.color_queries, vec![ColorQuery::Background]);
    }

    #[test]
    fn osc_10_query_pushes_foreground_color_query() {
        let s = parse(b"\x1b]10;?\x07");
        assert_eq!(s.color_queries, vec![ColorQuery::Foreground]);
    }

    #[test]
    fn osc_12_query_pushes_cursor_color_query() {
        let s = parse(b"\x1b]12;?\x07");
        assert_eq!(s.color_queries, vec![ColorQuery::Cursor]);
    }

    #[test]
    fn osc_11_set_form_is_ignored() {
        let s = parse(b"\x1b]11;#1d1c19\x07");
        assert!(s.color_queries.is_empty());
    }

    #[test]
    fn take_color_queries_drains() {
        let mut s = Screen::new(8, 24);
        s.color_queries.push(ColorQuery::Background);
        s.color_queries.push(ColorQuery::Foreground);
        let drained = s.take_color_queries();
        assert_eq!(drained, vec![ColorQuery::Background, ColorQuery::Foreground]);
        assert!(s.color_queries.is_empty());
    }

    #[test]
    fn standalone_bel_sets_bell_pending() {
        assert!(parse(b"\x07").bell_pending, "a lone BEL sets the flag");
        assert!(!parse(b"hi ").bell_pending, "ordinary output does not");
        // A BEL terminating an OSC string is routed to osc_dispatch, not
        // execute_c0, so it must NOT register as a standalone bell.
        assert!(!parse(b"\x1b]0;title\x07").bell_pending, "OSC-terminating BEL is not a bell");
    }

    #[test]
    fn take_bell_drains() {
        let mut s = Screen::new(8, 24);
        s.bell_pending = true;
        assert!(s.take_bell());
        assert!(!s.take_bell(), "second take is false");
    }

    #[test]
    fn xtmodkeys_csi_gt_4_2_m_is_not_sgr() {
        use crate::attrs::Attrs;
        // \e[>4;2m is XTMODKEYS (modifyOtherKeys level 2), NOT SGR 4;2. Claude
        // Code emits it during keyboard-protocol setup; the '>' intermediate must
        // route it away from `handle_sgr` so 'X' is not painted underline+dim.
        let s = drive(b"\x1b[>4;2mX");
        let c0 = s.active.get_cell(0, 0).unwrap();
        assert_eq!(c0.grapheme.as_str(), "X");
        assert!(
            !c0.attrs.contains(Attrs::UNDERLINE),
            "CSI >4;2m must not set UNDERLINE on following text"
        );
        assert!(
            !c0.attrs.contains(Attrs::DIM),
            "CSI >4;2m must not set DIM on following text"
        );
    }

    #[test]
    fn xtmodkeys_then_claude_reset_pattern_no_spurious_underline() {
        use crate::attrs::Attrs;
        // The realistic Claude Code shape: XTMODKEYS, then a 256-color run that
        // resets with 39/22 (never 24/0). If >4;2m had leaked as SGR 4;2 the
        // underline would never clear and bleed onto 'z'.
        let s = drive(b"\x1b[>4;2m\x1b[38;5;174mhi\x1b[39m\x1b[22m\x1b[0mz");
        for (col, want) in [(0u16, 'h'), (1, 'i')] {
            let c = s.active.get_cell(0, col).unwrap();
            assert_eq!(c.grapheme.chars().next(), Some(want));
            assert!(
                !c.attrs.contains(Attrs::UNDERLINE),
                "col {col} ({want}) must not be underlined"
            );
        }
        let z = s.active.get_cell(0, 2).unwrap();
        assert_eq!(z.grapheme.as_str(), "z");
        assert!(!z.attrs.contains(Attrs::UNDERLINE), "'z' must not be underlined");
    }

    #[test]
    fn sgr_58_colon_indexed_sets_underline_color() {
        use crate::color::Color;
        let s = parse(b"\x1b[58:5:9mX");
        assert_eq!(s.active.get_cell(0, 0).unwrap().underline_color, Color::Indexed(9));
    }
    #[test]
    fn sgr_58_colon_rgb_kitty_form_sets_underline_color() {
        use crate::color::Color;
        let s = parse(b"\x1b[58:2:10:20:30mX");
        assert_eq!(s.active.get_cell(0, 0).unwrap().underline_color, Color::Rgb(10, 20, 30));
    }
    #[test]
    fn sgr_58_colon_rgb_iso_form_with_colorspace_slot() {
        use crate::color::Color;
        // 58:2::r:g:b is the ISO form with an empty colorspace slot (vte yields 0
        // for empty).
        let s = parse(b"\x1b[58:2::10:20:30mX");
        assert_eq!(s.active.get_cell(0, 0).unwrap().underline_color, Color::Rgb(10, 20, 30));
    }
    #[test]
    fn sgr_58_semicolon_form_sets_underline_color() {
        use crate::color::Color;
        let s = parse(b"\x1b[58;2;10;20;30mX");
        assert_eq!(s.active.get_cell(0, 0).unwrap().underline_color, Color::Rgb(10, 20, 30));
    }
    #[test]
    fn sgr_59_resets_underline_color() {
        use crate::color::Color;
        let s = parse(b"\x1b[58:5:9m\x1b[59mX");
        assert_eq!(s.active.get_cell(0, 0).unwrap().underline_color, Color::Default);
    }
    #[test]
    fn sgr_0_resets_underline_color() {
        use crate::color::Color;
        let s = parse(b"\x1b[58:5:9m\x1b[0mX");
        assert_eq!(s.active.get_cell(0, 0).unwrap().underline_color, Color::Default);
    }
    #[test]
    fn parse_extended_color_iso_colorspace_slot_consumes_exact_count() {
        use crate::attrs::Attrs;
        // 38:2::1:2:3 (ISO, colorspace slot) must consume all its elements so the
        // following \e[4m underlines Y, not leak a stray param onto X.
        let s = parse(b"\x1b[38:2::1:2:3mX\x1b[4mY");
        let x = s.active.get_cell(0, 0).unwrap();
        assert!(!x.attrs.contains(Attrs::UNDERLINE), "X must NOT be underlined");
        assert_eq!(x.fg, crate::color::Color::Rgb(1, 2, 3));
        let y = s.active.get_cell(0, 1).unwrap();
        assert!(y.attrs.contains(Attrs::UNDERLINE), "Y SHOULD be underlined (the \\e[4m)");
    }
    #[test]
    fn sgr_combined_semicolon_truecolor_fg_and_bg() {
        use crate::color::Color;
        // fg + bg truecolor in ONE semicolon SGR. fg must consume exactly 4 params
        // and leave 48 for bg, since a 5-element RGB arm would corrupt the second color.
        let s = parse(b"\x1b[38;2;1;2;3;48;2;4;5;6mX");
        let c0 = s.active.get_cell(0, 0).unwrap();
        assert_eq!(c0.fg, Color::Rgb(1, 2, 3));
        assert_eq!(c0.bg, Color::Rgb(4, 5, 6));
    }

    #[test]
    fn xtmodkeys_set_level_2_then_query_reports_it() {
        // \e[>4;2m sets modifyOtherKeys level 2; \e[?4m queries it.
        let mut p = crate::parser::Parser::new();
        let mut s = Screen::new(8, 24);
        p.advance(&mut s, b"\x1b[>4;2m\x1b[?4mX");
        p.flush(&mut s);
        assert_eq!(s.kbd.modify_other_keys(), 2);
        assert_eq!(s.replies, vec![b"\x1b[>4;2m".to_vec()]);
    }

    #[test]
    fn xtmodkeys_bare_resets_to_zero() {
        let s = parse(b"\x1b[>4;2m\x1b[>4mX");
        assert_eq!(s.kbd.modify_other_keys(), 0);
    }

    #[test]
    fn xtmodkeys_does_not_mutate_sgr() {
        use crate::attrs::Attrs;
        let s = parse(b"\x1b[>4;2mX");
        let c0 = s.active.get_cell(0, 0).unwrap();
        assert!(!c0.attrs.contains(Attrs::UNDERLINE), "X must not be underlined");
        assert!(!c0.attrs.contains(Attrs::DIM), "X must not be dim");
    }

    #[test]
    fn kitty_push_then_query_reports_flags() {
        // \e[>15u pushes flags 15; \e[?u queries → \e[?15u.
        let s = parse(b"\x1b[>15u\x1b[?uX");
        assert_eq!(s.kbd.kitty_flags(false), 15);
        assert_eq!(s.replies, vec![b"\x1b[?15u".to_vec()]);
    }
    #[test]
    fn kitty_set_clear_bits_mode_3() {
        // Set flags to 15 (mode 1 set-exactly), then \e[=4;3u clears bit 4 → 11.
        let s = parse(b"\x1b[=15;1u\x1b[=4;3uX");
        assert_eq!(s.kbd.kitty_flags(false), 11);
    }
    #[test]
    fn kitty_pop_empty_stack_resets_to_zero() {
        let s = parse(b"\x1b[>7u\x1b[<u\x1b[<uX");
        assert_eq!(s.kbd.kitty_flags(false), 0);
    }
    #[test]
    fn kitty_stacks_are_per_screen() {
        let mut p = crate::parser::Parser::new();
        let mut s = Screen::new(8, 24);
        p.advance(&mut s, b"\x1b[>5u\x1b[?1049h\x1b[>9uX");
        p.flush(&mut s);
        assert_eq!(s.kbd.kitty_flags(true), 9, "alt screen flags");
        assert_eq!(s.kbd.kitty_flags(false), 5, "main screen flags unchanged");
    }
    #[test]
    fn kitty_flags_cleared_on_ris() {
        let s = parse(b"\x1b[>15u\x1bcX");
        assert_eq!(s.kbd.kitty_flags(false), 0, "RIS clears Kitty flags");
        assert_eq!(s.kbd.modify_other_keys(), 0);
    }
    #[test]
    fn kitty_flags_cleared_on_decstr() {
        // Also set an underline color so DECSTR's cursor-rendition reset is
        // covered (X is painted AFTER \e[!p, so it reflects the post-reset pen).
        let s = parse(b"\x1b[>4;2m\x1b[>15u\x1b[58:5:9m\x1b[!pX");
        assert_eq!(s.kbd.kitty_flags(false), 0, "DECSTR clears Kitty flags");
        assert_eq!(s.kbd.modify_other_keys(), 0, "DECSTR clears modifyOtherKeys");
        assert_eq!(
            s.active.get_cell(0, 0).unwrap().underline_color,
            crate::color::Color::Default,
            "DECSTR resets the cursor's underline color"
        );
    }
    #[test]
    fn xtversion_replies_with_dcs() {
        // \e[>q (XTVERSION) → DCS \eP>|plexy-glass(<ver>)\e\\.
        let s = parse(b"\x1b[>qX");
        let expected = format!("\x1bP>|plexy-glass({})\x1b\\", env!("CARGO_PKG_VERSION")).into_bytes();
        assert_eq!(s.replies, vec![expected]);
    }
    #[test]
    fn da2_still_answers_with_packed_version() {
        let s = parse(b"\x1b[>cX");
        let ver = pack_da2_version();
        let expected = format!("\x1b[>0;{ver};0c").into_bytes();
        assert_eq!(s.replies, vec![expected]);
    }
}
