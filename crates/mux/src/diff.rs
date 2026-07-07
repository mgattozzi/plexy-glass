//! Cell-level diff renderer: compares the current `VirtualScreen` against the
//! previous one and emits minimal ANSI to bring the host TTY up to date.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use plexy_glass_emulator::{AnimControl, Attrs, Cell, Color, Frame, UnderlineStyle};

use crate::virtual_screen::{VirtualScreen, VisiblePlacement};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// Same, but for virtual (Unicode-placeholder) placements, whose wire id the
    /// compositor folds per pane via `virtual_host_image_id` (a 24-bit fold, to fit
    /// the placeholder cell's fg encoding) rather than the classic `host_image_id`.
    /// Kept separate so a virtual id can't collide with a classic host id in one map.
    transmitted_virtual: HashMap<u32, u64>,
    /// Placement key → what we last emitted, for the per-frame placement diff.
    placed: HashMap<u64, PlacedRect>,
    /// Placement key → the placeholder box last drawn (non-graphics clients).
    /// Mirrors `placed`: drawn on new/moved, cleared (repaint underlying cells)
    /// on vanish, so a client whose terminal can't render the image still keeps a
    /// consistent labelled-box layout.
    boxed: HashMap<u64, PlacedRect>,
    /// Virtual-placement key → (image_id, placement_id) of the `a=p,U=1` emitted,
    /// so it's sent once and deleted when the placement goes away.
    virtual_placed: HashMap<u64, (u32, u32)>,
    /// Placement key → rect for Sixel/iTerm2 placements (no by-id model: the
    /// data is re-emitted at the host cell, and the old rect repainted on
    /// move/vanish, like the placeholder box).
    placed_data: HashMap<u64, PlacedRect>,
    /// Set by `invalidate`: the next render first deletes ALL terminal images
    /// (session switch / re-point) before re-transmitting + re-placing.
    reset_images: bool,
    /// Host image id → the highest `Frame::seq` of that image's frame log this
    /// client has already received, for incremental animation replay. A
    /// brand-new client (absent from this map) gets the base transmit plus
    /// every buffered frame in one shot; an already-attached client only gets
    /// the frames whose seq is past its recorded watermark. Tracking a seq
    /// watermark rather than a received *count* matters because
    /// `ImageStore::push_frame` caps the stored log at `CAP_FRAMES_PER_IMAGE`
    /// and trims the front (`remove(0)`) past it — `frames.len()` then no
    /// longer reflects how many frames have ever arrived, so comparing a count
    /// against it can never advance again once the client catches up to the
    /// cap. `seq` is assigned once per frame and is stable across that
    /// trimming, so `seq > watermark` keeps working (2026-07-06 inline-
    /// graphics bug audit, finding #2).
    last_frame_seq: HashMap<u32, u64>,
    /// Host image id → the last `a=a` control state sent to this client, so
    /// it's only re-sent when it changes (or on first sight of the image).
    last_anim_sent: HashMap<u32, AnimControl>,
}

impl DiffRenderer {
    pub fn new() -> Self {
        Self {
            previous: None,
            graphics: GraphicsCaps::default(),
            transmitted: HashMap::new(),
            transmitted_virtual: HashMap::new(),
            placed: HashMap::new(),
            boxed: HashMap::new(),
            virtual_placed: HashMap::new(),
            placed_data: HashMap::new(),
            reset_images: false,
            last_frame_seq: HashMap::new(),
            last_anim_sent: HashMap::new(),
        }
    }

    /// Set this client's negotiated graphics capabilities.
    pub const fn set_graphics_caps(&mut self, caps: GraphicsCaps) {
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
            // Equivalent note (105:36/69/40/72): `&& → ||` is equivalent because
            // `transmitted`/`placed` are only populated when kitty=true, so they're
            // empty when kitty=false and the mutant fires the same false branch.
            // `|| → &&` differs only when one set is non-empty and the other empty,
            // which doesn't occur because both sets are populated and cleared together
            // by the same Kitty transmit/place path. Delete-`!` mutations invert each
            // guard but are untested without a Kitty test that exercises reset_images.
            if self.graphics.kitty && (!self.transmitted.is_empty() || !self.placed.is_empty()) {
                out.push_str("\x1b_Ga=d,d=A,q=2\x1b\\");
            }
            self.transmitted.clear();
            self.transmitted_virtual.clear();
            self.placed.clear();
            self.boxed.clear();
            self.virtual_placed.clear();
            self.placed_data.clear();
            self.last_frame_seq.clear();
            self.last_anim_sent.clear();
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
            // Equivalent note (128:69): `|| → &&` is equivalent in the common case,
            // since `transmitted` and `placed` are populated together for classic
            // placements, so one being non-empty while the other is empty does not
            // occur in normal operation. A virtual-placement-only session would be a
            // real gap.
            if self.graphics.kitty && (!self.transmitted.is_empty() || !self.placed.is_empty()) {
                out.push_str("\x1b_Ga=d,d=A,q=2\x1b\\");
            }
            self.transmitted.clear();
            self.transmitted_virtual.clear();
            self.placed.clear();
            self.boxed.clear();
            self.virtual_placed.clear();
            self.placed_data.clear();
            self.last_frame_seq.clear();
            self.last_anim_sent.clear();
            // Clear + home, then walk every row, every cell.
            out.push_str("\x1b[2J\x1b[H");
            let mut current_attrs = CellAttrs::default();
            for r in 0..current.rows {
                let _ = write!(out, "\x1b[{};1H", r + 1);
                let mut c = 0u16;
                // Equivalent note: `< → <=` at 143:25 is equivalent, the extra iteration
                // calls `current.cell(r, cols)` which returns `None` and we break.
                while c < current.cols {
                    let Some(cell) = current.cell(r, c) else {
                        break;
                    };
                    // Equivalent note: spacer-skip (146:27) is dead code in this full-repaint
                    // loop: wide chars advance `c` by `grapheme_advance()`=2, so the cursor
                    // always jumps over the spacer position and the `+= 1` branch (`-=` and
                    // `*=` mutations) is never reached.
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
                // Equivalent note: `< → <=` at 166:25 is equivalent, the extra iteration
                // calls `current.cell(r, cols)` which returns `None` and we break.
                while c < current.cols {
                    let pc = prev.cell(r, c);
                    let cc = current.cell(r, c);
                    if pc == cc {
                        c += 1;
                        continue;
                    }
                    // Run start.
                    let _ = write!(out, "\x1b[{};{}H", r + 1, c + 1);
                    // Equivalent note: `< → <=` at 175:29 is equivalent, same reason: the
                    // extra iteration gets `None` and we break.
                    while c < current.cols {
                        let Some(cell) = current.cell(r, c) else {
                            break;
                        };
                        if Some(cell) == prev.cell(r, c) {
                            break;
                        }
                        // Equivalent note: spacer-skip (181:31) is dead code in this
                        // incremental-diff loop for the same reason as 146:27: wide chars advance
                        // `c` by `grapheme_advance()`=2, so the cursor never lands on a spacer
                        // position in mid-run traversal.
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

        // Inline-image placements, dispatched per placement by (source protocol,
        // this client's caps): Kitty gets transmit-once/place-by-id, Sixel/iTerm2
        // re-emit data at the host cell, and a placement whose protocol the
        // client can't render becomes a labelled placeholder box of the same
        // footprint, so heterogeneous clients keep a consistent layout. Each pass
        // owns a disjoint set of placement keys, so their diff maps never overlap.
        self.render_kitty_placements(&mut out, current);
        self.render_virtual_placements(&mut out, current);
        self.render_overlay_placements(&mut out, current);

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
    /// delete (placement only, data retained) for gone/moved. Only Kitty-protocol
    /// placements on a Kitty-capable client.
    fn render_kitty_placements(&mut self, out: &mut String, current: &VirtualScreen) {
        use plexy_glass_emulator::ImageProtocol;
        if !self.graphics.kitty {
            return;
        }
        let mut seen: HashSet<u64> = HashSet::with_capacity(current.placements.len());
        for p in &current.placements {
            if p.protocol != ImageProtocol::Kitty {
                continue;
            }
            seen.insert(p.key);
            // Transmit once per (id, content generation). An image with no data can't be
            // transmitted or placed, so skip it without poisoning the transmitted map
            // (poisoning it would block a later real transmit of the id).
            if p.data_b64.is_empty() {
                continue;
            }
            let is_new_generation = self.transmitted.get(&p.image_id) != Some(&p.generation);
            if is_new_generation {
                emit_transmit(out, p);
                self.transmitted.insert(p.image_id, p.generation);
                // A fresh generation means the base image content changed
                // (a=t reset it, which also clears the source's frame log —
                // see Task 2 Step 12), so any previously replayed frame log
                // no longer applies to this client's view of the content.
                self.last_frame_seq.insert(p.image_id, 0);
                self.last_anim_sent.remove(&p.image_id);
            }
            // Forward every frame whose seq is past this client's watermark,
            // in log order (the log is always ascending by seq — see
            // `ImageStore::push_frame`). Using `seq` instead of an index/count
            // keeps this correct even after the store has trimmed the front of
            // `p.frames` past `CAP_FRAMES_PER_IMAGE` (finding #2).
            if let Some(last) = p.frames.last() {
                let watermark = self.last_frame_seq.get(&p.image_id).copied().unwrap_or(0);
                // Equivalent note: `> → >=` is equivalent. At the boundary
                // (last.seq == watermark) a `>=` mutant enters the guard where
                // the real code wouldn't, but the inner filter still finds zero
                // frames with `seq > watermark`, so the loop emits nothing, and
                // re-inserting the same `watermark` value is a no-op — no
                // observable difference either way.
                if last.seq > watermark {
                    for f in p.frames.iter().filter(|f| f.seq > watermark) {
                        emit_frame(out, p.image_id, f);
                    }
                    self.last_frame_seq.insert(p.image_id, last.seq);
                }
            }
            if let Some(ctrl) = &p.anim_control
                && self.last_anim_sent.get(&p.image_id) != Some(ctrl)
            {
                emit_anim_control(out, p.image_id, ctrl);
                self.last_anim_sent.insert(p.image_id, ctrl.clone());
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
        let gone: Vec<u64> = self
            .placed
            .keys()
            .copied()
            .filter(|k| !seen.contains(k))
            .collect();
        for k in gone {
            if let Some(rect) = self.placed.remove(&k) {
                emit_delete(out, rect.image_id, rect.placement_id);
            }
        }
        // Bound the transmitted set: transmit-once keeps scrolled-off ids cached,
        // so over a long session with many distinct images it would grow without
        // limit. Past the cap, schedule a full graphics reset next frame (delete
        // all + re-transmit the visible set), which is rare and self-healing.
        // Equivalent note (283:35): `> → >=` shifts the trigger by one element
        // (256 vs 257); `> → ==` misses everything above 256. Both are real
        // behavioral differences, but testing requires >256 distinct image
        // transmissions in a single test fixture, so this is left as an untested
        // gap.
        const TRANSMIT_CAP: usize = 256;
        if self.transmitted.len() > TRANSMIT_CAP {
            self.reset_images = true;
        }
    }

    /// Overlay placements that paint into cells (not by-id): the placeholder box
    /// (for a placement whose protocol this client can't render) and Sixel/iTerm2
    /// data images. Both are renderer-injected output, NOT cells in the
    /// `VirtualScreen` grid the cell diff tracks, so the cell diff or one pass's
    /// repaint can clobber the other. To stay correct, do it in strict order:
    ///
    ///   1. repaint every stale region (vanished/moved boxes AND data images),
    ///      collecting the repainted rects,
    ///   2. draw ALL current boxes (cheap, every frame),
    ///   3. (re)emit data images, lowest z first, re-emitting when new, when
    ///      the rect changed, when the footprint overlaps a region repainted
    ///      in step 1 (or by an earlier, lower-z placement re-emitted in this
    ///      same step — so a higher placement isn't left stale under a fresh
    ///      lower paint, which would invert the stack order), or when an
    ///      underlying cell changed this frame (so an in-place redraw like a
    ///      status line, prompt, or spinner can't leave a hole).
    ///
    /// Data emit runs last so no later repaint can clobber the (expensive) image
    /// bytes; boxes redraw every frame so they're never holed.
    fn render_overlay_placements(&mut self, out: &mut String, current: &VirtualScreen) {
        use plexy_glass_emulator::ImageProtocol;
        let caps = self.graphics; // Copy, so the closures don't borrow self
        let is_box = move |p: &VisiblePlacement| match p.protocol {
            ImageProtocol::Kitty => !caps.kitty,
            ImageProtocol::Sixel => !caps.sixel,
            ImageProtocol::Iterm2 => !caps.iterm2,
        };
        let is_data = move |p: &VisiblePlacement| match p.protocol {
            ImageProtocol::Sixel => caps.sixel,
            ImageProtocol::Iterm2 => caps.iterm2,
            ImageProtocol::Kitty => false,
        };
        let rect_of = |p: &VisiblePlacement| PlacedRect {
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

        // ── Step 1: repaint stale regions (box + data), recording the rects. ──
        let mut repainted: Vec<PlacedRect> = Vec::new();
        let box_seen: HashSet<u64> = current
            .placements
            .iter()
            .filter(|p| is_box(p))
            .map(|p| p.key)
            .collect();
        let data_seen: HashSet<u64> = current
            .placements
            .iter()
            .filter(|p| is_data(p))
            .map(|p| p.key)
            .collect();
        for (k, prev) in self.boxed.iter().chain(self.placed_data.iter()) {
            let live = current.placements.iter().find(|p| p.key == *k);
            let stale = match live {
                None => true,
                Some(p) => {
                    (prev.host_row, prev.host_col, prev.rows, prev.cols)
                        != (p.host_row, p.host_col, p.rows, p.cols)
                }
            };
            if stale {
                repainted.push(*prev);
            }
        }
        for rect in &repainted {
            paint_cells_rect(
                out,
                current,
                rect.host_row,
                rect.host_col,
                rect.rows,
                rect.cols,
            );
        }
        self.boxed.retain(|k, _| box_seen.contains(k));
        self.placed_data.retain(|k, _| data_seen.contains(k));

        // ── Step 2: draw every current box (over any just-repainted region). ──
        for p in current.placements.iter().filter(|p| is_box(p)) {
            emit_placeholder_box(out, p);
            self.boxed.insert(p.key, rect_of(p));
        }

        // ── Step 3: (re)emit data images last, lowest z first so a higher z
        // paints on top — mirrors Kitty's own tie-break (same z → lower
        // image id under, since Sixel/iTerm2 have no native compositor and we
        // must explicitly order paint sequences).
        let prev = self.previous.as_ref();
        let mut data_placements: Vec<&VisiblePlacement> =
            current.placements.iter().filter(|p| is_data(p)).collect();
        data_placements.sort_by_key(|p| (p.z, p.image_id));
        for p in data_placements {
            if p.data_b64.is_empty() {
                continue;
            }
            let rect = rect_of(p);
            let disturbed = self.placed_data.get(&p.key) != Some(&rect)
                || repainted.iter().any(|r| rects_overlap(r, &rect))
                || footprint_cells_changed(prev, current, &rect);
            if disturbed {
                let _ = write!(out, "\x1b7\x1b[{};{}H", p.host_row + 1, p.host_col + 1);
                emit_data_image(out, p);
                out.push_str("\x1b8");
                // This rect just got fresh pixels painted over it, so any
                // later (higher z / higher image-id) placement overlapping it
                // must also re-emit this pass, or the paint we just did would
                // cover it and invert the stack order.
                repainted.push(rect);
            }
            self.placed_data.insert(p.key, rect);
        }
    }

    /// Unicode-placeholder (virtual) placements for a Kitty client: transmit the
    /// image once (per-pane-folded wire id + generation, in the dedicated
    /// `transmitted_virtual` cache) and emit `a=p,U=1` once. The `image_id` here is
    /// already the folded wire id (the compositor rewrote the placeholder cells' fg
    /// to match), so we place under it directly. The placeholder cells render via
    /// the ordinary cell diff; deleting the virtual placement removes it.
    fn render_virtual_placements(&mut self, out: &mut String, current: &VirtualScreen) {
        if !self.graphics.kitty {
            return; // virtual placements are Kitty-only
        }
        let mut seen: HashSet<u64> = HashSet::with_capacity(current.virtual_placements.len());
        for vp in &current.virtual_placements {
            if vp.data_b64.is_empty() {
                continue; // nothing to transmit/place
            }
            seen.insert(vp.key);
            if self.transmitted_virtual.get(&vp.image_id) != Some(&vp.generation) {
                emit_transmit_bytes(
                    out,
                    vp.image_id,
                    vp.format.kitty_f(),
                    vp.pixel_w,
                    vp.pixel_h,
                    &vp.data_b64,
                );
                self.transmitted_virtual.insert(vp.image_id, vp.generation);
            }
            if let Entry::Vacant(slot) = self.virtual_placed.entry(vp.key) {
                // Omit c=/r= when 0 (the placeholder cells define the extent).
                let _ = write!(out, "\x1b_Ga=p,U=1,i={},p={}", vp.image_id, vp.placement_id);
                if vp.cols > 0 {
                    let _ = write!(out, ",c={}", vp.cols);
                }
                if vp.rows > 0 {
                    let _ = write!(out, ",r={}", vp.rows);
                }
                out.push_str(",q=2\x1b\\");
                slot.insert((vp.image_id, vp.placement_id));
            }
        }
        // Delete virtual placements that vanished this frame.
        let gone: Vec<u64> = self
            .virtual_placed
            .keys()
            .copied()
            .filter(|k| !seen.contains(k))
            .collect();
        for k in gone {
            if let Some((img, pid)) = self.virtual_placed.remove(&k) {
                emit_delete(out, img, pid);
            }
        }
        // Bound the virtual transmit cache like the classic one.
        const TRANSMIT_CAP: usize = 256;
        if self.transmitted_virtual.len() > TRANSMIT_CAP {
            self.reset_images = true;
        }
    }
}

/// Do two cell rectangles overlap?
const fn rects_overlap(a: &PlacedRect, b: &PlacedRect) -> bool {
    a.host_row < b.host_row.saturating_add(b.rows)
        && b.host_row < a.host_row.saturating_add(a.rows)
        && a.host_col < b.host_col.saturating_add(b.cols)
        && b.host_col < a.host_col.saturating_add(a.cols)
}

/// Did any cell within `rect`'s footprint change between the previous frame and
/// `current`? We use this to redraw a data image when an in-place redraw (status
/// line, prompt, spinner) repainted a cell under it. A `None` previous is treated
/// as changed (first frame; handled as a fresh emit anyway).
fn footprint_cells_changed(
    prev: Option<&VirtualScreen>,
    current: &VirtualScreen,
    rect: &PlacedRect,
) -> bool {
    let Some(prev) = prev else { return true };
    for r in rect.host_row..rect.host_row.saturating_add(rect.rows) {
        for c in rect.host_col..rect.host_col.saturating_add(rect.cols) {
            if prev.cell(r, c) != current.cell(r, c) {
                return true;
            }
        }
    }
    false
}

/// Emit a Sixel or iTerm2 image's wire bytes (the cursor is already positioned
/// and saved by the caller).
fn emit_data_image(out: &mut String, p: &VisiblePlacement) {
    use plexy_glass_emulator::ImageProtocol;
    match p.protocol {
        ImageProtocol::Sixel => {
            out.push_str("\x1bP");
            out.push_str(&String::from_utf8_lossy(&p.data_b64));
            out.push_str("\x1b\\");
        }
        ImageProtocol::Iterm2 => {
            out.push_str("\x1b]1337;File=");
            if let Some(args) = &p.iterm_args {
                out.push_str(args);
            }
            out.push(':');
            out.push_str(&String::from_utf8_lossy(&p.data_b64));
            out.push('\x07');
        }
        ImageProtocol::Kitty => {} // not a data-emit protocol
    }
}

/// Repaint a rectangle of cells from `screen` (used to clear a placeholder box
/// when its placement vanishes or moves).
fn paint_cells_rect(
    out: &mut String,
    screen: &VirtualScreen,
    r0: u16,
    c0: u16,
    rows: u16,
    cols: u16,
) {
    let mut attrs = CellAttrs::default();
    out.push_str("\x1b[0m");
    for r in r0..r0.saturating_add(rows).min(screen.rows) {
        let _ = write!(out, "\x1b[{};{}H", r + 1, c0 + 1);
        let mut c = c0;
        // Equivalent note: `< → <=` on the loop bound is equivalent, the extra
        // iteration calls `screen.cell(r, saturated_limit)` which returns `None`
        // and we break. Spacer-skip `+= 1` mutations (`-=`, `*=`) are dead code
        // for the same reason as the main render loops: wide chars advance `c` by
        // `grapheme_advance()`=2, so the cursor never lands on a spacer position.
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
    let inner = cols - 2;
    // The label is ASCII digits + 'x', but we still route width/truncation
    // through the width module per the project's display-column rule.
    let raw = format!("{}x{}", p.pixel_w, p.pixel_h);
    let label = plexy_glass_emulator::truncate_to_width(&raw, inner);
    let label_w = plexy_glass_emulator::display_width(label);
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
            let pad = inner.saturating_sub(label_w);
            let left = pad / 2;
            for _ in 0..left {
                out.push(' ');
            }
            out.push_str(label);
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
    emit_transmit_bytes(
        out,
        p.image_id,
        p.format.kitty_f(),
        p.pixel_w,
        p.pixel_h,
        &p.data_b64,
    );
}

/// Shared transmit emitter (classic + virtual placements).
fn emit_transmit_bytes(
    out: &mut String,
    image_id: u32,
    f: u32,
    pixel_w: u32,
    pixel_h: u32,
    data: &[u8],
) {
    if data.is_empty() {
        return;
    }
    const CHUNK: usize = 4096;
    let n = data.len();
    let mut i = 0;
    let mut first = true;
    while i < n {
        let end = (i + CHUNK).min(n);
        let more = u8::from(end < n);
        if first {
            let _ = write!(
                out,
                "\x1b_Gi={image_id},a=t,f={f},s={pixel_w},v={pixel_h},q=2,m={more};"
            );
            first = false;
        } else {
            let _ = write!(out, "\x1b_Gm={more};");
        }
        out.push_str(&String::from_utf8_lossy(&data[i..end]));
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
    let cropped = p.src_x > 0 || p.src_y > 0 || p.src_w < p.pixel_w || p.src_h < p.pixel_h;
    let _ = write!(out, "\x1b_Ga=p,i={},p={}", p.image_id, p.placement_id);
    if cropped {
        let _ = write!(
            out,
            ",x={},y={},w={},h={}",
            p.src_x, p.src_y, p.src_w, p.src_h
        );
    }
    if p.z != 0 {
        let _ = write!(out, ",z={}", p.z);
    }
    let _ = write!(out, ",r={},c={},q=2\x1b\\", p.rows, p.cols);
}

/// Delete a single placement (lowercase `d=i` keeps the image data for re-place).
fn emit_delete(out: &mut String, image_id: u32, placement_id: u32) {
    let _ = write!(out, "\x1b_Ga=d,d=i,i={image_id},p={placement_id},q=2\x1b\\");
}

/// Replay one stored `a=f` command verbatim (chunked like `emit_transmit_bytes`).
fn emit_frame(out: &mut String, image_id: u32, f: &Frame) {
    if f.data_b64.is_empty() {
        return;
    }
    const CHUNK: usize = 4096;
    let n = f.data_b64.len();
    let mut i = 0;
    let mut first = true;
    while i < n {
        let end = (i + CHUNK).min(n);
        let more = u8::from(end < n);
        if first {
            let _ = write!(out, "\x1b_Gi={image_id},a=f,f={}", f.format.kitty_f());
            if let Some(r) = f.frame_number {
                let _ = write!(out, ",r={r}");
            }
            if let Some(c) = f.canvas_source {
                let _ = write!(out, ",c={c}");
            }
            if f.x != 0 {
                let _ = write!(out, ",x={}", f.x);
            }
            if f.y != 0 {
                let _ = write!(out, ",y={}", f.y);
            }
            if f.width != 0 {
                let _ = write!(out, ",s={}", f.width);
            }
            if f.height != 0 {
                let _ = write!(out, ",v={}", f.height);
            }
            if f.overwrite {
                let _ = write!(out, ",X=1");
            }
            if f.bg_color != 0 {
                let _ = write!(out, ",Y={}", f.bg_color);
            }
            if f.gap_ms != 0 {
                let _ = write!(out, ",z={}", f.gap_ms);
            }
            let _ = write!(out, ",q=2,m={more};");
            first = false;
        } else {
            let _ = write!(out, "\x1b_Ga=f,i={image_id},m={more};");
        }
        out.push_str(&String::from_utf8_lossy(&f.data_b64[i..end]));
        out.push_str("\x1b\\");
        i = end;
    }
}

/// Replay the latest `a=a` control command for an image.
fn emit_anim_control(out: &mut String, image_id: u32, ctrl: &AnimControl) {
    let _ = write!(out, "\x1b_Gi={image_id},a=a");
    if let Some(s) = ctrl.state {
        let _ = write!(out, ",s={s}");
    }
    if let Some(v) = ctrl.loop_count {
        let _ = write!(out, ",v={v}");
    }
    if let Some(c) = ctrl.current_frame {
        let _ = write!(out, ",c={c}");
    }
    out.push_str(",q=2\x1b\\");
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
    const fn from_cell(c: &Cell) -> Self {
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
    use std::sync::Arc;

    use hegel::{TestCase, generators as gs};
    use plexy_glass_emulator::{Image, ImageFormat, ImageProtocol, ImageStore};
    use smol_str::SmolStr;

    use super::*;
    use crate::virtual_screen::VisibleVirtualPlacement;

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
        assert!(
            s.starts_with("\x1b[2J\x1b[H"),
            "expected initial clear: {s:?}"
        );
        assert!(s.contains('A'));
    }

    #[test]
    fn second_render_no_change_emits_only_cursor_reset() {
        let mut d = DiffRenderer::new();
        let v = lettered(&[(0, 0, "A")], 2, 2);
        let _ = d.render(&v);
        let bytes = d.render(&v);
        let s = String::from_utf8_lossy(&bytes);
        assert!(
            !s.contains('A'),
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
        assert!(s.contains('B'));
        assert!(
            s.contains("\x1b[2;2H"),
            "expected CUP to row 2 col 2: {s:?}"
        );
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
    fn rows_only_change_forces_full_repaint() {
        // `|| → &&` at line 119:47 would skip full-repaint when only ONE dimension
        // changes (requires BOTH to differ under &&). This test changes only rows.
        let mut d = DiffRenderer::new();
        let v1 = lettered(&[(0, 0, "A")], 2, 4);
        let v2 = lettered(&[(0, 0, "A")], 6, 4); // only rows differ
        let _ = d.render(&v1);
        let bytes = d.render(&v2);
        let s = String::from_utf8_lossy(&bytes);
        assert!(
            s.starts_with("\x1b[2J\x1b[H"),
            "row-only resize must trigger full repaint (2J): {s:?}"
        );
    }

    #[test]
    fn cols_only_change_forces_full_repaint() {
        // Symmetric companion to the above: only cols differ.
        let mut d = DiffRenderer::new();
        let v1 = lettered(&[(0, 0, "A")], 4, 2);
        let v2 = lettered(&[(0, 0, "A")], 4, 8); // only cols differ
        let _ = d.render(&v1);
        let bytes = d.render(&v2);
        let s = String::from_utf8_lossy(&bytes);
        assert!(
            s.starts_with("\x1b[2J\x1b[H"),
            "col-only resize must trigger full repaint (2J): {s:?}"
        );
    }

    #[test]
    fn wide_grapheme_full_repaint_skips_spacer() {
        let mut d = DiffRenderer::new();
        let mut v = VirtualScreen::blank(1, 4);
        v.put(
            0,
            0,
            Cell {
                grapheme: SmolStr::new("世"),
                ..Cell::default()
            },
        );
        v.put(0, 1, Cell::wide_spacer());
        v.put(
            0,
            2,
            Cell {
                grapheme: SmolStr::new("X"),
                ..Cell::default()
            },
        );
        let bytes = d.render(&v);
        let s = String::from_utf8_lossy(&bytes);
        assert_eq!(
            s.matches('世').count(),
            1,
            "wide grapheme emitted once: {s:?}"
        );
        assert!(
            s.contains("世X"),
            "spacer skipped; X immediately follows 世: {s:?}"
        );
        assert!(
            !s.contains("世 X"),
            "no stray space painted for the spacer: {s:?}"
        );
    }

    #[test]
    fn wide_grapheme_incremental_diff_targets_only_changed_cell() {
        let mut d = DiffRenderer::new();
        let mut v1 = VirtualScreen::blank(1, 4);
        v1.put(
            0,
            0,
            Cell {
                grapheme: SmolStr::new("世"),
                ..Cell::default()
            },
        );
        v1.put(0, 1, Cell::wide_spacer());
        v1.put(
            0,
            2,
            Cell {
                grapheme: SmolStr::new("a"),
                ..Cell::default()
            },
        );
        let _ = d.render(&v1);
        let mut v2 = VirtualScreen::blank(1, 4);
        v2.put(
            0,
            0,
            Cell {
                grapheme: SmolStr::new("世"),
                ..Cell::default()
            },
        );
        v2.put(0, 1, Cell::wide_spacer());
        v2.put(
            0,
            2,
            Cell {
                grapheme: SmolStr::new("b"),
                ..Cell::default()
            },
        );
        let bytes = d.render(&v2);
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains('b'), "changed cell emitted: {s:?}");
        assert!(
            !s.contains('世'),
            "unchanged wide grapheme not re-emitted: {s:?}"
        );
        assert!(
            s.contains("\x1b[1;3H"),
            "CUP targets the changed cell at col 3: {s:?}"
        );
    }

    #[test]
    fn underline_color_rgb_emits_58_2() {
        let mut d = DiffRenderer::new();
        let mut v = VirtualScreen::blank(1, 2);
        v.put(
            0,
            0,
            Cell {
                grapheme: SmolStr::new("U"),
                underline_color: Color::Rgb(10, 20, 30),
                ..Cell::default()
            },
        );
        let bytes = d.render(&v);
        let s = String::from_utf8_lossy(&bytes);
        assert!(
            s.contains("\x1b[58:2:10:20:30m"),
            "expected RGB underline-color SGR: {s:?}"
        );
    }

    #[test]
    fn underline_color_indexed_emits_58_5() {
        let mut d = DiffRenderer::new();
        let mut v = VirtualScreen::blank(1, 2);
        v.put(
            0,
            0,
            Cell {
                grapheme: SmolStr::new("U"),
                underline_color: Color::Indexed(9),
                ..Cell::default()
            },
        );
        let bytes = d.render(&v);
        let s = String::from_utf8_lossy(&bytes);
        assert!(
            s.contains("\x1b[58:5:9m"),
            "expected indexed underline-color SGR: {s:?}"
        );
    }

    #[test]
    fn default_underline_color_emits_no_58() {
        let mut d = DiffRenderer::new();
        let v = lettered(&[(0, 0, "A")], 1, 2);
        let bytes = d.render(&v);
        let s = String::from_utf8_lossy(&bytes);
        assert!(
            !s.contains("\x1b[58"),
            "default underline color must emit no 58: {s:?}"
        );
    }

    #[test]
    fn underline_style_curly_emits_4_3() {
        let mut d = DiffRenderer::new();
        let mut v = VirtualScreen::blank(1, 2);
        v.put(
            0,
            0,
            Cell {
                grapheme: SmolStr::new("U"),
                attrs: Attrs::UNDERLINE,
                underline_style: UnderlineStyle::Curly,
                ..Cell::default()
            },
        );
        let bytes = d.render(&v);
        let s = String::from_utf8_lossy(&bytes);
        assert!(
            s.contains("\x1b[4:3m"),
            "expected curly underline SGR: {s:?}"
        );
    }

    #[test]
    fn underline_style_single_emits_plain_4() {
        let mut d = DiffRenderer::new();
        let mut v = VirtualScreen::blank(1, 2);
        v.put(
            0,
            0,
            Cell {
                grapheme: SmolStr::new("U"),
                attrs: Attrs::UNDERLINE,
                underline_style: UnderlineStyle::Single,
                ..Cell::default()
            },
        );
        let bytes = d.render(&v);
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("\x1b[4m"), "expected plain underline SGR: {s:?}");
        assert!(
            !s.contains("\x1b[4:"),
            "single must not emit a colon form: {s:?}"
        );
    }

    #[test]
    fn no_underline_emits_no_4() {
        let mut d = DiffRenderer::new();
        let v = lettered(&[(0, 0, "A")], 1, 2);
        let bytes = d.render(&v);
        let s = String::from_utf8_lossy(&bytes);
        assert!(
            !s.contains("\x1b[4m"),
            "no-underline cell must emit no 4: {s:?}"
        );
        assert!(
            !s.contains("\x1b[4:"),
            "no-underline cell must emit no 4:N: {s:?}"
        );
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
        v2.put(
            0,
            0,
            Cell {
                grapheme: SmolStr::new("U"),
                attrs: Attrs::UNDERLINE,
                underline_style: UnderlineStyle::Curly,
                ..Cell::default()
            },
        );
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
        assert!(
            s.contains("\x1b[58:2:1:2:3m"),
            "diff path must emit 58: {s:?}"
        );
    }

    // ── inline-image placement diff ───────────────────────────────────────────

    fn vp(
        key: u64,
        image_id: u32,
        placement_id: u32,
        host_row: u16,
        host_col: u16,
    ) -> VisiblePlacement {
        VisiblePlacement {
            key,
            image_id,
            placement_id,
            protocol: plexy_glass_emulator::ImageProtocol::Kitty,
            iterm_args: None,
            generation: 1,
            format: plexy_glass_emulator::ImageFormat::Png,
            pixel_w: 30,
            pixel_h: 40,
            src_x: 0,
            src_y: 0,
            src_w: 30,
            src_h: 40,
            data_b64: Arc::from(&b"QUJD"[..]),
            host_row,
            host_col,
            rows: 2,
            cols: 3,
            z: 0,
            frames: Arc::new(Vec::new()),
            anim_control: None,
        }
    }

    /// A renderer with Kitty graphics enabled (the default is now all-off).
    fn kitty_renderer() -> DiffRenderer {
        let mut d = DiffRenderer::new();
        d.set_graphics_caps(GraphicsCaps {
            kitty: true,
            sixel: false,
            iterm2: false,
        });
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
        assert!(
            s.contains("\x1b_Gi=7,a=t,f=100,s=30,v=40"),
            "transmit once: {s:?}"
        );
        // Place at host (row 3, col 4) 1-based, by id, forcing r/c.
        assert!(
            s.contains("\x1b[3;4H\x1b_Ga=p,i=7,p=1,r=2,c=3,q=2\x1b\\"),
            "place by id: {s:?}"
        );
    }

    #[test]
    fn unchanged_frame_re_emits_nothing() {
        let mut d = kitty_renderer();
        let f = frame_with(vec![vp(1, 7, 1, 2, 3)]);
        render_str(&mut d, &f);
        let s = render_str(&mut d, &f);
        assert!(
            !s.contains("\x1b_G"),
            "no graphics re-emitted for an unchanged frame: {s:?}"
        );
    }

    #[test]
    fn moved_placement_deletes_old_and_places_new_without_retransmit() {
        let mut d = kitty_renderer();
        render_str(&mut d, &frame_with(vec![vp(1, 7, 1, 2, 3)]));
        let s = render_str(&mut d, &frame_with(vec![vp(1, 7, 1, 4, 3)])); // moved down 2 rows
        assert!(!s.contains("a=t"), "image already transmitted: {s:?}");
        assert!(
            s.contains("\x1b_Ga=d,d=i,i=7,p=1,q=2\x1b\\"),
            "delete old placement: {s:?}"
        );
        assert!(
            s.contains("\x1b[5;4H\x1b_Ga=p,i=7,p=1"),
            "re-place at new row: {s:?}"
        );
    }

    #[test]
    fn vanished_placement_is_deleted() {
        let mut d = kitty_renderer();
        render_str(&mut d, &frame_with(vec![vp(1, 7, 1, 2, 3)]));
        let s = render_str(&mut d, &frame_with(vec![]));
        assert!(
            s.contains("\x1b_Ga=d,d=i,i=7,p=1,q=2\x1b\\"),
            "delete vanished placement: {s:?}"
        );
    }

    #[test]
    fn kitty_placement_emits_nonzero_z() {
        let mut d = kitty_renderer();
        let mut p = vp(1, 7, 1, 2, 3);
        p.z = 7;
        let s = render_str(&mut d, &frame_with(vec![p]));
        assert!(
            s.contains(",z=7"),
            "expected z=7 in the emitted a=p command, got: {s:?}"
        );
    }

    #[test]
    fn kitty_placement_omits_z_when_zero() {
        let mut d = kitty_renderer();
        let s = render_str(&mut d, &frame_with(vec![vp(1, 7, 1, 2, 3)])); // z defaults to 0
        assert!(!s.contains(",z="), "z=0 should be omitted, got: {s:?}");
    }

    #[test]
    fn non_kitty_client_emits_no_graphics() {
        let mut d = DiffRenderer::new();
        d.set_graphics_caps(GraphicsCaps {
            kitty: false,
            sixel: false,
            iterm2: false,
        });
        let s = render_str(&mut d, &frame_with(vec![vp(1, 7, 1, 2, 3)]));
        assert!(
            !s.contains("\x1b_G"),
            "no graphics bytes for a non-kitty client: {s:?}"
        );
    }

    #[test]
    fn invalidate_resets_images_then_retransmits() {
        let mut d = kitty_renderer();
        render_str(&mut d, &frame_with(vec![vp(1, 7, 1, 2, 3)]));
        d.invalidate(); // session switch / re-point
        let s = render_str(&mut d, &frame_with(vec![vp(1, 7, 1, 2, 3)]));
        assert!(
            s.contains("\x1b_Ga=d,d=A,q=2\x1b\\"),
            "reset deletes all images: {s:?}"
        );
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
        p.data_b64 = Arc::from(&b"WFla"[..]);
        let s = render_str(&mut d, &frame_with(vec![p]));
        assert!(
            s.contains("a=t"),
            "changed content re-transmits id 7: {s:?}"
        );
    }

    #[test]
    fn invalidate_with_nothing_transmitted_emits_no_reset_delete() {
        // Kills the two `delete !` survivors on the reset_images guard
        // (`self.graphics.kitty && (!self.transmitted.is_empty() ||
        // !self.placed.is_empty())`): the existing `invalidate_resets_images_
        // then_retransmits` test only exercises the case where both maps are
        // non-empty, so removing either `!` alone still leaves the `||` true
        // (the other term already covers it) and the mutant survives. Here
        // nothing was ever transmitted, so both maps are empty and the real
        // guard is false; a `delete !` mutant flips one side to `.is_empty()`,
        // making the guard true and spuriously emitting the reset-delete.
        let mut d = kitty_renderer();
        d.invalidate(); // reset before any Kitty placement was ever rendered
        let s = render_str(&mut d, &frame_with(vec![]));
        assert!(
            !s.contains("\x1b_Ga=d,d=A,q=2\x1b\\"),
            "nothing was ever transmitted; no reset-delete expected: {s:?}"
        );
    }

    #[test]
    fn transmit_cap_exceeded_schedules_reset_next_frame() {
        // Real coverage gap called out in source at the TRANSMIT_CAP guard:
        // "testing requires >256 distinct image transmissions in a single
        // test fixture" — that's just 257 placements with distinct ids.
        let mut d = kitty_renderer();
        let placements: Vec<VisiblePlacement> = (1..=257u32)
            .map(|id| vp(u64::from(id), id, 1, 2, 3))
            .collect();
        render_str(&mut d, &frame_with(placements.clone()));
        // Frame is unchanged, so an ordinary render would emit nothing new for
        // these placements — but crossing TRANSMIT_CAP schedules a full reset
        // for the *next* render.
        let s = render_str(&mut d, &frame_with(placements));
        assert!(
            s.contains("\x1b_Ga=d,d=A,q=2\x1b\\"),
            "exceeding TRANSMIT_CAP must schedule a full reset: {s:?}"
        );
    }

    #[test]
    fn transmit_cap_at_exactly_256_does_not_reset() {
        // Boundary companion to `transmit_cap_exceeded_schedules_reset_next_frame`:
        // distinguishes `>` from a `>= TRANSMIT_CAP` mutant. At exactly 256
        // distinct transmissions the real guard (`> 256`) is false, so an
        // unchanged second frame must stay silent; a `>=` mutant would wrongly
        // schedule a reset here.
        let mut d = kitty_renderer();
        let placements: Vec<VisiblePlacement> = (1..=256u32)
            .map(|id| vp(u64::from(id), id, 1, 2, 3))
            .collect();
        render_str(&mut d, &frame_with(placements.clone()));
        let s = render_str(&mut d, &frame_with(placements));
        assert!(
            !s.contains("\x1b_G"),
            "exactly at TRANSMIT_CAP, unchanged frame must stay silent: {s:?}"
        );
    }

    // ── animation frame/control replay ─────────────────────────────────────────

    fn sample_frame_visible(seq: u64, data: &[u8]) -> Frame {
        Frame {
            frame_number: None,
            canvas_source: None,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            overwrite: false,
            bg_color: 0,
            gap_ms: 0,
            format: plexy_glass_emulator::ImageFormat::Rgba,
            data_b64: Arc::from(data),
            seq,
        }
    }

    #[test]
    fn new_client_gets_base_transmit_all_frames_and_latest_control() {
        let mut d = kitty_renderer();
        let mut p = vp(1, 7, 1, 2, 3);
        p.frames = Arc::new(vec![
            sample_frame_visible(1, b"f1"),
            sample_frame_visible(2, b"f2"),
        ]);
        p.anim_control = Some(AnimControl {
            state: Some(3),
            loop_count: Some(1),
            current_frame: None,
        });
        let s = render_str(&mut d, &frame_with(vec![p]));
        assert!(s.contains("a=t"), "expected the base transmit, got: {s}");
        let f1_pos = s.find("f1").expect("f1 must be replayed");
        let f2_pos = s.find("f2").expect("f2 must be replayed");
        assert!(
            f1_pos < f2_pos,
            "frames must replay in arrival order: {s:?}"
        );
        assert!(
            s.contains("a=a"),
            "expected the animation control command, got: {s}"
        );
        assert!(s.contains(",s=3"), "expected control state s=3: {s:?}");
    }

    #[test]
    fn already_attached_client_only_gets_new_frames() {
        let mut d = kitty_renderer();
        let mut p = vp(1, 7, 1, 2, 3);
        p.frames = Arc::new(vec![sample_frame_visible(1, b"f1")]);
        render_str(&mut d, &frame_with(vec![p.clone()])); // client is now caught up to 1 frame

        p.frames = Arc::new(vec![
            sample_frame_visible(1, b"f1"),
            sample_frame_visible(2, b"f2"),
        ]);
        let s = render_str(&mut d, &frame_with(vec![p]));
        assert!(
            !s.contains("f1"),
            "f1 was already sent, must not repeat: {s:?}"
        );
        assert!(s.contains("f2"), "f2 is new, must be sent: {s:?}");
    }

    #[test]
    fn unchanged_anim_control_not_resent() {
        let mut d = kitty_renderer();
        let mut p = vp(1, 7, 1, 2, 3);
        p.anim_control = Some(AnimControl {
            state: Some(3),
            loop_count: None,
            current_frame: None,
        });
        render_str(&mut d, &frame_with(vec![p.clone()]));
        let s = render_str(&mut d, &frame_with(vec![p]));
        assert!(
            !s.contains("a=a"),
            "unchanged control must not be re-sent: {s:?}"
        );
    }

    #[test]
    fn changed_anim_control_is_resent() {
        let mut d = kitty_renderer();
        let mut p = vp(1, 7, 1, 2, 3);
        p.anim_control = Some(AnimControl {
            state: Some(3),
            loop_count: None,
            current_frame: None,
        });
        render_str(&mut d, &frame_with(vec![p.clone()]));
        p.anim_control = Some(AnimControl {
            state: Some(1),
            loop_count: None,
            current_frame: None,
        });
        let s = render_str(&mut d, &frame_with(vec![p]));
        assert!(
            s.contains("a=a") && s.contains(",s=1"),
            "changed control must be re-sent: {s:?}"
        );
    }

    #[test]
    fn new_generation_resets_frame_and_control_replay_bookkeeping() {
        // A fresh base-image generation means the source's frame log/control
        // were also reset (Tasks 2-3), so a client's replay bookkeeping for
        // that image must reset too: the next frame with the SAME frame data
        // (now representing a fresh log on the new generation) must replay in
        // full, not be treated as "already sent".
        let mut d = kitty_renderer();
        let mut p = vp(1, 7, 1, 2, 3);
        p.frames = Arc::new(vec![sample_frame_visible(1, b"f1")]);
        p.anim_control = Some(AnimControl {
            state: Some(3),
            loop_count: None,
            current_frame: None,
        });
        render_str(&mut d, &frame_with(vec![p.clone()]));

        // Base image content changes (new generation) and the frame log/
        // control reset on the source, then a new frame arrives on top.
        p.generation = 2;
        p.data_b64 = Arc::from(&b"WFla"[..]);
        p.frames = Arc::new(vec![sample_frame_visible(1, b"f1")]);
        p.anim_control = Some(AnimControl {
            state: Some(3),
            loop_count: None,
            current_frame: None,
        });
        let s = render_str(&mut d, &frame_with(vec![p]));
        assert!(s.contains("a=t"), "new generation re-transmits: {s:?}");
        assert!(
            s.contains("f1"),
            "frame log reset means f1 replays again on the new generation: {s:?}"
        );
        assert!(
            s.contains("a=a") && s.contains(",s=3"),
            "control reset means it's re-sent even though value is unchanged: {s:?}"
        );
    }

    #[test]
    fn z_order_and_animation_frames_coexist_on_one_placement() {
        // Z-ordering (Task ?) and animation replay (Tasks 2-4) touch different
        // fields on the same `VisiblePlacement` and are otherwise independent
        // code paths; prove they actually compose on one placement instead of
        // just each being tested in isolation.
        let mut d = kitty_renderer();
        let mut p = vp(1, 7, 1, 2, 3);
        p.z = 7;
        p.frames = Arc::new(vec![
            sample_frame_visible(1, b"FR0Z"),
            sample_frame_visible(2, b"FR1Z"),
        ]);
        p.anim_control = Some(AnimControl {
            state: Some(3),
            loop_count: Some(1),
            current_frame: None,
        });
        let s = render_str(&mut d, &frame_with(vec![p]));
        assert!(
            s.contains(",z=7"),
            "z-order must still be emitted on an animated placement: {s:?}"
        );
        assert!(
            s.contains("FR0Z") && s.contains("FR1Z"),
            "both frames must replay on a z-ordered placement: {s:?}"
        );
    }

    // ── property: per-client replay ordering/idempotence ────────────────────

    /// A Kitty placement carrying `n` synthetic animation frames, each tagged
    /// `FR{i}Z`. The trailing `Z` matters: it rules out `FR1Z` false-matching
    /// inside `FR10Z`/`FR11Z`/…, so a property test can search for one frame's
    /// marker in the wire output without ambiguity.
    fn vp_with_frames(image_id: u32, n: usize) -> VisiblePlacement {
        let frames = (0..n)
            .map(|i| sample_frame_visible(i as u64 + 1, format!("FR{i}Z").as_bytes()))
            .collect::<Vec<_>>();
        VisiblePlacement {
            frames: Arc::new(frames),
            anim_control: (n > 0).then_some(AnimControl {
                state: Some(3),
                loop_count: Some(1),
                current_frame: None,
            }),
            ..vp(1, image_id, 1, 0, 0)
        }
    }

    /// As an image's frame log grows by a *randomized* amount each tick
    /// (0, 1, or several at once), a client that renders every tick must see
    /// each frame exactly once, in arrival order, and a multi-frame batch
    /// must replay in the same relative order it was appended — the
    /// invariant the per-client `last_frame_seq` bookkeeping and the
    /// seq-watermark replay loop in `render_kitty_placements` are supposed to
    /// guarantee. Randomizing the batch size (rather than always growing the
    /// log by exactly one frame per tick) matters: a fixed-growth-of-one loop
    /// makes every batch length 1, and a batch of length 1 is identical
    /// whether replayed forwards or reversed, so a reordering bug (e.g.
    /// `.rev()` sneaking into that loop) would pass undetected. With batches
    /// of 2+, this test checks the newly appended frames' markers appear in
    /// the wire output at strictly increasing byte offsets, in addition to
    /// the skip/repeat/stall checks (no new marker missing, no stale marker
    /// reappearing, `last_frame_seq` landing exactly on the new total).
    #[hegel::test(test_cases = 100)]
    fn prop_client_never_repeats_a_frame_and_sees_them_in_order(tc: TestCase) {
        let batch_sizes =
            tc.draw(gs::vecs(gs::integers::<usize>().min_value(0).max_value(5)).max_size(20));
        tc.note(&format!("batch_sizes={batch_sizes:?}"));
        let image_id: u32 = 42;
        let mut d = kitty_renderer();
        let mut total = 0usize;
        for growth in batch_sizes {
            let new_total = total + growth;
            let s = render_str(
                &mut d,
                &frame_with(vec![vp_with_frames(image_id, new_total)]),
            );
            if growth == 0 {
                assert!(
                    (0..new_total).all(|i| !s.contains(&format!("FR{i}Z"))),
                    "no new frames since last tick, nothing should replay: {s:?}"
                );
            } else {
                // The newly appended frames (indices `total..new_total`) must
                // replay this tick, in the order they were appended.
                let positions: Vec<usize> = (total..new_total)
                    .map(|i| {
                        let marker = format!("FR{i}Z");
                        s.find(&marker).unwrap_or_else(|| {
                            panic!("batch of {growth}: frame {i} must replay: {s:?}")
                        })
                    })
                    .collect();
                assert!(
                    positions.windows(2).all(|w| w[0] < w[1]),
                    "batch of {growth} new frames must replay in arrival order: {s:?}"
                );
                for old in 0..total {
                    let stale = format!("FR{old}Z");
                    assert!(
                        !s.contains(&stale),
                        "frame {old} was already sent, must not repeat: {s:?}"
                    );
                }
            }
            assert_eq!(
                d.last_frame_seq.get(&image_id).copied(),
                Some(new_total as u64),
                "last_frame_seq must land on exactly {new_total}: {s:?}"
            );
            total = new_total;
        }
    }

    #[test]
    fn animation_replay_survives_frame_count_cap_eviction() {
        // Finding #2 (2026-07-06 inline-graphics bug audit): the old
        // `frames_sent` bookkeeping compared an absolute received-count
        // against `p.frames.len()`. `ImageStore::push_frame` caps the stored
        // log at CAP_FRAMES_PER_IMAGE (512), trimming the front with
        // `remove(0)` past it, so `frames.len()` pins at 512 forever once an
        // image's log has been trimmed. A client that had caught up to the
        // cap could then never see `len() > sent` fire again, freezing
        // playback even as fresh frames kept arriving. Drive a real
        // `ImageStore` (not a hand-built Vec, so the front-eviction is
        // genuine) past the cap and confirm the renderer keeps delivering.
        let mut store = ImageStore::default();
        store.insert(Image {
            id: 7,
            protocol: ImageProtocol::Kitty,
            format: ImageFormat::Rgba,
            pixel_w: 1,
            pixel_h: 1,
            data_b64: Arc::from(&b"QUJD"[..]),
            iterm_args: None,
            generation: 1,
            frames: Arc::new(Vec::new()),
            anim_control: None,
        });

        let mut d = kitty_renderer();
        const PAST_CAP: usize = 600; // > ImageStore::CAP_FRAMES_PER_IMAGE (512)
        for i in 0..PAST_CAP {
            store.push_frame(7, sample_frame_visible(0, format!("FR{i}Z").as_bytes()));
            let mut p = vp(1, 7, 1, 2, 3);
            p.frames = store.get(7).unwrap().frames.clone();
            let s = render_str(&mut d, &frame_with(vec![p]));
            if i >= 512 {
                assert!(
                    s.contains(&format!("FR{i}Z")),
                    "frame {i}, past the 512-frame cap, must still be delivered \
                     (playback must not stall): {s:?}"
                );
            }
        }
    }

    #[test]
    fn empty_data_placement_neither_transmits_nor_poisons() {
        // A placement whose image has no data can't be transmitted/placed, and
        // must not mark the id transmitted, so a later real-data frame still
        // sends it.
        let mut d = kitty_renderer();
        let mut empty = vp(1, 7, 1, 2, 3);
        empty.data_b64 = Arc::from(&b""[..]);
        let s = render_str(&mut d, &frame_with(vec![empty]));
        assert!(
            !s.contains("\x1b_G"),
            "no graphics for an empty-data image: {s:?}"
        );
        let s2 = render_str(&mut d, &frame_with(vec![vp(1, 7, 1, 2, 3)]));
        assert!(
            s2.contains("a=t"),
            "real data still transmits id 7 later: {s2:?}"
        );
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
        assert!(
            s.contains("\x1b_Ga=d,d=A,q=2\x1b\\"),
            "full repaint drops old images: {s:?}"
        );
        assert!(s.contains("a=t"), "re-transmits after the repaint: {s:?}");
        assert!(s.contains("a=p,i=7"), "re-places the image: {s:?}");
    }

    #[test]
    fn cropped_place_emits_source_rect_full_place_omits_it() {
        // Full source -> minimal place (no x/y/w/h).
        let mut d = kitty_renderer();
        let s = render_str(&mut d, &frame_with(vec![vp(1, 7, 1, 2, 3)]));
        assert!(
            s.contains("\x1b_Ga=p,i=7,p=1,r=2,c=3,q=2"),
            "full place minimal: {s:?}"
        );
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

    // ── rects_overlap (box↔data repaint-overlap detection) ─────────────────────

    fn rect(host_row: u16, host_col: u16, rows: u16, cols: u16) -> PlacedRect {
        PlacedRect {
            host_row,
            host_col,
            image_id: 0,
            placement_id: 0,
            src_x: 0,
            src_y: 0,
            src_w: 0,
            src_h: 0,
            rows,
            cols,
        }
    }

    #[test]
    fn rects_overlap_edge_touching_is_not_overlap_but_shifted_is() {
        // `rects_overlap` ANDs four half-open-interval checks (row-start,
        // row-end, col-start, col-end); this exercises every `<` boundary and
        // every `&&` in the chain, since render_overlay_placements relies on
        // it to decide whether a stale repaint must force a data placement to
        // re-emit (the box↔data transition's core geometry).
        let a = rect(0, 0, 2, 2); // rows 0..2, cols 0..2

        // Row-adjacent: b starts exactly where a's rows end (touching, not
        // overlapping). Real code: false either direction.
        let below = rect(2, 0, 2, 2); // rows 2..4, cols 0..2
        assert!(
            !rects_overlap(&a, &below),
            "row-touching rects must not overlap: {a:?} vs {below:?}"
        );
        assert!(
            !rects_overlap(&below, &a),
            "overlap must be symmetric (row case): {below:?} vs {a:?}"
        );

        // Col-adjacent: b starts exactly where a's cols end.
        let right = rect(0, 2, 2, 2); // rows 0..2, cols 2..4
        assert!(
            !rects_overlap(&a, &right),
            "col-touching rects must not overlap: {a:?} vs {right:?}"
        );
        assert!(
            !rects_overlap(&right, &a),
            "overlap must be symmetric (col case): {right:?} vs {a:?}"
        );

        // Shifted by one cell in both axes: genuinely overlaps at (1,1).
        let overlapping = rect(1, 1, 2, 2); // rows 1..3, cols 1..3
        assert!(
            rects_overlap(&a, &overlapping),
            "one-cell-shifted rects must overlap: {a:?} vs {overlapping:?}"
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
        assert!(
            s.contains('┌') && s.contains('┐') && s.contains('└') && s.contains('┘'),
            "box border: {s:?}"
        );
        assert!(s.contains("30x40"), "centred WxH label: {s:?}");
        assert!(
            !s.contains("\x1b_G"),
            "no Kitty bytes for a non-graphics client: {s:?}"
        );
    }

    /// Snapshot the placeholder box rendered by a non-graphics client.
    /// The golden captures the box-drawing glyphs and dimension label.
    /// The raw output is escape-sequence heavy, so `escape_debug` keeps it
    /// readable.
    #[test]
    fn snapshot_placeholder_box() {
        let mut d = DiffRenderer::new();
        let out = render_str(&mut d, &frame_with(vec![boxed_vp(3, 10)]));
        insta::assert_snapshot!(out.escape_debug().to_string());
    }

    #[test]
    fn kitty_client_draws_no_placeholder_box() {
        let mut d = kitty_renderer();
        let s = render_str(&mut d, &frame_with(vec![boxed_vp(3, 10)]));
        assert!(
            s.contains("a=t"),
            "Kitty client transmits the real image: {s:?}"
        );
        assert!(
            !s.contains('┌'),
            "no placeholder box for a Kitty client: {s:?}"
        );
    }

    #[test]
    fn placeholder_box_cleared_when_placement_vanishes() {
        let mut d = DiffRenderer::new();
        render_str(&mut d, &frame_with(vec![boxed_vp(3, 10)]));
        let s = render_str(&mut d, &frame_with(vec![]));
        assert!(
            s.contains("\x1b[3;4H"),
            "repaints the vacated box region: {s:?}"
        );
        assert!(!s.contains('┌'), "box not redrawn after vanish: {s:?}");
    }

    #[test]
    fn tiny_placeholder_footprint_hatches_without_panic() {
        let mut d = DiffRenderer::new();
        let s = render_str(&mut d, &frame_with(vec![boxed_vp(1, 1)]));
        assert!(s.contains('▒'), "tiny footprint hatched: {s:?}");
        assert!(!s.contains('┌'), "no border when too small: {s:?}");
    }

    #[test]
    fn placeholder_box_redrawn_over_changed_underlying_cell() {
        // The box is injected glyphs, not grid cells. If a cell under the box
        // changes the cell diff repaints it, so the box must be redrawn to avoid
        // a hole.
        let mut d = DiffRenderer::new();
        render_str(&mut d, &frame_with(vec![boxed_vp(3, 10)]));
        let mut f2 = frame_with(vec![boxed_vp(3, 10)]);
        f2.put(
            2,
            3,
            Cell {
                grapheme: SmolStr::new("X"),
                ..Cell::default()
            },
        ); // under the box
        let s = render_str(&mut d, &f2);
        assert!(
            s.contains('┌'),
            "box redrawn over the changed cell (no hole): {s:?}"
        );
    }

    #[test]
    fn surviving_box_redrawn_when_overlapping_neighbor_vanishes() {
        let mut d = DiffRenderer::new();
        let a = {
            let mut p = boxed_vp(3, 10);
            p.key = 1;
            p.host_row = 2;
            p.host_col = 3;
            p
        };
        let b = {
            let mut p = boxed_vp(3, 10);
            p.key = 2;
            p.host_row = 3;
            p.host_col = 4;
            p
        }; // overlaps A
        render_str(&mut d, &frame_with(vec![a, b.clone()]));
        let s = render_str(&mut d, &frame_with(vec![b])); // A vanishes
        assert!(
            s.contains('┌'),
            "surviving overlapping box B redrawn after A vanished: {s:?}"
        );
    }

    #[test]
    fn kitty_placement_on_sixel_only_client_gets_a_box() {
        // Per-protocol gating: a Kitty image on a client that only speaks Sixel
        // can't be rendered, so it gets a placeholder box and no Kitty bytes.
        let mut d = DiffRenderer::new();
        d.set_graphics_caps(GraphicsCaps {
            kitty: false,
            sixel: true,
            iterm2: false,
        });
        let s = render_str(&mut d, &frame_with(vec![boxed_vp(3, 10)]));
        assert!(
            s.contains('┌'),
            "kitty image boxed on a sixel-only client: {s:?}"
        );
        assert!(!s.contains("\x1b_G"), "no kitty bytes: {s:?}");
    }

    #[test]
    fn placeholder_box_move_repaints_old_and_draws_new() {
        let mut d = DiffRenderer::new();
        render_str(&mut d, &frame_with(vec![boxed_vp(3, 10)])); // host_row 2
        let mut moved = boxed_vp(3, 10);
        moved.host_row = 4;
        let s = render_str(&mut d, &frame_with(vec![moved]));
        assert!(
            s.contains("\x1b[3;4H"),
            "repaints the old rect (row 3): {s:?}"
        );
        assert!(s.contains("\x1b[5;4H"), "draws the new box (row 5): {s:?}");
    }

    // ── virtual (Unicode-placeholder) placements ───────────────────────────────

    fn vvp(key: u64, image_id: u32) -> VisibleVirtualPlacement {
        VisibleVirtualPlacement {
            key,
            image_id,
            placement_id: 1,
            generation: 1,
            format: plexy_glass_emulator::ImageFormat::Png,
            pixel_w: 30,
            pixel_h: 40,
            data_b64: Arc::from(&b"QUJD"[..]),
            rows: 2,
            cols: 3,
        }
    }

    fn frame_with_virtual(vps: Vec<VisibleVirtualPlacement>) -> VirtualScreen {
        let mut v = VirtualScreen::blank(8, 20);
        v.virtual_placements = vps;
        v
    }

    #[test]
    fn virtual_placement_transmits_once_and_emits_unicode_place() {
        let mut d = kitty_renderer();
        let s = render_str(&mut d, &frame_with_virtual(vec![vvp(1, 7)]));
        // Transmitted under the wire id the compositor already folded into
        // `image_id` (here 7 for the unit test); diff places under it verbatim.
        assert!(s.contains("\x1b_Gi=7,a=t,f=100"), "transmit wire id: {s:?}");
        assert!(
            s.contains("\x1b_Ga=p,U=1,i=7,p=1,c=3,r=2,q=2\x1b\\"),
            "virtual place: {s:?}"
        );
        // Second identical frame re-emits nothing.
        let s2 = render_str(&mut d, &frame_with_virtual(vec![vvp(1, 7)]));
        assert!(
            !s2.contains("\x1b_G"),
            "unchanged virtual frame is silent: {s2:?}"
        );
    }

    #[test]
    fn vanished_virtual_placement_is_deleted() {
        let mut d = kitty_renderer();
        render_str(&mut d, &frame_with_virtual(vec![vvp(1, 7)]));
        let s = render_str(&mut d, &frame_with_virtual(vec![]));
        assert!(
            s.contains("\x1b_Ga=d,d=i,i=7,p=1,q=2\x1b\\"),
            "delete vanished virtual placement: {s:?}"
        );
    }

    #[test]
    fn virtual_transmit_cap_exceeded_reschedules_transmit_next_frame() {
        // Mirrors the classic TRANSMIT_CAP gap for the virtual-placement path
        // (`transmitted_virtual`, real coverage gap same as the classic one).
        // Once exceeded, the reset-images guard checks only the CLASSIC maps
        // (`self.transmitted`/`self.placed`), which stay empty in a
        // virtual-only session, so no `a=d,d=A,q=2` bytes are emitted — but
        // `transmitted_virtual`/`virtual_placed` ARE cleared, so the very
        // next render re-transmits the unchanged placements (an ordinary
        // unchanged frame emits nothing, per
        // `virtual_placement_transmits_once_and_emits_unicode_place`).
        let mut d = kitty_renderer();
        let vps: Vec<VisibleVirtualPlacement> =
            (1..=257u32).map(|id| vvp(u64::from(id), id)).collect();
        render_str(&mut d, &frame_with_virtual(vps.clone()));
        let s = render_str(&mut d, &frame_with_virtual(vps));
        assert!(
            s.contains("a=t"),
            "exceeding virtual TRANSMIT_CAP must re-transmit next frame: {s:?}"
        );
    }

    #[test]
    fn virtual_transmit_cap_at_exactly_256_does_not_reset() {
        // Boundary companion, same reasoning as the classic
        // `transmit_cap_at_exactly_256_does_not_reset`: distinguishes `>`
        // from a `>= TRANSMIT_CAP` mutant.
        let mut d = kitty_renderer();
        let vps: Vec<VisibleVirtualPlacement> =
            (1..=256u32).map(|id| vvp(u64::from(id), id)).collect();
        render_str(&mut d, &frame_with_virtual(vps.clone()));
        let s = render_str(&mut d, &frame_with_virtual(vps));
        assert!(
            !s.contains("\x1b_G"),
            "exactly at virtual TRANSMIT_CAP, unchanged frame must stay silent: {s:?}"
        );
    }

    // ── Sixel data placements ──────────────────────────────────────────────────

    fn sixel_vp(key: u64, host_row: u16, host_col: u16) -> VisiblePlacement {
        let mut p = vp(key, 7, 1, host_row, host_col);
        p.protocol = plexy_glass_emulator::ImageProtocol::Sixel;
        p.data_b64 = Arc::from(&b"0q\"1;1;10;20~~~"[..]);
        p
    }

    fn sixel_caps() -> GraphicsCaps {
        GraphicsCaps {
            kitty: false,
            sixel: true,
            iterm2: false,
        }
    }

    #[test]
    fn sixel_placement_emitted_at_position_for_sixel_client() {
        let mut d = DiffRenderer::new();
        d.set_graphics_caps(sixel_caps());
        let s = render_str(&mut d, &frame_with(vec![sixel_vp(1, 2, 3)]));
        assert!(s.contains("\x1bP0q"), "sixel data emitted: {s:?}");
        assert!(
            s.contains("\x1b[3;4H"),
            "positioned at the host cell: {s:?}"
        );
        assert!(!s.contains('┌'), "no box for a sixel-capable client: {s:?}");
    }

    #[test]
    fn sixel_placement_boxed_for_kitty_only_client() {
        let mut d = kitty_renderer(); // kitty only
        let s = render_str(&mut d, &frame_with(vec![sixel_vp(1, 2, 3)]));
        assert!(s.contains('┌'), "sixel boxed on a kitty-only client: {s:?}");
        assert!(!s.contains("\x1bP0q"), "no sixel bytes: {s:?}");
    }

    #[test]
    fn sixel_unchanged_frame_not_re_emitted() {
        let mut d = DiffRenderer::new();
        d.set_graphics_caps(sixel_caps());
        let f = frame_with(vec![sixel_vp(1, 2, 3)]);
        render_str(&mut d, &f);
        let s = render_str(&mut d, &f);
        assert!(!s.contains("\x1bP0q"), "unchanged sixel not re-sent: {s:?}");
    }

    #[test]
    fn moved_sixel_repaints_old_and_re_emits() {
        let mut d = DiffRenderer::new();
        d.set_graphics_caps(sixel_caps());
        render_str(&mut d, &frame_with(vec![sixel_vp(1, 2, 3)]));
        let s = render_str(&mut d, &frame_with(vec![sixel_vp(1, 5, 3)])); // moved down
        assert!(s.contains("\x1b[3;4H"), "repaints old rect (row 3): {s:?}");
        assert!(
            s.contains("\x1bP0q"),
            "re-emits sixel at the new position: {s:?}"
        );
    }

    #[test]
    fn sixel_redrawn_over_changed_underlying_cell() {
        // Hole-punch regression: an in-place redraw under the image (status line,
        // spinner) repaints a cell, so the sixel must be re-emitted to cover it.
        let mut d = DiffRenderer::new();
        d.set_graphics_caps(sixel_caps());
        render_str(&mut d, &frame_with(vec![sixel_vp(1, 2, 3)]));
        let mut f2 = frame_with(vec![sixel_vp(1, 2, 3)]);
        f2.put(
            2,
            3,
            Cell {
                grapheme: SmolStr::new("X"),
                ..Cell::default()
            },
        ); // under the sixel
        let s = render_str(&mut d, &f2);
        assert!(
            s.contains("\x1bP0q"),
            "sixel re-emitted over the changed cell (no hole): {s:?}"
        );
    }

    #[test]
    fn sixel_vanish_repaints_old_rect() {
        let mut d = DiffRenderer::new();
        d.set_graphics_caps(sixel_caps());
        render_str(&mut d, &frame_with(vec![sixel_vp(1, 2, 3)]));
        let s = render_str(&mut d, &frame_with(vec![]));
        assert!(
            s.contains("\x1b[3;4H"),
            "repaints the vacated sixel rect: {s:?}"
        );
        assert!(
            !s.contains("\x1bP0q"),
            "vanished sixel not re-emitted: {s:?}"
        );
    }

    #[test]
    fn vanishing_box_does_not_clobber_overlapping_sixel() {
        // Cross-pass regression: on a sixel-only client a Kitty placement is
        // boxed and a Sixel is a real image; when the overlapping box vanishes,
        // its repaint must not erase the surviving sixel, so it re-emits.
        let mut d = DiffRenderer::new();
        d.set_graphics_caps(sixel_caps()); // sixel yes, kitty no
        let sixel = sixel_vp(1, 2, 3);
        let kitty = vp(2, 9, 1, 2, 3); // Kitty proto → boxed here, overlaps the sixel
        render_str(&mut d, &frame_with(vec![sixel.clone(), kitty]));
        let s = render_str(&mut d, &frame_with(vec![sixel])); // box vanishes
        assert!(
            s.contains("\x1bP0q"),
            "overlapping sixel re-emitted after the box vanished: {s:?}"
        );
    }

    #[test]
    fn sixel_reestablished_after_invalidate() {
        let mut d = DiffRenderer::new();
        d.set_graphics_caps(sixel_caps());
        render_str(&mut d, &frame_with(vec![sixel_vp(1, 2, 3)]));
        d.invalidate(); // reattach / session switch
        let s = render_str(&mut d, &frame_with(vec![sixel_vp(1, 2, 3)]));
        assert!(
            s.contains("\x1bP0q"),
            "sixel re-emitted after invalidate: {s:?}"
        );
    }

    #[test]
    fn sixel_placements_emit_lowest_z_first() {
        let mut d = DiffRenderer::new();
        d.set_graphics_caps(sixel_caps());
        let mut low = sixel_vp(1, 2, 3); // host row 2 -> "\x1b[3;4H"
        low.z = -5;
        let mut high = sixel_vp(2, 5, 3); // host row 5 -> "\x1b[6;4H"
        high.z = 5;
        // Push high before low, so ordering in the vec does NOT already match z order.
        let s = render_str(&mut d, &frame_with(vec![high, low]));
        let pos_low = s.find("\x1b[3;4H");
        let pos_high = s.find("\x1b[6;4H");
        assert!(
            pos_low.is_some() && pos_high.is_some() && pos_low < pos_high,
            "expected the z=-5 placement's data emitted before the z=5 placement's, got: {s:?}"
        );
    }

    #[test]
    fn disturbed_lower_z_forces_overlapping_higher_z_to_reemit() {
        // Regression for finding #7: step 3 emits data placements lowest-z
        // first so a higher z paints on top. If only the lower one is
        // disturbed and re-emitted, its fresh paint covers the still-cached
        // higher one at their overlap, inverting the stack order for that
        // frame. A disturbed placement must become a disturbance source for
        // later (higher-z) overlapping placements in the same pass.
        let mut d = DiffRenderer::new();
        d.set_graphics_caps(sixel_caps());
        let mut low = sixel_vp(1, 0, 0); // host cols 0-2
        low.image_id = 7;
        low.z = 0;
        let mut high = sixel_vp(2, 0, 2); // host cols 2-4, overlaps low at col 2
        high.image_id = 8;
        high.z = 1;
        render_str(&mut d, &frame_with(vec![low.clone(), high.clone()]));

        // Disturb only `low`'s footprint: cell (0,0) is inside low's rect
        // (cols 0-2) but outside high's rect (cols 2-4).
        let mut f2 = frame_with(vec![low, high]);
        f2.put(
            0,
            0,
            Cell {
                grapheme: SmolStr::new("X"),
                ..Cell::default()
            },
        );
        let s = render_str(&mut d, &f2);

        assert_eq!(
            s.matches("\x1bP0q").count(),
            2,
            "both placements must re-emit so the higher-z one stays on top: {s:?}"
        );
    }

    /// The overlay data-emit orders Sixel/iTerm2 placements by (z, image_id):
    /// after rendering N placements, their data appears in non-decreasing
    /// (z, image_id) order, each exactly once (a stable permutation — the sort
    /// neither drops nor duplicates a placement). Each placement gets a distinct
    /// host row so its emit order is recoverable from the cursor-position escape
    /// (`\x1b[{row+1};4H`) that precedes its Sixel data, the same technique
    /// `sixel_placements_emit_lowest_z_first` uses.
    #[hegel::test(test_cases = 100)]
    fn prop_overlay_z_sort_is_ordered_permutation(tc: TestCase) {
        let n = tc.draw(gs::integers::<usize>().min_value(1).max_value(6));
        // Each placement: distinct image_id (i+1), distinct host_row (2 + 3*i, so
        // the position markers \x1b[3;4H, \x1b[6;4H, … never collide), random z in
        // a small range so ties on z are common (exercising the image_id
        // tie-break).
        let mut specs: Vec<(i32, u32, u16)> = Vec::new(); // (z, image_id, host_row)
        for i in 0..n {
            let z = tc.draw(gs::integers::<i32>().min_value(-3).max_value(3));
            let host_row = 2 + 3 * i as u16;
            specs.push((z, i as u32 + 1, host_row));
        }
        tc.note(&format!("specs (z,id,row) = {specs:?}"));
        let mut d = DiffRenderer::new();
        d.set_graphics_caps(sixel_caps());
        let placements: Vec<VisiblePlacement> = specs
            .iter()
            .enumerate()
            .map(|(i, &(z, id, host_row))| {
                let mut p = sixel_vp(i as u64, host_row, 3); // key i, host_col 3 -> ";4H"
                p.image_id = id;
                p.z = z;
                p
            })
            .collect();
        let out = render_str(&mut d, &frame_with(placements));
        // Recover the on-wire order via each placement's distinct position marker.
        let mut emitted: Vec<(usize, (i32, u32))> = specs
            .iter()
            .map(|&(z, id, host_row)| {
                let marker = format!("\x1b[{};4H", host_row + 1);
                let pos = out.find(&marker).unwrap_or_else(|| {
                    panic!("placement id {id} (row {host_row}) was not emitted: {out:?}")
                });
                (pos, (z, id))
            })
            .collect();
        emitted.sort_by_key(|&(pos, _)| pos);
        let wire_order: Vec<(i32, u32)> = emitted.iter().map(|&(_, k)| k).collect();
        // A permutation of the input (same multiset) — nothing dropped/duplicated.
        let mut got = wire_order.clone();
        let mut want: Vec<(i32, u32)> = specs.iter().map(|&(z, id, _)| (z, id)).collect();
        got.sort_unstable();
        want.sort_unstable();
        assert_eq!(got, want, "emit order dropped or duplicated a placement");
        // … and non-decreasing in (z, image_id).
        assert!(
            wire_order.windows(2).all(|w| w[0] <= w[1]),
            "emit order not sorted by (z, image_id): {wire_order:?}"
        );
    }

    // ── iTerm2 data placements ─────────────────────────────────────────────────

    fn iterm_vp(key: u64, host_row: u16, host_col: u16) -> VisiblePlacement {
        let mut p = vp(key, 7, 1, host_row, host_col);
        p.protocol = plexy_glass_emulator::ImageProtocol::Iterm2;
        p.iterm_args = Some(Arc::from("inline=1;width=10px"));
        p.data_b64 = Arc::from(&b"QUJD"[..]);
        p
    }

    #[test]
    fn iterm2_placement_emitted_for_iterm2_client() {
        let mut d = DiffRenderer::new();
        d.set_graphics_caps(GraphicsCaps {
            kitty: false,
            sixel: false,
            iterm2: true,
        });
        let s = render_str(&mut d, &frame_with(vec![iterm_vp(1, 2, 3)]));
        assert!(
            s.contains("\x1b]1337;File=inline=1;width=10px:QUJD\x07"),
            "iterm2 sequence: {s:?}"
        );
        assert!(
            s.contains("\x1b[3;4H"),
            "positioned at the host cell: {s:?}"
        );
        assert!(
            !s.contains('┌'),
            "no box for an iterm2-capable client: {s:?}"
        );
    }

    #[test]
    fn iterm2_placement_boxed_for_kitty_only_client() {
        let mut d = kitty_renderer();
        let s = render_str(&mut d, &frame_with(vec![iterm_vp(1, 2, 3)]));
        assert!(
            s.contains('┌'),
            "iterm2 boxed on a kitty-only client: {s:?}"
        );
        assert!(!s.contains("1337"), "no iterm2 bytes: {s:?}");
    }

    #[test]
    fn mixed_protocol_frame_each_in_its_own_protocol() {
        // An all-capable client renders each placement in its native protocol:
        // no boxes, no cross-pass interference (non-overlapping footprints).
        let mut d = DiffRenderer::new();
        d.set_graphics_caps(GraphicsCaps {
            kitty: true,
            sixel: true,
            iterm2: true,
        });
        let mut v = VirtualScreen::blank(10, 20);
        v.placements = vec![vp(1, 7, 1, 0, 0), sixel_vp(2, 3, 0), iterm_vp(3, 6, 0)];
        let s = render_str(&mut d, &v);
        assert!(
            s.contains("a=t") && s.contains("a=p,i=7"),
            "kitty emitted: {s:?}"
        );
        assert!(s.contains("\x1bP0q"), "sixel emitted: {s:?}");
        assert!(s.contains("\x1b]1337;File="), "iterm2 emitted: {s:?}");
        assert!(!s.contains('┌'), "no box for any supported protocol: {s:?}");
    }

    #[test]
    fn all_protocols_boxed_for_a_no_graphics_client() {
        let mut d = DiffRenderer::new(); // caps all-off
        let mut v = VirtualScreen::blank(10, 20);
        v.placements = vec![vp(1, 7, 1, 0, 0), sixel_vp(2, 3, 0), iterm_vp(3, 6, 0)];
        let s = render_str(&mut d, &v);
        assert!(
            !s.contains("\x1b_G") && !s.contains("\x1bP0q") && !s.contains("1337"),
            "no graphics: {s:?}"
        );
        assert_eq!(
            s.matches('┌').count(),
            3,
            "all three placements boxed: {s:?}"
        );
    }

    #[test]
    fn non_kitty_client_emits_no_virtual_graphics() {
        let mut d = DiffRenderer::new(); // no graphics caps
        let s = render_str(&mut d, &frame_with_virtual(vec![vvp(1, 7)]));
        assert!(
            !s.contains("\x1b_G"),
            "no graphics for a non-kitty client: {s:?}"
        );
    }

    #[test]
    fn virtual_placement_retransmits_on_generation_change() {
        let mut d = kitty_renderer();
        render_str(&mut d, &frame_with_virtual(vec![vvp(1, 7)]));
        let mut vp = vvp(1, 7);
        vp.generation = 2;
        vp.data_b64 = Arc::from(&b"WFla"[..]);
        let s = render_str(&mut d, &frame_with_virtual(vec![vp]));
        assert!(
            s.contains("a=t"),
            "changed content re-transmits the virtual image: {s:?}"
        );
    }

    #[test]
    fn empty_data_virtual_placement_neither_transmits_nor_places() {
        let mut d = kitty_renderer();
        let mut vp = vvp(1, 7);
        vp.data_b64 = Arc::from(&b""[..]);
        let s = render_str(&mut d, &frame_with_virtual(vec![vp]));
        assert!(
            !s.contains("\x1b_G"),
            "no transmit/place for an empty-data virtual placement: {s:?}"
        );
        // A later real-data frame still works (raw-id cache not poisoned).
        let s2 = render_str(&mut d, &frame_with_virtual(vec![vvp(1, 7)]));
        assert!(
            s2.contains("a=t") && s2.contains("U=1"),
            "real data transmits + places: {s2:?}"
        );
    }

    #[test]
    fn virtual_place_omits_zero_cell_box() {
        let mut d = kitty_renderer();
        let mut vp = vvp(1, 7);
        vp.cols = 0;
        vp.rows = 0;
        let s = render_str(&mut d, &frame_with_virtual(vec![vp]));
        assert!(
            s.contains("\x1b_Ga=p,U=1,i=7,p=1,q=2\x1b\\"),
            "no c=/r= when zero: {s:?}"
        );
    }

    #[test]
    fn classic_and_virtual_images_with_same_raw_id_dont_share_transmit_cache() {
        // A classic placement (folded host id) and a virtual placement (raw id)
        // in one frame each transmit independently (separate caches).
        let mut d = kitty_renderer();
        let mut v = VirtualScreen::blank(8, 20);
        v.placements = vec![vp(1, 7, 1, 2, 3)];
        v.virtual_placements = vec![vvp(99, 7)]; // same raw id 7, different key
        let s = render_str(&mut d, &v);
        assert!(s.contains("a=p,i=7"), "classic place present: {s:?}");
        assert!(s.contains("U=1,i=7"), "virtual place present: {s:?}");
        // Both transmitted (classic under host fold, virtual under raw 7).
        assert!(
            s.matches("a=t").count() >= 2,
            "both images transmitted: {s:?}"
        );
    }

    fn placeholder_vp(rows: u16, cols: u16) -> VisiblePlacement {
        use plexy_glass_emulator::{ImageFormat, ImageProtocol};
        VisiblePlacement {
            key: 1,
            image_id: 7,
            placement_id: 1,
            protocol: ImageProtocol::Kitty,
            iterm_args: None,
            generation: 1,
            format: ImageFormat::Png,
            pixel_w: 30,
            pixel_h: 40,
            src_x: 0,
            src_y: 0,
            src_w: 30,
            src_h: 40,
            data_b64: Arc::from(&b"QUJD"[..]),
            host_row: 0,
            host_col: 0,
            rows,
            cols,
            z: 0,
            frames: Arc::new(Vec::new()),
            anim_control: None,
        }
    }

    #[test]
    fn placeholder_box_zero_rows_emits_nothing_beyond_reset() {
        // `rows == 0 || cols == 0` → `&&` at 501:18: a placement with rows=0
        // and cols>0 would NOT early-return under &&, proceeding to the hatch/
        // border path with rows=0 and producing erroneous output.
        let mut out = String::new();
        emit_placeholder_box(&mut out, &placeholder_vp(0, 5));
        assert_eq!(
            out, "\x1b[0m",
            "zero-rows must early-return (only reset): {out:?}"
        );
    }

    #[test]
    fn placeholder_box_zero_cols_emits_nothing_beyond_reset() {
        // Symmetric: cols=0, rows>0.
        let mut out = String::new();
        emit_placeholder_box(&mut out, &placeholder_vp(5, 0));
        assert_eq!(
            out, "\x1b[0m",
            "zero-cols must early-return (only reset): {out:?}"
        );
    }

    #[test]
    fn placeholder_box_one_row_uses_hatch_not_border() {
        // `rows < 2 || cols < 2` → `rows < 2 && cols < 2` at 505:17: a 1×5
        // placement (rows=1 < 2, cols=5 ≥ 2) would skip the hatch and attempt to
        // draw borders, and with inner = cols-2=3 but rows=1 there is no top/bottom
        // split, so the output comes out malformed.
        let mut out = String::new();
        emit_placeholder_box(&mut out, &placeholder_vp(1, 5));
        assert!(out.contains('▒'), "1×5 must use hatch fill: {out:?}");
        assert!(!out.contains('┌'), "1×5 must not draw box border: {out:?}");
    }

    #[test]
    fn placeholder_box_one_col_uses_hatch_not_border() {
        // `< → ==` at 505:25 (`cols < 2` → `cols == 2`): cols=1 would not enter
        // the hatch path, so inner = cols-2 = u16::MAX (wrapping underflow).
        let mut out = String::new();
        emit_placeholder_box(&mut out, &placeholder_vp(5, 1));
        assert!(out.contains('▒'), "5×1 must use hatch fill: {out:?}");
        assert!(!out.contains('┌'), "5×1 must not draw box border: {out:?}");
    }
}
