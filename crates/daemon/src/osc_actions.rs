//! Side-effecting handlers for OSC sequences: opening URLs, writing the
//! system clipboard, and synthesizing keystrokes for click-to-position.

use crate::error::DaemonError;
use bytes::Bytes;

use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Shell out to the system default URL opener. macOS: `open`. Linux:
/// `xdg-open`. Failure is logged at warn level and swallowed; the user
/// should see no panic / popup if the opener is missing.
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
            Ok(())
        }
    }
}

/// Write `payload` bytes to the system clipboard. Tries platform-appropriate
/// CLIs in order; first available wins. When no tool is found, a warning is
/// logged on every call; a tool that runs but fails is ignored silently.
pub async fn write_clipboard(payload: &[u8]) -> Result<(), DaemonError> {
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
            Ok(()) => return Ok(()),
            Err(_) => {
                tracing::warn!(program, "clipboard write timed out");
                return Ok(()); // child killed on drop; don't multiply the stall
            }
        }
    }

    tracing::warn!("no clipboard tool found (pbcopy/wl-copy/xclip/xsel)");
    Ok(())
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

    #[tokio::test]
    async fn open_url_returns_ok_even_when_opener_is_missing() {
        let r = open_url("about:blank").await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn write_clipboard_does_not_error_even_when_tool_missing() {
        let r = write_clipboard(b"hello").await;
        assert!(r.is_ok());
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
        let r = write_clipboard(b"hello").await;
        let elapsed = start.elapsed();
        assert!(r.is_ok());
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
