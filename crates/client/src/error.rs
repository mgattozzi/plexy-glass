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
}
