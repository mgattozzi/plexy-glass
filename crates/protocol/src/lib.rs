//! plexy-glass wire protocol.

pub mod errors;
pub mod messages;

pub use errors::{CodecError, ProtocolError};
pub use messages::{
    ClientHello, ClientMsg, ExitStatus, PROTOCOL_VERSION, PtySize, ServerHello, ServerMsg,
    SpawnSpec,
};
