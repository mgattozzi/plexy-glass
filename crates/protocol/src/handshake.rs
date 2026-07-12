use std::env;

use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    ClientHello, Codec, CodecError, GraphicsCaps, NegotiatedKbd, PROTOCOL_VERSION, ProtocolError,
    ProtocolVersion, ServerHello,
};

/// Errors that can occur during the version handshake.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HandshakeError {
    #[error(transparent)]
    Codec(#[from] CodecError),
    #[error("peer hung up before handshake completed")]
    PeerClosed,
    #[error("peer speaks protocol version {peer}, we speak {ours}")]
    VersionMismatch { ours: u16, peer: u16 },
}

/// Client handshake carrying the negotiated outer-terminal capabilities.
///
/// Returns the server's hello once versions are confirmed compatible.
pub async fn client_handshake_with<R, W>(
    reader: &mut R,
    writer: &mut W,
    hello: ClientHello,
) -> Result<ServerHello, HandshakeError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let bytes = postcard::to_allocvec(&hello).map_err(|e| CodecError::Encode(e.to_string()))?;
    Codec::write_frame(writer, &bytes).await?;

    let frame = Codec::read_frame(reader)
        .await?
        .ok_or(HandshakeError::PeerClosed)?;
    let server: ServerHello =
        postcard::from_bytes(&frame).map_err(|e| CodecError::Decode(e.to_string()))?;
    // Accept any server whose version is >= ours (we speak our older subset;
    // a newer server can always serve an older client's request shape).
    // Reject only an OLDER server: it cannot have produced a hello our newer
    // code can rely on, and there is no forward-compat guarantee in that
    // direction. (The ServerHello itself already decoded above; a genuinely
    // incompatible wire layout surfaces as HandshakeError::Codec, a clean
    // failure, before this check.)
    if server.version < PROTOCOL_VERSION {
        return Err(HandshakeError::VersionMismatch {
            ours: u16::from(PROTOCOL_VERSION),
            peer: u16::from(server.version),
        });
    }
    Ok(server)
}

/// Convenience handshake for non-interactive subcommands (list/kill/reload):
/// advertises `$TERM` and legacy keyboard.
pub async fn client_handshake<R, W>(
    reader: &mut R,
    writer: &mut W,
) -> Result<ServerHello, HandshakeError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let term = env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string());
    let hello = ClientHello {
        version: PROTOCOL_VERSION,
        term,
        kbd: NegotiatedKbd::Legacy,
        graphics: GraphicsCaps::default(),
        // A one-shot request/reply handshake never registers a client, so its
        // remoteness is irrelevant here.
        remote: false,
    };
    client_handshake_with(reader, writer, hello).await
}

/// Send our `ServerHello` followed by a `ServerMsg::Error(VersionMismatch)`,
/// so a version-mismatched peer gets a structured wire error instead of a
/// bare disconnect, then return the `HandshakeError` for the caller to
/// propagate. Shared by both mismatch directions in [`server_handshake`].
async fn reject_version_mismatch<W>(
    writer: &mut W,
    daemon_pid: u32,
    peer_version: ProtocolVersion,
) -> Result<HandshakeError, HandshakeError>
where
    W: AsyncWrite + Unpin,
{
    // Send our hello first so the peer can decode a structured error.
    let our_hello = ServerHello {
        version: PROTOCOL_VERSION,
        daemon_pid,
    };
    let bytes = postcard::to_allocvec(&our_hello).map_err(|e| CodecError::Encode(e.to_string()))?;
    Codec::write_frame(writer, &bytes).await?;

    // Then surface the mismatch as a wire error.
    let err = crate::ServerMsg::Error(ProtocolError::VersionMismatch {
        client: u16::from(peer_version),
        server: u16::from(PROTOCOL_VERSION),
    });
    let bytes = postcard::to_allocvec(&err).map_err(|e| CodecError::Encode(e.to_string()))?;
    Codec::write_frame(writer, &bytes).await?;

    Ok(HandshakeError::VersionMismatch {
        ours: u16::from(PROTOCOL_VERSION),
        peer: u16::from(peer_version),
    })
}

/// Run the server side. Returns the client's hello.
///
/// Version policy: exact match → accept; any mismatch, older or newer → a
/// structured `VersionMismatch`, never a bare decode failure.
///
/// The version is peeled off the frame *before* attempting the full
/// `ClientHello` decode. postcard is positional, not forward/backward
/// compatible: a peer whose `ClientHello` shape predates a field addition
/// sends a shorter payload, and decoding it straight into our (larger)
/// current struct hits end-of-buffer — a `CodecError` that used to propagate
/// as an opaque "peer hung up" instead of a structured `VersionMismatch`.
/// Peeling `version` first (it is always the leading field, a transparent
/// `u16` varint — see `protocol_version_wire_matches_u16`) means we always
/// learn what the peer claims before gambling on decoding the rest, so an
/// older peer is rejected up front without ever attempting the full decode.
/// A newer peer's payload is decoded (postcard's struct decode does not
/// require the whole buffer be consumed, so extra append-only trailing
/// fields are silently ignored) purely to report its claimed version back
/// accurately; either way it is rejected too — no graceful downgrade in
/// either direction any more.
pub async fn server_handshake<R, W>(
    reader: &mut R,
    writer: &mut W,
    daemon_pid: u32,
) -> Result<ClientHello, HandshakeError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let frame = Codec::read_frame(reader)
        .await?
        .ok_or(HandshakeError::PeerClosed)?;

    let (peer_version, _) = postcard::take_from_bytes::<ProtocolVersion>(&frame)
        .map_err(|e| CodecError::Decode(e.to_string()))?;
    if peer_version < PROTOCOL_VERSION {
        return Err(reject_version_mismatch(writer, daemon_pid, peer_version).await?);
    }

    let client: ClientHello =
        postcard::from_bytes(&frame).map_err(|e| CodecError::Decode(e.to_string()))?;
    if client.version > PROTOCOL_VERSION {
        return Err(reject_version_mismatch(writer, daemon_pid, client.version).await?);
    }

    let our_hello = ServerHello {
        version: PROTOCOL_VERSION,
        daemon_pid,
    };
    let bytes = postcard::to_allocvec(&our_hello).map_err(|e| CodecError::Encode(e.to_string()))?;
    Codec::write_frame(writer, &bytes).await?;

    Ok(client)
}

#[cfg(test)]
mod tests {
    use tokio::io;
    use tokio::io::duplex;

    use super::*;
    use crate::ProtocolVersion;

    #[tokio::test]
    async fn handshake_succeeds_when_versions_match() {
        let (client_side, server_side) = duplex(1024);
        let (mut cr, mut cw) = io::split(client_side);
        let (mut sr, mut sw) = io::split(server_side);

        let server = tokio::spawn(async move { server_handshake(&mut sr, &mut sw, 42).await });
        let client = tokio::spawn(async move { client_handshake(&mut cr, &mut cw).await });

        let server = server.await.unwrap().unwrap();
        let client = client.await.unwrap().unwrap();
        assert_eq!(server.version, PROTOCOL_VERSION);
        assert_eq!(client.version, PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn handshake_fails_on_version_mismatch() {
        // Hand-craft a client hello with a wrong version and run only the
        // server side; assert it returns VersionMismatch.
        let (mut a, server_side) = duplex(1024);
        let (mut sr, mut sw) = io::split(server_side);

        // Write a bogus (NEWER) ClientHello: we cannot decode a newer wire
        // safely, so the server must still reject it.
        let bogus = ClientHello {
            version: ProtocolVersion(999),
            term: "x".into(),
            kbd: NegotiatedKbd::Legacy,
            graphics: GraphicsCaps::default(),
            remote: false,
        };
        let bytes = postcard::to_allocvec(&bogus).unwrap();
        Codec::write_frame(&mut a, &bytes).await.unwrap();

        let err = server_handshake(&mut sr, &mut sw, 1).await.unwrap_err();
        match err {
            HandshakeError::VersionMismatch { ours, peer } => {
                assert_eq!(ours, PROTOCOL_VERSION.0);
                assert_eq!(peer, 999);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn client_accepts_newer_server() {
        // The client must accept a ServerHello whose version is >= ours (we
        // speak our older subset of a newer server's protocol). Drives only
        // the client side.
        let (mut a, client_side) = duplex(1024);
        let (mut cr, mut cw) = io::split(client_side);
        let newer = ServerHello {
            version: ProtocolVersion(PROTOCOL_VERSION.0 + 1),
            daemon_pid: 7,
        };
        Codec::write_frame(&mut a, &postcard::to_allocvec(&newer).unwrap())
            .await
            .unwrap();
        let hello = ClientHello {
            version: PROTOCOL_VERSION,
            term: "x".into(),
            kbd: NegotiatedKbd::Legacy,
            graphics: GraphicsCaps::default(),
            remote: false,
        };
        let got = client_handshake_with(&mut cr, &mut cw, hello)
            .await
            .unwrap();
        assert_eq!(got.version, ProtocolVersion(PROTOCOL_VERSION.0 + 1));
    }

    #[tokio::test]
    async fn client_rejects_older_server() {
        // An OLDER server cannot serve our newer protocol → VersionMismatch.
        let (mut a, client_side) = duplex(1024);
        let (mut cr, mut cw) = io::split(client_side);
        let older = ServerHello {
            version: ProtocolVersion(PROTOCOL_VERSION.0 - 1),
            daemon_pid: 7,
        };
        Codec::write_frame(&mut a, &postcard::to_allocvec(&older).unwrap())
            .await
            .unwrap();
        let hello = ClientHello {
            version: PROTOCOL_VERSION,
            term: "x".into(),
            kbd: NegotiatedKbd::Legacy,
            graphics: GraphicsCaps::default(),
            remote: false,
        };
        let err = client_handshake_with(&mut cr, &mut cw, hello)
            .await
            .unwrap_err();
        match err {
            HandshakeError::VersionMismatch { ours, peer } => {
                assert_eq!(ours, PROTOCOL_VERSION.0);
                assert_eq!(peer, PROTOCOL_VERSION.0 - 1);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn older_versioned_peer_is_rejected_even_with_current_shape() {
        // Even when an older-versioned peer's payload happens to still decode
        // byte-for-byte into the current `ClientHello` shape (only the version
        // number differs), the server no longer gambles on that: any version
        // delta, older or newer, is now a hard structured mismatch, never a
        // graceful downgrade.
        let (mut a, server_side) = duplex(1024);
        let (mut sr, mut sw) = io::split(server_side);

        let bogus = ClientHello {
            version: ProtocolVersion(PROTOCOL_VERSION.0 - 1),
            term: "vt100".into(),
            kbd: NegotiatedKbd::Kitty(31),
            graphics: GraphicsCaps::default(),
            remote: false,
        };
        let bytes = postcard::to_allocvec(&bogus).unwrap();
        Codec::write_frame(&mut a, &bytes).await.unwrap();

        let err = server_handshake(&mut sr, &mut sw, 1).await.unwrap_err();
        match err {
            HandshakeError::VersionMismatch { ours, peer } => {
                assert_eq!(ours, PROTOCOL_VERSION.0);
                assert_eq!(peer, PROTOCOL_VERSION.0 - 1);
            }
            other => panic!("expected VersionMismatch, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn old_shape_client_hello_gets_structured_version_mismatch_not_codec_error() {
        // A pre-graphics/remote ClientHello (the v9-and-earlier shape) is a
        // strict PREFIX of the current wire layout: just `version` + `term` +
        // `kbd`, missing the later `graphics`/`remote` fields. Decoding that
        // straight into the current (larger) `ClientHello` struct would hit
        // end-of-buffer once the decoder reaches a field this old sender never
        // wrote. The version-peel fix must catch this via the claimed version
        // alone, before ever attempting that decode, and report a structured
        // `VersionMismatch` — not an opaque `HandshakeError::Codec` or
        // `PeerClosed`.
        let (mut a, server_side) = duplex(1024);
        let (mut sr, mut sw) = io::split(server_side);

        let old_version = ProtocolVersion(PROTOCOL_VERSION.0 - 1);
        // postcard encodes a struct as the plain concatenation of its fields in
        // declaration order with no struct-level framing, so this 3-tuple's
        // bytes are an exact prefix of what a full (old-shaped) ClientHello
        // with these same first three fields would have produced.
        let prefix = (old_version, "vt100".to_string(), NegotiatedKbd::Legacy);
        let bytes = postcard::to_allocvec(&prefix).unwrap();
        Codec::write_frame(&mut a, &bytes).await.unwrap();

        let err = server_handshake(&mut sr, &mut sw, 1).await.unwrap_err();
        match err {
            HandshakeError::VersionMismatch { ours, peer } => {
                assert_eq!(ours, PROTOCOL_VERSION.0);
                assert_eq!(peer, old_version.0);
            }
            other => panic!("expected VersionMismatch, got: {other:?}"),
        }

        // The server must also have sent the structured wire response (hello +
        // error), not just dropped the connection.
        let hello_frame = Codec::read_frame(&mut a)
            .await
            .unwrap()
            .expect("server hello frame");
        let hello: ServerHello = postcard::from_bytes(&hello_frame).unwrap();
        assert_eq!(hello.version, PROTOCOL_VERSION);

        let err_frame = Codec::read_frame(&mut a)
            .await
            .unwrap()
            .expect("error frame");
        let msg: crate::ServerMsg = postcard::from_bytes(&err_frame).unwrap();
        assert!(
            matches!(
                msg,
                crate::ServerMsg::Error(ProtocolError::VersionMismatch { .. })
            ),
            "expected ServerMsg::Error(VersionMismatch), got: {msg:?}"
        );
    }
}
