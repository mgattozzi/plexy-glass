use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DaemonError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not acquire daemon lockfile at {path}: {source}")]
    LockfileBusy {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("socket path {path} is owned by a different user; refusing to clobber")]
    SocketOwnedByOtherUser { path: std::path::PathBuf },
    #[error("protocol: {0}")]
    Protocol(#[from] plexy_glass_protocol::errors::CodecError),
    #[error("handshake: {0}")]
    Handshake(#[from] plexy_glass_protocol::HandshakeError),
    #[error("not yet implemented")]
    NotYetImplemented,
}
