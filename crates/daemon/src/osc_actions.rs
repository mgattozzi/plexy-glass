//! Side-effecting handlers for OSC sequences: opening URLs, writing the
//! system clipboard, and synthesizing keystrokes for click-to-position.

use crate::error::DaemonError;
use std::process::Stdio;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_url_returns_ok_even_when_opener_is_missing() {
        let r = open_url("about:blank").await;
        assert!(r.is_ok());
    }
}
