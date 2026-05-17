use crate::{
    Codec, CodecError, ClientHello, PROTOCOL_VERSION, ProtocolError, ServerHello,
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

/// Run the client side of the handshake.
///
/// Returns the server's hello once versions are confirmed compatible.
pub async fn client_handshake<R, W>(
    reader: &mut R,
    writer: &mut W,
) -> Result<ServerHello, HandshakeError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let hello = ClientHello { version: PROTOCOL_VERSION };
    let bytes = postcard::to_allocvec(&hello).map_err(|e| CodecError::Encode(e.to_string()))?;
    Codec::write_frame(writer, &bytes).await?;

    let frame = Codec::read_frame(reader).await?.ok_or(HandshakeError::PeerClosed)?;
    let server: ServerHello = postcard::from_bytes(&frame).map_err(CodecError::from)?;
    if server.version != PROTOCOL_VERSION {
        return Err(HandshakeError::VersionMismatch {
            ours: PROTOCOL_VERSION,
            peer: server.version,
        });
    }
    Ok(server)
}

/// Run the server side.
///
/// Returns the client's hello once versions match, otherwise sends a
/// `ServerMsg::Error` and returns `VersionMismatch`.
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
    let client: ClientHello = postcard::from_bytes(&frame).map_err(CodecError::from)?;

    if client.version != PROTOCOL_VERSION {
        // Send our hello first so the peer can decode a structured error.
        let our_hello = ServerHello { version: PROTOCOL_VERSION, daemon_pid };
        let bytes = postcard::to_allocvec(&our_hello).map_err(|e| CodecError::Encode(e.to_string()))?;
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

        // Write a bogus ClientHello.
        let bogus = ClientHello { version: 999 };
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
}
