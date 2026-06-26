//! Side-effecting handlers for OSC sequences: opening URLs, writing the
//! system clipboard, and synthesizing keystrokes for click-to-position.

use crate::error::DaemonError;
use bytes::Bytes;

use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

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
        let child = match Command::new(program)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(_) => continue, // tool not present; try next
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
        match tokio::time::timeout(std::time::Duration::from_secs(2), write_and_wait).await {
            Ok(()) => return true,
            Err(_) => {
                tracing::warn!(program, "clipboard write timed out");
                return false; // child killed on drop; don't multiply the stall
            }
        }
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
        match tokio::time::timeout(std::time::Duration::from_secs(2), fut).await {
            Ok(Ok(out)) if out.status.success() => return out.stdout,
            Ok(_) => continue, // tool missing or non-zero: try the next one
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

/// Move the shell cursor in `pane` to local column `click_col` by
/// synthesizing arrow-key bytes. Only fires when the cursor's row carries a
/// `PROMPT_END` (`OSC 133;B`) mark whose column is <= `click_col`.
/// Returns `Ok(false)` if no movement was performed; `Ok(true)` if arrow
/// keys were sent.
pub async fn click_to_position(pane: &crate::pane::Pane, click_col: u16) -> Result<bool, DaemonError> {
    let plan = pane.with_screen(|s| {
        let cursor = &s.cursor;
        let row = s.active.rows.get(cursor.row as usize);
        let row_mark = row.map(|r| r.mark).unwrap_or_default();
        row_mark.prompt_end_col().and_then(|prompt_col| {
            if click_col < prompt_col {
                return None; // click is inside the prompt itself
            }
            // The shell's line editor moves one GRAPHEME per arrow press, but a
            // wide (CJK/emoji) grapheme spans two grid columns. Count real
            // grapheme cells (skipping wide spacers) in the column span so the
            // synthesized arrows land on the click target instead of
            // overshooting by one per wide char.
            let (lo, hi) = (cursor.col.min(click_col), cursor.col.max(click_col));
            let count = row
                .map(|r| graphemes_in_span(&r.cells, lo, hi))
                .unwrap_or(usize::from(hi - lo));
            Some((click_col > cursor.col, count))
        })
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

    #[tokio::test]
    async fn open_url_errors_when_no_opener_on_path() {
        // Stub PATH to an empty dir so `open`/`xdg-open` can't be spawned; the
        // caller relies on this Err to show "couldn't open (no system opener)".
        let dir = tempfile::tempdir().unwrap();
        let old = std::env::var("PATH").unwrap_or_default();
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("PATH", dir.path()) };
        let r = open_url("about:blank").await;
        unsafe { std::env::set_var("PATH", old) };
        assert!(r.is_err(), "no opener on PATH must surface as Err, not a silent Ok");
    }

    #[tokio::test]
    async fn write_clipboard_reports_false_when_no_tool() {
        // Empty PATH → no pbcopy/wl-copy/xclip/xsel → the write can't happen, and
        // the caller must learn that (so it warns instead of claiming "copied").
        let dir = tempfile::tempdir().unwrap();
        let old = std::env::var("PATH").unwrap_or_default();
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("PATH", dir.path()) };
        let wrote = write_clipboard(b"hello").await;
        unsafe { std::env::set_var("PATH", old) };
        assert!(!wrote, "no clipboard tool must report false");
    }

    #[tokio::test]
    async fn read_clipboard_returns_bounded_without_panic() {
        // No helper is guaranteed in the test env; whatever happens (empty,
        // real contents, or a missing tool) it must return quickly without panic.
        let start = std::time::Instant::now();
        let _ = read_clipboard().await;
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
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
            std::fs::write(&stub, "#!/bin/sh\nexec sleep 10\n").unwrap();
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let old_path = std::env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{old_path}", dir.path().display());
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("PATH", &new_path) };

        let start = std::time::Instant::now();
        let wrote = write_clipboard(b"hello").await;
        let elapsed = start.elapsed();
        assert!(!wrote, "a hung helper times out and reports false");
        assert!(
            elapsed < std::time::Duration::from_secs(5),
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

    #[tokio::test]
    async fn click_to_position_emits_arrow_keys() {
        use crate::pane::Pane;
        use plexy_glass_mux::PaneId;
        use plexy_glass_protocol::{PtySize, SpawnSpec};
        use std::sync::Arc;
        use tokio::sync::Notify;

        let size = PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
        let spec = SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };
        let cfg = Arc::new(plexy_glass_config::built_in_default());
        let p = Pane::spawn(PaneId(0), spec, size, Arc::new(Notify::new()), None, cfg, None).unwrap();
        // Inject a PROMPT_END mark at row 0 col 2 (via the row mark) and put
        // cursor at col 2.
        p.with_screen_mut(|s| {
            s.active.rows[0].mark.set_prompt_end(2);
            s.cursor.row = 0;
            s.cursor.col = 2;
        });
        // Click at col 6 → expect movement (4 right-arrow sequences).
        let moved = click_to_position(&p, 6).await.unwrap();
        assert!(moved);
        // Send EOF so cat exits cleanly.
        let _ = p.send_input(Bytes::from_static(&[0x04])).await;
    }
}
