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
    pub format: ImageFormat,
    pub pixel_w: u32,
    pub pixel_h: u32,
    pub data_b64: Arc<[u8]>,
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
            generation: 1,
        });
        assert!(ev1.is_empty());
        let ev2 = store.insert(Image {
            id: 2,
            format: ImageFormat::Png,
            pixel_w: 1,
            pixel_h: 1,
            data_b64: big.into(),
            generation: 2,
        });
        assert_eq!(ev2, vec![1], "oldest image evicted over budget");
        assert!(!store.contains(1));
        assert!(store.contains(2));
    }
}
