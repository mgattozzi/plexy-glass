//! One connection from a client.

use crate::{error::DaemonError, registry::SessionRegistry};
use plexy_glass_protocol::{
    ClientMsg, Codec, ProtocolError, ServerMsg, server_handshake,
};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};

pub struct Connection;

impl Connection {
    pub async fn serve<S>(
        stream: S,
        daemon_pid: u32,
        registry: Arc<SessionRegistry>,
    ) -> Result<(), DaemonError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut reader, mut writer) = tokio::io::split(stream);
        server_handshake(&mut reader, &mut writer, daemon_pid).await?;

        let frame = Codec::read_frame(&mut reader).await?.ok_or_else(|| {
            DaemonError::Io(std::io::Error::other("client closed before first message"))
        })?;
        let msg: ClientMsg = postcard::from_bytes(&frame)
            .map_err(|e| plexy_glass_protocol::errors::CodecError::Decode(e.to_string()))?;

        match msg {
            ClientMsg::ListSessions => {
                let entries = registry.list().await;
                send_msg(&mut writer, &ServerMsg::SessionList { entries }).await?;
                Ok(())
            }
            ClientMsg::KillSession { name } => match registry.kill(&name).await {
                Ok(()) => send_msg(&mut writer, &ServerMsg::SessionKilled { name }).await,
                Err(DaemonError::Protocol(perr)) => {
                    send_msg(&mut writer, &ServerMsg::Error(perr)).await
                }
                Err(e) => Err(e),
            },
            ClientMsg::AttachOrCreate { .. } => {
                // Task 12 wires this. For now: placeholder error.
                send_msg(
                    &mut writer,
                    &ServerMsg::Error(ProtocolError::SessionNotFound {
                        name: "(attach mode not yet implemented)".into(),
                    }),
                )
                .await?;
                Ok(())
            }
            other => {
                send_msg(
                    &mut writer,
                    &ServerMsg::Error(ProtocolError::UnexpectedMessage(format!("{other:?}"))),
                )
                .await?;
                Ok(())
            }
        }
    }
}

async fn send_msg<W>(writer: &mut W, msg: &ServerMsg) -> Result<(), DaemonError>
where
    W: AsyncWrite + Unpin,
{
    let bytes = postcard::to_allocvec(msg)
        .map_err(|e| plexy_glass_protocol::errors::CodecError::Encode(e.to_string()))?;
    Codec::write_frame(writer, &bytes).await?;
    Ok(())
}
