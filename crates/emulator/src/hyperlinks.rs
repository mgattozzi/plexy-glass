//! OSC 8 hyperlink URL table. IDs are `u16`; the table dedups by URL.

use tracing::warn;

/// Index into a `HyperlinkTable` (stored per-cell/-cursor). A newtype so a raw
/// `u16` can't be mistaken for one; mirrors the `PaneId`/`WindowId` pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HyperlinkId(pub u16);

impl HyperlinkId {
    pub const fn get(self) -> u16 {
        self.0
    }
}

#[derive(Debug, Default, Clone)]
pub struct HyperlinkTable {
    urls: Vec<String>,
    full_warned: bool,
}

impl HyperlinkTable {
    /// Intern a URL and return its ID. If the URL is already present, returns
    /// the existing ID. Returns `None` only when the table has hit `u16::MAX`
    /// distinct URLs (one warning is logged the first time).
    pub fn intern(&mut self, url: &str) -> Option<HyperlinkId> {
        if let Some(idx) = self.urls.iter().position(|u| u == url) {
            return Some(HyperlinkId(idx as u16));
        }
        if self.urls.len() >= u16::MAX as usize {
            if !self.full_warned {
                warn!("hyperlink table full; dropping new hyperlinks");
                self.full_warned = true;
            }
            return None;
        }
        let id = self.urls.len() as u16;
        self.urls.push(url.to_string());
        Some(HyperlinkId(id))
    }

    pub fn get(&self, id: HyperlinkId) -> Option<&str> {
        self.urls.get(id.get() as usize).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_dedupes() {
        let mut t = HyperlinkTable::default();
        let id1 = t.intern("https://example.com").unwrap();
        let id2 = t.intern("https://example.com").unwrap();
        assert_eq!(id1, id2);
        // Only one distinct URL is interned, so id 0 maps and id 1 does not exist.
        assert_eq!(t.get(HyperlinkId(0)), Some("https://example.com"));
        assert_eq!(t.get(HyperlinkId(1)), None);
    }

    #[test]
    fn distinct_urls_get_distinct_ids() {
        let mut t = HyperlinkTable::default();
        let a = t.intern("https://a/").unwrap();
        let b = t.intern("https://b/").unwrap();
        assert_ne!(a, b);
        assert_eq!(t.get(a), Some("https://a/"));
        assert_eq!(t.get(b), Some("https://b/"));
    }

    #[test]
    fn get_unknown_returns_none() {
        let t = HyperlinkTable::default();
        assert_eq!(t.get(HyperlinkId(0)), None);
    }
}
