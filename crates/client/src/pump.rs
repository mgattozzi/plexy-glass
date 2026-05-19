use crate::error::ClientError;
use bytes::BytesMut;
use plexy_glass_protocol::{ClientMsg, Codec, ExitStatus, PtySize, ServerMsg};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

const STDIN_CHUNK: usize = 4096;

/// Run the three concurrent pumps:
///   stdin  -> ClientMsg::Input(bytes)  -> daemon
///   daemon -> ServerMsg::Output(bytes) -> stdout
///   SIGWINCH (delivered via `resize_rx`) -> ClientMsg::Resize(size) -> daemon
///
/// Returns the child's exit status when the daemon sends `Exited`.
pub async fn pump<R, W, In, Out>(
    mut daemon_read: R,
    mut daemon_write: W,
    mut stdin: In,
    mut stdout: Out,
    mut resize_rx: mpsc::Receiver<PtySize>,
) -> Result<ExitStatus, ClientError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    In: AsyncRead + Unpin + Send + 'static,
    Out: AsyncWrite + Unpin + Send + 'static,
{
    let mut stdin_buf = BytesMut::with_capacity(STDIN_CHUNK);
    let exit_status: ExitStatus;
    loop {
        stdin_buf.clear();
        stdin_buf.resize(STDIN_CHUNK, 0);
        tokio::select! {
            // Daemon -> client
            frame = Codec::read_frame(&mut daemon_read) => {
                let frame = match frame? {
                    Some(f) => f,
                    None => {
                        exit_status = ExitStatus::Unknown;
                        break;
                    }
                };
                let msg: ServerMsg = postcard::from_bytes(&frame)
                    .map_err(|e| plexy_glass_protocol::errors::CodecError::Decode(e.to_string()))?;
                match msg {
                    ServerMsg::Output(b) => {
                        stdout.write_all(&b).await.map_err(ClientError::Io)?;
                        stdout.flush().await.map_err(ClientError::Io)?;
                    }
                    ServerMsg::Exited { status } => {
                        exit_status = status;
                        break;
                    }
                    ServerMsg::Error(e) => {
                        return Err(ClientError::DaemonError(e));
                    }
                    ServerMsg::Attached { .. } => {} // already saw it in the caller
                    // `ServerMsg` is `#[non_exhaustive]`; future variants are ignored here.
                    #[allow(unreachable_patterns)]
                    _ => {}
                }
            }
            // Client -> daemon (stdin)
            n = stdin.read(&mut stdin_buf) => {
                let n = n.map_err(ClientError::Io)?;
                if n == 0 {
                    // stdin closed; we keep the session alive until the child exits.
                    continue;
                }
                let chunk = stdin_buf.split_to(n).freeze();
                let msg = ClientMsg::Input(chunk);
                send_client_msg(&mut daemon_write, &msg).await?;
            }
            // Client -> daemon (resize)
            Some(size) = resize_rx.recv() => {
                send_client_msg(&mut daemon_write, &ClientMsg::Resize(size)).await?;
            }
        }
    }
    Ok(exit_status)
}

pub async fn send_client_msg<W>(writer: &mut W, msg: &ClientMsg) -> Result<(), ClientError>
where
    W: AsyncWrite + Unpin,
{
    let bytes = postcard::to_allocvec(msg)
        .map_err(|e| plexy_glass_protocol::errors::CodecError::Encode(e.to_string()))?;
    Codec::write_frame(writer, &bytes).await?;
    Ok(())
}

/// Send the initial `AttachOrCreate` request and wait for `Attached`.
pub async fn handshake_spawn<R, W>(
    reader: &mut R,
    writer: &mut W,
    spec: plexy_glass_protocol::SpawnSpec,
    size: PtySize,
) -> Result<(), ClientError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    send_client_msg(
        writer,
        &ClientMsg::AttachOrCreate {
            name: None,
            create_if_missing: true,
            cmd: Some(spec),
            size,
        },
    )
    .await?;
    let frame = Codec::read_frame(reader)
        .await?
        .ok_or_else(|| ClientError::Io(std::io::Error::other("daemon closed before Attached")))?;
    let msg: ServerMsg = postcard::from_bytes(&frame)
        .map_err(|e| plexy_glass_protocol::errors::CodecError::Decode(e.to_string()))?;
    match msg {
        ServerMsg::Attached { .. } => Ok(()),
        ServerMsg::Error(e) => Err(ClientError::DaemonError(e)),
        other => Err(ClientError::Io(std::io::Error::other(format!(
            "expected Attached, got {other:?}"
        )))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use plexy_glass_protocol::{ExitStatus, ServerMsg};
    use tokio::io::duplex;

    #[tokio::test]
    async fn pump_writes_output_to_stdout_and_exits_on_exited() {
        let (mut server_w, client_r) = duplex(64 * 1024);
        let (server_r, client_w) = duplex(64 * 1024);
        drop(server_r); // we don't read from the client in this test
        let (stdin_w, stdin_r) = duplex(64);
        drop(stdin_w);
        let (mut stdout_r, stdout_w) = duplex(64 * 1024);
        let (_tx, resize_rx) = mpsc::channel(4);

        // Server-side: emit one `Output` and then `Exited`.
        let server = tokio::spawn(async move {
            let out = ServerMsg::Output(Bytes::from_static(b"abc"));
            let bytes = postcard::to_allocvec(&out).unwrap();
            Codec::write_frame(&mut server_w, &bytes).await.unwrap();
            let done = ServerMsg::Exited { status: ExitStatus::Code(0) };
            let bytes = postcard::to_allocvec(&done).unwrap();
            Codec::write_frame(&mut server_w, &bytes).await.unwrap();
        });

        let status = pump(client_r, client_w, stdin_r, stdout_w, resize_rx)
            .await
            .unwrap();
        assert!(matches!(status, ExitStatus::Code(0)), "got: {status:?}");

        let mut out = Vec::new();
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            stdout_r.read_to_end(&mut out),
        )
        .await;
        assert_eq!(&out, b"abc");

        server.await.unwrap();
    }
}
