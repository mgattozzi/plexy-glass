use std::io;
use std::path::PathBuf;

use plexy_glass_protocol::errors::CodecError;
use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DaemonError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("could not acquire daemon lockfile at {path}: {source}")]
    LockfileBusy {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("socket path {path} is owned by a different user; refusing to clobber")]
    SocketOwnedByOtherUser { path: PathBuf },
    #[error("codec: {0}")]
    Codec(#[from] CodecError),
    #[error("protocol: {0}")]
    Protocol(#[from] plexy_glass_protocol::ProtocolError),
    #[error("handshake: {0}")]
    Handshake(#[from] plexy_glass_protocol::HandshakeError),
    #[error("config: {0}")]
    Config(#[from] plexy_glass_config::ConfigError),
}
