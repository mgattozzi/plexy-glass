//! Inline-graphics model: captured images and their on-screen placements.
//!
//! Phase 2 feeds this from the Kitty graphics protocol (APC `ESC _ G … ST`);
//! later phases feed the same model from Sixel (DCS) and iTerm2 (OSC 1337).
//! The emulator only *models* images (id → data, plus placements anchored in
//! the grid); the compositor/renderer turn placements into per-client protocol
//! bytes.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

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
    pub fn from_kitty_f(f: u32) -> Option<Self> {
        match f {
            100 => Some(Self::Png),
            24 => Some(Self::Rgb),
            32 => Some(Self::Rgba),
            _ => None,
        }
    }

    pub fn kitty_f(self) -> u32 {
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

    /// Insert (or replace) an image, evicting oldest entries while over the
    /// byte budget. Returns the ids evicted, so the caller can drop placements
    /// that reference them.
    pub fn insert(&mut self, img: Image) -> Vec<u32> {
        let id = img.id;
        if let Some(old) = self.map.remove(&id) {
            self.bytes = self.bytes.saturating_sub(old.data_b64.len());
            self.order.retain(|&i| i != id);
        }
        self.bytes += img.data_b64.len();
        self.map.insert(id, img);
        self.order.push_back(id);
        let mut evicted = Vec::new();
        while self.bytes > Self::CAP_BYTES && self.order.len() > 1 {
            if let Some(victim) = self.order.pop_front() {
                if let Some(img) = self.map.remove(&victim) {
                    self.bytes = self.bytes.saturating_sub(img.data_b64.len());
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
    pub id: Option<u32>,         // i=
    pub placement_id: Option<u32>, // p=
    pub format: Option<u32>,     // f=
    pub width: Option<u32>,      // s=
    pub height: Option<u32>,     // v=
    pub rows: Option<u16>,       // r=
    pub cols: Option<u16>,       // c=
    pub more: bool,              // m=1 (more chunks coming)
    pub delete_target: Option<u8>, // d= (for a=d)
    pub unicode: bool,           // U=1 (Unicode-placeholder / virtual placement)
    pub payload: Vec<u8>,        // base64 chunk
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
        let (Some(k), Some(v)) = (it.next(), it.next()) else { continue };
        let val = std::str::from_utf8(v).unwrap_or("");
        match k {
            b"a" => cmd.action = v.first().copied().unwrap_or(b't'),
            b"i" => cmd.id = val.parse().ok(),
            b"p" => cmd.placement_id = val.parse().ok(),
            b"f" => cmd.format = val.parse().ok(),
            b"s" => cmd.width = val.parse().ok(),
            b"v" => cmd.height = val.parse().ok(),
            b"r" => cmd.rows = val.parse().ok(),
            b"c" => cmd.cols = val.parse().ok(),
            b"m" => cmd.more = val == "1",
            b"d" => cmd.delete_target = v.first().copied(),
            b"U" => cmd.unicode = val == "1",
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
                b'0'..=b'9' => cur = Some(cur.unwrap_or(0) * 10 + u32::from(b - b'0')),
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
                // Repeat count, then one data byte.
                let mut n: u32 = 0;
                i += 1;
                while i < payload.len() && payload[i].is_ascii_digit() {
                    n = n * 10 + u32::from(payload[i] - b'0');
                    i += 1;
                }
                if i < payload.len() && (0x3f..=0x7e).contains(&payload[i]) {
                    x += n.max(1);
                    saw_data = true;
                    i += 1;
                }
                max_x = max_x.max(x);
                continue;
            }
            b'$' => x = 0,
            b'-' => {
                bands += 1;
                x = 0;
            }
            b'#' | b'"' => {
                // Skip the parameter run (digits + `;`).
                i += 1;
                while i < payload.len() && (payload[i].is_ascii_digit() || payload[i] == b';') {
                    i += 1;
                }
                continue;
            }
            0x3f..=0x7e => {
                x += 1;
                saw_data = true;
                max_x = max_x.max(x);
            }
            _ => {}
        }
        i += 1;
    }
    if saw_data && max_x > 0 {
        Some((max_x, bands * 6))
    } else {
        None
    }
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
        assert_eq!(cmd.payload, b"BBBB");
    }

    #[test]
    fn non_graphics_apc_is_none() {
        assert!(parse_command(b"\x1b_qfoo\x1b\\").is_none());
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
        assert_eq!(sixel_dimensions(b"\"1;1;640;480#0;2;0;0;0~~~"), Some((640, 480)));
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
        });
        assert_eq!(ev2, vec![1], "oldest image evicted over budget");
        assert!(!store.contains(1));
        assert!(store.contains(2));
    }
}
