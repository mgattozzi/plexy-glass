//! Streaming parallel per-host session query: query every roster host's
//! daemon for its session list in parallel, each bounded by a per-host
//! timeout, streaming each result back on an `mpsc` channel as it resolves so
//! the picker can fill rows in incrementally instead of waiting on the
//! slowest host.

use std::io;
use std::time::Duration;

use plexy_glass_protocol::errors::CodecError;
use plexy_glass_protocol::{
    ClientMsg, Codec, HandshakeError, ServerMsg, SessionEntry, client_handshake,
};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::timeout;

use crate::error::ClientError;
use crate::transport::{Connect, Host, InstallPolicy, RemoteName, Target, open_transport};

/// The outcome of querying one host for its session list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostStatus {
    /// The daemon answered with at least one session.
    Live(Vec<SessionEntry>),
    /// The daemon answered but has no sessions running.
    Empty,
    /// Connect failure, timeout, or an unexpected reply — the host is genuinely
    /// not answering (down, no route, no daemon).
    Unreachable,
    /// The host is REACHABLE but the ssh connection couldn't authenticate
    /// non-interactively — a passphrase-only key, or a server-side check like
    /// Tailscale SSH's web auth. Distinct from `Unreachable` so the picker can
    /// say "press Enter to authenticate" instead of lying that the host is down;
    /// the interactive attach then runs ssh in cooked mode so the prompt (or a
    /// fresh Tailscale URL) lands normally.
    NeedsAuth,
    /// The remote daemon speaks an older protocol version (carried, for the
    /// picker to show a "run --install to upgrade" hint).
    VersionMismatch(u16),
}

/// Classify a single-host query result into a [`HostStatus`]. Used by the LOCAL
/// socket probe, which can't need auth or be an ssh failure — a remote probe
/// goes through [`probe_host`], which folds in captured ssh stderr.
pub fn classify(result: Result<ServerMsg, ClientError>) -> HostStatus {
    match result {
        Ok(ServerMsg::SessionList { entries }) if entries.is_empty() => HostStatus::Empty,
        Ok(ServerMsg::SessionList { entries }) => HostStatus::Live(entries),
        Err(ClientError::Handshake(HandshakeError::VersionMismatch { peer, .. })) => {
            HostStatus::VersionMismatch(peer)
        }
        _ => HostStatus::Unreachable,
    }
}

/// Classify a failed/timed-out remote probe's captured ssh stderr into
/// `NeedsAuth` vs `Unreachable`. ssh flattens every failure to exit 255, so the
/// exit code carries nothing — the signal is the message text. A transport that
/// CAME UP and then refused non-interactive auth or demanded an extra check is
/// reachable-but-locked → `NeedsAuth`; a connect-phase failure (the host never
/// came up) or nothing recognizable → `Unreachable`.
///
/// **Connect-phase markers win first**, because a bare `Permission denied` is
/// ALSO what OpenSSH prints for an EACCES `connect()` (a firewall REJECT with
/// `icmp-admin-prohibited`): `ssh: connect to host … : Permission denied`. That
/// host is genuinely unreachable, so the `connect to host` prefix short-circuits
/// to `Unreachable` before the auth check runs — otherwise we'd tell the user to
/// "press Enter to authenticate" a host that isn't there. The real auth-refusal
/// summary is always a parenthesized method list (`Permission denied
/// (publickey,…)`), so the auth arm matches the open paren, which the
/// connect-phase line never has.
///
/// A FIRST-CONNECT unknown host key (`Host key verification failed` without the
/// changed-key banner) is reachable and one interactive `yes` away, so it's
/// `NeedsAuth` too; a CHANGED key (`REMOTE HOST IDENTIFICATION HAS CHANGED`,
/// possible MITM) is not something "press Enter" can safely clear, so it stays
/// `Unreachable` — no worse than before.
///
/// This is a heuristic — it matches ssh/tailscale prose, which can shift across
/// versions and locales. The `Permission denied`, host-key, and connect-phase
/// strings were verified against a live OpenSSH `ssh -o BatchMode=yes`; the
/// Tailscale strings come from the check banner (`Tailscale SSH requires an
/// additional check. To authenticate, visit …`).
fn classify_ssh_stderr(stderr: &str) -> HostStatus {
    // Connect-phase failure: the host never came up. Wins even when the errno
    // string is "Permission denied" (EACCES) or "Connection refused".
    if stderr.contains("connect to host ") || stderr.contains("Could not resolve hostname") {
        return HostStatus::Unreachable;
    }
    let host_key_first_connect = stderr.contains("Host key verification failed")
        && !stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED");
    if stderr.contains("Permission denied (")
        || stderr.contains("requires an additional check")
        || stderr.contains("login.tailscale.com")
        || host_key_first_connect
    {
        HostStatus::NeedsAuth
    } else {
        HostStatus::Unreachable
    }
}

/// Probe one remote host for its session list over a `Connect::Probe` ssh
/// (BatchMode + captured stderr), bounded by `per_host`. A clean `SessionList`
/// answers `Live`/`Empty`; a version-skewed peer answers `VersionMismatch`;
/// anything else — a handshake/read error OR the per-host timeout — reaps ssh's
/// stderr and classifies it into `NeedsAuth`/`Unreachable`. Never spawns a
/// daemon and never prompts, so it's safe to fan out across the whole roster.
async fn probe_host(host: RemoteName, bin: Option<String>, per_host: Duration) -> HostStatus {
    let target = Target {
        host: Host::Remote(host),
        // A configured host with a `bin` must be probed with it, or the query
        // hits the same not-found a manual attach would.
        remote_bin: bin,
        install: InstallPolicy::UseExisting,
    };
    // The ssh child failed to even spawn (no `ssh` on PATH, say). Nothing to
    // diagnose; the host is effectively unreachable from here.
    let Ok(mut t) = open_transport(&target, Connect::Probe).await else {
        return HostStatus::Unreachable;
    };
    let exchange = async {
        client_handshake(&mut t.reader, &mut t.writer).await?;
        let payload = postcard::to_allocvec(&ClientMsg::ListSessions)
            .map_err(|e| CodecError::Encode(e.to_string()))?;
        Codec::write_frame(&mut t.writer, &payload).await?;
        let frame = Codec::read_frame(&mut t.reader)
            .await?
            .ok_or_else(|| ClientError::Io(io::Error::other("daemon closed before reply")))?;
        let msg: ServerMsg =
            postcard::from_bytes(&frame).map_err(|e| CodecError::Decode(e.to_string()))?;
        Ok::<ServerMsg, ClientError>(msg)
    };
    match timeout(per_host, exchange).await {
        Ok(Ok(ServerMsg::SessionList { entries })) if entries.is_empty() => HostStatus::Empty,
        Ok(Ok(ServerMsg::SessionList { entries })) => HostStatus::Live(entries),
        Ok(Err(ClientError::Handshake(HandshakeError::VersionMismatch { peer, .. }))) => {
            HostStatus::VersionMismatch(peer)
        }
        // A handshake/read error, an unexpected reply, or our per-host timeout:
        // reap whatever ssh wrote to stderr and let it say what went wrong.
        _ => classify_ssh_stderr(&t.probe_diagnose().await),
    }
}

/// Query every host in `hosts` in parallel, each bounded by `per_host`, and
/// send `(host, HostStatus)` on `tx` as each one resolves — the picker can
/// fill rows in incrementally instead of waiting on the slowest host. Each host
/// is probed non-interactively (`Connect::Probe`: BatchMode + captured stderr);
/// a clean answer is `Live`/`Empty`, and a failure or per-host timeout is
/// classified into `Unreachable` / `NeedsAuth` / `VersionMismatch` from ssh's
/// captured stderr (see [`probe_host`]).
///
/// Returns immediately; the query runs on a detached task that owns `tx` and
/// exits early if the receiver is dropped (the picker closed).
pub fn spawn_query(
    hosts: Vec<(RemoteName, Option<String>)>,
    per_host: Duration,
    tx: mpsc::UnboundedSender<(RemoteName, HostStatus)>,
) {
    tokio::spawn(async move {
        let mut set = JoinSet::new();
        for (host, bin) in hosts {
            set.spawn(async move {
                let status = probe_host(host.clone(), bin, per_host).await;
                (host, status)
            });
        }
        while let Some(joined) = set.join_next().await {
            if let Ok(pair) = joined
                && tx.send(pair).is_err()
            {
                break; // picker closed
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;

    fn sample_entry() -> SessionEntry {
        SessionEntry {
            name: "main".to_string(),
            windows: 1,
            panes: 1,
            clients: 0,
            created: SystemTime::now(),
            last_active: SystemTime::now(),
        }
    }

    #[test]
    fn classify_maps_results_to_status() {
        assert!(matches!(
            classify(Ok(ServerMsg::SessionList {
                entries: vec![sample_entry()]
            })),
            HostStatus::Live(_)
        ));
        assert_eq!(
            classify(Ok(ServerMsg::SessionList { entries: vec![] })),
            HostStatus::Empty
        );
        // a v11 remote vs v12 client surfaces as
        // ClientError::Handshake(HandshakeError::VersionMismatch{peer})
        let vm = ClientError::Handshake(HandshakeError::VersionMismatch { ours: 12, peer: 11 });
        assert_eq!(classify(Err(vm)), HostStatus::VersionMismatch(11));
        // any other error (connect fail, timeout, unexpected reply) → Unreachable
        assert_eq!(
            classify(Err(ClientError::Io(io::Error::other("x")))),
            HostStatus::Unreachable
        );
    }

    #[test]
    fn classify_ssh_stderr_splits_needs_auth_from_unreachable() {
        // Reachable but auth failed / needs a check → NeedsAuth. These are the
        // exact strings a live `ssh -o BatchMode=yes` and the Tailscale check
        // banner emit.
        for reachable in [
            "git@github.com: Permission denied (publickey).",
            "Tailscale SSH requires an additional check. To authenticate, visit: \
             https://login.tailscale.com/a/1bd2ecdb389ec0",
            "some preamble\nlogin.tailscale.com/a/abc\n",
            // First-connect unknown host key (BatchMode + StrictHostKeyChecking):
            // reachable, one interactive `yes` away.
            "No ED25519 host key is known for newhost.internal and you have \
             requested strict checking.\nHost key verification failed.",
        ] {
            assert_eq!(
                classify_ssh_stderr(reachable),
                HostStatus::NeedsAuth,
                "reachable-but-locked stderr should be NeedsAuth: {reachable:?}"
            );
        }

        // Connect-phase failures and nothing-recognizable → Unreachable.
        for dead in [
            "ssh: connect to host 192.0.2.1 port 22: Operation timed out",
            "ssh: Could not resolve hostname nope.invalid: nodename nor servname provided",
            "ssh: connect to host 127.0.0.1 port 1: Connection refused",
            // EACCES connect(): a firewall REJECT (icmp-admin-prohibited). The
            // errno string is "Permission denied", but it's a connect-phase
            // failure — the `connect to host` prefix must beat the auth arm.
            "ssh: connect to host 10.0.0.5 port 22: Permission denied",
            // Changed host key (possible MITM): reachable, but not something
            // "press Enter to authenticate" can clear — stays Unreachable.
            "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
             @    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\n\
             Host key verification failed.",
            "", // timed out with nothing on stderr
            "kex_exchange_identification: read: Connection reset by peer",
        ] {
            assert_eq!(
                classify_ssh_stderr(dead),
                HostStatus::Unreachable,
                "connect-phase / unknown stderr should be Unreachable: {dead:?}"
            );
        }
    }
}
