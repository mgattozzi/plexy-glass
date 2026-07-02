//! Side-effecting handlers for OSC sequences: opening URLs, writing the
//! system clipboard, and synthesizing keystrokes for click-to-position.

use crate::error::DaemonError;
use crate::pane::Pane;
use crate::window_manager::Severity;
use bytes::Bytes;

use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time;

/// Build the user-facing acknowledgement for a clipboard write. A multi-line
/// copy reports the line count; a single line reports the width-truncated text.
/// Returns no leading glyph, the `Success` severity prepends `✓` at paint time.
pub(crate) fn copied_message(text: &str) -> String {
    let line_count = text.lines().count();
    if line_count > 1 {
        format!("copied {line_count} lines")
    } else {
        let one = text.trim_end_matches(['\n', '\r']);
        let shown = plexy_glass_emulator::truncate_to_width(one, 40);
        if shown.len() < one.len() {
            format!("copied \"{shown}…\"")
        } else {
            format!("copied \"{one}\"")
        }
    }
}

/// Pick the status message + severity for a clipboard yank given whether the OS
/// clipboard write actually landed. On failure the message is honest
/// ("clipboard unavailable") instead of a false "copied"; when the text was also
/// pushed to a paste buffer (`paste_fallback`), it points the user at `Ctrl+a ]`.
/// Shared by every yank site (copy-mode Enter, block-mode, copy-mode mouse,
/// mouse-drag release) so the honesty is decided in one tested place.
pub(crate) fn yank_status(
    wrote: bool,
    text: &str,
    paste_fallback: bool,
) -> (String, Severity) {
    use crate::window_manager::Severity;
    if wrote {
        (copied_message(text), Severity::Success)
    } else if paste_fallback {
        (
            "clipboard unavailable — paste with Ctrl+a ]".to_string(),
            Severity::Warn,
        )
    } else {
        ("clipboard unavailable".to_string(), Severity::Warn)
    }
}

/// Shell out to the system default URL opener. macOS: `open`. Linux:
/// `xdg-open`. Returns `Err` when the opener binary can't be spawned (e.g. a
/// headless box with no `xdg-open`), so the caller can surface an honest "no
/// system opener" message instead of a silent no-op. Note that a successful
/// spawn only means the opener launched, it does not guarantee the URL opened.
pub async fn open_url(url: &str) -> Result<(), DaemonError> {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(target_os = "macos"))]
    let program = "xdg-open";

    let result = Command::new(program)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();

    match result {
        Ok(_child) => Ok(()),
        Err(e) => {
            tracing::warn!(error = %e, %url, "failed to invoke URL opener");
            Err(DaemonError::Io(e))
        }
    }
}

/// Write `payload` bytes to the system clipboard. Tries platform-appropriate
/// CLIs in order; first available wins. Returns `true` only when a tool actually
/// completed the write, `false` when none is present or the write timed out, so
/// the caller can honestly distinguish "copied" from "couldn't reach the
/// clipboard" (the content is still pushed to the in-app paste buffer either way).
pub async fn write_clipboard(payload: &[u8]) -> bool {
    let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else {
        &[
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ]
    };

    for (program, args) in candidates {
        let Ok(child) = Command::new(program)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
        else {
            continue; // tool not present; try next
        };
        // Bounded like read_clipboard: a wedged helper (stuck pbcopy/xclip) must
        // not stall the caller, since several sites await this directly in the
        // per-connection serve loop. kill_on_drop reaps the child on timeout.
        let write_and_wait = async move {
            let mut child = child;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(payload).await.ok();
                drop(stdin);
            }
            let _ = child.wait().await;
        };
        if time::timeout(Duration::from_secs(2), write_and_wait).await == Ok(()) { return true }
        tracing::warn!(program, "clipboard write timed out");
        return false; // child killed on drop; don't multiply the stall
    }

    tracing::warn!("no clipboard tool found (pbcopy/wl-copy/xclip/xsel)");
    false
}

/// Read the current system clipboard contents. Tries platform-appropriate
/// CLIs in order; first available wins. Returns an empty `Vec` if no tool is
/// available, the clipboard is empty, or a tool hangs past the timeout.
///
/// Each invocation is bounded by a 2s timeout with `kill_on_drop`: a wedged
/// clipboard helper (e.g. a stuck `pbpaste`/`xclip`) must not stall the
/// caller. This matters because middle-click paste runs this while the
/// window-manager lock is held, and an unbounded hang would block session
/// teardown (`kill`) and client detach.
pub async fn read_clipboard() -> Vec<u8> {
    let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbpaste", &[])]
    } else {
        &[
            ("wl-paste", &["-n"]),
            ("xclip", &["-selection", "clipboard", "-o"]),
            ("xsel", &["--clipboard", "--output"]),
        ]
    };
    for (program, args) in candidates {
        let fut = Command::new(program)
            .args(*args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output();
        match time::timeout(Duration::from_secs(2), fut).await {
            Ok(Ok(out)) if out.status.success() => return out.stdout,
            Ok(_) => {} // tool missing or non-zero: try the next one
            Err(_) => {
                // Timed out; the child is killed on drop. Don't try others, a wedged
                // clipboard system shouldn't multiply the stall.
                tracing::warn!(program, "clipboard read timed out; skipping paste");
                return Vec::new();
            }
        }
    }
    Vec::new()
}

/// Reposition the shell cursor to the clicked cell by synthesizing arrow-key
/// bytes, Ghostty-style cursor-click-to-move. `click_row`/`click_col` are
/// pane-local (viewport == live grid; the caller only invokes this on the live,
/// unscrolled view).
///
/// Fires only when the click lands on the cursor's OWN row of the primary
/// screen (the editable line), so a click on output, scrollback, or inside a
/// full-screen (alt-screen) application never injects stray arrows. An
/// `OSC 133;B` (`PROMPT_END`) mark, when present, is honored as a FLOOR so a
/// click in the prompt prefix can't drag the cursor backwards into it, but it
/// is NOT required: most shells running inside a multiplexer never emit it
/// (the outer terminal's shell integration injects only into the shell it
/// spawns directly), and the heuristic matches what a bare terminal does.
///
/// Returns `Ok(false)` if no movement was performed; `Ok(true)` if the click
/// was consumed as a reposition (arrow keys sent, or already on target).
pub async fn click_to_position(
    pane: &Pane,
    click_row: u16,
    click_col: u16,
) -> Result<bool, DaemonError> {
    let plan = pane.with_screen(|s| {
        // Only the cursor's own row is the editable line.
        if click_row != s.cursor.row {
            return None;
        }
        // Never move the cursor of a full-screen app; it owns the grid.
        if s.modes.contains(plexy_glass_emulator::Modes::ALT_SCREEN) {
            return None;
        }
        let cursor = &s.cursor;
        let row = s.active.rows.get(cursor.row as usize);
        // PROMPT_END is a floor, not a gate: refuse clicks in the prompt prefix
        // when we know where input starts, but still fire when it's absent.
        if let Some(prompt_col) = row.and_then(|r| r.mark.prompt_end_col())
            && click_col < prompt_col
        {
            return None;
        }
        // The line editor moves one GRAPHEME per arrow press, but a wide
        // (CJK/emoji) grapheme spans two grid columns. Count real grapheme cells
        // (skipping wide spacers) so the arrows land on the click target instead
        // of overshooting by one per wide char.
        let (lo, hi) = (cursor.col.min(click_col), cursor.col.max(click_col));
        let count = row
            .map_or_else(|| usize::from(hi - lo), |r| graphemes_in_span(&r.cells, lo, hi));
        Some((click_col > cursor.col, count))
    });

    let Some((rightward, count)) = plan else {
        return Ok(false);
    };
    if count == 0 {
        return Ok(true);
    }

    let arrow: &[u8] = if rightward { b"\x1b[C" } else { b"\x1b[D" };
    pane.send_input(Bytes::from(arrow.repeat(count))).await?;
    Ok(true)
}

/// Count grapheme cells (skipping wide spacers) in the half-open column span
/// `[lo, hi)` of a row, i.e. the number of one-grapheme-per-press cursor moves
/// a shell line editor makes across that span.
fn graphemes_in_span(cells: &[plexy_glass_emulator::Cell], lo: u16, hi: u16) -> usize {
    cells
        .iter()
        .enumerate()
        .filter(|&(c, cell)| {
            let col = c as u16;
            col >= lo && col < hi && !cell.is_wide_spacer()
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs;
    use std::time::Instant;

    #[test]
    fn copied_message_reports_lines_or_truncated_text() {
        assert_eq!(copied_message("one line"), "copied \"one line\"");
        // Trailing newline on a single line is not counted as a second line.
        assert_eq!(copied_message("one line\n"), "copied \"one line\"");
        assert_eq!(copied_message("a\nb\nc"), "copied 3 lines");
        // Long single line is width-truncated with an ellipsis.
        let long = "x".repeat(80);
        let msg = copied_message(&long);
        assert!(msg.starts_with("copied \"") && msg.ends_with("…\""));
        assert!(msg.len() < long.len() + 12, "should be truncated");
    }

    #[test]
    fn yank_status_is_honest_about_clipboard_failure() {
        use crate::window_manager::Severity;
        // Success: reports the copied text regardless of paste_fallback.
        let (msg, sev) = yank_status(true, "hi", true);
        assert_eq!(sev, Severity::Success);
        assert_eq!(msg, "copied \"hi\"");
        // Failure with a paste-buffer fallback: warn + point at Ctrl+a ].
        let (msg, sev) = yank_status(false, "hi", true);
        assert_eq!(sev, Severity::Warn);
        assert_eq!(msg, "clipboard unavailable — paste with Ctrl+a ]");
        // Failure with no fallback (mouse paths): warn, no false paste promise.
        let (msg, sev) = yank_status(false, "hi", false);
        assert_eq!(sev, Severity::Warn);
        assert_eq!(msg, "clipboard unavailable");
    }

    #[tokio::test]
    async fn open_url_errors_when_no_opener_on_path() {
        // Stub PATH to an empty dir so `open`/`xdg-open` can't be spawned; the
        // caller relies on this Err to show "couldn't open (no system opener)".
        let dir = tempfile::tempdir().unwrap();
        let old = env::var("PATH").unwrap_or_default();
        // SAFETY: nextest runs each test in its own process.
        unsafe { env::set_var("PATH", dir.path()) };
        let r = open_url("about:blank").await;
        unsafe { env::set_var("PATH", old) };
        assert!(r.is_err(), "no opener on PATH must surface as Err, not a silent Ok");
    }

    #[tokio::test]
    async fn write_clipboard_reports_false_when_no_tool() {
        // Empty PATH → no pbcopy/wl-copy/xclip/xsel → the write can't happen, and
        // the caller must learn that (so it warns instead of claiming "copied").
        let dir = tempfile::tempdir().unwrap();
        let old = env::var("PATH").unwrap_or_default();
        // SAFETY: nextest runs each test in its own process.
        unsafe { env::set_var("PATH", dir.path()) };
        let wrote = write_clipboard(b"hello").await;
        unsafe { env::set_var("PATH", old) };
        assert!(!wrote, "no clipboard tool must report false");
    }

    #[tokio::test]
    async fn read_clipboard_returns_bounded_without_panic() {
        // No helper is guaranteed in the test env; whatever happens (empty,
        // real contents, or a missing tool) it must return quickly without panic.
        let start = Instant::now();
        let _ = read_clipboard().await;
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "read_clipboard must be bounded"
        );
    }

    #[tokio::test]
    async fn write_clipboard_is_bounded_when_helper_hangs() {
        use std::os::unix::fs::PermissionsExt;
        // Stub every clipboard helper to hang well past the 2s bound; `exec` so
        // the sleeper is the direct child (kill_on_drop reaps it). Whichever the
        // current OS tries first is the hanging stub.
        let dir = tempfile::tempdir().unwrap();
        for name in ["pbcopy", "wl-copy", "xclip", "xsel"] {
            let stub = dir.path().join(name);
            fs::write(&stub, "#!/bin/sh\nexec sleep 10\n").unwrap();
            fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let old_path = env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{old_path}", dir.path().display());
        // SAFETY: nextest runs each test in its own process.
        unsafe { env::set_var("PATH", &new_path) };

        let start = Instant::now();
        let wrote = write_clipboard(b"hello").await;
        let elapsed = start.elapsed();
        assert!(!wrote, "a hung helper times out and reports false");
        assert!(
            elapsed < Duration::from_secs(5),
            "write_clipboard must be bounded by the timeout, took {elapsed:?}"
        );
    }

    #[test]
    fn graphemes_in_span_counts_one_per_grapheme_not_per_column() {
        use plexy_glass_emulator::Cell;
        let cellch = |s: &str| Cell {
            grapheme: s.into(),
            ..Cell::default()
        };
        // Row: "a" "あ"(wide)+spacer "b" "c"  -> columns 0,1,2,3,4
        let cells = vec![
            cellch("a"),
            cellch("あ"),
            Cell::wide_spacer(),
            cellch("b"),
            cellch("c"),
        ];
        // ASCII-only span [0,1): one grapheme.
        assert_eq!(graphemes_in_span(&cells, 0, 1), 1);
        // Span [0,3) covers "a" + "あ"(+spacer): two graphemes, not three columns.
        assert_eq!(graphemes_in_span(&cells, 0, 3), 2);
        // Span [0,5) covers a あ b c: four graphemes across five columns.
        assert_eq!(graphemes_in_span(&cells, 0, 5), 4);
    }

    fn cat_pane() -> Pane {
        use crate::pane::Pane;
        use plexy_glass_mux::PaneId;
        use plexy_glass_protocol::{PtySize, SpawnSpec};
        use std::sync::Arc;
        use tokio::sync::Notify;
        let size = PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
        let spec = SpawnSpec { program: "/bin/cat".into(), args: vec![], env: vec![], cwd: None };
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        Pane::spawn(PaneId(0), spec, size, Arc::new(Notify::new()), None, cfg).unwrap()
    }

    #[tokio::test]
    async fn click_to_position_emits_arrow_keys() {
        let p = cat_pane();
        // PROMPT_END at row 0 col 2; cursor at col 2; click the same row at col 6.
        p.with_screen_mut(|s| {
            s.active.rows[0].mark.set_prompt_end(2);
            s.cursor.row = 0;
            s.cursor.col = 2;
        });
        assert!(click_to_position(&p, 0, 6).await.unwrap());
        let _ = p.send_input(Bytes::from_static(&[0x04])).await; // EOF
    }

    #[tokio::test]
    async fn click_to_position_fires_without_a_prompt_mark() {
        // The fix: a shell with no OSC 133 integration (no PROMPT_END mark) must
        // STILL reposition on a click on the cursor's row, matching a bare
        // terminal. Regression guard for the dead-on-non-integrated-shell bug.
        let p = cat_pane();
        p.with_screen_mut(|s| {
            s.cursor.row = 5;
            s.cursor.col = 8;
        });
        assert!(
            click_to_position(&p, 5, 4).await.unwrap(),
            "click on the cursor row must reposition even without OSC 133;B"
        );
        let _ = p.send_input(Bytes::from_static(&[0x04])).await;
    }

    #[tokio::test]
    async fn click_to_position_ignores_clicks_off_the_cursor_row() {
        let p = cat_pane();
        p.with_screen_mut(|s| {
            s.cursor.row = 5;
            s.cursor.col = 8;
        });
        // Click on row 3 (not the cursor's row 5) → not the editable line.
        assert!(!click_to_position(&p, 3, 4).await.unwrap());
        let _ = p.send_input(Bytes::from_static(&[0x04])).await;
    }

    #[tokio::test]
    async fn click_to_position_skips_alt_screen() {
        let p = cat_pane();
        p.with_screen_mut(|s| {
            s.modes.insert(plexy_glass_emulator::Modes::ALT_SCREEN);
            s.cursor.row = 5;
            s.cursor.col = 8;
        });
        assert!(
            !click_to_position(&p, 5, 4).await.unwrap(),
            "must not inject arrows into a full-screen application"
        );
        let _ = p.send_input(Bytes::from_static(&[0x04])).await;
    }

    #[tokio::test]
    async fn click_to_position_respects_the_prompt_prefix_floor() {
        let p = cat_pane();
        p.with_screen_mut(|s| {
            s.active.rows[0].mark.set_prompt_end(4);
            s.cursor.row = 0;
            s.cursor.col = 7;
        });
        // Click at col 2 (< prompt input col 4) → refuse, it's the prompt prefix.
        assert!(!click_to_position(&p, 0, 2).await.unwrap());
        // Click at col 5 (>= 4) → reposition.
        assert!(click_to_position(&p, 0, 5).await.unwrap());
        let _ = p.send_input(Bytes::from_static(&[0x04])).await;
    }
}
