use std::io;
use std::path::PathBuf;

use plexy_glass_protocol::errors::{CodecError, ProtocolError};
use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ClientError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("tty error: {0}")]
    Tty(String),
    #[error("could not connect to daemon at {path}: {source}")]
    Connect {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("handshake: {0}")]
    Handshake(#[from] plexy_glass_protocol::HandshakeError),
    #[error("codec: {0}")]
    Codec(#[from] CodecError),
    #[error("daemon reported error: {0}")]
    DaemonError(ProtocolError),
    #[error("daemon sent an unexpected reply")]
    UnexpectedReply,
    #[error("config reload error: {0}")]
    Reload(String),
    #[error(
        "no working remote `plexy-glass` on the host: tried PATH, ~/.cargo/bin, \
         ~/.local/bin and ~/.cache/plexy-glass/bin. Note ssh runs your login shell \
         NON-interactively, so a PATH set in an interactive rc (or in ~/.profile, \
         which nushell never reads) is not visible here — pass --remote-bin <path>, \
         or run with --install"
    )]
    RemoteNotFound,
    #[error("install: {0}")]
    Install(String),
    /// A remote daemon on a different protocol version. Distinct from the bare
    /// `Handshake` variant because this is the one skew with an actionable
    /// answer, and the action depends on `provisioned` — the fields are the API,
    /// `Display` just renders them for a human.
    #[error("{}", version_skew_advice(*peer, *ours, *provisioned))]
    RemoteVersionSkew {
        /// The remote daemon's protocol version.
        peer: u16,
        /// Ours.
        ours: u16,
        /// Whether this connection already ran `--install`. If it did and we
        /// STILL mismatch, the nightly is behind this build and re-running it
        /// cannot help — which is the advice everything used to give.
        provisioned: bool,
    },
}

/// What to tell someone whose remote daemon is on another protocol version.
///
/// The old advice, everywhere, was "run `--install`". That is right exactly once
/// and wrong the rest of the time. `--install` fetches the rolling `nightly`
/// release, so it can only ever move a remote to whatever last passed CI — and if
/// you are running a dev build that is AHEAD of the nightly (or CI is red, which
/// froze the nightly for two days recently), re-running it installs the same
/// too-old binary and reports success. Say which case you are in.
fn version_skew_advice(peer: u16, ours: u16, provisioned: bool) -> String {
    let head = format!("the remote daemon speaks protocol v{peer}, this client speaks v{ours}");
    if provisioned {
        format!(
            "{head}. --install already ran, so the nightly release is BEHIND this client and \
             cannot fix it: either point --remote-bin at a matching binary, or use a client \
             built from the same nightly"
        )
    } else {
        format!(
            "{head}. Reattach with --install to upgrade the remote (this restarts its daemon, \
             ending its sessions). If that leaves the versions still mismatched, the nightly \
             release is behind this client and only a matching binary will do"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two skews need DIFFERENT advice, which is the entire reason this
    /// variant exists rather than a bare `Handshake`. Telling someone to run
    /// `--install` when `--install` is what just failed them is how you get a
    /// user re-running a no-op and concluding the tool is broken.
    #[test]
    fn version_skew_advice_depends_on_whether_install_already_ran() {
        let fresh = ClientError::RemoteVersionSkew {
            peer: 12,
            ours: 13,
            provisioned: false,
        }
        .to_string();
        assert!(fresh.contains("v12") && fresh.contains("v13"), "{fresh}");
        assert!(
            fresh.contains("--install"),
            "an un-provisioned remote should be told to try it: {fresh}"
        );

        let already = ClientError::RemoteVersionSkew {
            peer: 12,
            ours: 13,
            provisioned: true,
        }
        .to_string();
        assert!(
            already.contains("BEHIND"),
            "if --install already ran and we still mismatch, the nightly is the \
             stale side and saying so is the whole point: {already}"
        );
        assert!(
            already.contains("--remote-bin"),
            "and there should be a way out: {already}"
        );
        assert_ne!(fresh, already, "the two cases must not read the same");
    }
}
