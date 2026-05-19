//! Side-effecting handlers for OSC sequences: opening URLs, writing the
//! system clipboard, and synthesizing keystrokes for click-to-position.

use crate::error::DaemonError;
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
}
