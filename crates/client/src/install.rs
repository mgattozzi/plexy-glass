//! `--install`: provision the remote `plexy-glass` from the `nightly` release,
//! local-download-then-push. Pure decision helpers here are unit-tested; the
//! effectful flow shells `ssh`/`curl`/`sha256sum`|`shasum` (no crate deps).

use std::process::Command as StdCommand;
use std::process::Stdio;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::error::ClientError;

/// The `~/.cache` path `--install` writes to and the SSH bridge falls back to.
pub const REMOTE_CACHE_BIN: &str = "~/.cache/plexy-glass/bin/plexy-glass";

/// Base URL for the rolling nightly release assets.
const NIGHTLY_BASE: &str = "https://github.com/mgattozzi/plexy-glass/releases/download/nightly";

/// Map `uname -s` / `uname -m` to a Rust target triple, or `None` if
/// unsupported. Linux → static musl; Darwin → apple-darwin.
pub fn uname_to_triple(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("Linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        ("Linux", "aarch64" | "arm64") => Some("aarch64-unknown-linux-musl"),
        ("Darwin", "x86_64") => Some("x86_64-apple-darwin"),
        ("Darwin", "arm64") => Some("aarch64-apple-darwin"),
        _ => None,
    }
}

/// Whether to (re)install: true unless the remote's cached binary already has
/// the expected checksum.
#[must_use]
pub fn install_needed(remote_sha: Option<&str>, expected_sha: &str) -> bool {
    remote_sha != Some(expected_sha)
}

/// Extract the checksum for `filename` from a `SHA256SUMS` body
/// (`<hex>␠␠<name>` lines).
pub fn sha_for<'a>(sums: &'a str, filename: &str) -> Option<&'a str> {
    sums.lines().find_map(|l| {
        let (hex, name) = l.split_once("  ")?;
        (name.trim() == filename).then_some(hex.trim())
    })
}

/// Parse the batched remote probe: line 1 = `uname -sm` ("Linux x86_64"),
/// line 2 (optional) = the cached binary's checksum (first field).
pub fn parse_probe(output: &str) -> Option<(String, String, Option<String>)> {
    let mut lines = output.lines();
    let mut uname = lines.next()?.split_whitespace();
    let os = uname.next()?.to_string();
    let arch = uname.next()?.to_string();
    let sha = lines
        .next()
        .and_then(|l| l.split_whitespace().next())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some((os, arch, sha))
}

/// Provision `REMOTE_CACHE_BIN` on `host` from the nightly release, idempotently.
pub async fn install_remote(host: &str) -> Result<(), ClientError> {
    // 1. One SSH call: uname + the cached binary's checksum (if any).
    // `cut -c1-64` (the sha256 hex is 64 chars) not `cut -d' '` — the script
    // must contain no single quote (see `remote_sh`).
    let probe = ssh_capture(
        host,
        &format!(
            "uname -sm; (sha256sum {REMOTE_CACHE_BIN} 2>/dev/null || shasum -a 256 {REMOTE_CACHE_BIN} 2>/dev/null) | cut -c1-64"
        ),
    )
    .await?;
    let (os, arch, remote_sha) = parse_probe(&probe)
        .ok_or_else(|| ClientError::Install("could not read remote uname".into()))?;
    let triple = uname_to_triple(&os, &arch).ok_or_else(|| {
        ClientError::Install(format!(
            "unsupported remote platform `{os} {arch}`; use --remote-bin"
        ))
    })?;
    let asset = format!("plexy-glass-{triple}");

    // 2. Expected checksum from SHA256SUMS.
    let sums = curl(&format!("{NIGHTLY_BASE}/SHA256SUMS")).await?;
    let sums = String::from_utf8_lossy(&sums).into_owned();
    let expected = sha_for(&sums, &asset)
        .ok_or_else(|| ClientError::Install(format!("no nightly artifact for {triple}")))?;

    // 3. Idempotent.
    if !install_needed(remote_sha.as_deref(), expected) {
        return Ok(());
    }

    // 4. Download + verify LOCALLY.
    let bytes = curl(&format!("{NIGHTLY_BASE}/{asset}")).await?;
    let got = sha256_hex(&bytes).await?;
    if got != expected {
        return Err(ClientError::Install(format!(
            "checksum mismatch for {asset} (expected {expected}, got {got}); refusing to install"
        )));
    }

    // 5. Push over SSH (binary streamed on stdin).
    ssh_push(host, &bytes).await?;
    Ok(())
}

/// Whether a local program exists (for the sha256sum/shasum fallback).
fn have(prog: &str) -> bool {
    StdCommand::new(prog)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Wrap `script` as ONE ssh command argument that runs it under POSIX `sh` on the
/// remote, whatever the remote LOGIN shell is. `ssh host a b c` joins its command
/// argv with spaces into one string the login shell re-parses, and that shell may
/// be nushell/fish, not POSIX sh. Single-quoting the whole script makes every
/// login shell we care about (POSIX sh, nushell, fish) pass it verbatim to
/// `sh -c`. The script MUST contain no single quote: portable single-quote
/// escaping across those shells doesn't exist (POSIX `'\''` mis-parses under
/// nushell), so every caller here uses a quote-free script. This keeps the login
/// shell out of the STDIN byte path, so a command run this way reads a clean
/// stdin from the start and `ssh_push` can stream the raw binary through it. An
/// `sh -s` script fed on stdin fails instead: shells that block-buffer the `-s`
/// read swallow trailing binary bytes into the buffer. Shared with the transport,
/// which wraps its PATH-then-cache-path bridge fallback the same way.
pub(crate) fn remote_sh(script: &str) -> String {
    debug_assert!(
        !script.contains('\''),
        "remote script must contain no single quote"
    );
    format!("sh -c '{script}'")
}

/// `ssh <host> "sh -c '<script>'"`, returning stdout (stderr inherited so SSH's
/// own prompts/errors reach the user). No stdin — the probe only runs commands.
async fn ssh_capture(host: &str, script: &str) -> Result<String, ClientError> {
    let out = Command::new("ssh")
        .arg(host)
        .arg(remote_sh(script))
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .await
        .map_err(ClientError::Io)?;
    if !out.status.success() {
        return Err(ClientError::Install(format!(
            "ssh {host}: remote query failed"
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `curl -fsSL <url>` on the LOCAL machine, returning the body bytes.
async fn curl(url: &str) -> Result<Vec<u8>, ClientError> {
    let out = Command::new("curl")
        .arg("-fsSL")
        .arg(url)
        .output()
        .await
        .map_err(ClientError::Io)?;
    if !out.status.success() {
        return Err(ClientError::Install(format!("download failed: {url}")));
    }
    Ok(out.stdout)
}

/// Hex SHA-256 of `bytes`, via `sha256sum` or (macOS) `shasum -a 256`.
async fn sha256_hex(bytes: &[u8]) -> Result<String, ClientError> {
    let (prog, args): (&str, &[&str]) = if have("sha256sum") {
        ("sha256sum", &[])
    } else {
        ("shasum", &["-a", "256"])
    };
    let mut child = Command::new(prog)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(ClientError::Io)?;
    // invariant: stdin piped above.
    let mut stdin = child.stdin.take().expect("hasher stdin piped");
    stdin.write_all(bytes).await.map_err(ClientError::Io)?;
    drop(stdin); // EOF so the hasher finishes
    let out = child.wait_with_output().await.map_err(ClientError::Io)?;
    if !out.status.success() {
        return Err(ClientError::Install("local checksum tool failed".into()));
    }
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .map(str::to_string)
        .ok_or_else(|| ClientError::Install("empty checksum output".into()))
}

/// Install `bytes` as the remote cache binary in one SSH call. The mkdir, `cat >
/// file`, and chmod script rides the argv via `remote_sh`, so the binary is the
/// only thing on stdin and `cat` reads it clean from the start. Feeding the
/// script itself on stdin via `sh -s` instead fails on shells that block-buffer
/// the `-s` read: they swallow trailing binary bytes into the script buffer and
/// truncate the file. This needs no base64/scp; nushell and sh forward the raw
/// stdin through to `cat` intact.
async fn ssh_push(host: &str, bytes: &[u8]) -> Result<(), ClientError> {
    let script = format!(
        "mkdir -p ~/.cache/plexy-glass/bin && cat > {REMOTE_CACHE_BIN} && chmod +x {REMOTE_CACHE_BIN}"
    );
    let mut child = Command::new("ssh")
        .arg(host)
        .arg(remote_sh(&script))
        .stdin(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(ClientError::Io)?;
    // invariant: stdin piped above.
    let mut stdin = child.stdin.take().expect("ssh push stdin piped");
    stdin.write_all(bytes).await.map_err(ClientError::Io)?;
    drop(stdin);
    if !child.wait().await.map_err(ClientError::Io)?.success() {
        return Err(ClientError::Install(format!("push to {host} failed")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_sh_single_quote_wraps_for_sh_c() {
        assert_eq!(remote_sh("uname -sm"), "sh -c 'uname -sm'");
        // The real scripts (pipes, redirects, &&) stay literal inside the quotes
        // — the remote login shell hands the whole thing to `sh -c`.
        assert_eq!(
            remote_sh("a 2>/dev/null | cut -c1-64 && b"),
            "sh -c 'a 2>/dev/null | cut -c1-64 && b'"
        );
    }

    #[test]
    fn triple_map_covers_supported_pairs_and_rejects_others() {
        assert_eq!(
            uname_to_triple("Linux", "x86_64"),
            Some("x86_64-unknown-linux-musl")
        );
        assert_eq!(
            uname_to_triple("Linux", "aarch64"),
            Some("aarch64-unknown-linux-musl")
        );
        assert_eq!(
            uname_to_triple("Darwin", "x86_64"),
            Some("x86_64-apple-darwin")
        );
        assert_eq!(
            uname_to_triple("Darwin", "arm64"),
            Some("aarch64-apple-darwin")
        );
        assert_eq!(uname_to_triple("Linux", "riscv64"), None);
        assert_eq!(uname_to_triple("FreeBSD", "x86_64"), None);
    }

    #[test]
    fn install_needed_skips_only_on_exact_match() {
        assert!(!install_needed(Some("abc"), "abc"));
        assert!(install_needed(Some("abc"), "def"));
        assert!(install_needed(None, "abc"));
    }

    #[test]
    fn sha_for_finds_the_named_asset() {
        let sums = "aaa  plexy-glass-x86_64-unknown-linux-musl\nbbb  SHA256SUMS\n";
        assert_eq!(
            sha_for(sums, "plexy-glass-x86_64-unknown-linux-musl"),
            Some("aaa")
        );
        assert_eq!(sha_for(sums, "nope"), None);
    }

    #[test]
    fn parse_probe_reads_uname_and_optional_sha() {
        assert_eq!(
            parse_probe("Linux x86_64\ndeadbeef\n"),
            Some(("Linux".into(), "x86_64".into(), Some("deadbeef".into())))
        );
        // No cached binary → no sha line.
        assert_eq!(
            parse_probe("Darwin arm64\n"),
            Some(("Darwin".into(), "arm64".into(), None))
        );
    }
}
