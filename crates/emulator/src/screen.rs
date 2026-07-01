//! Screen state composes the active grid, scrollback, cursor, modes, and
//! associated metadata, and provides the methods the parser dispatches into.

use crate::{
    cell::Cell,
    cursor::Cursor,
    graphics::{Image, ImageFormat, ImageProtocol, ImageStore, Placement, VirtualPlacement},
    grid::{Grid, RowMark, WrapOrigin},
    hyperlinks::HyperlinkTable,
    keyboard::KeyboardState,
    modes::Modes,
    parser::ScreenOps,
    scrollback::Scrollback,
    tabs::TabStops,
};
use std::sync::Arc;
use unicode_width::UnicodeWidthStr;

/// In-progress chunked image transmission (`m=1 … m=0`). Accumulated across
/// `handle_graphics` calls until the final chunk, then finalized into an
/// `Image` (+ a `Placement` when the action is display).
#[derive(Clone, Debug)]
struct PendingTx {
    id: u32,
    format: ImageFormat,
    width: Option<u32>,
    height: Option<u32>,
    data_b64: Vec<u8>,
    display: bool,
    place_rows: Option<u16>,
    place_cols: Option<u16>,
    /// `U=1`: on finalize, record a virtual placement instead of an anchored one.
    unicode: bool,
    placement_id: Option<u32>,
}

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
    /// OSC 52 clipboard payloads queued for the daemon to flush via
    /// `pbcopy` / `xclip`. Drained by `take_clipboard_writes`.
    pub clipboard_writes: Vec<Vec<u8>>,
    /// OSC 10/11/12 color queries from the child. Drained by
    /// `take_color_queries` and answered by the daemon with palette colors.
    /// Each entry records the number of raw `replies` queued before it, so the
    /// daemon can re-interleave color replies with DA/DSR replies in the order
    /// the child emitted the queries (apps probe OSC-support with a color query
    /// followed by a DA1 and rely on in-order responses).
    pub color_queries: Vec<(usize, ColorQuery)>,
    /// Set when the child emits a standalone BEL (`0x07`); drained by
    /// `take_bell`. (A BEL that terminates an OSC string is routed to
    /// `osc_dispatch`, not here, so this flags only genuine bells.) Used by the
    /// daemon for per-window bell monitoring.
    pub bell_pending: bool,
    /// Per-pane keyboard-protocol negotiation state (modifyOtherKeys level +
    /// Kitty flag stacks). Read by the daemon's key re-encode stage.
    pub kbd: KeyboardState,
    /// `$TERM` value advertised to children via XTGETTCAP `TN`/`name`. This must
    /// match the `$TERM` the pane actually exports into the child's environment
    /// (a per-PANE value set at spawn, NOT a per-client handshake value, which
    /// would be wrong under multi-client). Defaults to a 256-color xterm.
    pub term: String,
    /// Last known outer-terminal color scheme (true = dark), set by the daemon
    /// from the client's `\e[?997;Xn` relay. Answered to a `\e[?996n` query so
    /// the one-shot query agrees with the `?2031` subscription push. Default dark.
    pub color_scheme_dark: bool,
    /// Monotonic count of completed blocks (`133;D` received on the main grid).
    /// Survives row eviction and `ED 2J` clears because it is not row state.
    /// Transient across daemon restart (like all block state). Reset to 0 by
    /// RIS (via the screen rebuild).
    pub blocks_completed: u64,
    /// Exit payload of the most recent completed block (`133;D[;exit]`).
    /// `None` when no `D` has been received, or when the last `D` carried no
    /// parseable exit payload. Survives eviction and clears; reset by RIS.
    pub last_block_exit: Option<i32>,
    /// Duration (millis) of the most recent completed block (`C`→`D`), the
    /// session-level mirror of `last_block_exit` for the notification policy.
    /// `None` until a `D` that had a preceding `C`. Survives eviction; reset by RIS.
    pub last_block_duration: Option<u32>,
    /// Wall-clock instant of the most recent `OSC 133;C` (command start),
    /// consumed at `;D` to record the block duration. Cleared at `;A` so a
    /// command that emits `C` but never `D` can't mis-attribute its time to the
    /// next block. Transient (never persisted/serialized).
    // invariant: the one deliberate clock read in the otherwise-pure emulator,
    // scoped to C->D timing; tests assert presence/ordering, never exact millis.
    pub pending_command_start: Option<std::time::Instant>,
    /// Text-area size in pixels, relayed from the attached client's terminal
    /// (`0` = unknown). Used to derive the cell pixel size for graphics scaling
    /// and to answer `CSI 14/16/18t` size reports.
    pub area_px_w: u16,
    pub area_px_h: u16,
    /// Transmitted images (id → data), with an LRU byte budget.
    pub images: ImageStore,
    /// On-screen image placements, anchored to absolute unified lines.
    pub placements: Vec<Placement>,
    /// Unicode-placeholder (virtual) placements, positioned by the app's
    /// `U+10EEEE` cells, not anchored to a line.
    pub virtual_placements: Vec<VirtualPlacement>,
    /// In-progress chunked transmission (`None` between images).
    pending_tx: Option<PendingTx>,
    graphics_seq: u64,
    placement_id_seq: u32,
    image_id_seq: u32,
    /// Monotonic content version, bumped on every finalized transmission, so a
    /// re-transmit of an existing id carries a fresh `Image::generation` and the
    /// per-client renderer re-sends the new pixels instead of the stale ones.
    image_gen: u64,
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
            clipboard_writes: Vec::new(),
            color_queries: Vec::new(),
            bell_pending: false,
            kbd: KeyboardState::default(),
            term: String::from("xterm-256color"),
            color_scheme_dark: true,
            blocks_completed: 0,
            last_block_exit: None,
            last_block_duration: None,
            pending_command_start: None,
            area_px_w: 0,
            area_px_h: 0,
            images: ImageStore::default(),
            placements: Vec::new(),
            virtual_placements: Vec::new(),
            pending_tx: None,
            graphics_seq: 0,
            placement_id_seq: 0,
            image_id_seq: 0x8000_0000, // synthesized ids; high to avoid child collisions
            image_gen: 0,
        }
    }

    /// Handle a captured Kitty-graphics APC (`framed` = `ESC _ G … ESC \`).
    /// Transmissions accumulate into `images`; display actions add a `Placement`
    /// at the cursor and advance the cursor by the image's cell footprint.
    /// Not captured on the alt screen (a full-screen app owns the pane).
    /// Capture a Sixel image (DCS `<params> q <data> ST`): store it under the
    /// unified model and add an anchored placement at the cursor (cursor advances
    /// by the footprint, like a Kitty `a=T`). `params` are the pre-`q` DCS params,
    /// `payload` is the sixel data; both are kept (reconstructed) for re-emit.
    pub(crate) fn handle_sixel(&mut self, params: &[Vec<u16>], payload: &[u8]) {
        if self.alt.is_some() || payload.is_empty() {
            return;
        }
        let (w, h) = crate::graphics::sixel_dimensions(payload).unwrap_or((0, 0));
        // Reconstruct the inner DCS payload for re-emit: <params>q<data>.
        let mut inner = Vec::with_capacity(payload.len() + 8);
        for (i, g) in params.iter().enumerate() {
            if i > 0 {
                inner.push(b';');
            }
            if let Some(&v) = g.first() {
                inner.extend_from_slice(v.to_string().as_bytes());
            }
        }
        inner.push(b'q');
        inner.extend_from_slice(payload);

        self.image_id_seq = self.image_id_seq.wrapping_add(1);
        let id = self.image_id_seq;
        self.image_gen = self.image_gen.wrapping_add(1);
        let image = Image {
            id,
            protocol: ImageProtocol::Sixel,
            format: ImageFormat::Rgba, // unused for sixel re-emit
            pixel_w: w,
            pixel_h: h,
            data_b64: Arc::from(inner.as_slice()),
            iterm_args: None,
            generation: self.image_gen,
        };
        let evicted = self.images.insert(image);
        if !evicted.is_empty() {
            self.placements.retain(|p| !evicted.contains(&p.image_id));
            self.virtual_placements.retain(|p| !evicted.contains(&p.image_id));
        }
        self.add_placement(id, ImageProtocol::Sixel, w, h, None, None);
    }

    pub(crate) fn handle_graphics(&mut self, framed: &[u8]) {
        if self.alt.is_some() {
            return;
        }
        let Some(cmd) = crate::graphics::parse_command(framed) else {
            return;
        };
        match cmd.action {
            b'd' => {
                // Delete: by image id when given, else all placements.
                match cmd.id {
                    Some(id) => {
                        self.placements.retain(|p| p.image_id != id);
                        self.virtual_placements.retain(|p| p.image_id != id);
                    }
                    None => {
                        self.placements.clear();
                        self.virtual_placements.clear();
                    }
                }
            }
            b'p' => {
                // Place an already-transmitted image. `U=1` is a virtual
                // (Unicode-placeholder) placement positioned by the app's cells;
                // otherwise it's a classic placement anchored at the cursor.
                if let Some(id) = cmd.id
                    && let Some(img) = self.images.get(id)
                {
                    if cmd.unicode {
                        self.add_virtual_placement(id, cmd.placement_id, cmd.rows, cmd.cols);
                    } else {
                        let (w, h, proto) = (img.pixel_w, img.pixel_h, img.protocol);
                        self.add_placement(id, proto, w, h, cmd.rows, cmd.cols);
                    }
                }
            }
            b't' | b'T' => self.accumulate_transmission(cmd),
            _ => {} // query (q) and unknown: ignored in Phase 2
        }
    }

    fn accumulate_transmission(&mut self, cmd: crate::graphics::GraphicsCommand) {
        let is_continuation = self.pending_tx.is_some()
            && cmd.format.is_none()
            && cmd.id.is_none()
            && cmd.width.is_none()
            && cmd.height.is_none()
            && cmd.rows.is_none()
            && cmd.cols.is_none();
        if is_continuation {
            if let Some(tx) = self.pending_tx.as_mut() {
                tx.data_b64.extend_from_slice(&cmd.payload);
            }
        } else {
            // Start a new transmission (drop any abandoned one).
            let id = cmd.id.unwrap_or_else(|| {
                self.image_id_seq = self.image_id_seq.wrapping_add(1);
                self.image_id_seq
            });
            let format = cmd
                .format
                .and_then(ImageFormat::from_kitty_f)
                .unwrap_or(ImageFormat::Png);
            self.pending_tx = Some(PendingTx {
                id,
                format,
                width: cmd.width,
                height: cmd.height,
                data_b64: cmd.payload,
                display: cmd.action == b'T',
                place_rows: cmd.rows,
                place_cols: cmd.cols,
                unicode: cmd.unicode,
                placement_id: cmd.placement_id,
            });
        }
        if !cmd.more {
            self.finalize_transmission();
        }
    }

    fn finalize_transmission(&mut self) {
        let Some(tx) = self.pending_tx.take() else {
            return;
        };
        let (mut w, mut h) = (tx.width.unwrap_or(0), tx.height.unwrap_or(0));
        if (w == 0 || h == 0) && tx.format == ImageFormat::Png
            && let Some((pw, ph)) = decode_png_dims(&tx.data_b64)
        {
            w = pw;
            h = ph;
        }
        self.image_gen = self.image_gen.wrapping_add(1);
        let image = Image {
            id: tx.id,
            protocol: ImageProtocol::Kitty,
            format: tx.format,
            pixel_w: w,
            pixel_h: h,
            data_b64: Arc::from(tx.data_b64.as_slice()),
            iterm_args: None,
            generation: self.image_gen,
        };
        let evicted = self.images.insert(image);
        if !evicted.is_empty() {
            self.placements.retain(|p| !evicted.contains(&p.image_id));
            self.virtual_placements.retain(|p| !evicted.contains(&p.image_id));
        }
        // Only a *display* transmission (a=T) places. a=t (transmit only) just
        // stores the image, even with U=1 set.
        if tx.display {
            if tx.unicode {
                // a=T,U=1: virtual placement, so no anchor and no cursor advance.
                self.add_virtual_placement(tx.id, tx.placement_id, tx.place_rows, tx.place_cols);
            } else {
                self.add_placement(tx.id, ImageProtocol::Kitty, w, h, tx.place_rows, tx.place_cols);
            }
        }
    }

    /// Record a Unicode-placeholder (virtual) placement. The app positions the
    /// image via its `U+10EEEE` cells, so this neither anchors to a line nor
    /// advances the cursor; it just notes that the image has a virtual placement
    /// for the per-client renderer to transmit + emit `a=p,U=1` once.
    fn add_virtual_placement(&mut self, image_id: u32, placement_id: Option<u32>, r: Option<u16>, c: Option<u16>) {
        let placement_id = placement_id.unwrap_or(0);
        let seq = self.graphics_seq;
        self.graphics_seq = self.graphics_seq.wrapping_add(1);
        // Replace an existing virtual placement for the same (image, placement).
        self.virtual_placements
            .retain(|p| !(p.image_id == image_id && p.placement_id == placement_id));
        const MAX_VIRTUAL: usize = 1024;
        if self.virtual_placements.len() >= MAX_VIRTUAL {
            self.virtual_placements.remove(0);
        }
        self.virtual_placements.push(VirtualPlacement {
            image_id,
            placement_id,
            rows: r.unwrap_or(0),
            cols: c.unwrap_or(0),
            seq,
        });
    }

    /// Add a placement at the cursor and advance the cursor by its row footprint
    /// (so subsequent output lands below the image, the spike's overlap fix).
    fn add_placement(&mut self, image_id: u32, protocol: ImageProtocol, pixel_w: u32, pixel_h: u32, r: Option<u16>, c: Option<u16>) {
        let (cell_w, cell_h) = self.cell_pixels();
        let rows = r.unwrap_or_else(|| {
            pixel_h.div_ceil(u32::from(cell_h).max(1)).clamp(1, 1000) as u16
        });
        let cols = c.unwrap_or_else(|| {
            pixel_w.div_ceil(u32::from(cell_w).max(1)).clamp(1, 1000) as u16
        });
        let anchor_line = self.scrollback.len() as u32 + u32::from(self.cursor.row);
        self.placement_id_seq = self.placement_id_seq.wrapping_add(1);
        let placement_id = self.placement_id_seq;
        let seq = self.graphics_seq;
        self.graphics_seq = self.graphics_seq.wrapping_add(1);
        const MAX_PLACEMENTS: usize = 1024;
        if self.placements.len() >= MAX_PLACEMENTS {
            self.placements.remove(0);
        }
        self.placements.push(Placement {
            image_id,
            placement_id,
            protocol,
            anchor_line,
            col: self.cursor.col,
            rows,
            cols,
            seq,
        });
        for _ in 0..rows {
            self.advance_to_next_row(false);
        }
        self.cursor.col = 0;
    }

    /// Push a row into scrollback, keeping placement anchors valid: each evicted
    /// front row shifts every absolute `anchor_line` down by one, and placements
    /// whose anchor falls off the front (their rows left history) are dropped.
    fn push_scrollback(&mut self, row: crate::grid::Row) {
        let evicted = self.scrollback.push(row) as u32;
        if evicted == 0 || self.placements.is_empty() {
            return;
        }
        self.placements.retain_mut(|p| match p.anchor_line.checked_sub(evicted) {
            Some(a) => {
                p.anchor_line = a;
                true
            }
            None => false,
        });
    }

    /// Set the text-area pixel size relayed from the client's terminal.
    pub fn set_pixel_area(&mut self, w: u16, h: u16) {
        self.area_px_w = w;
        self.area_px_h = h;
    }

    /// Cell size in pixels `(width, height)`, derived from the text-area pixel
    /// size ÷ the current rows/cols. Falls back to a 10×20 default when the area
    /// is unknown so pixel-aware children always get a usable answer.
    pub fn cell_pixels(&self) -> (u16, u16) {
        const DEFAULT_CELL_W: u16 = 10;
        const DEFAULT_CELL_H: u16 = 20;
        let cols = self.cols().max(1);
        let rows = self.rows().max(1);
        if self.area_px_w == 0 || self.area_px_h == 0 {
            (DEFAULT_CELL_W, DEFAULT_CELL_H)
        } else {
            // max(1): a degenerate area smaller than the grid would otherwise
            // floor to 0 px/cell and blow the footprint up to the 1000-row clamp.
            ((self.area_px_w / cols).max(1), (self.area_px_h / rows).max(1))
        }
    }

    /// Set the `$TERM` this pane advertises via XTGETTCAP `TN`. The daemon calls
    /// this at spawn with the value the child actually inherits in its
    /// environment, so `TN` fingerprinting reflects reality.
    pub fn set_term(&mut self, term: String) {
        self.term = term;
    }

    /// Record the outer-terminal color scheme (true = dark) so a `\e[?996n`
    /// query answers the real preference. The daemon calls this when it relays a
    /// `\e[?997;Xn` from the client.
    pub fn set_color_scheme_dark(&mut self, dark: bool) {
        self.color_scheme_dark = dark;
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
    pub fn take_color_queries(&mut self) -> Vec<(usize, ColorQuery)> {
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
                    // The pad blank can land on a wide grapheme's spacer at the last
                    // column, so clean up the orphaned grapheme to its left.
                    self.clear_wide_straddle(self.cursor.col, self.cursor.col);
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

        // Overwriting one half of an existing wide grapheme destroys the whole
        // char (xterm/VTE): blank the orphaned half so the row stays well-formed.
        self.clear_wide_straddle(self.cursor.col, self.cursor.col + u16::from(w == 2));

        let cell = Cell {
            grapheme: cluster.into(),
            fg: self.cursor.fg,
            bg: self.cursor.bg,
            underline_color: self.cursor.underline_color,
            underline_style: self.cursor.underline_style,
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

    /// A grapheme about to be written into cols `[start, end]` can straddle an
    /// existing wide grapheme, and overwriting one half orphans the other. Blank
    /// the orphaned half on each boundary so the row stays well-formed (a width-2
    /// grapheme is always followed by its spacer; a spacer always follows one).
    /// Mirrors the erase path's `normalize_wide_pairs`, but O(1) for the hot
    /// print path: only the two straddled boundary cells can ever be orphaned.
    fn clear_wide_straddle(&mut self, start: u16, end: u16) {
        let row = self.cursor.row;
        // Left edge lands on a spacer, so its grapheme to the left is orphaned.
        if start > 0 && self.active.get_cell(row, start).is_some_and(|c| c.is_wide_spacer()) {
            self.active.put_cell(row, start - 1, Cell::default());
        }
        // Right edge lands on a wide grapheme → its spacer to the right is orphaned.
        if self
            .active
            .get_cell(row, end)
            .is_some_and(|c| crate::width::display_width(c.grapheme.as_str()) == 2)
        {
            self.active.put_cell(row, end + 1, Cell::default());
        }
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
    /// Resolve a 0-based row argument from CUP/HVP/VPA into an absolute grid
    /// row, honoring DEC origin mode (DECOM, `?6`): when set the argument is
    /// relative to the scroll-region top and confined to `[top, bottom]`;
    /// otherwise it is an absolute grid row clamped to the grid.
    fn absolute_row(&self, row_arg: u16) -> u16 {
        if self.modes.contains(crate::modes::Modes::ORIGIN) {
            let (top, bottom) = self.scroll_region;
            top.saturating_add(row_arg).min(bottom)
        } else {
            row_arg.min(self.rows().saturating_sub(1))
        }
    }

    pub fn advance_to_next_row(&mut self, soft_wrap: bool) {
        let (top, bottom) = self.scroll_region;
        if self.cursor.row == bottom {
            // At the bottom margin: scroll the region up by one. Only the
            // physical top line of the screen (region top at row 0) feeds
            // scrollback. A partial scroll region (DECSTBM top>0) scrolls an
            // interior region, and rows leaving the top of THAT region are
            // discarded (matching xterm/tmux/wezterm/VTE), not pushed into
            // scrollback, which would corrupt history/block marks.
            let mut popped: Vec<crate::grid::Row> = Vec::new();
            let target = if self.alt.is_none() && top == 0 {
                Some(&mut popped)
            } else {
                None
            };
            self.active.scroll_up(top, bottom, 1, target);
            for r in popped {
                self.push_scrollback(r);
            }
            // Stay at the bottom; new content goes there.
            self.cursor.row = bottom;
        } else if self.cursor.row > bottom {
            // Below the scroll region: a line feed moves the cursor down toward
            // the grid bottom WITHOUT scrolling the region (per xterm, the cursor
            // is outside the region, so the region's content is untouched).
            self.cursor.row = (self.cursor.row + 1).min(self.rows().saturating_sub(1));
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
                // CUU stops at the top scroll margin when the cursor is at or
                // below it (xterm), and at grid row 0 when above the region.
                let (top, _) = self.scroll_region;
                let floor = if self.cursor.row >= top { top } else { 0 };
                self.cursor.row = self.cursor.row.saturating_sub(n).max(floor);
                self.cursor.pending_wrap = false;
            }
            'B' => {
                let n = first.filter(|&n| n > 0).unwrap_or(1);
                // CUD stops at the bottom scroll margin when at or above it, and
                // at the last grid row when below the region.
                let (_, bottom) = self.scroll_region;
                let ceil = if self.cursor.row <= bottom {
                    bottom
                } else {
                    self.rows().saturating_sub(1)
                };
                self.cursor.row = self.cursor.row.saturating_add(n).min(ceil);
                self.cursor.pending_wrap = false;
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
                let row_arg = first.unwrap_or(1).saturating_sub(1);
                let col = nth(params, 1).unwrap_or(1).saturating_sub(1);
                let max_cols = self.cols();
                self.cursor.row = self.absolute_row(row_arg);
                self.cursor.col = col.min(max_cols.saturating_sub(1));
                self.cursor.pending_wrap = false;
            }
            'd' => {
                let row_arg = first.unwrap_or(1).saturating_sub(1);
                self.cursor.row = self.absolute_row(row_arg);
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
                // DECSTBM homes the cursor: to the region top in origin mode,
                // else the absolute top-left.
                let max_rows = self.rows();
                let max_cols = self.cols();
                let home_row = if self.modes.contains(crate::modes::Modes::ORIGIN) {
                    self.scroll_region.0
                } else {
                    0
                };
                self.cursor.move_to(home_row, 0, max_rows, max_cols);
            }
            'S' => {
                let n = first.filter(|&n| n > 0).unwrap_or(1);
                let (top, bottom) = self.scroll_region;
                // Only a top-anchored region on the main screen feeds scrollback
                // (see advance_to_next_row): interior-region scroll-out is
                // discarded, not retained.
                let mut popped = Vec::new();
                let target = if self.alt.is_none() && top == 0 {
                    Some(&mut popped)
                } else {
                    None
                };
                self.active.scroll_up(top, bottom, n, target);
                for r in popped {
                    self.push_scrollback(r);
                }
            }
            'T' => {
                let n = first.filter(|&n| n > 0).unwrap_or(1);
                let (top, bottom) = self.scroll_region;
                self.active.scroll_down(top, bottom, n);
            }
            '@' => {
                // ICH: insert N blank cells at the cursor, shifting right (overflow lost).
                let n = first.filter(|&n| n > 0).unwrap_or(1);
                self.active.insert_cells(self.cursor.row, self.cursor.col, n);
                self.cursor.pending_wrap = false;
            }
            'P' => {
                // DCH: delete N cells at the cursor, shifting left (blanks fill from right).
                let n = first.filter(|&n| n > 0).unwrap_or(1);
                self.active.delete_cells(self.cursor.row, self.cursor.col, n);
                self.cursor.pending_wrap = false;
            }
            'X' => {
                // ECH: erase N cells from the cursor (overwrite blank, no shift).
                let n = first.filter(|&n| n > 0).unwrap_or(1);
                let (r, c) = (self.cursor.row, self.cursor.col);
                let last = c.saturating_add(n - 1).min(self.cols().saturating_sub(1));
                self.active.clear_rect(r, c, r, last);
                self.cursor.pending_wrap = false;
            }
            'L' => {
                // IL: insert N blank lines at the cursor row within the scroll region.
                // Cursor homes to column 0 (DEC VT220 pg. reference; xterm matches).
                let n = first.filter(|&n| n > 0).unwrap_or(1);
                let (top, bottom) = self.scroll_region;
                if self.cursor.row >= top && self.cursor.row <= bottom {
                    self.active.scroll_down(self.cursor.row, bottom, n);
                    self.cursor.col = 0;
                    self.cursor.pending_wrap = false;
                }
            }
            'M' => {
                // DL: delete N lines at the cursor row within the scroll region.
                // Cursor homes to column 0 (DEC VT220 pg. reference; xterm matches).
                let n = first.filter(|&n| n > 0).unwrap_or(1);
                let (top, bottom) = self.scroll_region;
                if self.cursor.row >= top && self.cursor.row <= bottom {
                    self.active.scroll_up(self.cursor.row, bottom, n, None);
                    self.cursor.col = 0;
                    self.cursor.pending_wrap = false;
                }
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
                if intermediates.first() == Some(&b'?') {
                    // Private DSR. ?996 is the color-scheme query, answered with
                    // \e[?997;Pm n. Pm: 1 = dark (default), 2 = light. Answered
                    // regardless of the ?2031 subscription.
                    if mode == 996 {
                        // Pm: 1 = dark, 2 = light, from the daemon-tracked
                        // preference (set via `set_color_scheme_dark`), not a
                        // hardcoded default, so it agrees with the ?2031 push.
                        let pm = if self.color_scheme_dark { 1 } else { 2 };
                        self.replies.push(format!("\x1b[?997;{pm}n").into_bytes());
                    } else {
                        tracing::trace!(mode, "unhandled private DSR");
                    }
                } else {
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
            't' => {
                // Window-manipulation REPORTS only. We never honor child-driven
                // window resize/move/title ops (1..13, 20..24), since we are a
                // multiplexer. Size reports let pixel-aware children (image
                // viewers) scale to our real cell size, and cell size is derived
                // from the client-relayed pixel area (or a 10×20 fallback).
                let (cell_w, cell_h) = self.cell_pixels();
                let rows = self.rows();
                let cols = self.cols();
                match first.unwrap_or(0) {
                    14 => {
                        // Text area size in pixels → CSI 4 ; height ; width t
                        let h = cell_h.saturating_mul(rows);
                        let w = cell_w.saturating_mul(cols);
                        self.replies.push(format!("\x1b[4;{h};{w}t").into_bytes());
                    }
                    15 => {
                        // Screen size in pixels → CSI 5 ; height ; width t
                        let h = cell_h.saturating_mul(rows);
                        let w = cell_w.saturating_mul(cols);
                        self.replies.push(format!("\x1b[5;{h};{w}t").into_bytes());
                    }
                    16 => {
                        // Cell size in pixels -> CSI 6 ; height ; width t
                        self.replies.push(format!("\x1b[6;{cell_h};{cell_w}t").into_bytes());
                    }
                    18 => {
                        // Text area size in chars → CSI 8 ; rows ; cols t
                        self.replies.push(format!("\x1b[8;{rows};{cols}t").into_bytes());
                    }
                    19 => {
                        // Screen size in chars → CSI 9 ; rows ; cols t
                        self.replies.push(format!("\x1b[9;{rows};{cols}t").into_bytes());
                    }
                    other => {
                        tracing::trace!(op = other, "ignored window-manipulation op");
                    }
                }
            }
            'p' => {
                // `\e[?Ps$p` (private) and `\e[Ps$p` (ANSI) are DECRQM; `\e[!p` is
                // DECSTR. vte places BOTH the `?` private prefix and the `$`
                // intermediate into `intermediates`, so detect by membership.
                if intermediates.contains(&b'$') {
                    let private = intermediates.contains(&b'?');
                    self.handle_decrqm(params, private); // → \e[?Ps;Pm$y (private) / \e[Ps;Pm$y (ANSI)
                } else if intermediates.first() == Some(&b'!') {
                    self.handle_decstr(); // \e[!p, DECSTR soft reset
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
        self.cursor.underline_style = crate::attrs::UnderlineStyle::None;
        self.cursor.pending_wrap = false;
        self.saved_cursor = None;
    }

    /// DECRQM (`\e[?Ps$p` private / `\e[Ps$p` ANSI). Reply `\e[?Ps;Pm$y`
    /// (private) or `\e[Ps;Pm$y` (ANSI), `Pm` from the pane's `Modes`. Unknown
    /// modes are echoed with `Pm=0`. We only track private modes; ANSI modes
    /// always report `Pm=0`.
    fn handle_decrqm(&mut self, params: &vte::Params, private: bool) {
        let Some(ps) = params.iter().next().and_then(|p| p.first().copied()) else {
            return;
        };
        let pm = if private { self.modes.decrqm_state(ps) } else { 0 };
        let prefix = if private { "?" } else { "" };
        self.replies
            .push(format!("\x1b[{prefix}{ps};{pm}$y").into_bytes());
    }

    /// XTGETTCAP: handle a `+q` DCS payload (`;`-separated hex cap names).
    /// Push one DCS reply per cap (foot model: echo name, continue past
    /// failures), always terminated with ST (`\e\\`).
    fn xtgettcap(&mut self, payload: &[u8]) {
        use crate::terminfo::{Capability, hex_decode, hex_encode, lookup};
        let payload = match std::str::from_utf8(payload) {
            Ok(p) => p,
            Err(_) => {
                tracing::trace!("XTGETTCAP payload not UTF-8; ignoring");
                return;
            }
        };
        for hexname in payload.split(';') {
            // Skip empty segments (e.g. a trailing `;` or empty payload) so we
            // never emit a nameless `0+r` reply.
            if hexname.is_empty() {
                continue;
            }
            let Some(name_bytes) = hex_decode(hexname) else {
                tracing::trace!(hexname, "XTGETTCAP: bad hex name; skipping");
                continue;
            };
            let Ok(name) = std::str::from_utf8(&name_bytes) else {
                continue;
            };
            let name_hex = hex_encode(&name_bytes);
            let reply = match lookup(name, &self.term) {
                Capability::Boolean => format!("\x1bP1+r{name_hex}\x1b\\"),
                Capability::Num(n) => {
                    let val_hex = hex_encode(n.to_string().as_bytes());
                    format!("\x1bP1+r{name_hex}={val_hex}\x1b\\")
                }
                Capability::Str(s) => {
                    let val_hex = hex_encode(s.as_bytes());
                    format!("\x1bP1+r{name_hex}={val_hex}\x1b\\")
                }
                Capability::Unsupported => format!("\x1bP0+r{name_hex}\x1b\\"),
            };
            self.replies.push(reply.into_bytes());
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
            6 => {
                // DECOM (origin mode). Changing it homes the cursor: to the
                // scroll-region top when set, the absolute top-left when reset.
                if on {
                    self.modes.insert(Modes::ORIGIN);
                } else {
                    self.modes.remove(Modes::ORIGIN);
                }
                let home_row = if on { self.scroll_region.0 } else { 0 };
                let (max_rows, max_cols) = (self.rows(), self.cols());
                self.cursor.move_to(home_row, 0, max_rows, max_cols);
                return;
            }
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
            1002 => Modes::MOUSE_BTN_EVENT,
            1003 => Modes::MOUSE_ANY,
            1004 => Modes::FOCUS_EVENTS,
            1006 => Modes::MOUSE_SGR,
            2031 => Modes::COLOR_SCHEME_UPDATES,
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
        // No ANSI (non-private) modes are honored. IRM (mode 4) in particular is
        // deliberately unsupported: ICH/DCH/IL/DL/ECH are now implemented as
        // explicit CSI ops, but IRM auto-insert-per-put_grapheme is not. DECRQM
        // already reports IRM unsupported, so we must not flip an INSERT bit that
        // put_grapheme never reads, which would silently lie about insert-mode.
        tracing::trace!(code, on, "unhandled ANSI mode");
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
        // render wrong. Resolve the styled-underline group fully in place (set
        // the underline-style pen AND the `Attrs::UNDERLINE` boolean, pushing
        // NOTHING to `codes`), canonicalize the extended-color colon groups
        // (38:/48:/58:) by dropping the ISO colorspace-id slot, and flatten the
        // rest. Semicolon forms like `38;2;r;g;b` arrive as separate
        // single-element groups and flow through unchanged; colon forms are
        // canonicalized here so parse_extended_color (shared with the semicolon
        // path) sees an unambiguous element count. (Semicolon forms like `4;3`
        // arrive as separate single-element groups and are intentionally left as
        // "underline; italic".)
        let mut codes: Vec<u16> = Vec::new();
        for g in params.iter() {
            match g {
                // Styled underline `4:x`: resolve here, push nothing. style 0
                // clears the underline; any other code sets it with the mapped
                // style (unknown codes clamp to Single via from_sgr_subparam).
                [4, style, ..] => {
                    if *style == 0 {
                        self.cursor.attrs.remove(crate::attrs::Attrs::UNDERLINE);
                        self.cursor.underline_style = crate::attrs::UnderlineStyle::None;
                    } else {
                        self.cursor.attrs.insert(crate::attrs::Attrs::UNDERLINE);
                        self.cursor.underline_style =
                            crate::attrs::UnderlineStyle::from_sgr_subparam(*style);
                    }
                }
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
                    self.cursor.underline_style = crate::attrs::UnderlineStyle::None;
                }
                1 => self.cursor.attrs.insert(crate::attrs::Attrs::BOLD),
                2 => self.cursor.attrs.insert(crate::attrs::Attrs::DIM),
                3 => self.cursor.attrs.insert(crate::attrs::Attrs::ITALIC),
                4 => {
                    self.cursor.attrs.insert(crate::attrs::Attrs::UNDERLINE);
                    self.cursor.underline_style = crate::attrs::UnderlineStyle::Single;
                }
                5 => self.cursor.attrs.insert(crate::attrs::Attrs::BLINK),
                7 => self.cursor.attrs.insert(crate::attrs::Attrs::REVERSE),
                8 => self.cursor.attrs.insert(crate::attrs::Attrs::HIDDEN),
                9 => self.cursor.attrs.insert(crate::attrs::Attrs::STRIKETHROUGH),
                22 => {
                    self.cursor.attrs.remove(crate::attrs::Attrs::BOLD);
                    self.cursor.attrs.remove(crate::attrs::Attrs::DIM);
                }
                23 => self.cursor.attrs.remove(crate::attrs::Attrs::ITALIC),
                24 => {
                    self.cursor.attrs.remove(crate::attrs::Attrs::UNDERLINE);
                    self.cursor.underline_style = crate::attrs::UnderlineStyle::None;
                }
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
                // RIS (full reset). Preserve the pane's spawn identity (`term`,
                // for XTGETTCAP `TN`); a child's reset must not change the $TERM
                // it was launched with for the rest of the pane's life.
                let (rows, cols) = (self.rows(), self.cols());
                let term = std::mem::take(&mut self.term);
                let scheme_dark = self.color_scheme_dark;
                *self = Screen::new(rows, cols);
                self.term = term;
                self.color_scheme_dark = scheme_dark;
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
            b'D' => {
                // IND (index): line feed without carriage return. Mirrors the
                // LF/VT/FF C0 handler exactly (`execute_c0` 0x0A..=0x0C) so IND
                // and LF never diverge: scroll-region-correct downward move,
                // column preserved, pending soft-wrap cleared.
                self.advance_to_next_row(false);
                self.cursor.pending_wrap = false;
            }
            b'E' => {
                // NEL (next line) = CR + IND.
                self.cursor.col = 0;
                self.advance_to_next_row(false);
                self.cursor.pending_wrap = false;
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
            "1337" => self.handle_iterm(params),
            other => {
                tracing::trace!(cmd = other, "unhandled OSC");
            }
        }
    }

    /// iTerm2 inline image: `OSC 1337 ; File = <k=v;…> : <base64> ST`. `vte`
    /// splits on `;`, so the `File=` args come in spread across `params[1..]` and we
    /// rejoin them, parse, and (when `inline=1`) store an `Image{protocol:Iterm2}`
    /// plus an anchored placement. The base64 data + args are kept for re-emit.
    fn handle_iterm(&mut self, params: &[&[u8]]) {
        if self.alt.is_some() {
            return;
        }
        // Rejoin params after the "1337" command with ';'.
        let mut joined = Vec::new();
        for (i, p) in params.iter().skip(1).enumerate() {
            if i > 0 {
                joined.push(b';');
            }
            joined.extend_from_slice(p);
        }
        let rejoined = String::from_utf8_lossy(&joined);
        let Some((args, b64)) = crate::graphics::parse_iterm_file(&rejoined) else {
            return;
        };
        // Only render images marked for inline display.
        if !args.split(';').any(|kv| kv == "inline=1") {
            return;
        }
        let (w, h) = crate::graphics::iterm_dimensions(args, b64).unwrap_or((0, 0));
        self.image_id_seq = self.image_id_seq.wrapping_add(1);
        let id = self.image_id_seq;
        self.image_gen = self.image_gen.wrapping_add(1);
        let image = Image {
            id,
            protocol: ImageProtocol::Iterm2,
            format: ImageFormat::Png, // unused for iTerm2 re-emit
            pixel_w: w,
            pixel_h: h,
            data_b64: Arc::from(b64.as_bytes()),
            iterm_args: Some(Arc::from(args)),
            generation: self.image_gen,
        };
        let evicted = self.images.insert(image);
        if !evicted.is_empty() {
            self.placements.retain(|p| !evicted.contains(&p.image_id));
            self.virtual_placements.retain(|p| !evicted.contains(&p.image_id));
        }
        self.add_placement(id, ImageProtocol::Iterm2, w, h, None, None);
    }

    fn handle_osc_color_query(&mut self, params: &[&[u8]], query: ColorQuery) {
        // params[0] = "10"/"11"/"12", params[1] = payload.
        // Query form: payload is exactly "?".
        // Set form (e.g. payload = "#1d1c19"): ignored, the palette is daemon-controlled.
        let Some(payload) = params.get(1) else { return };
        if *payload == b"?" {
            // Record where this query sits relative to the raw replies emitted
            // so far, so the daemon re-interleaves the color reply in order.
            self.color_queries.push((self.replies.len(), query));
        } else {
            tracing::trace!(?query, "OSC color set form ignored (palette is daemon-controlled)");
        }
    }

    fn handle_osc_133(&mut self, params: &[&[u8]]) {
        // params[0] is "133", params[1] is the subcommand letter, optional
        // params[2..] carry sub-arguments (e.g. exit code for D).
        //
        // All four marks annotate the cursor ROW itself (`Row.mark`), so the
        // annotation travels with the row into scrollback, dies on eviction,
        // and survives reflow alongside `wrap_origin`.
        if self.alt.is_some() {
            // Block marks are meaningless on the alternate screen
            // (full-screen apps); ignore all four.
            return;
        }
        let Some(subcmd) = params.get(1).and_then(|p| p.first().copied()) else {
            return;
        };
        match subcmd {
            b'A' => {
                // A new prompt abandons any in-flight command start (a `C` with
                // no matching `D`), so it can't mis-attribute to the next block.
                self.pending_command_start = None;
                self.mark_cursor_row(|m| m.set(RowMark::PROMPT_START));
            }
            b'C' => {
                self.pending_command_start = Some(std::time::Instant::now());
                self.mark_cursor_row(|m| m.set(RowMark::OUTPUT_START));
            }
            b'D' => {
                let exit_code = params
                    .get(2)
                    .and_then(|p| std::str::from_utf8(p).ok())
                    .and_then(|s| s.parse::<i32>().ok());
                let duration_ms = self
                    .pending_command_start
                    .take()
                    .map(|start| start.elapsed().as_millis().min(u32::MAX as u128) as u32);
                // BLOCK_END is set even when the code is missing/malformed:
                // "last completed block" must not depend on a parseable code.
                self.mark_cursor_row(|m| {
                    m.set(RowMark::BLOCK_END);
                    m.set_exit(exit_code);
                    m.set_duration(duration_ms);
                });
                self.blocks_completed += 1;
                self.last_block_exit = exit_code;
                self.last_block_duration = duration_ms;
            }
            b'B' => {
                let col = self.cursor.col;
                self.mark_cursor_row(|m| m.set_prompt_end(col));
            }
            other => {
                tracing::trace!(subcmd = other, "unhandled OSC 133 subcommand");
            }
        }
    }

    /// Apply `f` to the cursor row's block annotation. Flag setting is `|=`,
    /// so re-marking (shells redraw prompts) is idempotent.
    fn mark_cursor_row(&mut self, f: impl FnOnce(&mut RowMark)) {
        if let Some(row) = self.active.rows.get_mut(self.cursor.row as usize) {
            f(&mut row.mark);
        }
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
    fn handle_dcs(&mut self, intermediates: &[u8], action: u8, params: &[Vec<u16>], payload: &[u8]) {
        // XTGETTCAP: DCS + q <hexnames> ST.
        if intermediates.first() == Some(&b'+') && action == b'q' {
            Screen::xtgettcap(self, payload);
        } else if action == b'q' && intermediates.is_empty() {
            // Sixel: DCS <params> q <data> ST (no `+`/`$` intermediate).
            Screen::handle_sixel(self, params, payload);
        } else {
            tracing::trace!(?intermediates, action = %(action as char), "unhandled DCS");
        }
    }
    fn handle_graphics(&mut self, framed: &[u8]) {
        Screen::handle_graphics(self, framed);
    }
}

/// Decode the PNG width/height from the base64 transmission prefix (first chunk
/// contains the PNG signature + IHDR). `None` if the prefix isn't a PNG header.
fn decode_png_dims(b64: &[u8]) -> Option<(u32, u32)> {
    use base64::Engine as _;
    let take = (b64.len() / 4 * 4).min(64);
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&b64[..take])
        .ok()?;
    crate::graphics::png_dimensions(&decoded)
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

    /// A 30×40px PNG as `a=T` chunked transmission: stored as an image, placed
    /// at the cursor (footprint ceil(30/10)=3 cols × ceil(40/20)=2 rows with the
    /// default 10×20 cell), and the cursor advances by 2 rows ONLY after the
    /// final (`m=0`) chunk.
    #[test]
    fn graphics_chunked_transmit_and_display_models_image_and_placement() {
        let mut png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&30u32.to_be_bytes());
        png.extend_from_slice(&40u32.to_be_bytes());
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let (a, b) = b64.split_at(28);
        let chunk1 = format!("\x1b_Ga=T,i=7,f=100,m=1;{a}\x1b\\");
        let chunk2 = format!("\x1b_Gm=0;{b}\x1b\\");

        let mut e = crate::Emulator::new(24, 80);
        e.advance(chunk1.as_bytes());
        assert_eq!(e.screen().cursor.row, 0, "no cursor move before the final chunk");
        assert!(e.screen().images.is_empty(), "not finalized yet");

        e.advance(chunk2.as_bytes());
        let s = e.screen();
        assert!(s.images.contains(7), "image stored on finalize");
        assert_eq!(s.placements.len(), 1);
        let p = &s.placements[0];
        assert_eq!(p.image_id, 7);
        assert_eq!(p.anchor_line, 0, "anchored at the image start");
        assert_eq!((p.cols, p.rows), (3, 2), "footprint from dims ÷ default cell");
        assert_eq!(s.cursor.row, 2, "cursor advanced by the footprint after m=0");
    }

    #[test]
    fn graphics_delete_by_id_removes_placements() {
        // Inline single-chunk image (s=/v= dims given), then delete it.
        let mut e = crate::Emulator::new(24, 80);
        e.advance(b"\x1b_Ga=T,i=9,f=24,s=20,v=20;QUJD\x1b\\");
        assert_eq!(e.screen().placements.len(), 1);
        e.advance(b"\x1b_Ga=d,i=9\x1b\\");
        assert!(e.screen().placements.is_empty(), "a=d,i=9 removed the placement");
    }

    #[test]
    fn graphics_not_captured_on_alt_screen() {
        let mut e = crate::Emulator::new(24, 80);
        e.advance(b"\x1b[?1049h"); // enter alt
        e.advance(b"\x1b_Ga=T,i=1,f=24,s=10,v=10;QQ\x1b\\");
        assert!(e.screen().placements.is_empty() && e.screen().images.is_empty());
    }

    #[test]
    fn placement_dropped_when_its_scrollback_row_evicts() {
        // Regression: anchor_line must follow scrollback eviction, not freeze.
        let mut e = crate::Emulator::new(3, 80);
        e.screen_mut().scrollback = crate::scrollback::Scrollback::with_cap(2);
        e.advance(b"\x1b_Ga=T,i=1,f=24,s=10,v=20;QQ\x1b\\"); // 1×1-cell image, anchor 0
        assert_eq!(e.screen().placements.len(), 1);
        // Scroll well past cap+grid so the anchored row leaves history entirely.
        for _ in 0..20 {
            e.advance(b"\r\n");
        }
        assert!(
            e.screen().placements.is_empty(),
            "placement dropped once its row evicts — no frozen ghost"
        );
    }

    #[test]
    fn abandoned_chunked_transmission_stores_nothing() {
        let mut e = crate::Emulator::new(24, 80);
        // m=1 with no following m=0 chunk: never finalized.
        e.advance(b"\x1b_Ga=T,i=3,f=24,s=2,v=2,m=1;QQ\x1b\\");
        assert!(e.screen().images.is_empty(), "no image stored without finalize");
        assert!(e.screen().placements.is_empty(), "no placement without finalize");
    }

    #[test]
    fn ris_clears_images_and_placements() {
        let mut e = crate::Emulator::new(24, 80);
        e.advance(b"\x1b_Ga=T,i=5,f=24,s=10,v=20;QQ\x1b\\");
        assert_eq!(e.screen().images.len(), 1);
        assert_eq!(e.screen().placements.len(), 1);
        e.advance(b"\x1bc"); // RIS
        assert!(e.screen().images.is_empty() && e.screen().placements.is_empty());
    }

    #[test]
    fn place_of_unknown_image_is_noop() {
        let mut e = crate::Emulator::new(24, 80);
        e.advance(b"\x1b_Ga=p,i=999\x1b\\"); // place an id never transmitted
        assert!(e.screen().placements.is_empty(), "no placement for unknown id");
        assert_eq!(e.screen().cursor.row, 0, "cursor untouched");
    }

    #[test]
    fn resize_drops_placements() {
        let mut e = crate::Emulator::new(24, 80);
        e.advance(b"\x1b_Ga=T,i=5,f=24,s=10,v=20;QQ\x1b\\");
        assert_eq!(e.screen().placements.len(), 1);
        e.resize(24, 100); // width change reflows; absolute anchors no longer valid
        assert!(e.screen().placements.is_empty(), "placements dropped on resize");
    }

    #[test]
    fn folded_mark_rides_reflow_and_drops_on_eviction() {
        // The fold flag lives on the prompt row's `RowMark`, so it rides reflow and
        // drops when the row evicts (spec D1). We set the bit directly here, not
        // through the mux fold policy.
        let mut e = crate::Emulator::new(4, 20);
        e.advance(b"\x1b]133;A\x07$ cmd\r\nout1\r\nout2\r\n");
        e.screen_mut().active.rows[0].mark.set_folded(true);
        assert!(e.screen().active.rows[0].mark.contains(RowMark::PROMPT_START));

        e.resize(4, 12); // reflow at a new width
        let folded_after_reflow = e
            .screen()
            .active
            .rows
            .iter()
            .chain(e.screen().scrollback.rows().iter())
            .any(|r| r.mark.contains(RowMark::PROMPT_START) && r.mark.is_folded());
        assert!(folded_after_reflow, "FOLDED rides reflow on the prompt row");

        // Tiny scrollback cap + scroll past → the folded prompt row evicts.
        e.screen_mut().scrollback = crate::scrollback::Scrollback::with_cap(1);
        for _ in 0..20 {
            e.advance(b"\r\n");
        }
        let still_folded = e
            .screen()
            .active
            .rows
            .iter()
            .chain(e.screen().scrollback.rows().iter())
            .any(|r| r.mark.is_folded());
        assert!(!still_folded, "fold drops when its prompt row is evicted");
    }

    #[test]
    fn unicode_placement_records_virtual_not_anchored() {
        let mut e = crate::Emulator::new(24, 80);
        // Transmit, then a virtual placement (U=1), positioned by the app's cells.
        e.advance(b"\x1b_Ga=t,i=7,f=24,s=10,v=20;QUJD\x1b\\");
        let row_before = e.screen().cursor.row;
        e.advance(b"\x1b_Ga=p,U=1,i=7,p=1,c=3,r=2\x1b\\");
        let s = e.screen();
        assert!(s.placements.is_empty(), "no anchored placement for U=1");
        assert_eq!(s.virtual_placements.len(), 1);
        let vp = &s.virtual_placements[0];
        assert_eq!((vp.image_id, vp.placement_id, vp.cols, vp.rows), (7, 1, 3, 2));
        assert_eq!(s.cursor.row, row_before, "virtual placement does not move the cursor");
    }

    #[test]
    fn transmit_and_virtual_place_in_one_command() {
        let mut e = crate::Emulator::new(24, 80);
        e.advance(b"\x1b_Ga=T,U=1,i=9,f=24,s=10,v=20,c=2,r=1;QUJD\x1b\\");
        let s = e.screen();
        assert!(s.images.contains(9), "image stored");
        assert!(s.placements.is_empty(), "no anchored placement");
        assert_eq!(s.virtual_placements.len(), 1);
        assert_eq!(s.cursor.row, 0, "no cursor advance for a virtual placement");
    }

    #[test]
    fn delete_clears_virtual_placements() {
        let mut e = crate::Emulator::new(24, 80);
        e.advance(b"\x1b_Ga=T,U=1,i=9,f=24,s=10,v=20,c=2,r=1;QUJD\x1b\\");
        assert_eq!(e.screen().virtual_placements.len(), 1);
        e.advance(b"\x1b_Ga=d,i=9\x1b\\");
        assert!(e.screen().virtual_placements.is_empty(), "a=d,i=9 cleared the virtual placement");
    }

    #[test]
    fn ris_clears_virtual_placements() {
        let mut e = crate::Emulator::new(24, 80);
        e.advance(b"\x1b_Ga=T,U=1,i=9,f=24,s=10,v=20,c=2,r=1;QUJD\x1b\\");
        assert_eq!(e.screen().virtual_placements.len(), 1);
        e.advance(b"\x1bc");
        assert!(e.screen().virtual_placements.is_empty());
    }

    #[test]
    fn transmit_only_with_unicode_flag_does_not_place() {
        // a=t (lowercase, transmit only) with U=1 must just store the image,
        // since only a display (a=T) or an explicit a=p places.
        let mut e = crate::Emulator::new(24, 80);
        e.advance(b"\x1b_Ga=t,U=1,i=9,f=24,s=10,v=20;QUJD\x1b\\");
        let s = e.screen();
        assert!(s.images.contains(9), "image stored");
        assert!(s.virtual_placements.is_empty(), "a=t,U=1 does not place");
        assert!(s.placements.is_empty());
    }

    #[test]
    fn sixel_dcs_captured_as_image_and_placement() {
        let mut e = crate::Emulator::new(24, 80);
        // Raster 10×20px → 1×1 cell footprint at the default 10×20 cell.
        e.advance(b"\x1bPq\"1;1;10;20#0;2;0;0;0~~~\x1b\\");
        let s = e.screen();
        assert_eq!(s.images.len(), 1, "sixel stored as an image");
        assert_eq!(s.placements.len(), 1);
        assert_eq!(s.placements[0].protocol, ImageProtocol::Sixel);
        assert_eq!(s.cursor.row, 1, "cursor advanced by the 1-row footprint");
        // The stored payload is re-emittable: <params>q<data>, ending in the
        // sixel data, with the raster attrs intact right after `q`.
        let img = s.images.get(s.placements[0].image_id).unwrap();
        assert!(img.data_b64.windows(2).any(|w| w == b"q\""), "q + raster preserved");
        assert!(img.data_b64.ends_with(b"~~~"), "sixel data preserved");
    }

    #[test]
    fn sixel_not_captured_on_alt_screen() {
        let mut e = crate::Emulator::new(24, 80);
        e.advance(b"\x1b[?1049h");
        e.advance(b"\x1bPq\"1;1;10;20#0;2;0;0;0~~~\x1b\\");
        assert!(e.screen().images.is_empty() && e.screen().placements.is_empty());
    }

    #[test]
    fn iterm2_osc_captured_as_image_and_placement() {
        let mut e = crate::Emulator::new(24, 80);
        // vte splits the File= args on ';' across OSC params; handle_iterm rejoins.
        e.advance(b"\x1b]1337;File=inline=1;width=10px;height=20px:QUJD\x07");
        let s = e.screen();
        assert_eq!(s.images.len(), 1, "iterm2 image stored");
        assert_eq!(s.placements.len(), 1);
        assert_eq!(s.placements[0].protocol, ImageProtocol::Iterm2);
        assert_eq!(s.cursor.row, 1, "cursor advanced by the 1-row footprint");
        let img = s.images.get(s.placements[0].image_id).unwrap();
        assert_eq!(img.data_b64.as_ref(), b"QUJD", "base64 data kept for re-emit");
        assert_eq!(img.iterm_args.as_deref(), Some("inline=1;width=10px;height=20px"));
    }

    #[test]
    fn iterm2_non_inline_is_ignored() {
        let mut e = crate::Emulator::new(24, 80);
        e.advance(b"\x1b]1337;File=inline=0;width=10px:QUJD\x07");
        assert!(e.screen().images.is_empty(), "a non-inline File= is a download, not rendered");
    }

    #[test]
    fn iterm2_empty_or_malformed_params_ignored() {
        let mut e = crate::Emulator::new(24, 80);
        e.advance(b"\x1b]1337\x07"); // bare 1337, no File=
        e.advance(b"\x1b]1337;File=\x07"); // File= but no `:data`
        e.advance(b"\x1b]1337;Foo=bar\x07"); // not a File=
        assert!(e.screen().images.is_empty(), "malformed/empty 1337 captures nothing");
    }

    #[test]
    fn sixel_without_raster_attrs_uses_fallback_dims() {
        let mut e = crate::Emulator::new(24, 80);
        // No `"` raster: 3 data columns on one band → 3×6 px → 1×1 cell.
        e.advance(b"\x1bPq#0;2;0;0;0~~~\x1b\\");
        let s = e.screen();
        assert_eq!(s.images.len(), 1);
        assert_eq!(s.placements.len(), 1, "captured via the fallback scan");
        assert_eq!(s.placements[0].protocol, ImageProtocol::Sixel);
    }

    #[test]
    fn dcs_queries_are_not_captured_as_sixel() {
        let mut e = crate::Emulator::new(24, 80);
        e.advance(b"\x1bP$qm\x1b\\"); // DECRQSS
        e.advance(b"\x1bP+q544e\x1b\\"); // XTGETTCAP
        assert!(e.screen().images.is_empty(), "DCS queries ($q / +q) are not sixel");
    }

    #[test]
    fn virtual_placements_survive_resize() {
        // Unlike anchored placements (dropped on resize), virtual placements ride
        // the placeholder cells, which reflow, so they must NOT be dropped.
        let mut e = crate::Emulator::new(24, 80);
        e.advance(b"\x1b_Ga=T,U=1,i=9,f=24,s=10,v=20,c=2,r=1;QUJD\x1b\\");
        assert_eq!(e.screen().virtual_placements.len(), 1);
        e.resize(24, 100);
        assert_eq!(e.screen().virtual_placements.len(), 1, "virtual placement survives resize");
    }

    #[test]
    fn retransmit_same_id_bumps_generation() {
        let mut e = crate::Emulator::new(24, 80);
        e.advance(b"\x1b_Ga=t,i=5,f=24,s=10,v=20;QQ\x1b\\"); // transmit only
        let g1 = e.screen().images.get(5).expect("stored").generation;
        e.advance(b"\x1b_Ga=t,i=5,f=24,s=10,v=20;Qg\x1b\\"); // re-transmit same id, new data
        let g2 = e.screen().images.get(5).expect("stored").generation;
        assert!(g2 > g1, "re-transmit of an existing id bumps the content generation");
    }

    #[test]
    fn csi_16t_reports_cell_pixels_from_area() {
        // 1600x960 area over 80x24 cells → 20x40 px cells.
        let mut e = crate::Emulator::new(24, 80);
        e.set_pixel_area(1600, 960);
        e.advance(b"\x1b[16t");
        // CSI 6 ; height ; width t
        assert_eq!(e.take_replies(), vec![b"\x1b[6;40;20t".to_vec()]);
    }

    #[test]
    fn csi_16t_falls_back_to_default_cell_when_area_unknown() {
        let mut e = crate::Emulator::new(24, 80); // no set_pixel_area
        e.advance(b"\x1b[16t");
        assert_eq!(e.take_replies(), vec![b"\x1b[6;20;10t".to_vec()], "fallback 10x20");
    }

    #[test]
    fn csi_14t_reports_text_area_pixels() {
        let mut e = crate::Emulator::new(24, 80);
        e.set_pixel_area(1600, 960);
        e.advance(b"\x1b[14t");
        // CSI 4 ; height ; width t: height = 40*24 = 960, width = 20*80 = 1600.
        assert_eq!(e.take_replies(), vec![b"\x1b[4;960;1600t".to_vec()]);
    }

    #[test]
    fn csi_18t_reports_text_area_chars() {
        let mut e = crate::Emulator::new(24, 80);
        e.advance(b"\x1b[18t");
        assert_eq!(e.take_replies(), vec![b"\x1b[8;24;80t".to_vec()]);
    }

    #[test]
    fn csi_t_window_ops_are_ignored() {
        // A child asking to RESIZE the window (CSI 8 ; rows ; cols t) or move it
        // must produce no reply (we're a multiplexer).
        let mut e = crate::Emulator::new(24, 80);
        e.advance(b"\x1b[8;40;100t"); // resize request
        e.advance(b"\x1b[3;0;0t"); // move window
        assert!(e.take_replies().is_empty(), "window-manipulation ops are ignored");
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
    fn overwrite_spacer_half_of_wide_char_clears_grapheme() {
        // "好x" → 好@0-1, x@2. CUP to col 1 (the spacer), write 'a'. Overwriting
        // half a wide char destroys the whole char (xterm/VTE semantics), so 好's
        // orphaned grapheme cell at col 0 is blanked too.
        let s = drive("好x\x1b[1;2Ha".as_bytes());
        assert!(s.active.get_cell(0, 0).unwrap().is_blank(), "好's grapheme half not cleared");
        assert_eq!(s.active.get_cell(0, 1).unwrap().grapheme.as_str(), "a");
        assert_eq!(s.active.get_cell(0, 2).unwrap().grapheme.as_str(), "x");
    }

    #[test]
    fn overwrite_grapheme_half_of_wide_char_clears_spacer() {
        // "好x" → 好@0-1, x@2. CUP to col 0 (the grapheme), write 'a'. The now-
        // orphaned spacer at col 1 is blanked (not left dangling).
        let s = drive("好x\x1b[1;1Ha".as_bytes());
        assert_eq!(s.active.get_cell(0, 0).unwrap().grapheme.as_str(), "a");
        let c1 = s.active.get_cell(0, 1).unwrap();
        assert!(c1.is_blank() && !c1.is_wide_spacer(), "orphaned spacer not cleared");
        assert_eq!(s.active.get_cell(0, 2).unwrap().grapheme.as_str(), "x");
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
    fn cuu_stops_at_top_scroll_margin() {
        // Region rows 3..7 (top=2). Cursor at row 5, CUU 10 stops at the margin.
        let s = parse(b"\x1b[3;7r\x1b[6;1H\x1b[10A");
        assert_eq!(s.cursor.row, 2, "CUU must stop at the top margin, not grid 0");
    }

    #[test]
    fn cud_stops_at_bottom_scroll_margin() {
        // Region rows 3..7 (bottom=6). Cursor at row 3, CUD 10 stops at the margin.
        let s = parse(b"\x1b[3;7r\x1b[4;1H\x1b[10B");
        assert_eq!(s.cursor.row, 6, "CUD must stop at the bottom margin, not the grid edge");
    }

    #[test]
    fn line_feed_below_bottom_margin_moves_down_without_scrolling() {
        // Region rows 1..3 (bottom=2). Place the cursor at row 4 (below it); a LF
        // must move it DOWN to row 5, not snap it up to the bottom margin.
        let s = parse(b"\x1b[1;3r\x1b[5;1H\n");
        assert_eq!(s.cursor.row, 5, "LF below the region must move down, not snap to the margin");
    }

    #[test]
    fn decom_makes_cup_region_relative_and_homes() {
        // Region rows 5..8 (top=4). DECSET ?6h homes to the region top...
        let s = parse(b"\x1b[5;8r\x1b[?6h");
        assert_eq!(s.cursor.row, 4, "DECOM set homes the cursor to the region top");
        // ...and CUP rows become relative to the region top.
        let s = parse(b"\x1b[5;8r\x1b[?6h\x1b[3;1H");
        assert_eq!(s.cursor.row, 6, "CUP row 3 in origin mode = top(4) + 2");
        // Reset returns to absolute addressing.
        let s = parse(b"\x1b[5;8r\x1b[?6h\x1b[?6l\x1b[3;1H");
        assert_eq!(s.cursor.row, 2, "CUP row 3 with origin reset = absolute grid row 2");
    }

    #[test]
    fn claude_code_startup_replay_has_no_spurious_underline() {
        // Regression (spec G8): the original bug misrouted Claude Code's
        // XTMODKEYS `\e[>4;2m` to SGR, reading it as `4;2` = underline + dim and
        // applying that to every subsequently painted cell. On this real
        // captured Claude Code startup stream that underlined 438/438 non-blank
        // cells. Deleting the 7-byte `\e[>4;2m` (or, as shipped, guarding CSI-m
        // on intermediates) fixes it. This replays the capture and asserts NO
        // non-blank cell carries an underline.
        //
        // We assert on UNDERLINE only, not DIM: the capture legitimately emits
        // `\e[2m` (and `\e[22m` to reset) for some text, so dim is ambiguous,
        // but Claude Code emits no bare `\e[4m` at all and no `\e[24m`/`\e[0m`
        // underline reset, so any underlined cell here is the bug's signature.
        const STREAM: &[u8] = include_bytes!("../testdata/claude-code-startup.raw");
        let mut p = crate::parser::Parser::new();
        // Sized wider than the capture's furthest column move (`\e[152G`) so the
        // splash never wraps; over-sizing only leaves extra blank cells.
        let mut s = Screen::new(50, 200);
        p.advance(&mut s, STREAM);
        p.flush(&mut s);

        let mut non_blank = 0usize;
        let mut underlined = 0usize;
        for r in 0..s.rows() {
            for c in 0..s.cols() {
                let Some(cell) = s.active.get_cell(r, c) else { continue };
                if cell.is_blank() || cell.is_wide_spacer() {
                    continue;
                }
                non_blank += 1;
                if cell.attrs.contains(crate::attrs::Attrs::UNDERLINE)
                    || cell.underline_style != crate::attrs::UnderlineStyle::None
                {
                    underlined += 1;
                }
            }
        }
        assert!(non_blank > 0, "fixture produced no visible cells — wrong screen size or empty capture");
        assert_eq!(
            underlined, 0,
            "{underlined}/{non_blank} non-blank cells spuriously underlined (the `\\e[>4;2m`-as-SGR bug)"
        );
    }

    #[test]
    fn xtgettcap_numeric_cap_colors() {
        // \eP+q636f6c6f7273\e\\ queries "colors" → \eP1+r636f6c6f7273=323536\e\\.
        let s = parse(b"\x1bP+q636f6c6f7273\x1b\\X");
        assert_eq!(s.replies, vec![b"\x1bP1+r636f6c6f7273=323536\x1b\\".to_vec()]);
    }

    #[test]
    fn set_term_then_xtgettcap_tn_reports_it() {
        // TN (544e) reports the term we were spawned with.
        let mut p = crate::parser::Parser::new();
        let mut s = Screen::new(4, 8);
        s.set_term("xterm-ghostty".into());
        p.advance(&mut s, b"\x1bP+q544e\x1b\\X");
        p.flush(&mut s);
        let hex = crate::terminfo::hex_encode(b"xterm-ghostty");
        assert_eq!(s.replies, vec![format!("\x1bP1+r544e={hex}\x1b\\").into_bytes()]);
    }

    #[test]
    fn ris_preserves_term() {
        // RIS (\ec) must keep the spawn-time $TERM (pane spawn identity).
        let mut p = crate::parser::Parser::new();
        let mut s = Screen::new(4, 8);
        s.set_term("xterm-ghostty".into());
        p.advance(&mut s, b"\x1bcX");
        p.flush(&mut s);
        assert_eq!(s.term, "xterm-ghostty");
    }

    #[test]
    fn xtgettcap_boolean_cap_su() {
        // "Su" (5375) is a value-less boolean → \eP1+r5375\e\\ (no =value).
        let s = parse(b"\x1bP+q5375\x1b\\X");
        assert_eq!(s.replies, vec![b"\x1bP1+r5375\x1b\\".to_vec()]);
    }

    #[test]
    fn xtgettcap_unsupported_cap_echoes_name() {
        // Unknown "Xx" (5878) → \eP0+r5878\e\\ (echo name, continue).
        let s = parse(b"\x1bP+q5878\x1b\\X");
        assert_eq!(s.replies, vec![b"\x1bP0+r5878\x1b\\".to_vec()]);
    }

    #[test]
    fn xtgettcap_setulc_is_supported() {
        // "Setulc" (536574756c63) → 1+r with the parameterized hex value.
        let s = parse(b"\x1bP+q536574756c63\x1b\\X");
        let val = "\\E[58:2:%p1%{65536}%/%d:%p1%{256}%/%{255}%&%d:%p1%{255}%&%d%;m";
        let expected = format!(
            "\x1bP1+r536574756c63={}\x1b\\",
            crate::terminfo::hex_encode(val.as_bytes())
        )
        .into_bytes();
        assert_eq!(s.replies, vec![expected]);
    }

    #[test]
    fn xtgettcap_multiple_caps_one_reply_each() {
        // \eP+q<colors>;<Su>\e\\ → two replies, in order.
        let s = parse(b"\x1bP+q636f6c6f7273;5375\x1b\\X");
        assert_eq!(
            s.replies,
            vec![
                b"\x1bP1+r636f6c6f7273=323536".to_vec(),
                // first reply terminated with ST, then the second:
            ]
            .into_iter()
            .map(|mut v: Vec<u8>| {
                v.extend_from_slice(b"\x1b\\");
                v
            })
            .chain(std::iter::once({
                let mut v = b"\x1bP1+r5375".to_vec();
                v.extend_from_slice(b"\x1b\\");
                v
            }))
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn xtgettcap_trailing_semicolon_emits_no_empty_reply() {
        // A trailing `;` yields an empty split segment; it must be skipped, not
        // answered with a nameless \eP0+r\e\\.
        let s = parse(b"\x1bP+q636f6c6f7273;\x1b\\X");
        assert_eq!(s.replies, vec![b"\x1bP1+r636f6c6f7273=323536\x1b\\".to_vec()]);
    }

    #[test]
    fn xtgettcap_c1_st_terminated_request_accepted() {
        // A DCS may be terminated by the C1 ST byte (0x9C) as well as ESC '\'.
        // (Unlike OSC, a DCS is NOT terminated by BEL. vte routes BEL into the
        // DCS payload, matching the VT spec, and only ST/CAN/SUB/ESC end a DCS.)
        // Our reply still terminates with \e\\.
        let s = parse(b"\x1bP+q5375\x9cX");
        assert_eq!(s.replies, vec![b"\x1bP1+r5375\x1b\\".to_vec()]);
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
    fn underline_style_curly_sets_style_and_underline() {
        use crate::attrs::{Attrs, UnderlineStyle};
        // \e[4:3m must record Curly style AND set the UNDERLINE boolean (and must
        // NOT set italic, cross-checking `styled_underline_colon_subparams`).
        let s = parse(b"\x1b[4:3mX");
        let c = s.active.get_cell(0, 0).unwrap();
        assert_eq!(c.underline_style, UnderlineStyle::Curly, "4:3 = curly");
        assert!(c.attrs.contains(Attrs::UNDERLINE), "UNDERLINE boolean still set");
        assert!(!c.attrs.contains(Attrs::ITALIC), "4:3 is not SGR 3");
    }

    #[test]
    fn underline_style_double_dotted_dashed() {
        use crate::attrs::{Attrs, UnderlineStyle};
        let s = parse(b"\x1b[4:2mD\x1b[4:4mO\x1b[4:5mA");
        let d = s.active.get_cell(0, 0).unwrap();
        assert_eq!(d.underline_style, UnderlineStyle::Double, "4:2 = double");
        assert!(d.attrs.contains(Attrs::UNDERLINE));
        let o = s.active.get_cell(0, 1).unwrap();
        assert_eq!(o.underline_style, UnderlineStyle::Dotted, "4:4 = dotted");
        let a = s.active.get_cell(0, 2).unwrap();
        assert_eq!(a.underline_style, UnderlineStyle::Dashed, "4:5 = dashed");
    }

    #[test]
    fn underline_style_off_clears_style_and_underline() {
        use crate::attrs::{Attrs, UnderlineStyle};
        // \e[4:0m turns underline OFF: style `None` and the boolean cleared.
        let s = parse(b"\x1b[4:3mX\x1b[4:0mY");
        let y = s.active.get_cell(0, 1).unwrap();
        assert_eq!(y.underline_style, UnderlineStyle::None, "4:0 = none");
        assert!(!y.attrs.contains(Attrs::UNDERLINE), "4:0 clears UNDERLINE");
    }

    #[test]
    fn plain_underline_sets_single_style() {
        use crate::attrs::{Attrs, UnderlineStyle};
        // Plain \e[4m is Single + UNDERLINE; \e[24m clears to None.
        let s = parse(b"\x1b[4mX\x1b[24mY");
        let x = s.active.get_cell(0, 0).unwrap();
        assert_eq!(x.underline_style, UnderlineStyle::Single, "plain 4 = single");
        assert!(x.attrs.contains(Attrs::UNDERLINE));
        let y = s.active.get_cell(0, 1).unwrap();
        assert_eq!(y.underline_style, UnderlineStyle::None, "24 = none");
        assert!(!y.attrs.contains(Attrs::UNDERLINE));
    }

    #[test]
    fn sgr_reset_clears_underline_style() {
        use crate::attrs::UnderlineStyle;
        // \e[0m must reset the style pen back to `None`.
        let s = parse(b"\x1b[4:3mX\x1b[0mY");
        assert_eq!(
            s.active.get_cell(0, 1).unwrap().underline_style,
            UnderlineStyle::None,
            "\\e[0m resets the underline style"
        );
    }

    #[test]
    fn underline_style_and_color_are_independent() {
        use crate::attrs::UnderlineStyle;
        use crate::color::Color;
        // SGR 58 (color) and 4:3 (style) on the same cell: both survive, distinct.
        let s = parse(b"\x1b[58:5:9m\x1b[4:3mX");
        let c = s.active.get_cell(0, 0).unwrap();
        assert_eq!(c.underline_style, UnderlineStyle::Curly, "style = curly");
        assert_eq!(c.underline_color, Color::Indexed(9), "color = indexed 9");
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
    fn partial_region_scroll_does_not_pollute_scrollback() {
        // A DECSTBM region with a non-zero top margin scrolls an INTERIOR
        // region; rows leaving the top of that region must be discarded, not
        // pushed into scrollback (which would corrupt command history / marks).
        let mut p = crate::parser::Parser::new();
        let mut s = Screen::new(4, 4);
        p.advance(&mut s, b"\x1b[2;4r"); // scroll_region = (1, 3), top margin > 0
        p.advance(&mut s, b"\x1b[2;1H"); // cursor to the region top
        p.advance(&mut s, b"L1\nL2\nL3\nL4\nL5"); // overflow the region via LF
        p.flush(&mut s);
        assert!(
            s.scrollback.is_empty(),
            "interior-region scroll must not feed scrollback, got {} rows",
            s.scrollback.rows().len()
        );

        // Same via CSI S (SU) on a top>0 region.
        let mut s2 = Screen::new(4, 4);
        p.advance(&mut s2, b"\x1b[2;4r\x1b[2;1HXXXX\x1b[S\x1b[S\x1b[S");
        p.flush(&mut s2);
        assert!(s2.scrollback.is_empty());

        // Sanity: a full-screen (top==0) region still feeds scrollback.
        let mut s3 = Screen::new(2, 4);
        p.advance(&mut s3, b"AAAA\nBBBB\nCCCC");
        p.flush(&mut s3);
        assert!(!s3.scrollback.is_empty());
    }

    #[test]
    fn irm_insert_mode_is_not_honored() {
        // IRM (ANSI mode 4) is deliberately unsupported: enabling it must not
        // shift cells, since writing overwrites at the cursor.
        let mut p = crate::parser::Parser::new();
        let mut s = Screen::new(2, 8);
        p.advance(&mut s, b"abc");
        p.advance(&mut s, b"\x1b[H"); // home (0,0)
        p.advance(&mut s, b"\x1b[4h"); // enable IRM (must be ignored)
        p.advance(&mut s, b"X");
        p.flush(&mut s);
        // Overwrite, not insert: row 0 is "Xbc" (insert mode would give "Xabc").
        assert_eq!(s.active.get_cell(0, 0).unwrap().grapheme.as_str(), "X");
        assert_eq!(s.active.get_cell(0, 1).unwrap().grapheme.as_str(), "b");
        assert_eq!(s.active.get_cell(0, 2).unwrap().grapheme.as_str(), "c");
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
        assert!(s.active.rows.iter().all(|r| r.mark.is_empty()));
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
    fn osc_133_a_sets_prompt_start_flag_on_cursor_row() {
        // Cursor sits on row 1 when A arrives; the flag lands on THAT row.
        let s = parse(b"\r\n\x1b]133;A\x07");
        let mark = s.active.rows[1].mark;
        assert!(mark.contains(RowMark::PROMPT_START));
        assert!(!mark.contains(RowMark::OUTPUT_START));
        assert!(!mark.contains(RowMark::BLOCK_END));
        assert_eq!(mark.exit(), None);
        assert!(s.active.rows[0].mark.is_empty(), "row 0 must stay unmarked");
    }

    #[test]
    fn osc_133_c_sets_output_start_flag_on_cursor_row() {
        let s = parse(b"\x1b]133;C\x07");
        let mark = s.active.rows[0].mark;
        assert!(mark.contains(RowMark::OUTPUT_START));
        assert!(!mark.contains(RowMark::PROMPT_START));
    }

    #[test]
    fn osc_133_prompt_end_sets_row_mark_and_col() {
        // "abc" moves cursor to col 3; B records that col on the row.
        let s = parse(b"abc\x1b]133;B\x07");
        let mark = s.active.rows[0].mark;
        assert!(mark.contains(RowMark::PROMPT_END), "PROMPT_END flag must be set");
        assert_eq!(mark.prompt_end_col(), Some(3), "col must equal cursor col at B time");
    }

    #[test]
    fn osc_133_command_end_carries_exit_code() {
        let s = parse(b"\x1b]133;D;42\x07");
        let mark = s.active.rows[0].mark;
        assert!(mark.contains(RowMark::BLOCK_END));
        assert_eq!(mark.exit(), Some(42));
    }

    #[test]
    fn duration_recorded_from_c_then_d() {
        // C ... D on its own row; the D row carries a duration (value unasserted).
        let s = parse(b"\x1b]133;A\x07$ cmd\r\n\x1b]133;C\x07out\r\n\x1b]133;D;0\x07x");
        let d_row = s
            .active
            .rows
            .iter()
            .find(|r| r.mark.contains(RowMark::BLOCK_END))
            .expect("a BLOCK_END row");
        assert!(d_row.mark.duration_ms().is_some(), "C->D must record a duration");
    }

    #[test]
    fn duration_none_without_c() {
        let s = parse(b"\x1b]133;A\x07$ cmd\r\n\x1b]133;D;0\x07x");
        let d_row = s
            .active
            .rows
            .iter()
            .find(|r| r.mark.contains(RowMark::BLOCK_END))
            .expect("a BLOCK_END row");
        assert_eq!(d_row.mark.duration_ms(), None, "D without C has no duration");
    }

    #[test]
    fn prompt_start_clears_dangling_command_start() {
        // C with no D, then a new prompt A, then a D with no fresh C: no duration.
        let s = parse(b"\x1b]133;C\x07out\r\n\x1b]133;A\x07$ cmd\r\n\x1b]133;D;0\x07x");
        let d_row = s
            .active
            .rows
            .iter()
            .find(|r| r.mark.contains(RowMark::BLOCK_END))
            .expect("a BLOCK_END row");
        assert_eq!(d_row.mark.duration_ms(), None, "A must clear a dangling start");
    }

    #[test]
    fn osc_133_command_end_without_exit_code() {
        // D with no code: still a block end, exit unknown.
        let s = parse(b"\x1b]133;D\x07");
        let mark = s.active.rows[0].mark;
        assert!(mark.contains(RowMark::BLOCK_END));
        assert_eq!(mark.exit(), None);
    }

    #[test]
    fn osc_133_command_end_malformed_code_still_marks_block_end() {
        let s = parse(b"\x1b]133;D;xyz\x07");
        let mark = s.active.rows[0].mark;
        assert!(mark.contains(RowMark::BLOCK_END));
        assert_eq!(mark.exit(), None);
    }

    #[test]
    fn osc_133_remark_is_idempotent() {
        // Shells redraw prompts: re-marking the same row must not change it.
        let once = parse(b"\x1b]133;A\x07");
        let twice = parse(b"\x1b]133;A\x07\x1b]133;A\x07");
        assert_eq!(once.active.rows[0].mark, twice.active.rows[0].mark);
        assert!(twice.active.rows[0].mark.contains(RowMark::PROMPT_START));
    }

    #[test]
    fn osc_133_ignored_on_alt_screen() {
        // All four marks while the alt screen is active: no row flags on either grid.
        let s = parse(b"\x1b[?1049h\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07\x1b]133;D;0\x07\x1b[?1049l");
        assert!(s.alt.is_none(), "test must end back on the main screen");
        for row in &s.active.rows {
            assert!(row.mark.is_empty(), "main grid must be unmarked");
        }
    }

    #[test]
    fn prompt_end_b_sets_prompt_end_flag_on_cursor_row() {
        // All four 133 marks now live on the row.
        let s = parse(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07\x1b]133;D;0\x07");
        let mark = s.active.rows[0].mark;
        assert!(mark.contains(RowMark::PROMPT_START), "A must set PROMPT_START");
        assert!(mark.contains(RowMark::PROMPT_END), "B must set PROMPT_END");
        assert!(mark.contains(RowMark::OUTPUT_START), "C must set OUTPUT_START");
        assert!(mark.contains(RowMark::BLOCK_END), "D must set BLOCK_END");
    }

    #[test]
    fn prompt_end_mark_rides_into_scrollback_with_row() {
        // B lands on row 1, then a linefeed scrolls everything up (2-row screen).
        // The row carrying PROMPT_END travels into scrollback.
        let mut p = crate::parser::Parser::new();
        let mut s = Screen::new(2, 8);
        // "b" + B puts cursor on row 1 at col 1; then one linefeed scrolls the grid.
        p.advance(&mut s, b"a\r\nb\x1b]133;B\x07\r\nc");
        p.flush(&mut s);
        // Row 1's content ("b"+mark) is now at row 0 after scrolling.
        let mark = s.active.rows[0].mark;
        assert!(mark.contains(RowMark::PROMPT_END), "PROMPT_END must follow its row on scroll");
    }

    #[test]
    fn prompt_end_mark_rides_into_scrollback_when_row_scrolls_away() {
        // B lands on row 0; two scrolls push that row into scrollback.
        let mut p = crate::parser::Parser::new();
        let mut s = Screen::new(2, 8);
        p.advance(&mut s, b"a\x1b]133;B\x07\r\nb\r\nc\r\nd");
        p.flush(&mut s);
        // The row with `PROMPT_END` is in scrollback, so the active grid has no B rows.
        let active_has_b = s.active.rows.iter().any(|r| r.mark.contains(RowMark::PROMPT_END));
        assert!(!active_has_b, "PROMPT_END row scrolled away; active grid must be clean");
        // It rides in scrollback.
        let sb_has_b = s.scrollback.rows().iter().any(|r| r.mark.contains(RowMark::PROMPT_END));
        assert!(sb_has_b, "PROMPT_END row must exist in scrollback");
    }

    #[test]
    fn prompt_end_mark_shifts_with_scroll_down() {
        // CSI T scrolls the region down: the mark rides its row downward.
        let s = parse(b"\x1b]133;B\x07\x1b[T");
        // Before scroll: mark is on row 0. After CSI T, row 0 → row 1.
        assert!(
            s.active.rows[1].mark.contains(RowMark::PROMPT_END),
            "PROMPT_END must follow its row down after CSI T"
        );
        assert!(
            !s.active.rows[0].mark.contains(RowMark::PROMPT_END),
            "row 0 must not have PROMPT_END after the scroll"
        );
    }

    #[test]
    fn row_marks_ride_into_scrollback() {
        // A marked row that scrolls off the top keeps its flags and exit code
        // in scrollback, the mark lives ON the Row so no transfer code is needed.
        let mut p = crate::parser::Parser::new();
        let mut s = Screen::new(2, 8);
        p.advance(&mut s, b"\x1b]133;A\x07\x1b]133;D;7\x07a\r\nb\r\nc\r\nd");
        p.flush(&mut s);
        assert!(s.scrollback.len() >= 2, "row 'a' must have scrolled away");
        let mark = s.scrollback.rows()[0].mark;
        assert!(mark.contains(RowMark::PROMPT_START));
        assert!(mark.contains(RowMark::BLOCK_END));
        assert_eq!(mark.exit(), Some(7));
    }

    #[test]
    fn scrollback_eviction_drops_marks_with_their_rows() {
        // At the scrollback cap, evicted rows take their marks with them, so
        // nothing retains a reference to an evicted block.
        let mut p = crate::parser::Parser::new();
        let mut s = Screen::new(2, 8);
        s.scrollback = crate::scrollback::Scrollback::with_cap(1);
        // Mark row 'a', then scroll enough that 'a' is evicted (cap 1 keeps
        // only the most recent scrolled-out row).
        p.advance(&mut s, b"\x1b]133;A\x07\x1b]133;D;7\x07a\r\nb\r\nc\r\nd\r\ne");
        p.flush(&mut s);
        assert_eq!(s.scrollback.len(), 1);
        assert!(
            s.scrollback.rows()[0].mark.is_empty(),
            "the surviving (unmarked) row must not have inherited the evicted mark"
        );
    }

    #[test]
    fn ed2_clears_row_marks() {
        // Ctrl-L style full clear (ED 2): the blocks are gone from the screen,
        // so the marks must go too, otherwise blank rows read as phantom blocks.
        let s = parse(b"\x1b]133;A\x07\x1b]133;D;0\x07hi\x1b[2J");
        for (i, row) in s.active.rows.iter().enumerate() {
            assert!(row.mark.is_empty(), "row {i} must be markless after 2J");
        }
    }

    #[test]
    fn partial_erase_keeps_row_marks() {
        // EL (erase line) and ED 0 (erase below) blank cells but do not unmake
        // the block: the prompt row keeps its mark.
        let s = parse(b"\x1b]133;A\x07abc\x1b[2K");
        assert!(s.active.rows[0].mark.contains(RowMark::PROMPT_START));

        let s = parse(b"\x1b]133;A\x07abc\x1b[H\x1b[J");
        assert!(
            s.active.rows[0].mark.contains(RowMark::PROMPT_START),
            "ED 0 (clear_rect path) must not clear marks"
        );
    }

    #[test]
    fn prompt_end_remark_updates_col() {
        // A second B on the same row updates the stored col (idempotent re-mark).
        // Emitting "abc" moves cursor to col 3, then B records col 3.
        // Emitting "x" moves cursor to col 4, then a second B updates to col 4.
        let s = parse(b"abc\x1b]133;B\x07x\x1b]133;B\x07");
        let mark = s.active.rows[0].mark;
        assert!(mark.contains(RowMark::PROMPT_END));
        assert_eq!(mark.prompt_end_col(), Some(4), "second B must update the col");
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
        assert_eq!(s.color_queries, vec![(0, ColorQuery::Background)]);
    }

    #[test]
    fn osc_10_query_pushes_foreground_color_query() {
        let s = parse(b"\x1b]10;?\x07");
        assert_eq!(s.color_queries, vec![(0, ColorQuery::Foreground)]);
    }

    #[test]
    fn osc_12_query_pushes_cursor_color_query() {
        let s = parse(b"\x1b]12;?\x07");
        assert_eq!(s.color_queries, vec![(0, ColorQuery::Cursor)]);
    }

    #[test]
    fn osc_color_query_records_replies_emitted_before_it() {
        // DA1 (`CSI c`) emits a raw reply, THEN the OSC 11 query, so the query
        // records index 1 and the daemon emits the DA reply before the color
        // reply. This is the order the standard OSC-support probe expects.
        let s = parse(b"\x1b[c\x1b]11;?\x07");
        assert_eq!(s.replies.len(), 1, "DA1 queued one raw reply");
        assert_eq!(s.color_queries, vec![(1, ColorQuery::Background)]);
    }

    #[test]
    fn osc_11_set_form_is_ignored() {
        let s = parse(b"\x1b]11;#1d1c19\x07");
        assert!(s.color_queries.is_empty());
    }

    #[test]
    fn take_color_queries_drains() {
        let mut s = Screen::new(8, 24);
        s.color_queries.push((0, ColorQuery::Background));
        s.color_queries.push((1, ColorQuery::Foreground));
        let drained = s.take_color_queries();
        assert_eq!(
            drained,
            vec![(0, ColorQuery::Background), (1, ColorQuery::Foreground)]
        );
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

    #[test]
    fn decrqm_reports_enabled_bracketed_paste() {
        let s = parse(b"\x1b[?2004h\x1b[?2004$pX");
        assert_eq!(s.replies, vec![b"\x1b[?2004;1$y".to_vec()]);
    }
    #[test]
    fn decrqm_echoes_unknown_mode_with_pm_zero() {
        let s = parse(b"\x1b[?9999$pX");
        assert_eq!(s.replies, vec![b"\x1b[?9999;0$y".to_vec()]);
    }
    #[test]
    fn decrqm_reports_reset_when_off() {
        let s = parse(b"\x1b[?1004$pX");
        assert_eq!(s.replies, vec![b"\x1b[?1004;2$y".to_vec()]);
    }

    #[test]
    fn decrqm_ansi_mode_always_reports_pm_zero() {
        // ANSI form (no '?'): \e[Ps$p → \e[Ps;0$y, no `?` mirrored, regardless
        // of mode state. We only track DEC-private modes.
        let s = parse(b"\x1b[4$pX"); // IRM, would be Pm=2 if it were private
        assert_eq!(s.replies, vec![b"\x1b[4;0$y".to_vec()]);
    }
    #[test]
    fn focus_events_mode_sets_bit() {
        let s = parse(b"\x1b[?1004hX");
        assert!(s.modes.contains(crate::modes::Modes::FOCUS_EVENTS));
    }
    #[test]
    fn color_scheme_query_replies_dark() {
        let s = parse(b"\x1b[?996nX");
        assert_eq!(s.replies, vec![b"\x1b[?997;1n".to_vec()]);
    }

    #[test]
    fn color_scheme_query_reflects_set_preference() {
        // After the daemon records a light scheme, ?996n must answer light (;2n).
        let mut p = crate::parser::Parser::new();
        let mut s = Screen::new(4, 8);
        s.set_color_scheme_dark(false);
        p.advance(&mut s, b"\x1b[?996nX");
        p.flush(&mut s);
        assert_eq!(s.replies, vec![b"\x1b[?997;2n".to_vec()]);
        // And RIS preserves the daemon-set scheme.
        let mut s2 = Screen::new(4, 8);
        s2.set_color_scheme_dark(false);
        let mut p2 = crate::parser::Parser::new();
        p2.advance(&mut s2, b"\x1bc\x1b[?996nX");
        p2.flush(&mut s2);
        assert_eq!(s2.replies, vec![b"\x1b[?997;2n".to_vec()]);
    }

    // ── blocks_completed / last_block_exit ────────────────────────────────────

    #[test]
    fn fresh_screen_has_zero_counter_and_no_exit() {
        let s = Screen::new(4, 8);
        assert_eq!(s.blocks_completed, 0);
        assert_eq!(s.last_block_exit, None);
    }

    #[test]
    fn single_d_with_exit_zero_increments_counter_and_records_exit() {
        let s = parse(b"\x1b]133;D;0\x07");
        assert_eq!(s.blocks_completed, 1);
        assert_eq!(s.last_block_exit, Some(0));
    }

    #[test]
    fn last_block_duration_recorded_on_c_then_d() {
        let s = parse(b"\x1b]133;A\x07$ cmd\r\n\x1b]133;C\x07out\r\n\x1b]133;D;0\x07x");
        assert!(s.last_block_duration.is_some(), "C->D records a session-level duration");
    }

    #[test]
    fn last_block_duration_none_without_c() {
        let s = parse(b"\x1b]133;A\x07$ cmd\r\n\x1b]133;D;0\x07x");
        assert_eq!(s.last_block_duration, None, "D without C has no duration");
    }

    #[test]
    fn single_d_with_nonzero_exit_records_that_exit() {
        let s = parse(b"\x1b]133;D;7\x07");
        assert_eq!(s.blocks_completed, 1);
        assert_eq!(s.last_block_exit, Some(7));
    }

    #[test]
    fn bare_d_increments_counter_and_exit_is_none() {
        // D with no exit payload: counter goes up, exit stays None.
        let s = parse(b"\x1b]133;D\x07");
        assert_eq!(s.blocks_completed, 1);
        assert_eq!(s.last_block_exit, None);
    }

    #[test]
    fn two_ds_counter_is_2_exit_is_last() {
        // Two D's: counter = 2, exit = the second payload.
        let s = parse(b"\x1b]133;D;3\x07\x1b]133;D;9\x07");
        assert_eq!(s.blocks_completed, 2);
        assert_eq!(s.last_block_exit, Some(9));
    }

    #[test]
    fn d_on_alt_screen_does_not_increment_counter() {
        // Enter alt screen, emit D, return to main, and the counter must stay 0.
        let s = parse(b"\x1b[?1049h\x1b]133;D;0\x07\x1b[?1049l");
        assert_eq!(s.blocks_completed, 0);
        assert_eq!(s.last_block_exit, None);
    }

    #[test]
    fn ed2_clear_does_not_affect_counter_or_exit() {
        // A D then an ED 2 clear: counter and exit survive (they are not row state).
        let s = parse(b"\x1b]133;D;5\x07\x1b[2J");
        assert_eq!(s.blocks_completed, 1);
        assert_eq!(s.last_block_exit, Some(5));
    }

    #[test]
    fn scroll_eviction_does_not_affect_counter_or_exit() {
        // Feed enough lines to scroll the marked row into scrollback and then
        // evict it; the counter and exit must survive eviction.
        let mut p = crate::parser::Parser::new();
        let mut s = Screen::new(2, 8);
        s.scrollback = crate::scrollback::Scrollback::with_cap(1);
        // Emit D on row 0, then scroll many lines past the scrollback cap.
        p.advance(&mut s, b"\x1b]133;D;4\x07");
        p.flush(&mut s);
        // Generate enough newlines to push the marked row beyond the cap.
        let lots = b"\r\na\r\nb\r\nc\r\nd\r\ne\r\nf";
        p.advance(&mut s, lots);
        p.flush(&mut s);
        assert_eq!(s.blocks_completed, 1, "eviction must not reset the counter");
        assert_eq!(s.last_block_exit, Some(4), "eviction must not clear last exit");
    }

    #[test]
    fn ris_resets_counter_and_exit() {
        // RIS (\ec) rebuilds Screen::new, so counter, exit, and duration all reset.
        // Seed a full C->D block first so last_block_duration is Some before RIS.
        let s = parse(b"\x1b]133;A\x07\x1b]133;C\x07\x1b]133;D;2\x07\x1bc");
        assert_eq!(s.blocks_completed, 0);
        assert_eq!(s.last_block_exit, None);
        assert_eq!(s.last_block_duration, None, "RIS clears last_block_duration too");
    }
}
