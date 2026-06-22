//! Cell-level diff renderer: compares the current `VirtualScreen` against the
//! previous one and emits minimal ANSI to bring the host TTY up to date.

use crate::virtual_screen::{VirtualScreen, VisiblePlacement};
use plexy_glass_emulator::{Attrs, Cell, Color, UnderlineStyle};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

/// Which inline-graphics protocols the *outer* terminal of a given client
/// supports. Negotiated per client (Phase 2 Task 4). The renderer emits a
/// protocol's bytes only when its flag is set; clients without a flag get blank
/// cells where the image would be (a richer placeholder is later-phase work).
/// `Default` is all-off (conservative): no graphics until the daemon proves them
/// from the negotiated `ClientHello` (it always sets caps per client). Matches
/// the protocol type's default, so a renderer can never implicitly turn images
/// on for a terminal that didn't advertise them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GraphicsCaps {
    pub kitty: bool,
    pub sixel: bool,
    pub iterm2: bool,
}

/// What the renderer last emitted for a placement key (to diff across frames).
/// Includes the source crop and the displayed cell box, not just the host cell:
/// scrolling a tall image through the top of a short pane keeps the host cell
/// fixed while the crop walks, so a crop-only change must still re-place.
#[derive(Clone, Copy, PartialEq, Eq)]
struct PlacedRect {
    host_row: u16,
    host_col: u16,
    image_id: u32,
    placement_id: u32,
    src_x: u32,
    src_y: u32,
    src_w: u32,
    src_h: u32,
    rows: u16,
    cols: u16,
}

pub struct DiffRenderer {
    previous: Option<VirtualScreen>,
    graphics: GraphicsCaps,
    /// Host image id → the content generation last transmitted to this client's
    /// terminal. A changed generation means the id's pixels changed, so we
    /// re-transmit (Kitty `a=t` with the same `i=` replaces) instead of showing
    /// stale data.
    transmitted: HashMap<u32, u64>,
    /// Placement key → what we last emitted, for the per-frame placement diff.
    placed: HashMap<u64, PlacedRect>,
    /// Placement key → the placeholder box last drawn (non-graphics clients).
    /// Mirrors `placed`: drawn on new/moved, cleared (repaint underlying cells)
    /// on vanish, so a client whose terminal can't render the image still keeps a
    /// consistent labelled-box layout.
    boxed: HashMap<u64, PlacedRect>,
    /// Set by `invalidate`: the next render first deletes ALL terminal images
    /// (session switch / re-point) before re-transmitting + re-placing.
    reset_images: bool,
}

impl DiffRenderer {
    pub fn new() -> Self {
        Self {
            previous: None,
            graphics: GraphicsCaps::default(),
            transmitted: HashMap::new(),
            placed: HashMap::new(),
            boxed: HashMap::new(),
            reset_images: false,
        }
    }

    /// Set this client's negotiated graphics capabilities.
    pub fn set_graphics_caps(&mut self, caps: GraphicsCaps) {
        self.graphics = caps;
    }

    /// Forcibly invalidate the cached frame so the next render is a full repaint,
    /// and (for a session switch / re-point) delete all terminal images first.
    pub fn invalidate(&mut self) {
        self.previous = None;
        self.reset_images = true;
    }

    pub fn render(&mut self, current: &VirtualScreen) -> Vec<u8> {
        let mut out = String::new();

        // Session switch / re-point: drop all terminal images + state before
        // re-transmitting (the new content's placements transmit fresh).
        if self.reset_images {
            if self.graphics.kitty && (!self.transmitted.is_empty() || !self.placed.is_empty()) {
                out.push_str("\x1b_Ga=d,d=A,q=2\x1b\\");
            }
            self.transmitted.clear();
            self.placed.clear();
            self.boxed.clear();
            self.reset_images = false;
        }

        let full_repaint = match &self.previous {
            None => true,
            Some(p) => p.rows != current.rows || p.cols != current.cols,
        };

        if full_repaint {
            // A full repaint (first frame or resize) re-walks the whole grid; the
            // graphics layer must be re-established too. Drop any terminal images
            // first so a stale placement can't ghost at a wrong cell after a 2J,
            // then re-transmit/re-place from the current frame below. (No-op when
            // reset_images already cleared the state just above.)
            if self.graphics.kitty && (!self.transmitted.is_empty() || !self.placed.is_empty()) {
                out.push_str("\x1b_Ga=d,d=A,q=2\x1b\\");
            }
            self.transmitted.clear();
            self.placed.clear();
            self.boxed.clear();
            // Clear + home, then walk every row, every cell.
            out.push_str("\x1b[2J\x1b[H");
            let mut current_attrs = CellAttrs::default();
            for r in 0..current.rows {
                let _ = write!(out, "\x1b[{};1H", r + 1);
                let mut c = 0u16;
                while c < current.cols {
                    let Some(cell) = current.cell(r, c) else { break };
                    if cell.is_wide_spacer() {
                        c += 1;
                        continue;
                    }
                    apply_sgr_delta(&mut out, &current_attrs, cell);
                    current_attrs = CellAttrs::from_cell(cell);
                    out.push_str(cell.grapheme.as_str());
                    let w = plexy_glass_emulator::grapheme_advance(cell.grapheme.as_str());
                    c += w;
                }
            }
        } else {
            // Diff per row.
            // invariant: full_repaint == false implies self.previous is Some.
            let prev = self
                .previous
                .as_ref()
                .expect("non-full-repaint => previous is Some");
            let mut current_attrs = CellAttrs::default();
            for r in 0..current.rows {
                let mut c = 0u16;
                while c < current.cols {
                    let pc = prev.cell(r, c);
                    let cc = current.cell(r, c);
                    if pc == cc {
                        c += 1;
                        continue;
                    }
                    // Run start.
                    let _ = write!(out, "\x1b[{};{}H", r + 1, c + 1);
                    while c < current.cols {
                        let Some(cell) = current.cell(r, c) else { break };
                        if Some(cell) == prev.cell(r, c) {
                            break;
                        }
                        if cell.is_wide_spacer() {
                            c += 1;
                            continue;
                        }
                        apply_sgr_delta(&mut out, &current_attrs, cell);
                        current_attrs = CellAttrs::from_cell(cell);
                        out.push_str(cell.grapheme.as_str());
                        let w = plexy_glass_emulator::grapheme_advance(cell.grapheme.as_str());
                        c += w;
                    }
                }
            }
        }

        // Inline-image placements. A Kitty-capable client gets the real image
        // (transmit-once, place-by-id, scroll-follow). A client whose terminal
        // can't render the placement's protocol gets a labelled placeholder box
        // of the same footprint, so heterogeneous clients keep a consistent
        // layout instead of blank cells.
        if self.graphics.kitty {
            self.render_kitty_placements(&mut out, current);
        } else {
            self.render_placeholder_boxes(&mut out, current);
        }

        // Cursor.
        if current.cursor_visible {
            if let Some((r, c)) = current.cursor {
                let _ = write!(out, "\x1b[{};{}H\x1b[?25h", r + 1, c + 1);
            } else {
                out.push_str("\x1b[?25l");
            }
        } else {
            out.push_str("\x1b[?25l");
        }

        // Reset SGR at the very end so we don't leave attrs leaking into the host.
        out.push_str("\x1b[0m");

        self.previous = Some(current.clone());
        out.into_bytes()
    }

    /// Per-frame Kitty placement diff: transmit-once, place-by-id for new/moved,
    /// delete (placement only, data retained) for gone/moved.
    fn render_kitty_placements(&mut self, out: &mut String, current: &VirtualScreen) {
        let mut seen: HashSet<u64> = HashSet::with_capacity(current.placements.len());
        for p in &current.placements {
            seen.insert(p.key);
            // Transmit once per (id, content generation). An image with no data can't be
            // transmitted or placed, so skip it without poisoning the transmitted map
            // (poisoning it would block a later real transmit of the id).
            if p.data_b64.is_empty() {
                continue;
            }
            if self.transmitted.get(&p.image_id) != Some(&p.generation) {
                emit_transmit(out, p);
                self.transmitted.insert(p.image_id, p.generation);
            }
            let rect = PlacedRect {
                host_row: p.host_row,
                host_col: p.host_col,
                image_id: p.image_id,
                placement_id: p.placement_id,
                src_x: p.src_x,
                src_y: p.src_y,
                src_w: p.src_w,
                src_h: p.src_h,
                rows: p.rows,
                cols: p.cols,
            };
            match self.placed.get(&p.key) {
                Some(prev) if *prev == rect => {} // unchanged, already on screen
                Some(prev) => {
                    emit_delete(out, prev.image_id, prev.placement_id);
                    emit_place(out, p);
                    self.placed.insert(p.key, rect);
                }
                None => {
                    emit_place(out, p);
                    self.placed.insert(p.key, rect);
                }
            }
        }
        // Delete placements that vanished this frame.
        let gone: Vec<u64> = self.placed.keys().copied().filter(|k| !seen.contains(k)).collect();
        for k in gone {
            if let Some(rect) = self.placed.remove(&k) {
                emit_delete(out, rect.image_id, rect.placement_id);
            }
        }
        // Bound the transmitted set: transmit-once keeps scrolled-off ids cached,
        // so over a long session with many distinct images it would grow without
        // limit. Past the cap, schedule a full graphics reset next frame (delete
        // all + re-transmit the visible set), which is rare and self-healing.
        const TRANSMIT_CAP: usize = 256;
        if self.transmitted.len() > TRANSMIT_CAP {
            self.reset_images = true;
        }
    }

    /// Placeholder fallback for a client whose terminal can't render the
    /// placement's protocol: draw a labelled box of the image's footprint,
    /// diffed across frames (drawn on new/moved, the old cells repainted on
    /// vanish) so the layout matches a graphics-capable client's.
    fn render_placeholder_boxes(&mut self, out: &mut String, current: &VirtualScreen) {
        let mut seen: HashSet<u64> = HashSet::with_capacity(current.placements.len());
        for p in &current.placements {
            seen.insert(p.key);
            let rect = PlacedRect {
                host_row: p.host_row,
                host_col: p.host_col,
                image_id: p.image_id,
                placement_id: p.placement_id,
                src_x: p.src_x,
                src_y: p.src_y,
                src_w: p.src_w,
                src_h: p.src_h,
                rows: p.rows,
                cols: p.cols,
            };
            match self.boxed.get(&p.key) {
                Some(prev) if *prev == rect => {} // unchanged, box already drawn
                Some(prev) => {
                    paint_cells_rect(out, current, prev.host_row, prev.host_col, prev.rows, prev.cols);
                    emit_placeholder_box(out, p);
                    self.boxed.insert(p.key, rect);
                }
                None => {
                    emit_placeholder_box(out, p);
                    self.boxed.insert(p.key, rect);
                }
            }
        }
        // Repaint the cells under boxes that vanished this frame.
        let gone: Vec<u64> = self.boxed.keys().copied().filter(|k| !seen.contains(k)).collect();
        for k in gone {
            if let Some(rect) = self.boxed.remove(&k) {
                paint_cells_rect(out, current, rect.host_row, rect.host_col, rect.rows, rect.cols);
            }
        }
    }
}

/// Repaint a rectangle of cells from `screen` (used to clear a placeholder box
/// when its placement vanishes or moves).
fn paint_cells_rect(out: &mut String, screen: &VirtualScreen, r0: u16, c0: u16, rows: u16, cols: u16) {
    let mut attrs = CellAttrs::default();
    out.push_str("\x1b[0m");
    for r in r0..r0.saturating_add(rows).min(screen.rows) {
        let _ = write!(out, "\x1b[{};{}H", r + 1, c0 + 1);
        let mut c = c0;
        while c < c0.saturating_add(cols).min(screen.cols) {
            let Some(cell) = screen.cell(r, c) else { break };
            if cell.is_wide_spacer() {
                c += 1;
                continue;
            }
            apply_sgr_delta(out, &attrs, cell);
            attrs = CellAttrs::from_cell(cell);
            out.push_str(cell.grapheme.as_str());
            c += plexy_glass_emulator::grapheme_advance(cell.grapheme.as_str());
        }
        out.push_str("\x1b[0m");
        attrs = CellAttrs::default();
    }
}

/// Draw a labelled placeholder box over a placement's host footprint. A box big
/// enough gets a unicode border + a centred `WxH` label; a tiny footprint is
/// filled with a hatch so it's still visibly an image stand-in.
fn emit_placeholder_box(out: &mut String, p: &VisiblePlacement) {
    out.push_str("\x1b[0m");
    let rows = p.rows;
    let cols = p.cols;
    if rows == 0 || cols == 0 {
        return;
    }
    // Too small for a border: fill with a light hatch.
    if rows < 2 || cols < 2 {
        for r in 0..rows {
            let _ = write!(out, "\x1b[{};{}H", p.host_row + r + 1, p.host_col + 1);
            for _ in 0..cols {
                out.push('▒');
            }
        }
        return;
    }
    let w = cols as usize;
    let inner = w - 2;
    let label = format!("{}x{}", p.pixel_w, p.pixel_h);
    let label: String = if label.chars().count() > inner {
        label.chars().take(inner).collect()
    } else {
        label
    };
    let mid = rows / 2;
    for r in 0..rows {
        let _ = write!(out, "\x1b[{};{}H", p.host_row + r + 1, p.host_col + 1);
        if r == 0 {
            out.push('┌');
            for _ in 0..inner {
                out.push('─');
            }
            out.push('┐');
        } else if r == rows - 1 {
            out.push('└');
            for _ in 0..inner {
                out.push('─');
            }
            out.push('┘');
        } else if r == mid {
            out.push('│');
            let pad = inner.saturating_sub(label.chars().count());
            let left = pad / 2;
            for _ in 0..left {
                out.push(' ');
            }
            out.push_str(&label);
            for _ in 0..(pad - left) {
                out.push(' ');
            }
            out.push('│');
        } else {
            out.push('│');
            for _ in 0..inner {
                out.push(' ');
            }
            out.push('│');
        }
    }
}

/// Transmit an image's data once (`a=t`), re-chunked to ≤4096 base64 bytes.
fn emit_transmit(out: &mut String, p: &VisiblePlacement) {
    if p.data_b64.is_empty() {
        return;
    }
    const CHUNK: usize = 4096;
    let f = p.format.kitty_f();
    let n = p.data_b64.len();
    let mut i = 0;
    let mut first = true;
    while i < n {
        let end = (i + CHUNK).min(n);
        let more = u8::from(end < n);
        if first {
            let _ = write!(
                out,
                "\x1b_Gi={},a=t,f={},s={},v={},q=2,m={};",
                p.image_id, f, p.pixel_w, p.pixel_h, more
            );
            first = false;
        } else {
            let _ = write!(out, "\x1b_Gm={};", more);
        }
        out.push_str(&String::from_utf8_lossy(&p.data_b64[i..end]));
        out.push_str("\x1b\\");
        i = end;
    }
}

/// Place a transmitted image by id at its host cell, forcing the cell box
/// (`r/c`) so it occupies the same cells on every client. When the visible part
/// is a strict sub-rectangle of the image (clipped by the viewport or pane
/// edges), include the Kitty source crop keys `x/y/w/h`.
fn emit_place(out: &mut String, p: &VisiblePlacement) {
    let _ = write!(out, "\x1b[{};{}H", p.host_row + 1, p.host_col + 1);
    let cropped =
        p.src_x > 0 || p.src_y > 0 || p.src_w < p.pixel_w || p.src_h < p.pixel_h;
    let _ = write!(out, "\x1b_Ga=p,i={},p={}", p.image_id, p.placement_id);
    if cropped {
        let _ = write!(out, ",x={},y={},w={},h={}", p.src_x, p.src_y, p.src_w, p.src_h);
    }
    let _ = write!(out, ",r={},c={},q=2\x1b\\", p.rows, p.cols);
}

/// Delete a single placement (lowercase `d=i` keeps the image data for re-place).
fn emit_delete(out: &mut String, image_id: u32, placement_id: u32) {
    let _ = write!(out, "\x1b_Ga=d,d=i,i={image_id},p={placement_id},q=2\x1b\\");
}

impl Default for DiffRenderer {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CellAttrs {
    fg: Color,
    bg: Color,
    underline_color: Color,
    underline_style: UnderlineStyle,
    attrs: Attrs,
}

impl CellAttrs {
    fn from_cell(c: &Cell) -> Self {
        Self {
            fg: c.fg,
            bg: c.bg,
            underline_color: c.underline_color,
            underline_style: c.underline_style,
            attrs: c.attrs,
        }
    }
}

fn apply_sgr_delta(out: &mut String, prev: &CellAttrs, cell: &Cell) {
    let new = CellAttrs::from_cell(cell);
    if &new == prev {
        return;
    }
    // For simplicity, emit a full reset + reset every attribute. Cell-diffing
    // gives most of the bandwidth win, and tighter SGR diffing is a later
    // optimization.
    out.push_str("\x1b[0m");
    if new.attrs.contains(Attrs::BOLD) {
        out.push_str("\x1b[1m");
    }
    if new.attrs.contains(Attrs::DIM) {
        out.push_str("\x1b[2m");
    }
    if new.attrs.contains(Attrs::ITALIC) {
        out.push_str("\x1b[3m");
    }
    // Underline: re-emit the styled form (`4:N`) so undercurl/dotted/dashed
    // survive to the outer terminal instead of flattening to a plain underline.
    // `Single` uses bare `4m` for back-compat with terminals that don't grok the
    // colon sub-parameter. `None` emits nothing, the `\x1b[0m` prefix above
    // already reset the underline. If `UNDERLINE` is set but the style is `None`
    // (shouldn't normally happen), fall back to a plain underline.
    if new.attrs.contains(Attrs::UNDERLINE) {
        match new.underline_style {
            UnderlineStyle::None | UnderlineStyle::Single => out.push_str("\x1b[4m"),
            UnderlineStyle::Double => out.push_str("\x1b[4:2m"),
            UnderlineStyle::Curly => out.push_str("\x1b[4:3m"),
            UnderlineStyle::Dotted => out.push_str("\x1b[4:4m"),
            UnderlineStyle::Dashed => out.push_str("\x1b[4:5m"),
        }
    }
    if new.attrs.contains(Attrs::REVERSE) {
        out.push_str("\x1b[7m");
    }
    if new.attrs.contains(Attrs::HIGHLIGHT) {
        // Bright-yellow background (16-colour) distinguishes search matches
        // from REVERSE-based copy-mode selection.
        out.push_str("\x1b[103m");
    }
    if new.attrs.contains(Attrs::STRIKETHROUGH) {
        out.push_str("\x1b[9m");
    }
    match new.fg {
        Color::Default => {}
        Color::Indexed(n @ 0..=7) => {
            let _ = write!(out, "\x1b[{}m", 30 + n);
        }
        Color::Indexed(n @ 8..=15) => {
            let _ = write!(out, "\x1b[{}m", 90 + (n - 8));
        }
        Color::Indexed(n) => {
            let _ = write!(out, "\x1b[38;5;{n}m");
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(out, "\x1b[38;2;{r};{g};{b}m");
        }
    }
    match new.bg {
        Color::Default => {}
        Color::Indexed(n @ 0..=7) => {
            let _ = write!(out, "\x1b[{}m", 40 + n);
        }
        Color::Indexed(n @ 8..=15) => {
            let _ = write!(out, "\x1b[{}m", 100 + (n - 8));
        }
        Color::Indexed(n) => {
            let _ = write!(out, "\x1b[48;5;{n}m");
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(out, "\x1b[48;2;{r};{g};{b}m");
        }
    }
    // Underline color (SGR 58). The `\x1b[0m` prefix earlier in this function
    // already reset it, so only a non-default value needs emitting. Use the colon
    // form (58:5:n / 58:2:r:g:b) for widest support; the outer terminal ignores it
    // when it draws no underline.
    match new.underline_color {
        Color::Default => {}
        Color::Indexed(n) => {
            let _ = write!(out, "\x1b[58:5:{n}m");
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(out, "\x1b[58:2:{r}:{g}:{b}m");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smol_str::SmolStr;

    fn lettered(cells: &[(u16, u16, &str)], rows: u16, cols: u16) -> VirtualScreen {
        let mut v = VirtualScreen::blank(rows, cols);
        for (r, c, s) in cells {
            let cell = Cell {
                grapheme: SmolStr::new(*s),
                ..Cell::default()
            };
            v.put(*r, *c, cell);
        }
        v
    }

    #[test]
    fn first_render_full_repaint() {
        let mut d = DiffRenderer::new();
        let v = lettered(&[(0, 0, "A")], 2, 2);
        let bytes = d.render(&v);
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.starts_with("\x1b[2J\x1b[H"), "expected initial clear: {s:?}");
        assert!(s.contains("A"));
    }

    #[test]
    fn second_render_no_change_emits_only_cursor_reset() {
        let mut d = DiffRenderer::new();
        let v = lettered(&[(0, 0, "A")], 2, 2);
        let _ = d.render(&v);
        let bytes = d.render(&v);
        let s = String::from_utf8_lossy(&bytes);
        assert!(
            !s.contains("A"),
            "second render should not re-emit unchanged cells: {s:?}"
        );
    }

    #[test]
    fn changed_cell_emits_cup_for_that_cell() {
        let mut d = DiffRenderer::new();
        let v1 = lettered(&[(0, 0, "A")], 2, 2);
        let v2 = lettered(&[(0, 0, "A"), (1, 1, "B")], 2, 2);
        let _ = d.render(&v1);
        let bytes = d.render(&v2);
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("B"));
        assert!(s.contains("\x1b[2;2H"), "expected CUP to row 2 col 2: {s:?}");
    }

    #[test]
    fn size_change_forces_full_repaint() {
        let mut d = DiffRenderer::new();
        let v1 = lettered(&[(0, 0, "A")], 2, 2);
        let v2 = lettered(&[(0, 0, "A")], 4, 4);
        let _ = d.render(&v1);
        let bytes = d.render(&v2);
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.starts_with("\x1b[2J\x1b[H"));
    }

    #[test]
    fn wide_grapheme_full_repaint_skips_spacer() {
        let mut d = DiffRenderer::new();
        let mut v = VirtualScreen::blank(1, 4);
        v.put(0, 0, Cell { grapheme: SmolStr::new("世"), ..Cell::default() });
        v.put(0, 1, Cell::wide_spacer());
        v.put(0, 2, Cell { grapheme: SmolStr::new("X"), ..Cell::default() });
        let bytes = d.render(&v);
        let s = String::from_utf8_lossy(&bytes);
        assert_eq!(s.matches('世').count(), 1, "wide grapheme emitted once: {s:?}");
        assert!(s.contains("世X"), "spacer skipped; X immediately follows 世: {s:?}");
        assert!(!s.contains("世 X"), "no stray space painted for the spacer: {s:?}");
    }

    #[test]
    fn wide_grapheme_incremental_diff_targets_only_changed_cell() {
        let mut d = DiffRenderer::new();
        let mut v1 = VirtualScreen::blank(1, 4);
        v1.put(0, 0, Cell { grapheme: SmolStr::new("世"), ..Cell::default() });
        v1.put(0, 1, Cell::wide_spacer());
        v1.put(0, 2, Cell { grapheme: SmolStr::new("a"), ..Cell::default() });
        let _ = d.render(&v1);
        let mut v2 = VirtualScreen::blank(1, 4);
        v2.put(0, 0, Cell { grapheme: SmolStr::new("世"), ..Cell::default() });
        v2.put(0, 1, Cell::wide_spacer());
        v2.put(0, 2, Cell { grapheme: SmolStr::new("b"), ..Cell::default() });
        let bytes = d.render(&v2);
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains('b'), "changed cell emitted: {s:?}");
        assert!(!s.contains('世'), "unchanged wide grapheme not re-emitted: {s:?}");
        assert!(s.contains("\x1b[1;3H"), "CUP targets the changed cell at col 3: {s:?}");
    }

    #[test]
    fn underline_color_rgb_emits_58_2() {
        let mut d = DiffRenderer::new();
        let mut v = VirtualScreen::blank(1, 2);
        v.put(0, 0, Cell { grapheme: SmolStr::new("U"), underline_color: Color::Rgb(10, 20, 30), ..Cell::default() });
        let bytes = d.render(&v);
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("\x1b[58:2:10:20:30m"), "expected RGB underline-color SGR: {s:?}");
    }

    #[test]
    fn underline_color_indexed_emits_58_5() {
        let mut d = DiffRenderer::new();
        let mut v = VirtualScreen::blank(1, 2);
        v.put(0, 0, Cell { grapheme: SmolStr::new("U"), underline_color: Color::Indexed(9), ..Cell::default() });
        let bytes = d.render(&v);
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("\x1b[58:5:9m"), "expected indexed underline-color SGR: {s:?}");
    }

    #[test]
    fn default_underline_color_emits_no_58() {
        let mut d = DiffRenderer::new();
        let v = lettered(&[(0, 0, "A")], 1, 2);
        let bytes = d.render(&v);
        let s = String::from_utf8_lossy(&bytes);
        assert!(!s.contains("\x1b[58"), "default underline color must emit no 58: {s:?}");
    }

    #[test]
    fn underline_style_curly_emits_4_3() {
        let mut d = DiffRenderer::new();
        let mut v = VirtualScreen::blank(1, 2);
        v.put(0, 0, Cell {
            grapheme: SmolStr::new("U"),
            attrs: Attrs::UNDERLINE,
            underline_style: UnderlineStyle::Curly,
            ..Cell::default()
        });
        let bytes = d.render(&v);
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("\x1b[4:3m"), "expected curly underline SGR: {s:?}");
    }

    #[test]
    fn underline_style_single_emits_plain_4() {
        let mut d = DiffRenderer::new();
        let mut v = VirtualScreen::blank(1, 2);
        v.put(0, 0, Cell {
            grapheme: SmolStr::new("U"),
            attrs: Attrs::UNDERLINE,
            underline_style: UnderlineStyle::Single,
            ..Cell::default()
        });
        let bytes = d.render(&v);
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("\x1b[4m"), "expected plain underline SGR: {s:?}");
        assert!(!s.contains("\x1b[4:"), "single must not emit a colon form: {s:?}");
    }

    #[test]
    fn no_underline_emits_no_4() {
        let mut d = DiffRenderer::new();
        let v = lettered(&[(0, 0, "A")], 1, 2);
        let bytes = d.render(&v);
        let s = String::from_utf8_lossy(&bytes);
        assert!(!s.contains("\x1b[4m"), "no-underline cell must emit no 4: {s:?}");
        assert!(!s.contains("\x1b[4:"), "no-underline cell must emit no 4:N: {s:?}");
    }

    #[test]
    fn underline_style_change_in_diff_emits_4_3() {
        // Exercise the incremental diff path: a cell that gains a curly underline
        // on a later render must emit 4:3, proving `CellAttrs` tracks
        // `underline_style`.
        let mut d = DiffRenderer::new();
        let v1 = VirtualScreen::blank(1, 2);
        let _ = d.render(&v1);
        let mut v2 = VirtualScreen::blank(1, 2);
        v2.put(0, 0, Cell {
            grapheme: SmolStr::new("U"),
            attrs: Attrs::UNDERLINE,
            underline_style: UnderlineStyle::Curly,
            ..Cell::default()
        });
        let bytes = d.render(&v2);
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("\x1b[4:3m"), "diff path must emit 4:3: {s:?}");
    }

    #[test]
    fn underline_color_change_in_diff_emits_58() {
        // Exercise the incremental diff path (not just full repaint): a cell that
        // gains an underline color on a later render must emit SGR 58, proving
        // that `CellAttrs` `PartialEq` + `from_cell` track `underline_color`.
        let mut d = DiffRenderer::new();
        let v1 = VirtualScreen::blank(1, 2);
        let _ = d.render(&v1);
        let mut v2 = VirtualScreen::blank(1, 2);
        v2.put(
            0,
            0,
            Cell {
                grapheme: SmolStr::new("U"),
                underline_color: Color::Rgb(1, 2, 3),
                ..Cell::default()
            },
        );
        let bytes = d.render(&v2);
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("\x1b[58:2:1:2:3m"), "diff path must emit 58: {s:?}");
    }

    // ── inline-image placement diff ───────────────────────────────────────────

    fn vp(key: u64, image_id: u32, placement_id: u32, host_row: u16, host_col: u16) -> VisiblePlacement {
        VisiblePlacement {
            key,
            image_id,
            placement_id,
            generation: 1,
            format: plexy_glass_emulator::ImageFormat::Png,
            pixel_w: 30,
            pixel_h: 40,
            src_x: 0,
            src_y: 0,
            src_w: 30,
            src_h: 40,
            data_b64: std::sync::Arc::from(&b"QUJD"[..]),
            host_row,
            host_col,
            rows: 2,
            cols: 3,
        }
    }

    /// A renderer with Kitty graphics enabled (the default is now all-off).
    fn kitty_renderer() -> DiffRenderer {
        let mut d = DiffRenderer::new();
        d.set_graphics_caps(GraphicsCaps { kitty: true, sixel: false, iterm2: false });
        d
    }

    fn frame_with(placements: Vec<VisiblePlacement>) -> VirtualScreen {
        let mut v = VirtualScreen::blank(8, 20);
        v.placements = placements;
        v
    }

    fn render_str(d: &mut DiffRenderer, v: &VirtualScreen) -> String {
        String::from_utf8_lossy(&d.render(v)).into_owned()
    }

    #[test]
    fn first_frame_transmits_then_places() {
        let mut d = kitty_renderer();
        let s = render_str(&mut d, &frame_with(vec![vp(1, 7, 1, 2, 3)]));
        assert!(s.contains("\x1b_Gi=7,a=t,f=100,s=30,v=40"), "transmit once: {s:?}");
        // Place at host (row 3, col 4) 1-based, by id, forcing r/c.
        assert!(s.contains("\x1b[3;4H\x1b_Ga=p,i=7,p=1,r=2,c=3,q=2\x1b\\"), "place by id: {s:?}");
    }

    #[test]
    fn unchanged_frame_re_emits_nothing() {
        let mut d = kitty_renderer();
        let f = frame_with(vec![vp(1, 7, 1, 2, 3)]);
        render_str(&mut d, &f);
        let s = render_str(&mut d, &f);
        assert!(!s.contains("\x1b_G"), "no graphics re-emitted for an unchanged frame: {s:?}");
    }

    #[test]
    fn moved_placement_deletes_old_and_places_new_without_retransmit() {
        let mut d = kitty_renderer();
        render_str(&mut d, &frame_with(vec![vp(1, 7, 1, 2, 3)]));
        let s = render_str(&mut d, &frame_with(vec![vp(1, 7, 1, 4, 3)])); // moved down 2 rows
        assert!(!s.contains("a=t"), "image already transmitted: {s:?}");
        assert!(s.contains("\x1b_Ga=d,d=i,i=7,p=1,q=2\x1b\\"), "delete old placement: {s:?}");
        assert!(s.contains("\x1b[5;4H\x1b_Ga=p,i=7,p=1"), "re-place at new row: {s:?}");
    }

    #[test]
    fn vanished_placement_is_deleted() {
        let mut d = kitty_renderer();
        render_str(&mut d, &frame_with(vec![vp(1, 7, 1, 2, 3)]));
        let s = render_str(&mut d, &frame_with(vec![]));
        assert!(s.contains("\x1b_Ga=d,d=i,i=7,p=1,q=2\x1b\\"), "delete vanished placement: {s:?}");
    }

    #[test]
    fn non_kitty_client_emits_no_graphics() {
        let mut d = DiffRenderer::new();
        d.set_graphics_caps(GraphicsCaps { kitty: false, sixel: false, iterm2: false });
        let s = render_str(&mut d, &frame_with(vec![vp(1, 7, 1, 2, 3)]));
        assert!(!s.contains("\x1b_G"), "no graphics bytes for a non-kitty client: {s:?}");
    }

    #[test]
    fn invalidate_resets_images_then_retransmits() {
        let mut d = kitty_renderer();
        render_str(&mut d, &frame_with(vec![vp(1, 7, 1, 2, 3)]));
        d.invalidate(); // session switch / re-point
        let s = render_str(&mut d, &frame_with(vec![vp(1, 7, 1, 2, 3)]));
        assert!(s.contains("\x1b_Ga=d,d=A,q=2\x1b\\"), "reset deletes all images: {s:?}");
        assert!(s.contains("a=t"), "re-transmits after reset: {s:?}");
    }

    #[test]
    fn retransmit_on_generation_change() {
        // Same id + key, but the image content changed (new generation), so the
        // renderer must re-transmit, not show the stale first image.
        let mut d = kitty_renderer();
        render_str(&mut d, &frame_with(vec![vp(1, 7, 1, 2, 3)]));
        let mut p = vp(1, 7, 1, 2, 3);
        p.generation = 2;
        p.data_b64 = std::sync::Arc::from(&b"WFla"[..]);
        let s = render_str(&mut d, &frame_with(vec![p]));
        assert!(s.contains("a=t"), "changed content re-transmits id 7: {s:?}");
    }

    #[test]
    fn empty_data_placement_neither_transmits_nor_poisons() {
        // A placement whose image has no data can't be transmitted/placed, and
        // must not mark the id transmitted, so a later real-data frame still
        // sends it.
        let mut d = kitty_renderer();
        let mut empty = vp(1, 7, 1, 2, 3);
        empty.data_b64 = std::sync::Arc::from(&b""[..]);
        let s = render_str(&mut d, &frame_with(vec![empty]));
        assert!(!s.contains("\x1b_G"), "no graphics for an empty-data image: {s:?}");
        let s2 = render_str(&mut d, &frame_with(vec![vp(1, 7, 1, 2, 3)]));
        assert!(s2.contains("a=t"), "real data still transmits id 7 later: {s2:?}");
    }

    #[test]
    fn resize_full_repaint_reestablishes_image() {
        // A size change forces a full repaint; the image must be re-placed (and
        // the old terminal state dropped) rather than silently vanishing.
        let mut d = kitty_renderer();
        render_str(&mut d, &frame_with(vec![vp(1, 7, 1, 2, 3)]));
        let mut bigger = VirtualScreen::blank(10, 24); // different size → full repaint
        bigger.placements = vec![vp(1, 7, 1, 2, 3)];
        let s = render_str(&mut d, &bigger);
        assert!(s.contains("\x1b_Ga=d,d=A,q=2\x1b\\"), "full repaint drops old images: {s:?}");
        assert!(s.contains("a=t"), "re-transmits after the repaint: {s:?}");
        assert!(s.contains("a=p,i=7"), "re-places the image: {s:?}");
    }

    #[test]
    fn cropped_place_emits_source_rect_full_place_omits_it() {
        // Full source -> minimal place (no x/y/w/h).
        let mut d = kitty_renderer();
        let s = render_str(&mut d, &frame_with(vec![vp(1, 7, 1, 2, 3)]));
        assert!(s.contains("\x1b_Ga=p,i=7,p=1,r=2,c=3,q=2"), "full place minimal: {s:?}");
        assert!(!s.contains(",x="), "no crop keys for a full image: {s:?}");

        // Cropped source → x/y/w/h present.
        let mut d2 = kitty_renderer();
        let mut p = vp(2, 8, 1, 0, 0);
        p.src_y = 20; // show lower half vertically
        p.src_h = 20;
        p.rows = 1;
        let s2 = render_str(&mut d2, &frame_with(vec![p]));
        assert!(
            s2.contains("a=p,i=8,p=1,x=0,y=20,w=30,h=20,r=1,c=3,q=2"),
            "cropped place carries the source rect: {s2:?}"
        );
    }

    #[test]
    fn crop_only_change_at_fixed_host_cell_re_places() {
        // Scrolling a tall image through the top of a short pane keeps the host
        // cell (and key/ids) fixed while the crop walks. A crop-only change must
        // still re-emit the place, or the terminal freezes the stale slice.
        let mut d = kitty_renderer();
        render_str(&mut d, &frame_with(vec![vp(1, 7, 1, 2, 3)])); // full image
        let mut p = vp(1, 7, 1, 2, 3); // same key, host, ids
        p.src_y = 20; // crop changed only
        p.src_h = 20;
        p.rows = 1;
        let s = render_str(&mut d, &frame_with(vec![p]));
        assert!(
            s.contains("a=p,i=7,p=1") && s.contains(",y=20,"),
            "crop-only change re-places with the new source rect: {s:?}"
        );
    }

    // ── placeholder box (non-graphics clients) ─────────────────────────────────

    fn boxed_vp(rows: u16, cols: u16) -> VisiblePlacement {
        let mut p = vp(1, 7, 1, 2, 3);
        p.rows = rows;
        p.cols = cols;
        p.pixel_w = 30;
        p.pixel_h = 40;
        p
    }

    #[test]
    fn non_kitty_client_draws_placeholder_box() {
        let mut d = DiffRenderer::new(); // default caps: no graphics
        let s = render_str(&mut d, &frame_with(vec![boxed_vp(3, 10)]));
        assert!(s.contains('┌') && s.contains('┐') && s.contains('└') && s.contains('┘'), "box border: {s:?}");
        assert!(s.contains("30x40"), "centred WxH label: {s:?}");
        assert!(!s.contains("\x1b_G"), "no Kitty bytes for a non-graphics client: {s:?}");
    }

    #[test]
    fn kitty_client_draws_no_placeholder_box() {
        let mut d = kitty_renderer();
        let s = render_str(&mut d, &frame_with(vec![boxed_vp(3, 10)]));
        assert!(s.contains("a=t"), "Kitty client transmits the real image: {s:?}");
        assert!(!s.contains('┌'), "no placeholder box for a Kitty client: {s:?}");
    }

    #[test]
    fn placeholder_box_cleared_when_placement_vanishes() {
        let mut d = DiffRenderer::new();
        render_str(&mut d, &frame_with(vec![boxed_vp(3, 10)]));
        let s = render_str(&mut d, &frame_with(vec![]));
        assert!(s.contains("\x1b[3;4H"), "repaints the vacated box region: {s:?}");
        assert!(!s.contains('┌'), "box not redrawn after vanish: {s:?}");
    }

    #[test]
    fn tiny_placeholder_footprint_hatches_without_panic() {
        let mut d = DiffRenderer::new();
        let s = render_str(&mut d, &frame_with(vec![boxed_vp(1, 1)]));
        assert!(s.contains('▒'), "tiny footprint hatched: {s:?}");
        assert!(!s.contains('┌'), "no border when too small: {s:?}");
    }
}
