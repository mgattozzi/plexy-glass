//! Daemon-global paste buffers (tmux's paste buffers): a bounded, newest-first
//! stack of named byte buffers. Pushed by copy-mode yanks; read by `paste-buffer`
//! and the choose-buffer overlay. Held behind a `Mutex` on `SessionRegistry`.

use plexy_glass_emulator::truncate_to_width;
use plexy_glass_mux::BufferEntry;
use std::collections::VecDeque;

/// Display-column cap for a buffer's one-line preview in the chooser. A
/// daemon-side cap independent of the overlay box width (the compositor re-clips
/// each row to the box, so a second truncation there is harmless).
const PREVIEW_WIDTH: u16 = 40;

pub struct PasteBuffer {
    pub name: String,
    pub content: Vec<u8>,
}

/// Newest-first, bounded. Names are monotonic (`buffer0`, `buffer1`, …) and
/// stable for a buffer's lifetime (no re-indexing on push/delete), so the
/// by-name paste/delete dispatch is race-tolerant.
pub struct PasteBufferStore {
    buffers: VecDeque<PasteBuffer>,
    next_id: u64,
    cap: usize,
}

impl PasteBufferStore {
    pub fn new(cap: usize) -> Self {
        Self { buffers: VecDeque::new(), next_id: 0, cap: cap.max(1) }
    }

    /// Push `content` as a new newest buffer; evict the oldest past `cap`.
    pub fn push(&mut self, content: Vec<u8>) {
        let name = format!("buffer{}", self.next_id);
        self.next_id += 1;
        self.buffers.push_front(PasteBuffer { name, content });
        while self.buffers.len() > self.cap {
            self.buffers.pop_back();
        }
    }

    /// The most-recently pushed buffer.
    pub fn top(&self) -> Option<&PasteBuffer> {
        self.buffers.front()
    }

    pub fn get(&self, name: &str) -> Option<&PasteBuffer> {
        self.buffers.iter().find(|b| b.name == name)
    }

    /// Remove the buffer named `name`. Returns whether one was removed.
    pub fn delete(&mut self, name: &str) -> bool {
        let before = self.buffers.len();
        self.buffers.retain(|b| b.name != name);
        self.buffers.len() != before
    }

    /// Newest-first `(name, preview)` rows for the choose-buffer overlay.
    pub fn entries(&self) -> Vec<BufferEntry> {
        self.buffers
            .iter()
            .map(|b| BufferEntry { name: b.name.clone(), preview: preview(&b.content) })
            .collect()
    }
}

/// First line of `content`, lossily decoded, control chars (incl. `\r`/`\t`)
/// mapped to spaces, then truncated to `PREVIEW_WIDTH` display columns. Control
/// stripping precedes truncation because `grapheme_advance` floors a control char
/// to one column, so an un-stripped control would consume a truncation column.
fn preview(content: &[u8]) -> String {
    let s = String::from_utf8_lossy(content);
    let first = s.split('\n').next().unwrap_or("");
    let cleaned: String = first.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    truncate_to_width(&cleaned, PREVIEW_WIDTH).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_bounds_to_cap_newest_first() {
        let mut s = PasteBufferStore::new(2);
        s.push(b"one".to_vec());
        s.push(b"two".to_vec());
        s.push(b"three".to_vec()); // evicts "one"
        assert_eq!(s.top().unwrap().content, b"three");
        let names: Vec<_> = s.entries().into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["buffer2", "buffer1"], "newest-first, oldest evicted");
    }

    #[test]
    fn get_and_delete_by_name() {
        let mut s = PasteBufferStore::new(10);
        s.push(b"a".to_vec()); // buffer0
        s.push(b"b".to_vec()); // buffer1
        assert_eq!(s.get("buffer0").unwrap().content, b"a");
        assert!(s.get("nope").is_none());
        assert!(s.delete("buffer0"));
        assert!(!s.delete("buffer0"), "second delete is a no-op");
        assert!(s.get("buffer0").is_none());
        assert_eq!(s.top().unwrap().name, "buffer1");
    }

    #[test]
    fn preview_strips_controls_and_truncates() {
        let mut s = PasteBufferStore::new(10);
        s.push(b"first line\r\tx\nsecond line".to_vec());
        let p = &s.entries()[0].preview;
        assert!(!p.contains('\n') && !p.contains('\r') && !p.contains('\t'), "controls stripped: {p:?}");
        assert!(p.starts_with("first line"));
        assert!(!p.contains("second"), "only the first line is previewed");

        // Wide truncation: 60 CJK cells (120 display cols) clamps to <= 40 cols.
        let wide = "字".repeat(60);
        s.push(wide.into_bytes());
        let p2 = &s.entries()[0].preview;
        assert!(plexy_glass_emulator::display_width(p2) <= 40, "preview truncated to width");
    }

    #[test]
    fn zero_cap_does_not_panic() {
        let mut s = PasteBufferStore::new(0); // clamped to 1
        s.push(b"x".to_vec());
        assert_eq!(s.top().unwrap().content, b"x");
    }
}
