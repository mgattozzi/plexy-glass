use crate::{
    ClientHello, Codec, CodecError, NegotiatedKbd, PROTOCOL_VERSION, ProtocolError, ServerHello,
};
use tokio::io::{AsyncRead, AsyncWrite};

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

    let frame = Codec::read_frame(reader).await?.ok_or(HandshakeError::PeerClosed)?;
    let server: ServerHello = postcard::from_bytes(&frame).map_err(CodecError::from)?;
    // Symmetric with server_handshake's policy: a NEWER server gracefully
    // downgrades to serve us (it forces kbd=Legacy in its hello), so accept any
    // server whose version is >= ours (we speak our older subset). Reject only an
    // OLDER server: it cannot have produced a hello our newer code can rely on,
    // and there is no forward-compat guarantee in that direction. (The ServerHello
    // itself already decoded above; a genuinely incompatible wire layout surfaces
    // as HandshakeError::Codec, a clean failure, before this check.)
    if server.version < PROTOCOL_VERSION {
        return Err(HandshakeError::VersionMismatch {
            ours: PROTOCOL_VERSION,
            peer: server.version,
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
    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string());
    let hello = ClientHello { version: PROTOCOL_VERSION, term, kbd: NegotiatedKbd::Legacy };
    client_handshake_with(reader, writer, hello).await
}

/// Run the server side. Returns the client's hello.
///
/// Version policy:
/// - exact match → accept as sent.
/// - older peer (peer < ours) → *if the frame still decodes into the current
///   `ClientHello` shape* (postcard is not forward-compatible, so a genuinely
///   older wire layout fails to decode before this check and surfaces as
///   `HandshakeError::Codec`, a clean connection failure), force `kbd = Legacy`
///   and proceed, so input falls back to legacy decode + raw passthrough.
/// - newer peer (peer > ours) → we cannot have decoded the hello reliably; send
///   a structured error and return `VersionMismatch`.
pub async fn server_handshake<R, W>(
    reader: &mut R,
    writer: &mut W,
    daemon_pid: u32,
) -> Result<ClientHello, HandshakeError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let frame = Codec::read_frame(reader).await?.ok_or(HandshakeError::PeerClosed)?;
    let mut client: ClientHello = postcard::from_bytes(&frame).map_err(CodecError::from)?;

    if client.version > PROTOCOL_VERSION {
        // Send our hello first so the peer can decode a structured error.
        let our_hello = ServerHello { version: PROTOCOL_VERSION, daemon_pid };
        let bytes =
            postcard::to_allocvec(&our_hello).map_err(|e| CodecError::Encode(e.to_string()))?;
        Codec::write_frame(writer, &bytes).await?;

        // Then surface the mismatch as a wire error.
        let err = crate::ServerMsg::Error(ProtocolError::VersionMismatch {
            client: client.version,
            server: PROTOCOL_VERSION,
        });
        let bytes = postcard::to_allocvec(&err).map_err(|e| CodecError::Encode(e.to_string()))?;
        Codec::write_frame(writer, &bytes).await?;

        return Err(HandshakeError::VersionMismatch {
            ours: PROTOCOL_VERSION,
            peer: client.version,
        });
    }

    if client.version < PROTOCOL_VERSION {
        // Graceful downgrade (only reached when the older-versioned frame still
        // decoded into the current struct above): never trust an older peer's
        // `kbd`; legacy decode is always safe.
        client.kbd = NegotiatedKbd::Legacy;
    }

    let our_hello = ServerHello { version: PROTOCOL_VERSION, daemon_pid };
    let bytes = postcard::to_allocvec(&our_hello).map_err(|e| CodecError::Encode(e.to_string()))?;
    Codec::write_frame(writer, &bytes).await?;

    Ok(client)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn handshake_succeeds_when_versions_match() {
        let (client_side, server_side) = duplex(1024);
        let (mut cr, mut cw) = tokio::io::split(client_side);
        let (mut sr, mut sw) = tokio::io::split(server_side);

        let server = tokio::spawn(async move {
            server_handshake(&mut sr, &mut sw, 42).await
        });
        let client = tokio::spawn(async move {
            client_handshake(&mut cr, &mut cw).await
        });

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
        let (mut sr, mut sw) = tokio::io::split(server_side);

        // Write a bogus (NEWER) ClientHello: we cannot decode a newer wire
        // safely, so the server must still reject it.
        let bogus = ClientHello { version: 999, term: "x".into(), kbd: NegotiatedKbd::Legacy };
        let bytes = postcard::to_allocvec(&bogus).unwrap();
        Codec::write_frame(&mut a, &bytes).await.unwrap();

        let err = server_handshake(&mut sr, &mut sw, 1).await.unwrap_err();
        match err {
            HandshakeError::VersionMismatch { ours, peer } => {
                assert_eq!(ours, PROTOCOL_VERSION);
                assert_eq!(peer, 999);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn client_accepts_newer_server() {
        // A newer server gracefully downgrades to serve us; the client must
        // accept a ServerHello whose version is >= ours (the previously-dead
        // downgrade path). Drives only the client side.
        let (mut a, client_side) = duplex(1024);
        let (mut cr, mut cw) = tokio::io::split(client_side);
        let newer = ServerHello { version: PROTOCOL_VERSION + 1, daemon_pid: 7 };
        Codec::write_frame(&mut a, &postcard::to_allocvec(&newer).unwrap()).await.unwrap();
        let hello =
            ClientHello { version: PROTOCOL_VERSION, term: "x".into(), kbd: NegotiatedKbd::Legacy };
        let got = client_handshake_with(&mut cr, &mut cw, hello).await.unwrap();
        assert_eq!(got.version, PROTOCOL_VERSION + 1);
    }

    #[tokio::test]
    async fn client_rejects_older_server() {
        // An OLDER server cannot serve our newer protocol → VersionMismatch.
        let (mut a, client_side) = duplex(1024);
        let (mut cr, mut cw) = tokio::io::split(client_side);
        let older = ServerHello { version: PROTOCOL_VERSION - 1, daemon_pid: 7 };
        Codec::write_frame(&mut a, &postcard::to_allocvec(&older).unwrap()).await.unwrap();
        let hello =
            ClientHello { version: PROTOCOL_VERSION, term: "x".into(), kbd: NegotiatedKbd::Legacy };
        let err = client_handshake_with(&mut cr, &mut cw, hello).await.unwrap_err();
        match err {
            HandshakeError::VersionMismatch { ours, peer } => {
                assert_eq!(ours, PROTOCOL_VERSION);
                assert_eq!(peer, PROTOCOL_VERSION - 1);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn older_peer_negotiates_legacy_instead_of_erroring() {
        // Server speaks PROTOCOL_VERSION; an older client sends a down-version
        // hello. The server must NOT error: it downgrades the recorded caps to
        // Legacy and proceeds.
        let (mut a, server_side) = duplex(1024);
        let (mut sr, mut sw) = tokio::io::split(server_side);

        let bogus = ClientHello {
            version: PROTOCOL_VERSION - 1,
            term: "vt100".into(),
            kbd: NegotiatedKbd::Kitty(31),
        };
        let bytes = postcard::to_allocvec(&bogus).unwrap();
        Codec::write_frame(&mut a, &bytes).await.unwrap();

        let client = server_handshake(&mut sr, &mut sw, 1).await.unwrap();
        assert_eq!(client.version, PROTOCOL_VERSION - 1);
        assert_eq!(client.kbd, NegotiatedKbd::Legacy, "old peer downgraded to legacy");
    }
}
