use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors carried inside `ServerMsg::Error`. Clients can observe these,
/// so they are part of the wire surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[non_exhaustive]
pub enum ProtocolError {
    #[error("protocol version mismatch (client wanted {client}, server speaks {server})")]
    VersionMismatch { client: u16, server: u16 },
    #[error("failed to open PTY: {reason}")]
    PtyOpenFailed { reason: String },
    #[error("failed to spawn child: {reason}")]
    SpawnFailed { reason: String },
    #[error("unexpected message: {0}")]
    UnexpectedMessage(String),
    #[error("internal daemon error: {0}")]
    Internal(String),
    #[error("session not found: {name}")]
    SessionNotFound { name: String },
    #[error("session already exists: {name}")]
    SessionAlreadyExists { name: String },
    #[error("ambiguous attach: {count} sessions exist; specify -n NAME")]
    AmbiguousSession { count: u8 },
    #[error("session name is empty or too long")]
    EmptyName,
    #[error("session name has invalid characters: {name}")]
    InvalidName { name: String },
}

/// Errors that surface from the framing codec itself (not part of the wire).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CodecError {
    #[error("frame exceeds maximum size of {max} bytes (got {got})")]
    FrameTooLarge { max: u32, got: u32 },
    #[error("connection closed before full frame was read")]
    UnexpectedEof,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("postcard decode error: {0}")]
    Decode(String),
    #[error("postcard encode error: {0}")]
    Encode(String),
}

impl From<postcard::Error> for CodecError {
    fn from(err: postcard::Error) -> Self {
        CodecError::Decode(err.to_string())
    }
}
