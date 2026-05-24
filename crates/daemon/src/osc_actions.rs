//! Side-effecting handlers for OSC sequences: opening URLs, writing the
//! system clipboard, and synthesizing keystrokes for click-to-position.

use crate::error::DaemonError;
use bytes::Bytes;
use plexy_glass_emulator::PromptMarkKind;
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
/// CLIs in order; first available wins. Failure is logged once per session.
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
        let mut child = match Command::new(program)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => continue, // tool not present; try next
        };
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(payload).await.ok();
            drop(stdin);
        }
        let _ = child.wait().await;
        return Ok(());
    }

    tracing::warn!("no clipboard tool found (pbcopy/wl-copy/xclip/xsel)");
    Ok(())
}

/// Read the current system clipboard contents. Tries platform-appropriate
/// CLIs in order; first available wins. Returns an empty `Vec` if no tool is
/// available or the clipboard is empty.
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
        match Command::new(program)
            .args(*args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .await
        {
            Ok(out) if out.status.success() => return out.stdout,
            _ => continue,
        }
    }
    Vec::new()
}

/// Move the shell cursor in `pane` to local column `click_col` by
/// synthesizing arrow-key bytes. Only fires when the most recent OSC 133 'B'
/// (prompt-end) mark on the cursor's row sits at a column <= `click_col`.
/// Returns `Ok(false)` if no movement was performed; `Ok(true)` if arrow
/// keys were sent.
pub async fn click_to_position(pane: &crate::pane::Pane, click_col: u16) -> Result<bool, DaemonError> {
    let plan = pane.with_screen(|s| {
        let cursor = &s.cursor;
        let mark = s
            .prompt_marks
            .iter()
            .rev()
            .find(|m| m.kind == PromptMarkKind::PromptEnd && m.row == cursor.row)
            .copied();
        mark.and_then(|m| {
            if click_col < m.col {
                return None; // click is inside the prompt itself
            }
            let delta: i32 = i32::from(click_col) - i32::from(cursor.col);
            Some(delta)
        })
    });

    let Some(delta) = plan else {
        return Ok(false);
    };
    if delta == 0 {
        return Ok(true);
    }

    let arrow: &[u8] = if delta > 0 { b"\x1b[C" } else { b"\x1b[D" };
    let count = delta.unsigned_abs() as usize;
    let mut keystream = Vec::with_capacity(count * arrow.len());
    for _ in 0..count {
        keystream.extend_from_slice(arrow);
    }
    pane.send_input(Bytes::from(keystream)).await?;
    Ok(true)
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
    async fn click_to_position_emits_arrow_keys() {
        use crate::pane::Pane;
        use plexy_glass_emulator::PromptMark;
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
        let p = Pane::spawn(PaneId(0), spec, size, Arc::new(Notify::new()), None, cfg).unwrap();
        // Inject a PromptEnd mark at row 0 col 2 and put cursor at col 2.
        p.with_screen_mut(|s| {
            s.prompt_marks.push(PromptMark {
                kind: PromptMarkKind::PromptEnd,
                row: 0,
                col: 2,
            });
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
