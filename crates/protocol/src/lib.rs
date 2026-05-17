//! plexy-glass wire protocol: messages, framing, version handshake.
//!
//! No async runtime decisions here and no I/O policy, just types and a
//! length-prefixed codec that any tokio-compatible reader/writer can use.
