//! Inline-graphics model: captured images and their on-screen placements.
//!
//! Phase 2 feeds this from the Kitty graphics protocol (APC `ESC _ G … ST`);
//! later phases feed the same model from Sixel (DCS) and iTerm2 (OSC 1337).
//! The emulator only *models* images (id → data, plus placements anchored in
//! the grid); the compositor/renderer turn placements into per-client protocol
//! bytes.

use std::collections::{HashMap, VecDeque};
use std::str;
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD;

/// Wire image format (Kitty `f=` key).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageFormat {
    /// `f=100`: PNG (dimensions read from the PNG header).
    Png,
    /// `f=24`: packed RGB (dimensions from `s=`/`v=`).
    Rgb,
    /// `f=32`: packed RGBA (dimensions from `s=`/`v=`).
    Rgba,
}

/// Which terminal graphics protocol an image was captured from. A client
/// re-emits an image only if its terminal supports this protocol (we don't
/// transcode, unsupported clients get a placeholder box).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ImageProtocol {
    #[default]
    Kitty,
    Sixel,
    Iterm2,
}

impl ImageFormat {
    pub const fn from_kitty_f(f: u32) -> Option<Self> {
        match f {
            100 => Some(Self::Png),
            24 => Some(Self::Rgb),
            32 => Some(Self::Rgba),
            _ => None,
        }
    }

    pub const fn kitty_f(self) -> u32 {
        match self {
            Self::Png => 100,
            Self::Rgb => 24,
            Self::Rgba => 32,
        }
    }
}

/// A transmitted image: the base64 payload (chunks joined) plus its format and
/// pixel dimensions. `data_b64` is an `Arc` so per-frame `Screen` clones and the
/// compositor's `VisiblePlacement` stay cheap.
#[derive(Clone, Debug)]
pub struct Image {
    pub id: u32,
    /// Source protocol, which decides how the renderer re-emits this image.
    pub protocol: ImageProtocol,
    pub format: ImageFormat,
    pub pixel_w: u32,
    pub pixel_h: u32,
    /// The protocol-specific re-emit payload: Kitty = base64 PNG/RGB(A); Sixel =
    /// the inner DCS payload (`<params>q<data>`); iTerm2 = base64 file data.
    pub data_b64: Arc<[u8]>,
    /// iTerm2 `File=` argument string (e.g. `inline=1;width=20`), for re-emit.
    pub iterm_args: Option<Arc<str>>,
    /// Bumped each time an id's content is (re)transmitted, so a per-client
    /// renderer keyed on `(id, generation)` re-transmits when the pixels change
    /// instead of showing the stale first image.
    pub generation: u64,
    /// Ordered log of every `a=f` command received for this image, replayed
    /// verbatim to clients (see `Frame`'s doc comment). `Arc`-wrapped so a
    /// compositor snapshot clone (every render pass) is a cheap refcount
    /// bump, not a deep copy; `ImageStore::push_frame` uses `Arc::make_mut`
    /// to append, which only deep-copies if an old snapshot is still alive.
    pub frames: Arc<Vec<Frame>>,
    /// The most recently received `a=a` animation-control state. `None`
    /// until the first `a=a` for this image arrives.
    pub anim_control: Option<AnimControl>,
}

impl Image {
    /// Bytes counted toward `ImageStore`'s budget: the base transmit plus
    /// every stored animation frame.
    pub fn total_bytes(&self) -> usize {
        self.data_b64.len() + self.frames.iter().map(|f| f.data_b64.len()).sum::<usize>()
    }
}

/// One `a=f` animation-frame command, stored verbatim — no compositing is
/// done by us. Kitty's own terminal composites frames client-side (alpha
/// blend or overwrite, onto whichever frame `canvas_source` names), and that
/// process is deterministic given the command stream in order, so replaying
/// these in original arrival order to any client reproduces exactly what the
/// originating child's terminal would have shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// `r=`: which frame number this command is. `None` means "append the
    /// next number" (Kitty auto-assigns root+1, root+2, ... in arrival
    /// order); `Some(n)` where `n` already exists means "edit frame `n` in
    /// place, using its own prior content as the canvas" rather than append.
    /// Stored exactly as received (present or absent) so replay reproduces
    /// the same auto-numbering the original stream would have triggered.
    pub frame_number: Option<u32>,
    /// `c=`: which frame to use as the canvas background (`1` = the root/base
    /// image). `None` if unspecified.
    pub canvas_source: Option<u32>,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// `X=1` vs default alpha blend.
    pub overwrite: bool,
    /// `Y=`, default 0 (transparent black).
    pub bg_color: u32,
    /// `z=` gap in ms: positive = delay before the next frame, negative =
    /// gapless, 0 = unspecified (same as absent).
    pub gap_ms: i32,
    pub format: ImageFormat,
    pub data_b64: Arc<[u8]>,
    /// Monotonic per-image sequence number, assigned by `ImageStore::push_frame`
    /// when the frame is appended (1, 2, 3, ...; 0 means "none sent yet" to a
    /// renderer that hasn't seen this image). Unlike a frame's position in
    /// `Image::frames`, this never changes once assigned, so it survives the
    /// `CAP_FRAMES_PER_IMAGE` front-eviction (`remove(0)`) that shifts every
    /// later frame's index down. A per-client renderer tracks "highest seq
    /// sent" instead of "frames sent so far" so replay can't stall once the log
    /// has been trimmed (2026-07-06 inline-graphics bug audit, finding #2).
    pub seq: u64,
}

/// The most recently received `a=a` (animation control) state for an image.
/// This is terminal *state*, not a data stream — unlike frames, only the
/// latest one matters, and it's replayed to a client after the frame log
/// (or re-replayed if it changed since the client last saw one).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct AnimControl {
    /// `s=`: 1 = stop, 2 = loading (wait for more frames at the end instead
    /// of looping), 3 = loop normally back to frame 1. `None` if `s=` was
    /// absent or `0` (ignored, same as unspecified).
    pub state: Option<u8>,
    /// `v=`: loop count. `0` is ignored (same as unspecified, stored as
    /// `None`), `1` = infinite, `n>1` = loop `n-1` additional times.
    pub loop_count: Option<u32>,
    /// `c=`: jump to this frame now (current-frame selector).
    pub current_frame: Option<u32>,
}

/// An on-screen placement of an image, anchored to an absolute unified line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    pub image_id: u32,
    pub placement_id: u32,
    /// Source protocol of the referenced image (mirrors `Image::protocol`), so
    /// the renderer can dispatch without a store lookup.
    pub protocol: ImageProtocol,
    /// Absolute unified line (scrollback rows first, then active grid).
    pub anchor_line: u32,
    pub col: u16,
    /// Cell footprint.
    pub rows: u16,
    pub cols: u16,
    /// Kitty `z=` placement key (default 0). Negative draws under text;
    /// same-z overlaps break ties by lower image id drawn under (handled at
    /// render time, not stored here).
    pub z: i32,
    /// Monotonic id (per Screen), stable across clones, for renderer dedupe.
    pub seq: u64,
}

/// A Unicode-placeholder (virtual) placement: the image is composited by the
/// terminal onto `U+10EEEE` placeholder cells the app wrote into the grid (those
/// cells carry the image id in their fg color and the row/col in diacritics, and
/// ride scroll/reflow like text). The emulator does not anchor it to a line or
/// advance the cursor; it only records that image id `image_id` has a virtual
/// placement so the per-client renderer transmits the image once and emits the
/// `a=p,U=1` once. `rows`/`cols` are the placement's declared cell box.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtualPlacement {
    pub image_id: u32,
    pub placement_id: u32,
    pub rows: u16,
    pub cols: u16,
    /// Monotonic id (per Screen), stable across clones, for renderer dedupe.
    pub seq: u64,
}

/// `id → Image` with a byte-budget, insertion-order LRU. A pathological child
/// can't grow this without bound; placements that reference an evicted image are
/// skipped at render time (the compositor/renderer tolerate a missing image).
#[derive(Clone, Debug, Default)]
pub struct ImageStore {
    map: HashMap<u32, Image>,
    order: VecDeque<u32>,
    bytes: usize,
}

impl ImageStore {
    const CAP_BYTES: usize = 64 * 1024 * 1024;
    // ponytail: fixed cap, revisit if a real animation exceeds it — real
    // GIF/APNG re-encoders send tens to low hundreds of frames, not
    // thousands.
    const CAP_FRAMES_PER_IMAGE: usize = 512;

    /// Insert (or replace) an image, evicting oldest entries while over the
    /// byte budget. Returns the ids evicted, so the caller can drop placements
    /// that reference them.
    pub fn insert(&mut self, img: Image) -> Vec<u32> {
        let id = img.id;
        if let Some(old) = self.map.remove(&id) {
            self.bytes = self.bytes.saturating_sub(old.total_bytes());
            self.order.retain(|&i| i != id);
        }
        self.bytes += img.total_bytes();
        self.map.insert(id, img);
        self.order.push_back(id);
        self.evict_over_budget()
    }

    /// Append an animation frame (`a=f`) to an already-transmitted image.
    /// Evicts the oldest stored frame past `CAP_FRAMES_PER_IMAGE`, then
    /// evicts whole images oldest-first if still over the byte budget.
    /// Returns evicted whole-image ids (mirrors `insert`); no-ops (frame
    /// dropped, empty Vec returned) if `id` isn't currently stored — a
    /// pathological `a=f` for an id that was never transmitted or was
    /// already evicted can't leak state.
    pub fn push_frame(&mut self, id: u32, mut frame: Frame) -> Vec<u32> {
        let Some(img) = self.map.get_mut(&id) else {
            return Vec::new();
        };
        // Assign the next seq from the current tail, not a separate counter:
        // the frame log is never fully emptied by the per-image cap eviction
        // below (only a fresh `insert` resets it to `Vec::new()`), so the last
        // entry's seq is always the running high-water mark.
        frame.seq = img.frames.last().map_or(1, |f| f.seq + 1);
        self.bytes += frame.data_b64.len();
        Arc::make_mut(&mut img.frames).push(frame);
        if img.frames.len() > Self::CAP_FRAMES_PER_IMAGE {
            let dropped = Arc::make_mut(&mut img.frames).remove(0);
            self.bytes = self.bytes.saturating_sub(dropped.data_b64.len());
        }
        self.evict_over_budget()
    }

    fn evict_over_budget(&mut self) -> Vec<u32> {
        let mut evicted = Vec::new();
        while self.bytes > Self::CAP_BYTES && self.order.len() > 1 {
            if let Some(victim) = self.order.pop_front() {
                if let Some(img) = self.map.remove(&victim) {
                    self.bytes = self.bytes.saturating_sub(img.total_bytes());
                    evicted.push(victim);
                }
            } else {
                break;
            }
        }
        evicted
    }

    pub fn get(&self, id: u32) -> Option<&Image> {
        self.map.get(&id)
    }

    /// Mutable access for updating in-place state (used to set `anim_control`
    /// from `a=a`). Frame appends go through `push_frame` instead, which keeps
    /// the byte budget accurate; this method does NOT track byte changes, so
    /// don't use it to touch `data_b64` or `frames` directly.
    pub fn get_mut(&mut self, id: u32) -> Option<&mut Image> {
        self.map.get_mut(&id)
    }

    pub fn contains(&self, id: u32) -> bool {
        self.map.contains_key(&id)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// A parsed Kitty graphics command: the `k=v` control keys plus the (base64)
/// payload. Only the keys Phase 2 needs are surfaced.
#[derive(Debug, Default, Clone)]
pub struct GraphicsCommand {
    /// `a=` action: `t` transmit, `T` transmit+display, `p` put/place, `d` delete,
    /// `q` query. Defaults to transmit-and-display when absent (`a` omitted on a
    /// data-bearing command means transmit, but timg sends `a=T`).
    pub action: u8,
    pub id: Option<u32>,           // i=
    pub placement_id: Option<u32>, // p=
    pub format: Option<u32>,       // f=
    pub width: Option<u32>,        // s=
    pub height: Option<u32>,       // v=
    pub rows: Option<u16>,         // r=
    pub cols: Option<u16>,         // c=
    pub more: bool,                // m=1 (more chunks coming)
    /// Whether the wire carried an explicit `m=` key at all, regardless of its
    /// value (`m=0` sets this too, even though it clears `more`). A real
    /// continuation chunk always carries `m=`; a metadata-light *fresh*
    /// single-shot command never does. `is_continuation` checks in
    /// `screen.rs` key on this instead of inferring "continuation" from the
    /// mere absence of other metadata, which used to conflate "no
    /// i=/f=/s=/v=" with "this must be a continuation" (2026-07-06 inline-
    /// graphics bug audit, findings #4/#5/#6).
    pub saw_m: bool,
    pub delete_target: Option<u8>, // d= (for a=d)
    pub unicode: bool,             // U=1 (Unicode-placeholder / virtual placement)
    /// `z=`: signed. Meaning depends on the action it's paired with — for
    /// `a=p`/`a=T` it's the placement z-index (negative draws under text);
    /// for `a=f` (Task 2) the same key means the inter-frame gap in ms
    /// (negative = gapless). One field, two call-site interpretations, same
    /// as `rows`/`cols` already are for `r=`/`c=` across different actions.
    pub z: Option<i32>,
    /// `x=` (`a=f` only): frame rect left, in pixels, within the canvas.
    pub frame_x: Option<u32>,
    /// `y=` (`a=f` only): frame rect top, in pixels, within the canvas.
    pub frame_y: Option<u32>,
    /// `X=1` (`a=f` only): overwrite the target rect. Absent/any other value
    /// means the default, alpha-blend the frame's pixels onto the canvas.
    pub compose_overwrite: bool,
    /// `Y=` (`a=f` only): solid background RGBA (32-bit). Default (absent)
    /// is transparent black (`0`).
    pub bg_color: Option<u32>,
    pub payload: Vec<u8>, // base64 chunk
}

/// Parse the framed `ESC _ G<params>;<payload> ESC \` bytes into a command.
/// Returns `None` if it isn't a graphics APC (doesn't start with `G`).
pub fn parse_command(framed: &[u8]) -> Option<GraphicsCommand> {
    // Strip the ESC_ prefix and ESC\ suffix.
    let inner = framed.strip_prefix(b"\x1b_")?;
    let inner = inner.strip_suffix(b"\x1b\\").unwrap_or(inner);
    let inner = inner.strip_prefix(b"G")?;
    let (params, payload): (&[u8], &[u8]) = match inner.iter().position(|&b| b == b';') {
        Some(i) => (&inner[..i], &inner[i + 1..]),
        None => (inner, &[]),
    };
    let mut cmd = GraphicsCommand {
        action: b't',
        payload: payload.to_vec(),
        ..Default::default()
    };
    for kv in params.split(|&b| b == b',') {
        let mut it = kv.splitn(2, |&b| b == b'=');
        let (Some(k), Some(v)) = (it.next(), it.next()) else {
            continue;
        };
        let val = str::from_utf8(v).unwrap_or("");
        match k {
            b"a" => cmd.action = v.first().copied().unwrap_or(b't'),
            b"i" => cmd.id = val.parse().ok(),
            b"p" => cmd.placement_id = val.parse().ok(),
            b"f" => cmd.format = val.parse().ok(),
            b"s" => cmd.width = val.parse().ok(),
            b"v" => cmd.height = val.parse().ok(),
            b"r" => cmd.rows = val.parse().ok(),
            b"c" => cmd.cols = val.parse().ok(),
            b"z" => cmd.z = val.parse().ok(),
            b"m" => {
                cmd.saw_m = true;
                cmd.more = val == "1";
            }
            b"d" => cmd.delete_target = v.first().copied(),
            b"U" => cmd.unicode = val == "1",
            b"x" => cmd.frame_x = val.parse().ok(),
            b"y" => cmd.frame_y = val.parse().ok(),
            b"X" => cmd.compose_overwrite = val == "1",
            b"Y" => cmd.bg_color = val.parse().ok(),
            _ => {}
        }
    }
    Some(cmd)
}

/// Read `(width, height)` in pixels from Sixel data. Prefers the raster-attributes
/// command (`"Pan;Pad;Ph;Pv` → `Ph`×`Pv`); falls back to scanning the sixel
/// stream for its extent (max column run × band count). `None` if it can't tell.
pub fn sixel_dimensions(payload: &[u8]) -> Option<(u32, u32)> {
    // Raster attributes: a `"` followed by Pan;Pad;Ph;Pv.
    if let Some(pos) = payload.iter().position(|&b| b == b'"') {
        let mut nums = [0u32; 4];
        let mut idx = 0;
        let mut cur: Option<u32> = None;
        for &b in &payload[pos + 1..] {
            match b {
                // saturating: the payload is child-controlled, so a pathological
                // run of digits must not overflow-panic.
                b'0'..=b'9' => {
                    cur = Some(
                        cur.unwrap_or(0)
                            .saturating_mul(10)
                            .saturating_add(u32::from(b - b'0')),
                    );
                }
                b';' => {
                    if idx < 4 {
                        nums[idx] = cur.take().unwrap_or(0);
                        idx += 1;
                    }
                }
                _ => {
                    if idx < 4 {
                        nums[idx] = cur.take().unwrap_or(0);
                        idx += 1;
                    }
                    break;
                }
            }
        }
        if idx >= 4 && nums[2] > 0 && nums[3] > 0 {
            return Some((nums[2], nums[3]));
        }
    }
    // Fallback: scan for the extent. Data bytes `?`..`~` advance x by 1; `!N`
    // repeats the next data byte N times; `$` carriage-returns; `-` starts a new
    // 6px band; `#`/`"` introduce color/raster (digits skipped, no x advance).
    let mut x: u32 = 0;
    let mut max_x: u32 = 0;
    let mut bands: u32 = 1;
    let mut saw_data = false;
    let mut i = 0;
    while i < payload.len() {
        match payload[i] {
            b'!' => {
                // Repeat count, then one data byte. Saturating since the count is child-controlled.
                let mut n: u32 = 0;
                i += 1;
                while i < payload.len() && payload[i].is_ascii_digit() {
                    n = n
                        .saturating_mul(10)
                        .saturating_add(u32::from(payload[i] - b'0'));
                    i += 1;
                }
                if i < payload.len() && (0x3f..=0x7e).contains(&payload[i]) {
                    x = x.saturating_add(n.max(1));
                    saw_data = true;
                    i += 1;
                }
                max_x = max_x.max(x);
                continue;
            }
            b'$' => x = 0,
            b'-' => {
                bands = bands.saturating_add(1);
                x = 0;
            }
            b'#' | b'"' => {
                // Skip the parameter run (digits + `;`).
                // Equivalent note (genuinely-equivalent survivors only):
                // - Deleting this arm: digits (0x30-0x39) and `;` (0x3b) fall through to
                //   `_ => {}` one by one, and all are below the data range 0x3f..=0x7e so
                //   x/bands are unchanged; the arm just skips them faster.
                // - `||` → `&&` on the inner condition: the conjunction `is_digit && == b';'`
                //   is always false (no byte can be both), so the loop never runs; digits and
                //   `;` fall through to `_ => {}` exactly as above, same net x/bands.
                // - `<` → `==` or `<` → `>` on the range guard: loop never runs; same.
                // NOT equivalent: `== b';'` → `!= b';'`, which turns data bytes (`~` etc.)
                // into loop members, consuming them before they can advance x/bands.
                // That mutation is killed by the `sixel_dims_color_register_then_data` test.
                i += 1;
                while i < payload.len() && (payload[i].is_ascii_digit() || payload[i] == b';') {
                    i += 1;
                }
                continue;
            }
            0x3f..=0x7e => {
                x = x.saturating_add(1);
                saw_data = true;
                max_x = max_x.max(x);
            }
            _ => {}
        }
        i += 1;
    }
    // Equivalent note: `saw_data && max_x > 0` vs mutations of `&&→||` or `>→>=`:
    // • `max_x > 0` is always true when `saw_data` is true (x is incremented before
    //   max_x is updated, so max_x ≥ 1 whenever a data byte or repeat was seen).
    // • `saw_data || max_x > 0` ≡ `saw_data` because max_x > 0 implies saw_data.
    // • `max_x >= 0` is always true for u32; same as dropping the second condition.
    // Both mutations produce the same predicate as `saw_data`.
    if saw_data && max_x > 0 {
        Some((max_x, bands.saturating_mul(6)))
    } else {
        None
    }
}

/// Split an iTerm2 `OSC 1337` body (the `;`-rejoined params after `1337`) into
/// `(File= args, base64 data)`. Returns `None` if it isn't a `File=…:data` form.
pub fn parse_iterm_file(rejoined: &str) -> Option<(&str, &str)> {
    let body = rejoined.strip_prefix("File=")?;
    // Args use `=`/`;` and base64 uses `A-Za-z0-9+/=`, so neither contains `:` and
    // the first `:` cleanly separates args from data.
    body.split_once(':')
}

/// Best-effort `(width, height)` in pixels for an iTerm2 image: prefer explicit
/// `width=Npx`/`height=Npx` args, else decode the file header (PNG or JPEG) from
/// the base64 data. `None` if neither yields pixel dimensions (cell/percent
/// sizing is not interpreted, so the footprint falls back to 1×1).
pub fn iterm_dimensions(args: &str, b64: &str) -> Option<(u32, u32)> {
    let arg_px = |key: &str| -> Option<u32> {
        for kv in args.split(';') {
            if let Some(v) = kv.strip_prefix(key)
                && let Some(px) = v.strip_suffix("px")
            {
                return px.parse().ok();
            }
        }
        None
    };
    if let (Some(w), Some(h)) = (arg_px("width="), arg_px("height=")) {
        return Some((w, h));
    }
    // Decode a prefix of the base64 (enough for the file header) and read it.
    use base64::Engine as _;
    let prefix: String = b64
        .chars()
        .filter(|c| !c.is_whitespace())
        .take(16384)
        .collect();
    let bytes = STANDARD.decode(prefix.as_bytes()).ok()?;
    png_dimensions(&bytes).or_else(|| jpeg_dimensions(&bytes))
}

/// Read `(width, height)` in pixels from a JPEG by scanning for the first SOF
/// marker (`FFC0`…`FFCF`, excluding the non-frame `C4`/`C8`/`CC`). `None` if not
/// found in `bytes`.
pub fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    // Equivalent note: `bytes.len() < 4` vs `bytes.len() <= 4` (`< → <=` mutation):
    // a 4-byte input with valid SOI still returns None because the while loop requires
    // `i + 8 < bytes.len()` = `2 + 8 < 4` = false, so the loop never runs. Both forms
    // return None for len=4, and the two conditions are observationally equivalent.
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None; // not a JPEG (no SOI)
    }
    let mut i = 2;
    while i + 8 < bytes.len() {
        if bytes[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = bytes[i + 1];
        // 0xFF fill bytes (and 0xFF00 stuffing) before a marker: skip one byte.
        if marker == 0xFF || marker == 0x00 {
            i += 1;
            continue;
        }
        // SOF markers carry the frame's height/width.
        if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            let h = u32::from(u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]));
            let w = u32::from(u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]));
            return Some((w, h));
        }
        // Standalone markers (no length): RSTn, SOI, EOI, TEM.
        if matches!(marker, 0xD0..=0xD9 | 0x01) {
            i += 2;
            continue;
        }
        // Otherwise a length-prefixed segment: skip it. A zero/short length would
        // stall, so always advance at least one byte.
        let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        i += (2 + len).max(1);
    }
    None
}

/// Read `(width, height)` in pixels from a PNG header (8-byte signature + IHDR).
/// `bytes` is the decoded PNG prefix (≥ 24 bytes). `None` if not a PNG.
pub fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Some((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_transmit_and_display_keys() {
        let cmd = parse_command(b"\x1b_Ga=T,i=42,f=100,s=640,v=480,m=1;AAAA\x1b\\").unwrap();
        assert_eq!(cmd.action, b'T');
        assert_eq!(cmd.id, Some(42));
        assert_eq!(cmd.format, Some(100));
        assert_eq!(cmd.width, Some(640));
        assert_eq!(cmd.height, Some(480));
        assert!(cmd.more);
        assert_eq!(cmd.payload, b"AAAA");
    }

    #[test]
    fn parses_continuation_chunk() {
        let cmd = parse_command(b"\x1b_Gm=0;BBBB\x1b\\").unwrap();
        assert_eq!(cmd.action, b't', "no a= → default transmit");
        assert!(!cmd.more);
        assert!(cmd.saw_m, "m=0 still sets saw_m even though more is false");
        assert_eq!(cmd.payload, b"BBBB");
    }

    #[test]
    fn saw_m_false_when_key_absent() {
        let cmd = parse_command(b"\x1b_Ga=T,i=1;AAAA\x1b\\").unwrap();
        assert!(!cmd.saw_m, "no m= key on the wire → saw_m stays false");
    }

    #[test]
    fn non_graphics_apc_is_none() {
        assert!(parse_command(b"\x1b_qfoo\x1b\\").is_none());
    }

    #[test]
    fn parse_command_z_index_key() {
        let cmd = parse_command(b"\x1b_Ga=p,i=5,z=-3\x1b\\").unwrap();
        assert_eq!(cmd.z, Some(-3));
    }

    #[test]
    fn parse_command_z_index_absent_by_default() {
        let cmd = parse_command(b"\x1b_Ga=p,i=5\x1b\\").unwrap();
        assert_eq!(cmd.z, None);
    }

    #[test]
    fn parse_command_frame_transmit_keys() {
        let cmd = parse_command(
            b"\x1b_Ga=f,i=9,r=2,c=1,x=10,y=5,s=20,v=30,X=1,Y=4278190335,z=-1,f=32\x1b\\",
        )
        .unwrap();
        assert_eq!(cmd.action, b'f');
        assert_eq!(cmd.id, Some(9));
        assert_eq!(cmd.rows, Some(2)); // r=: reused as frame number for a=f
        assert_eq!(cmd.cols, Some(1)); // c=: reused as canvas-source frame for a=f
        assert_eq!(cmd.frame_x, Some(10));
        assert_eq!(cmd.frame_y, Some(5));
        assert_eq!(cmd.width, Some(20)); // s=: frame rect width
        assert_eq!(cmd.height, Some(30)); // v=: frame rect height
        assert!(cmd.compose_overwrite);
        assert_eq!(cmd.bg_color, Some(4_278_190_335));
        assert_eq!(cmd.z, Some(-1)); // reused: gap_ms for a=f
        assert_eq!(cmd.format, Some(32));
    }

    #[test]
    fn parse_command_frame_defaults() {
        let cmd = parse_command(b"\x1b_Ga=f,i=9\x1b\\").unwrap();
        assert!(!cmd.compose_overwrite);
        assert_eq!(cmd.bg_color, None);
        assert_eq!(cmd.frame_x, None);
        assert_eq!(cmd.frame_y, None);
    }

    #[test]
    fn parse_command_animation_control_keys() {
        let cmd = parse_command(b"\x1b_Ga=a,i=6,s=3,v=0,c=2\x1b\\").unwrap();
        assert_eq!(cmd.action, b'a');
        assert_eq!(cmd.id, Some(6));
        assert_eq!(cmd.width, Some(3)); // s=: reused as animation state for a=a
        assert_eq!(cmd.height, Some(0)); // v=: reused as loop count for a=a
        assert_eq!(cmd.cols, Some(2)); // c=: reused as current-frame selector for a=a
    }

    #[test]
    fn png_dims_from_header() {
        let mut png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&480u32.to_be_bytes());
        assert_eq!(png_dimensions(&png), Some((640, 480)));
        assert_eq!(png_dimensions(b"not a png header...."), None);
    }

    #[test]
    fn sixel_dims_from_raster_attributes() {
        assert_eq!(
            sixel_dimensions(b"\"1;1;640;480#0;2;0;0;0~~~"),
            Some((640, 480))
        );
    }

    #[test]
    fn sixel_dims_fallback_scan_when_no_raster() {
        // 3 data columns, one band → 3×6.
        assert_eq!(sixel_dimensions(b"~~~"), Some((3, 6)));
        // Two bands (one `-`), max 2 wide → 2×12.
        assert_eq!(sixel_dimensions(b"~~-~"), Some((2, 12)));
        // RLE `!5~` → 5 columns wide.
        assert_eq!(sixel_dimensions(b"!5~"), Some((5, 6)));
        // No data at all.
        assert_eq!(sixel_dimensions(b"#0;2;0;0;0"), None);
    }

    #[test]
    fn sixel_dims_color_register_then_data() {
        // `#0~~~`: color-select register 0 (no semicolons) immediately followed by
        // three data bytes. This pins the `payload[i] == b';'` branch so the
        // `==`→`!=` mutant (which would also consume `~` in the skip loop,
        // returning None instead of Some) is caught.
        assert_eq!(sixel_dimensions(b"#0~~~"), Some((3, 6)));
        // With a semicolon-separated color spec the data must still be found.
        assert_eq!(sixel_dimensions(b"#0;2;0;0;0~~~"), Some((3, 6)));
    }

    #[test]
    fn sixel_dimensions_saturates_on_pathological_counts() {
        // The payload is child-controlled, so huge repeat counts / raster numbers must
        // not overflow-panic. We just assert these return without panicking.
        let _ = sixel_dimensions(b"!99999999999999999999~");
        let _ = sixel_dimensions(b"\"1;1;99999999999999999999;88888888888888888888~~~");
        let mut many = vec![b'~'; 10];
        many.splice(0..0, b"!4000000000".iter().copied());
        let _ = sixel_dimensions(&many);
    }

    #[test]
    fn iterm_file_splits_args_and_data() {
        assert_eq!(
            parse_iterm_file("File=inline=1;width=20px:QUJDQUJD"),
            Some(("inline=1;width=20px", "QUJDQUJD"))
        );
        assert_eq!(parse_iterm_file("NotFile=x:y"), None);
    }

    #[test]
    fn iterm_dims_from_pixel_args() {
        assert_eq!(
            iterm_dimensions("inline=1;width=20px;height=40px", "ignored"),
            Some((20, 40))
        );
        // Cell/percent sizing is not interpreted → falls through (no decodable data here).
        assert_eq!(
            iterm_dimensions("inline=1;width=10;height=50%", "!!notb64!!"),
            None
        );
    }

    #[test]
    fn iterm_dims_from_png_header() {
        let mut png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&30u32.to_be_bytes());
        png.extend_from_slice(&40u32.to_be_bytes());
        use base64::Engine as _;
        let b64 = STANDARD.encode(&png);
        assert_eq!(iterm_dimensions("inline=1", &b64), Some((30, 40)));
    }

    #[test]
    fn jpeg_dims_from_sof() {
        // SOI + SOF0: FFC0 len=0x0011 precision=8 height=0x0028(40) width=0x001E(30) …
        let mut jpg = vec![
            0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x00, 0x28, 0x00, 0x1E,
        ];
        jpg.extend_from_slice(&[0x03; 10]); // pad so i+9 < len during the scan
        assert_eq!(jpeg_dimensions(&jpg), Some((30, 40)));
        assert_eq!(jpeg_dimensions(b"not a jpeg"), None);
    }

    // --- helper ---
    fn make_img(id: u32, size: usize) -> Image {
        Image {
            id,
            format: ImageFormat::Png,
            pixel_w: 1,
            pixel_h: 1,
            data_b64: vec![0u8; size].into(),
            iterm_args: None,
            protocol: ImageProtocol::Kitty,
            generation: 1,
            frames: Arc::new(Vec::new()),
            anim_control: None,
        }
    }

    fn sample_image(id: u32, data: &[u8]) -> Image {
        Image {
            id,
            protocol: ImageProtocol::Kitty,
            format: ImageFormat::Rgba,
            pixel_w: 1,
            pixel_h: 1,
            data_b64: Arc::from(data),
            iterm_args: None,
            generation: 1,
            frames: Arc::new(Vec::new()),
            anim_control: None,
        }
    }

    fn sample_frame(data: &[u8]) -> Frame {
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
            format: ImageFormat::Rgba,
            data_b64: Arc::from(data),
            seq: 0, // overwritten by push_frame
        }
    }

    #[test]
    fn push_frame_no_op_for_unknown_id() {
        let mut store = ImageStore::default();
        let evicted = store.push_frame(999, sample_frame(b"data"));
        assert!(evicted.is_empty());
    }

    #[test]
    fn push_frame_appends_and_counts_bytes() {
        let mut store = ImageStore::default();
        store.insert(sample_image(1, b"base"));
        store.push_frame(1, sample_frame(b"frame-one"));
        assert_eq!(store.get(1).unwrap().frames.len(), 1);
        assert_eq!(
            store.get(1).unwrap().frames[0].data_b64.as_ref(),
            b"frame-one"
        );
        assert_eq!(
            store.get(1).unwrap().frames[0].seq,
            1,
            "first frame ever pushed for a fresh image gets seq 1"
        );
    }

    #[test]
    fn push_frame_evicts_oldest_frame_past_cap() {
        let mut store = ImageStore::default();
        store.insert(sample_image(1, b"base"));
        for i in 0..(ImageStore::CAP_FRAMES_PER_IMAGE + 5) {
            store.push_frame(1, sample_frame(format!("f{i}").as_bytes()));
        }
        let frames = &store.get(1).unwrap().frames;
        assert_eq!(frames.len(), ImageStore::CAP_FRAMES_PER_IMAGE);
        // The oldest 5 frames (f0..f4) were evicted; the log now starts at f5.
        assert_eq!(frames[0].data_b64.as_ref(), b"f5");
        // `seq` is assigned once per frame and never reused/rewritten by
        // eviction (finding #2): f5 kept its original seq of 6 (1-based,
        // f0..f4 were seq 1..5) even though it's now at index 0, and the log
        // is still monotonic front-to-back across the eviction boundary.
        assert_eq!(
            frames[0].seq, 6,
            "a surviving frame's seq must not shift when older frames are evicted"
        );
        assert!(
            frames.windows(2).all(|w| w[0].seq < w[1].seq),
            "seq must stay strictly increasing across the eviction boundary"
        );
    }

    // --- ImageFormat::from_kitty_f ---

    #[test]
    fn from_kitty_f_all_arms() {
        // kills: replace-whole-fn-with-None, delete arm 100, delete arm 24, delete arm 32
        assert_eq!(ImageFormat::from_kitty_f(100), Some(ImageFormat::Png));
        assert_eq!(ImageFormat::from_kitty_f(24), Some(ImageFormat::Rgb));
        assert_eq!(ImageFormat::from_kitty_f(32), Some(ImageFormat::Rgba));
        assert_eq!(ImageFormat::from_kitty_f(0), None);
        assert_eq!(ImageFormat::from_kitty_f(25), None);
        assert_eq!(ImageFormat::from_kitty_f(99), None);
        assert_eq!(ImageFormat::from_kitty_f(101), None);
        assert_eq!(ImageFormat::from_kitty_f(33), None);
    }

    #[test]
    fn kitty_f_correct_values() {
        // kills: replace-return-with-0, replace-return-with-1
        assert_eq!(ImageFormat::Png.kitty_f(), 100);
        assert_eq!(ImageFormat::Rgb.kitty_f(), 24);
        assert_eq!(ImageFormat::Rgba.kitty_f(), 32);
        // all three are distinct and non-zero/non-one
        assert_ne!(ImageFormat::Png.kitty_f(), 0);
        assert_ne!(ImageFormat::Rgb.kitty_f(), 1);
        assert_ne!(ImageFormat::Rgba.kitty_f(), 0);
        assert_ne!(ImageFormat::Rgba.kitty_f(), 1);
        // roundtrip
        for &f in &[100u32, 24, 32] {
            assert_eq!(ImageFormat::from_kitty_f(f).unwrap().kitty_f(), f);
        }
    }

    // --- ImageStore::len / is_empty ---

    #[test]
    fn store_len_is_empty() {
        // kills: len→1, is_empty→true
        let mut store = ImageStore::default();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        store.insert(make_img(1, 10));
        assert!(!store.is_empty());
        assert_eq!(store.len(), 1);
        store.insert(make_img(2, 10));
        assert!(!store.is_empty());
        assert_eq!(store.len(), 2);
    }

    // --- ImageStore::CAP_BYTES and eviction-loop mutations ---

    #[test]
    fn store_tiny_images_no_eviction() {
        // 200 bytes total << 64 MiB real cap.
        // kills: CAP_BYTES cap=0 and cap=64 mutations (would evict),
        //        `&& → ||` mutation (evicts whenever len > 1 regardless of bytes).
        let mut store = ImageStore::default();
        let ev1 = store.insert(make_img(1, 100));
        let ev2 = store.insert(make_img(2, 100));
        assert!(ev1.is_empty());
        assert!(ev2.is_empty());
        assert_eq!(store.len(), 2);
        assert!(store.contains(1));
        assert!(store.contains(2));
    }

    #[test]
    fn store_medium_images_no_eviction() {
        // 2 × 600 KiB = 1.2 MiB total.  Real cap = 64 MiB → no eviction.
        // kills: CAP_BYTES `64*1024+1024` (≈66 KiB) and `64+1024*1024` (≈1 MiB) mutations.
        let mut store = ImageStore::default();
        let ev1 = store.insert(make_img(1, 600 * 1024));
        let ev2 = store.insert(make_img(2, 600 * 1024));
        assert!(ev1.is_empty(), "600 KiB × 2 fits in the real 64 MiB cap");
        assert!(ev2.is_empty());
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn store_reinsertion_preserves_lru_order() {
        // Re-insert id=1 (after ids 1 and 2 are present) → id=1 moves to LRU back.
        // Then a large image forces eviction of the now-oldest id=2, not id=1.
        //
        // Mutation `retain(i == id)` corrupts order to [1,1,3]; evicts id=1 instead.
        //
        // Uses ~100 MiB (needed to cross the 64 MiB cap; mirrors existing eviction test).
        let mb25: Arc<[u8]> = vec![0u8; 25 * 1024 * 1024].into();
        let mut store = ImageStore::default();
        store.insert(Image {
            id: 1,
            format: ImageFormat::Png,
            pixel_w: 1,
            pixel_h: 1,
            data_b64: mb25.clone(),
            iterm_args: None,
            protocol: ImageProtocol::Kitty,
            generation: 1,
            frames: Arc::new(Vec::new()),
            anim_control: None,
        });
        store.insert(Image {
            id: 2,
            format: ImageFormat::Png,
            pixel_w: 1,
            pixel_h: 1,
            data_b64: mb25.clone(),
            iterm_args: None,
            protocol: ImageProtocol::Kitty,
            generation: 1,
            frames: Arc::new(Vec::new()),
            anim_control: None,
        });
        // re-insert id=1 → moves it to the back of the LRU; id=2 is now oldest
        store.insert(Image {
            id: 1,
            format: ImageFormat::Png,
            pixel_w: 1,
            pixel_h: 1,
            data_b64: mb25.clone(),
            iterm_args: None,
            protocol: ImageProtocol::Kitty,
            generation: 2,
            frames: Arc::new(Vec::new()),
            anim_control: None,
        });
        // insert id=3 → total ≈ 75 MiB > 64 MiB; id=2 (oldest) must be evicted
        let evicted = store.insert(Image {
            id: 3,
            format: ImageFormat::Png,
            pixel_w: 1,
            pixel_h: 1,
            data_b64: mb25,
            iterm_args: None,
            protocol: ImageProtocol::Kitty,
            generation: 1,
            frames: Arc::new(Vec::new()),
            anim_control: None,
        });
        assert_eq!(evicted, vec![2], "id=2 is oldest after re-insert of id=1");
        assert!(store.contains(1));
        assert!(!store.contains(2));
        assert!(store.contains(3));
    }

    #[test]
    fn store_exactly_at_cap_no_eviction() {
        // Two 32 MiB images → total = 64 MiB = CAP_BYTES exactly.
        // kills: `> → >=` at line 136:26. With `>=`, `bytes >= cap` fires and
        // evicts id=1, leaving only id=2. Original `>` → false → no eviction.
        let mb32: Arc<[u8]> = vec![0u8; 32 * 1024 * 1024].into();
        let mut store = ImageStore::default();
        let ev1 = store.insert(Image {
            id: 1,
            format: ImageFormat::Png,
            pixel_w: 1,
            pixel_h: 1,
            data_b64: mb32.clone(),
            iterm_args: None,
            protocol: ImageProtocol::Kitty,
            generation: 1,
            frames: Arc::new(Vec::new()),
            anim_control: None,
        });
        let ev2 = store.insert(Image {
            id: 2,
            format: ImageFormat::Png,
            pixel_w: 1,
            pixel_h: 1,
            data_b64: mb32,
            iterm_args: None,
            protocol: ImageProtocol::Kitty,
            generation: 1,
            frames: Arc::new(Vec::new()),
            anim_control: None,
        });
        assert!(ev1.is_empty(), "first insert: no eviction");
        assert!(
            ev2.is_empty(),
            "total == cap exactly: no eviction with `>`; mutation `>=` evicts"
        );
        assert_eq!(store.len(), 2);
        assert!(store.contains(1));
        assert!(store.contains(2));
    }

    #[test]
    fn store_single_oversized_image_never_evicted() {
        // One image slightly over cap: the `len > 1` guard prevents self-eviction.
        // kills: `> → >=` at line 136:64.
        // With mutation `len >= 1`: bytes > cap AND len=1 >= 1 → evicts the sole image.
        let mb65: Arc<[u8]> = vec![0u8; 65 * 1024 * 1024].into();
        let mut store = ImageStore::default();
        let evicted = store.insert(Image {
            id: 1,
            format: ImageFormat::Png,
            pixel_w: 1,
            pixel_h: 1,
            data_b64: mb65,
            iterm_args: None,
            protocol: ImageProtocol::Kitty,
            generation: 1,
            frames: Arc::new(Vec::new()),
            anim_control: None,
        });
        assert!(
            evicted.is_empty(),
            "sole image must survive even if it exceeds cap"
        );
        assert_eq!(store.len(), 1);
        assert!(store.contains(1));
    }

    #[test]
    fn store_eviction_keeps_last_image() {
        // Two 40 MiB images: after evicting the first, bytes drops below cap, so
        // exactly 1 image survives. Verifies the eviction loop terminates correctly.
        let mb40: Arc<[u8]> = vec![0u8; 40 * 1024 * 1024].into();
        let mut store = ImageStore::default();
        store.insert(Image {
            id: 1,
            format: ImageFormat::Png,
            pixel_w: 1,
            pixel_h: 1,
            data_b64: mb40.clone(),
            iterm_args: None,
            protocol: ImageProtocol::Kitty,
            generation: 1,
            frames: Arc::new(Vec::new()),
            anim_control: None,
        });
        let evicted = store.insert(Image {
            id: 2,
            format: ImageFormat::Png,
            pixel_w: 1,
            pixel_h: 1,
            data_b64: mb40,
            iterm_args: None,
            protocol: ImageProtocol::Kitty,
            generation: 1,
            frames: Arc::new(Vec::new()),
            anim_control: None,
        });
        // After the eviction loop: id=1 gone, id=2 remains (40 MiB < 64 MiB cap).
        assert_eq!(evicted, vec![1]);
        assert_eq!(store.len(), 1, "exactly one image survives");
        assert!(store.contains(2), "the surviving image is id=2");
    }

    // --- parse_command: d= key ---

    #[test]
    fn parse_command_delete_target_key() {
        // kills: delete match arm b"d" → delete_target never set
        let cmd = parse_command(b"\x1b_Ga=d,d=a,i=5\x1b\\").unwrap();
        assert_eq!(cmd.action, b'd');
        assert_eq!(cmd.delete_target, Some(b'a'));
        assert_eq!(cmd.id, Some(5));

        let cmd2 = parse_command(b"\x1b_Ga=d,d=z\x1b\\").unwrap();
        assert_eq!(cmd2.delete_target, Some(b'z'));

        // without the d= key → None
        let cmd3 = parse_command(b"\x1b_Ga=d,i=1\x1b\\").unwrap();
        assert_eq!(cmd3.delete_target, None);
    }

    // --- sixel_dimensions ---

    #[test]
    fn sixel_raster_attr_with_leading_bytes() {
        // `"` is not the first byte → raster path must find the correct position.
        // kills: `== vs !=` mutation on line 230 (with `!=`, position() finds
        // the first *non-*`"` byte instead, giving a wrong parse start).
        assert_eq!(
            sixel_dimensions(b"#0;2;0;100;100\"1;1;640;480q"),
            Some((640, 480))
        );
        assert_eq!(
            sixel_dimensions(b"#0;2;0;0;0#1;2;100;0;0\"1;1;320;240!"),
            Some((320, 240))
        );
    }

    #[test]
    fn sixel_raster_attr_extra_values_no_panic() {
        // Five semicolons in the raster attribute; only the first four values matter.
        // kills: `idx < 4` → `idx <= 4` mutations (line 248): with `<=`, the
        // code would try nums[4] on an out-of-bounds index and panic.
        assert_eq!(sixel_dimensions(b"\"1;1;640;480;999q"), Some((640, 480)));
        assert_eq!(sixel_dimensions(b"\"1;1;640;480;x"), Some((640, 480)));

        // A trailing `;` after the 4th value (triggers the b';' branch at idx==4).
        // kills: `idx < 4` → `idx <= 4` mutation (line 242): with `<=`, the
        // b';' branch fires for idx==4 and writes nums[4] → OOB panic.
        assert_eq!(sixel_dimensions(b"\"1;1;640;480;999;x"), Some((640, 480)));
    }

    #[test]
    fn sixel_raster_zero_dimensions_none() {
        // kills: `> vs >=` (nums[2] >= 0 is always true for u32) and
        //        `&& vs ||` (would return Some when only one dim is nonzero).
        //
        // Use `\x00` as the raster-attr terminator: it triggers the `_` branch
        // (flushing nums[3]) and is outside the data-byte range 0x3f..=0x7e,
        // so the fallback scan also finds no data and returns None.
        assert_eq!(sixel_dimensions(b"\"1;1;0;480\x00"), None); // width=0
        assert_eq!(sixel_dimensions(b"\"1;1;640;0\x00"), None); // height=0
        assert_eq!(sixel_dimensions(b"\"1;1;0;0\x00"), None); // both=0
        // Both nonzero: the raster path succeeds directly (no fallback).
        assert_eq!(sixel_dimensions(b"\"1;1;1;1\x00"), Some((1, 1)));
    }

    #[test]
    fn sixel_carriage_return_resets_column() {
        // kills: delete match arm `b'$'`
        // Without the `$` arm, x would keep growing instead of resetting.
        assert_eq!(sixel_dimensions(b"~~~$~~"), Some((3, 6))); // max=3, not 5
        assert_eq!(sixel_dimensions(b"~~~~~$~"), Some((5, 6))); // max=5, not 6
        assert_eq!(sixel_dimensions(b"~~~$~~~~~$~~"), Some((5, 6))); // max=5, not 10
    }

    #[test]
    fn sixel_new_band_increments_height() {
        // `-` starts a new 6-pixel band, so this verifies the bands counter is used.
        assert_eq!(sixel_dimensions(b"~-~"), Some((1, 12))); // 2 bands × 6 = 12
        assert_eq!(sixel_dimensions(b"~-~-~"), Some((1, 18))); // 3 bands
    }

    #[test]
    fn sixel_data_byte_range_boundary() {
        // kills: boundary mutations on the `0x3f..=0x7e` match arm (line 299).
        assert_eq!(sixel_dimensions(b"?"), Some((1, 6))); // 0x3f: inclusive lower bound
        assert_eq!(sixel_dimensions(b"~"), Some((1, 6))); // 0x7e: inclusive upper bound
        assert_eq!(sixel_dimensions(b"\x3e"), None); // 0x3e: just below → not data
        assert_eq!(sixel_dimensions(b"\x7f"), None); // 0x7f: just above → not data
    }

    #[test]
    fn sixel_repeat_at_end_no_data_byte() {
        // `!N` with no trailing data byte: tests the inner digit-loop boundary
        // (`while i < len`) and the outer `if i < len && range.contains` guard.
        // kills: `< vs <=` on line 274 (inner loop OOB) and line 278 (outer guard OOB).
        assert_eq!(sixel_dimensions(b"!3"), None); // repeat with no data byte
        assert_eq!(sixel_dimensions(b"!99"), None); // multi-digit, no data byte
        assert_eq!(sixel_dimensions(b"!3~"), Some((3, 6))); // normal repeat still works
    }

    #[test]
    fn sixel_repeat_out_of_range_byte_ignored() {
        // `!3` followed by a byte outside 0x3f..=0x7e: must not advance x.
        // kills: `|| vs &&` mutation on line 278 (`i < len || contains(...)` would
        // treat the out-of-range byte as a valid data byte when i is in bounds).
        assert_eq!(sixel_dimensions(b"!3\x7f"), None); // 0x7f: just above range
        assert_eq!(sixel_dimensions(b"!3\x3e"), None); // 0x3e: just below range
        assert_eq!(sixel_dimensions(b"!3\x3f"), Some((3, 6))); // 0x3f: lowest valid
    }

    // --- jpeg_dimensions ---

    #[test]
    fn jpeg_initial_check_short_and_bad_soi() {
        // kills mutations on the initial `bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8`
        assert_eq!(jpeg_dimensions(&[]), None);
        assert_eq!(jpeg_dimensions(&[0xFF]), None);
        assert_eq!(jpeg_dimensions(&[0xFF, 0xD8]), None);
        assert_eq!(jpeg_dimensions(&[0xFF, 0xD8, 0xFF]), None);
        // wrong first SOI byte
        assert_eq!(
            jpeg_dimensions(&[
                0x00, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x00, 0x28, 0x00, 0x1E, 0x00, 0x00, 0x00
            ]),
            None
        );
        // wrong second SOI byte
        assert_eq!(
            jpeg_dimensions(&[
                0xFF, 0x00, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x00, 0x28, 0x00, 0x1E, 0x00, 0x00, 0x00
            ]),
            None
        );
    }

    #[test]
    fn jpeg_loop_skips_rst_markers_before_sof() {
        // RST markers (0xD0..=0xD7) are standalone 2-byte markers, so the loop
        // must skip them and reach the SOF. This also makes `while i + 8 < len`
        // fire for multiple iterations.
        let mut jpg = vec![
            0xFF, 0xD8, // SOI
            0xFF, 0xD0, // RST0 (standalone)
            0xFF, 0xD1, // RST1 (standalone)
            0xFF, 0xC0, // SOF0
            0x00, 0x11, // segment length = 17
            0x08, // precision
            0x00, 0x64, // height = 100
            0x00, 0x50, // width = 80
        ];
        jpg.extend_from_slice(&[0x03u8; 10]); // padding so i+8 < len at the SOF
        assert_eq!(jpeg_dimensions(&jpg), Some((80, 100)));
    }

    #[test]
    fn jpeg_skips_length_prefixed_app_segment() {
        // APP0 (0xFFE0) is length-prefixed, so the parser has to skip its payload to
        // reach SOF. Exercises the `i += (2 + len).max(1)` path.
        let mut jpg = vec![
            0xFF, 0xD8, // SOI
            0xFF, 0xE0, // APP0
            0x00, 0x10, // length = 16 (includes the 2-byte length field)
        ];
        jpg.extend_from_slice(&[0x00u8; 14]); // 14 bytes of APP0 payload
        // SOF0
        jpg.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08, 0x00, 0xF0, 0x00, 0xA0]);
        jpg.extend_from_slice(&[0x03u8; 10]);
        // height = 0x00F0 = 240, width = 0x00A0 = 160
        assert_eq!(jpeg_dimensions(&jpg), Some((160, 240)));
    }

    #[test]
    fn jpeg_truncated_at_sof_boundary() {
        // 10-byte input: `i + 8 < 10` = false → skip loop → None.
        // kills: 357:17 `< → <=`: `i + 8 <= 10` = true → bytes[10] OOB.
        let data: &[u8] = &[0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x00, 0x28, 0x00];
        assert_eq!(jpeg_dimensions(data), None);
    }

    #[test]
    fn jpeg_garbage_byte_before_sof() {
        // Non-0xFF byte at position 2 must be skipped via `i += 1` (line 359).
        // kills: 359:15 `-=` (backward advance → wrong result → None instead of Some)
        //        359:15 `*=` (no advance → infinite loop → timeout → caught)
        let mut jpg = vec![
            0xFF, 0xD8, // SOI
            0x00, // garbage (not 0xFF): i=2 → `i += 1` → i=3
            0xFF, 0xC0, // SOF0 at i=3
            0x00, 0x11, // segment length = 17
            0x08, // precision
            0x00, 0x64, // height = 100
            0x00, 0x50, // width = 80
        ];
        jpg.extend_from_slice(&[0x03u8; 10]); // padding: i+8 < len at SOF
        assert_eq!(jpeg_dimensions(&jpg), Some((80, 100)));
    }

    #[test]
    fn jpeg_fill_byte_before_marker() {
        // 0xFF fill byte (consecutive 0xFF) must be skipped by `i += 1` (line 365).
        // kills: 364:27 `|| → &&` (condition false → fill not skipped → wrong parse)
        //        365:15 `-=` (backward advance → infinite loop / wrong result)
        //        365:15 `*=` (no advance → infinite loop → timeout → caught)
        let mut jpg = vec![
            0xFF, 0xD8, // SOI
            0xFF, 0xFF, // fill byte: bytes[2]=0xFF, marker=0xFF → `i += 1`
            0xFF, 0xC0, // SOF0
            0x00, 0x11, // segment length = 17
            0x08, // precision
            0x00, 0x64, // height = 100
            0x00, 0x50, // width = 80
        ];
        jpg.extend_from_slice(&[0x03u8; 10]);
        assert_eq!(jpeg_dimensions(&jpg), Some((80, 100)));
    }

    #[test]
    fn jpeg_two_app_segments_at_different_offsets() {
        // Two APP segments so the length-prefixed path is reached with i=10 (not i=2).
        // At i=10: bytes[i+2]=bytes[12] ≠ bytes[i*2]=bytes[20];
        //          bytes[i+3]=bytes[13] ≠ bytes[i*3]=bytes[30].
        //
        // kills: 381:47 `+ → *`, which reads bytes[20]=0xFF as high byte of length
        //   → length=0xFF08=65288 → huge skip → miss SOF → None instead of Some.
        // kills: 381:61 `+ → *`, which reads bytes[30]=0xBB=187 as low byte of length
        //   → length=0x00BB=187 → skip=189 bytes >> total → miss SOF → None.
        //
        // bytes[30] is the 2nd byte of padding, set to 0xBB so the `i*3` mutation
        // picks a large value that causes a definitive skip past the SOF.
        let mut jpg = vec![
            0xFF, 0xD8, // SOI (i=2)
            // APP0 at i=2: marker=0xE0, length-field=6 (4-byte payload). Skip 8 → i=10.
            0xFF, 0xE0, 0x00, 0x06, 0xAA, 0x11, 0xCC, 0xDD,
            // APP1 at i=10: marker=0xE1, length-field=8 (6-byte payload). Skip 10 → i=20.
            0xFF, 0xE1, 0x00, 0x08, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06,
            // SOF0 at i=20: height=0x00F0=240, width=0x00A0=160.
            0xFF, 0xC0, 0x00, 0x11, 0x08, 0x00, 0xF0, 0x00, 0xA0,
        ];
        // bytes[29]=0x03, bytes[30]=0xBB (large → `i*3` mutation computes wrong length).
        jpg.extend_from_slice(&[
            0x03u8, 0xBBu8, 0x11u8, 0x00u8, 0x02u8, 0x11u8, 0x01u8, 0x03u8, 0x11u8, 0x01u8,
        ]);
        assert_eq!(jpeg_dimensions(&jpg), Some((160, 240)));
    }

    // --- png_dimensions ---

    #[test]
    fn png_dimensions_length_boundary() {
        // Exactly 24 bytes, the minimum valid PNG size.
        let mut png: Vec<u8> = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]; // sig
        png.extend_from_slice(&13u32.to_be_bytes()); // IHDR chunk length
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&100u32.to_be_bytes()); // width
        png.extend_from_slice(&200u32.to_be_bytes()); // height
        assert_eq!(png.len(), 24);
        assert_eq!(png_dimensions(&png), Some((100, 200)));

        // 23 bytes → too short → None
        assert_eq!(png_dimensions(&png[..23]), None);

        // 25 bytes (one extra) still works.
        // kills the `< vs >` mutation: `> 24` would make len=25 return None.
        let mut longer = png.clone();
        longer.push(0x00);
        assert_eq!(png_dimensions(&longer), Some((100, 200)));

        // Wrong signature at the right length.
        let mut bad_sig = png.clone();
        bad_sig[0] = 0x00;
        assert_eq!(png_dimensions(&bad_sig), None);
    }

    #[test]
    fn image_store_lru_evicts_over_budget() {
        let mut store = ImageStore::default();
        // Two ~40 MiB images exceed the 64 MiB cap → the first evicts.
        let big = vec![b'A'; 40 * 1024 * 1024];
        let ev1 = store.insert(Image {
            id: 1,
            format: ImageFormat::Png,
            pixel_w: 1,
            pixel_h: 1,
            data_b64: big.clone().into(),
            iterm_args: None,
            protocol: ImageProtocol::Kitty,
            generation: 1,
            frames: Arc::new(Vec::new()),
            anim_control: None,
        });
        assert!(ev1.is_empty());
        let ev2 = store.insert(Image {
            id: 2,
            format: ImageFormat::Png,
            pixel_w: 1,
            pixel_h: 1,
            data_b64: big.into(),
            iterm_args: None,
            protocol: ImageProtocol::Kitty,
            generation: 2,
            frames: Arc::new(Vec::new()),
            anim_control: None,
        });
        assert_eq!(ev2, vec![1], "oldest image evicted over budget");
        assert!(!store.contains(1));
        assert!(store.contains(2));
    }
}
