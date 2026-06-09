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
/// host); a bare path passes through unchanged.
pub(crate) fn osc7_to_path(url: &str) -> Option<String> {
    match url.strip_prefix("file://") {
        Some(rest) => rest.find('/').map(|i| rest[i..].to_string()),
        None => Some(url.to_string()),
    }
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
}
