use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ClientError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("tty error: {0}")]
    Tty(String),
    #[error("could not connect to daemon at {path}: {source}")]
    Connect {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("handshake: {0}")]
    Handshake(#[from] plexy_glass_protocol::HandshakeError),
    #[error("codec: {0}")]
    Codec(#[from] plexy_glass_protocol::errors::CodecError),
    #[error("daemon reported error: {0}")]
    DaemonError(plexy_glass_protocol::errors::ProtocolError),
    #[error("not yet implemented")]
    NotYetImplemented,
}
