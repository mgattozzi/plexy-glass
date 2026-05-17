//! plexy-glass wire protocol.

pub mod codec;
pub mod errors;
pub mod messages;

pub use codec::{Codec, MAX_FRAME_BYTES};
pub use errors::{CodecError, ProtocolError};
pub use messages::{
    ClientHello, ClientMsg, ExitStatus, PROTOCOL_VERSION, PtySize, ServerHello, ServerMsg,
    SpawnSpec,
};
