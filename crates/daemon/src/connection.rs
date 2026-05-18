//! One attached client. Drives handshake -> Spawn -> bidirectional pump -> exit.

use crate::error::DaemonError;
use crate::pane::Pane;
use bytes::Bytes;
use plexy_glass_mux::PaneId;
use plexy_glass_protocol::{
    ClientMsg, Codec, ExitStatus, ProtocolError, ServerMsg, server_handshake,
};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::Notify;

/// One attached client.
pub struct Connection;

impl Connection {
    /// Run the full lifecycle of an attached client over the given duplex stream.
    pub async fn serve<S>(stream: S, daemon_pid: u32) -> Result<(), DaemonError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut reader, mut writer) = tokio::io::split(stream);
        server_handshake(&mut reader, &mut writer, daemon_pid).await?;

        // Wait for the first message; it must be `Spawn`.
        let frame = Codec::read_frame(&mut reader).await?.ok_or_else(|| {
            DaemonError::Io(std::io::Error::other("client closed before Spawn"))
        })?;
        let msg: ClientMsg = postcard::from_bytes(&frame)
            .map_err(|e| plexy_glass_protocol::errors::CodecError::Decode(e.to_string()))?;
        let (spec, size) = match msg {
            ClientMsg::Spawn { cmd, size } => (cmd, size),
            other => {
                send_msg(
                    &mut writer,
                    &ServerMsg::Error(ProtocolError::UnexpectedMessage(format!("{other:?}"))),
                )
                .await?;
                return Ok(());
            }
        };

        let session = match Pane::spawn(PaneId(0), spec, size, Arc::new(Notify::new())) {
            Ok(s) => s,
            Err(e) => {
                send_msg(
                    &mut writer,
                    &ServerMsg::Error(ProtocolError::SpawnFailed {
                        reason: e.to_string(),
                    }),
                )
                .await?;
                return Ok(());
            }
        };

        send_msg(&mut writer, &ServerMsg::Spawned).await?;

        let mut output_rx = session.subscribe_output();

        // Spawn the session-output -> socket forwarder. It owns `writer` for
        // the duration of the connection and hands it back on exit.
        let writer_task = tokio::spawn(async move {
            loop {
                match output_rx.recv().await {
                    Ok(chunk) => {
                        if send_msg(&mut writer, &ServerMsg::Output(chunk))
                            .await
                            .is_err()
                        {
                            return writer;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            writer
        });

        // Drive the socket-input + child-exit selector on this task.
        // We hold session refs in an inner scope so they drop (closing the
        // broadcast) once the loop exits, which signals writer_task to return.
        let exit_status: ExitStatus = {
            let session_clone = session.clone();
            let mut status = ExitStatus::Unknown;
            let exit_wait = async move { session_clone.wait().await };
            tokio::pin!(exit_wait);

            loop {
                tokio::select! {
                    biased;
                    s = &mut exit_wait => {
                        status = s;
                        break;
                    }
                    frame = Codec::read_frame(&mut reader) => {
                        match frame {
                            Ok(Some(buf)) => {
                                let msg: ClientMsg = match postcard::from_bytes(&buf) {
                                    Ok(m) => m,
                                    Err(_) => continue, // ignore garbage
                                };
                                match msg {
                                    ClientMsg::Input(bytes) => {
                                        let _ = session.send_input(bytes).await;
                                    }
                                    ClientMsg::Resize(size) => {
                                        let _ = session.resize(size);
                                    }
                                    ClientMsg::Shutdown => {
                                        // Phase 1: shutdown ends this client's read pump.
                                        break;
                                    }
                                    ClientMsg::Spawn { .. } => {
                                        // Already spawned; ignore.
                                    }
                                    // ClientMsg is #[non_exhaustive]; keep a fallback
                                    // arm for forward-compatibility with future variants.
                                    #[allow(unreachable_patterns)]
                                    _ => {}
                                }
                            }
                            Ok(None) => break,
                            Err(_) => break,
                        }
                    }
                }
            }
            status
        };

        // Drop our remaining Session handle so the broadcast Sender count hits 0
        // and writer_task sees `RecvError::Closed` and exits.
        drop(session);

        let mut writer = writer_task
            .await
            .map_err(|e| DaemonError::Io(std::io::Error::other(format!("writer task: {e}"))))?;
        let _ = send_msg(
            &mut writer,
            &ServerMsg::Exited {
                status: exit_status,
            },
        )
        .await;
        let _ = writer.shutdown().await;
        Ok(())
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

// Vestigial helper from drafting, kept silent so the `bytes` import has a
// use site.
#[allow(dead_code)] // retained for symmetry with protocol Bytes payloads in tests
fn _bytes_marker(_: Bytes) {}

#[cfg(test)]
mod tests {
    use super::*;
    use plexy_glass_protocol::{
        ClientMsg, PROTOCOL_VERSION, PtySize, ServerMsg, SpawnSpec, client_handshake,
    };
    use tokio::io::duplex;

    #[tokio::test]
    async fn full_flow_with_echo_program() {
        let (server_side, client_side) = duplex(64 * 1024);
        let server = tokio::spawn(async move { Connection::serve(server_side, 7).await });

        let (mut cr, mut cw) = tokio::io::split(client_side);
        let server_hello = client_handshake(&mut cr, &mut cw).await.unwrap();
        assert_eq!(server_hello.version, PROTOCOL_VERSION);
        assert_eq!(server_hello.daemon_pid, 7);

        // Send Spawn for /bin/echo hello.
        let spawn = ClientMsg::Spawn {
            cmd: SpawnSpec {
                program: "/bin/echo".into(),
                args: vec!["hello".into()],
                env: vec![],
                cwd: None,
            },
            size: PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        };
        let bytes = postcard::to_allocvec(&spawn).unwrap();
        Codec::write_frame(&mut cw, &bytes).await.unwrap();

        // Collect frames until we see Exited.
        let mut saw_output = false;
        let mut saw_exit = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while !saw_exit && tokio::time::Instant::now() < deadline {
            let frame = match tokio::time::timeout(
                std::time::Duration::from_millis(500),
                Codec::read_frame(&mut cr),
            )
            .await
            {
                Ok(Ok(Some(f))) => f,
                _ => break,
            };
            let msg: ServerMsg = postcard::from_bytes(&frame).unwrap();
            match msg {
                ServerMsg::Spawned => {}
                ServerMsg::Output(b) if b.windows(5).any(|w| w == b"hello") => {
                    saw_output = true;
                }
                ServerMsg::Output(_) => {}
                ServerMsg::Exited {
                    status: ExitStatus::Code(0),
                } => saw_exit = true,
                ServerMsg::Exited { status } => panic!("non-zero exit: {status:?}"),
                ServerMsg::Error(e) => panic!("got error: {e:?}"),
                // ServerMsg is #[non_exhaustive]; keep a fallback arm for
                // forward-compatibility with future variants.
                #[allow(unreachable_patterns)]
                _ => {}
            }
        }
        assert!(saw_output, "did not see 'hello'");
        assert!(saw_exit, "did not see Exited");

        let _ = server.await;
    }
}
