//! The floating popup pane: a transient PTY-backed pane rendered centered
//! above the layout.
//!
//! See docs/superpowers/specs/2026-06-09-popup-panes-design.md.

use crate::pane::Pane;
use plexy_glass_protocol::PtySize;

pub struct Popup {
    /// The PTY-backed child.
    ///
    /// NOT in any window's layout tree; its `PaneId` is allocated from the same
    /// counter so the death channel keys on it.
    pub pane: Pane,
    /// Painted on the popup's top border: the command text, or "popup".
    pub title: String,
}

/// The PTY size for a popup whose OUTER box is `rect` (1-cell border on
/// each side).
///
/// Shared by spawn and host-resize so the two can't drift.
pub(crate) fn popup_pty_size(rect: plexy_glass_mux::Rect) -> PtySize {
    PtySize {
        rows: rect.rows.saturating_sub(2).max(1),
        cols: rect.cols.saturating_sub(2).max(1),
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// Convert an OSC-7 `file://host/path` URL into a filesystem path.
///
/// Mirrors the status bar's `CwdWidget` parsing (strip scheme + optional
/// host) and percent-decodes the path (shells encode e.g. spaces as `%20`);
/// a bare path passes through unchanged.
pub(crate) fn osc7_to_path(url: &str) -> Option<String> {
    match url.strip_prefix("file://") {
        Some(rest) => rest.find('/').map(|i| percent_decode(&rest[i..])),
        None => Some(url.to_string()),
    }
}

/// Decode `%XX` percent-escapes.
///
/// Invalid sequences (non-hex digits, a truncated `%` at end of input) pass
/// through unchanged. Decodes at the byte level so multi-byte UTF-8 escapes
/// (`%C3%A9` → `é`) reassemble correctly; any resulting invalid UTF-8 is
/// replaced lossily.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hi) = bytes.get(i + 1).and_then(|b| (*b as char).to_digit(16))
            && let Some(lo) = bytes.get(i + 2).and_then(|b| (*b as char).to_digit(16))
        {
            out.push((hi as u8) << 4 | lo as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc7_to_path_strips_scheme_and_host() {
        assert_eq!(osc7_to_path("file:///tmp/x").as_deref(), Some("/tmp/x"));
        assert_eq!(
            osc7_to_path("file://localhost/tmp/x").as_deref(),
            Some("/tmp/x")
        );
        assert_eq!(
            osc7_to_path("/already/a/path").as_deref(),
            Some("/already/a/path")
        );
        assert_eq!(osc7_to_path("file://nohostnopath"), None);
    }

    #[test]
    fn osc7_to_path_percent_decodes() {
        assert_eq!(
            osc7_to_path("file:///tmp/with%20space").as_deref(),
            Some("/tmp/with space")
        );
        // A multi-byte UTF-8 escape sequence reassembles.
        assert_eq!(
            osc7_to_path("file:///tmp/caf%C3%A9").as_deref(),
            Some("/tmp/café")
        );
        // Invalid hex passes through unchanged.
        assert_eq!(
            osc7_to_path("file:///tmp/bad%G1seq").as_deref(),
            Some("/tmp/bad%G1seq")
        );
        // A truncated escape at end of input passes through.
        assert_eq!(osc7_to_path("file:///tmp/x%2").as_deref(), Some("/tmp/x%2"));
        // No percent: unchanged.
        assert_eq!(osc7_to_path("file:///tmp/plain").as_deref(), Some("/tmp/plain"));
    }
}
