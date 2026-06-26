//! One-time onboarding marker. The welcome modal shows on a user's first ever
//! attach, gated by a `first-run` file in the state dir. Independent of session
//! state (the daemon no longer persists sessions); this is the only thing the
//! daemon keeps on disk besides logs.

use std::path::PathBuf;

/// Path to the once-ever "onboarding shown" marker, in the state root:
/// `$PLEXY_GLASS_DIR/first-run`, else `$XDG_STATE_HOME/plexy-glass/first-run`,
/// else `~/.local/state/plexy-glass/first-run`.
fn first_run_marker() -> PathBuf {
    if let Some(root) = std::env::var_os("PLEXY_GLASS_DIR") {
        return PathBuf::from(root).join("first-run");
    }
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(xdg).join("plexy-glass").join("first-run");
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".local/state/plexy-glass/first-run")
}

/// Returns `true` exactly once per state dir.
///
/// `true` when the marker was absent (and creates it), `false` thereafter. Used
/// to show the first-attach welcome modal to genuinely new users only.
/// Best-effort: if the write fails we return `false` so the modal never loops
/// forever on a read-only dir.
pub fn take_first_run() -> bool {
    let path = first_run_marker();
    if path.exists() {
        return false;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&path, b"plexy-glass first-run marker\n").is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::isolate;

    #[test]
    fn take_first_run_is_true_exactly_once() {
        let _g = isolate();
        // `isolate()` pre-writes the marker (to suppress the welcome modal in
        // attach-based tests); remove it to exercise the genuine first-run path.
        let _ = std::fs::remove_file(first_run_marker());
        assert!(take_first_run(), "fresh state dir is a first run");
        assert!(!take_first_run(), "marker written → no longer a first run");
        assert!(!take_first_run(), "stays false thereafter");
    }
}
