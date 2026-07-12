//! Streaming parallel per-host session query: query every roster host's
//! daemon for its session list in parallel, each bounded by a per-host
//! timeout, streaming each result back on an `mpsc` channel as it resolves so
//! the picker can fill rows in incrementally instead of waiting on the
//! slowest host.

use std::io;
use std::time::Duration;

use plexy_glass_protocol::{ClientMsg, HandshakeError, ServerMsg, SessionEntry};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::timeout;

use crate::error::ClientError;
use crate::transport::{Connect, InstallPolicy, Target};

/// The outcome of querying one host for its session list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostStatus {
    /// The daemon answered with at least one session.
    Live(Vec<SessionEntry>),
    /// The daemon answered but has no sessions running.
    Empty,
    /// Connect failure, timeout, or an unexpected reply.
    Unreachable,
    /// The remote daemon speaks an older protocol version (carried, for the
    /// picker to show a "run --install to upgrade" hint).
    VersionMismatch(u16),
}

/// Classify a single-host query result into a [`HostStatus`].
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

/// Query every host in `hosts` in parallel, each bounded by `per_host`, and
/// send `(host, HostStatus)` on `tx` as each one resolves — the picker can
/// fill rows in incrementally instead of waiting on the slowest host. A
/// connect failure or per-host timeout classifies as `HostStatus::Unreachable`.
///
/// Returns immediately; the query runs on a detached task that owns `tx` and
/// exits early if the receiver is dropped (the picker closed).
pub fn spawn_query(
    hosts: Vec<String>,
    per_host: Duration,
    tx: mpsc::UnboundedSender<(String, HostStatus)>,
) {
    tokio::spawn(async move {
        let mut set = JoinSet::new();
        for host in hosts {
            set.spawn(async move {
                let target = Target {
                    host: Some(host.clone()),
                    remote_bin: None,
                    install: InstallPolicy::UseExisting,
                };
                let res = match timeout(
                    per_host,
                    crate::request_reply(&target, Connect::Only, ClientMsg::ListSessions),
                )
                .await
                {
                    Ok(r) => r,
                    Err(_) => Err(ClientError::Io(io::Error::other("timeout"))),
                };
                (host, classify(res))
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
}
