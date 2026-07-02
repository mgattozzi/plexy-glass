use plexy_glass_protocol::errors::{CodecError, ProtocolError};
use std::io;
use std::path::PathBuf;
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
    #[error("config reload error: {0}")]
    Reload(String),
}
