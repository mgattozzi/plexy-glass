//! plexy-glass wire protocol.

pub mod codec;
pub mod errors;
pub mod handshake;
pub mod messages;

pub use codec::{Codec, MAX_FRAME_BYTES};
pub use errors::{CodecError, ProtocolError};
pub use handshake::{HandshakeError, client_handshake, client_handshake_with, server_handshake};
pub use messages::{
    ClientHello, ClientMsg, ColorScheme, ExitStatus, GraphicsCaps, NegotiatedKbd, PROTOCOL_VERSION,
    PtySize, ServerHello, ServerMsg, SessionEntry, SpawnSpec,
};
