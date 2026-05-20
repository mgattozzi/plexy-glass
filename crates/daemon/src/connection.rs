//! One connection from a client.

use crate::{
    InputEvent, InputRouter, error::DaemonError, registry::SessionRegistry, renderer::Renderer,
    session::Session,
};
use plexy_glass_mux::{Command, Keymap, KeymapAction};
use plexy_glass_protocol::{
    ClientMsg, Codec, ProtocolError, PtySize, ServerMsg, SpawnSpec, server_handshake,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
            ClientMsg::AttachOrCreate { name, create_if_missing, cmd, size } => {
                serve_attach(reader, writer, registry, name, create_if_missing, cmd, size).await
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

#[allow(clippy::too_many_arguments)] // one internal entry point, splitting it up would lose clarity
async fn serve_attach<R, W>(
    mut reader: R,
    mut writer: W,
    registry: Arc<SessionRegistry>,
    name: Option<String>,
    create_if_missing: bool,
    cmd: Option<SpawnSpec>,
    size: PtySize,
) -> Result<(), DaemonError>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // Resolve or create the session.
    let session = match name {
        Some(n) => match registry.get(&n).await {
            Some(s) => s,
            None if create_if_missing => {
                let spec = cmd.unwrap_or_else(default_spawn_spec);
                match registry.create(n.clone(), spec, size).await {
                    Ok(s) => s,
                    Err(DaemonError::Protocol(perr)) => {
                        return send_msg(&mut writer, &ServerMsg::Error(perr)).await;
                    }
                    Err(e) => return Err(e),
                }
            }
            None => {
                return send_msg(
                    &mut writer,
                    &ServerMsg::Error(ProtocolError::SessionNotFound { name: n }),
                )
                .await;
            }
        },
        None => {
            // Smart default fallback: 0 -> create "main"; 1 -> attach to it; 2+ -> ambiguous.
            let entries = registry.list().await;
            match entries.len() {
                0 => {
                    let spec = cmd.unwrap_or_else(default_spawn_spec);
                    match registry.create("main".into(), spec, size).await {
                        Ok(s) => s,
                        Err(DaemonError::Protocol(perr)) => {
                            return send_msg(&mut writer, &ServerMsg::Error(perr)).await;
                        }
                        Err(e) => return Err(e),
                    }
                }
                1 => match registry.get(&entries[0].name).await {
                    Some(s) => s,
                    None => {
                        // We raced with a kill, so surface it as session-not-found.
                        return send_msg(
                            &mut writer,
                            &ServerMsg::Error(ProtocolError::SessionNotFound {
                                name: entries[0].name.clone(),
                            }),
                        )
                        .await;
                    }
                },
                n => {
                    let count = u8::try_from(n).unwrap_or(u8::MAX);
                    return send_msg(
                        &mut writer,
                        &ServerMsg::Error(ProtocolError::AmbiguousSession { count }),
                    )
                    .await;
                }
            }
        }
    };

    // Register this connection as a client. `register_client` calls
    // `blocking_lock` internally, so dispatch it off the async runtime.
    let session_for_register = Arc::clone(&session);
    let handle = match tokio::task::spawn_blocking(move || {
        session_for_register.register_client(size)
    })
    .await
    {
        Ok(Ok(h)) => h,
        Ok(Err(DaemonError::Protocol(perr))) => {
            return send_msg(&mut writer, &ServerMsg::Error(perr)).await;
        }
        Ok(Err(e)) => return Err(e),
        Err(join) => return Err(DaemonError::Io(std::io::Error::other(join.to_string()))),
    };

    let client_id = handle.client_id;
    let session_name = session.name.clone();

    send_msg(
        &mut writer,
        &ServerMsg::Attached { session_name, client_id },
    )
    .await?;

    // Spawn the per-Connection renderer task. It owns the writer half from
    // here on out.
    let frame_rx = handle.frame_rx.clone();
    let renderer = Renderer::new();
    let mut renderer_task = tokio::spawn(async move {
        let _ = renderer.run(frame_rx, writer).await;
    });

    // Input loop.
    let mut router = InputRouter::new();
    let mut keymap = Keymap::default_tmux();
    let prefix_active = Arc::new(AtomicBool::new(false));

    loop {
        let frame = tokio::select! {
            biased;
            // Renderer exits when its `frame_rx` is closed, i.e. the session's
            // coordinator dropped its `frame_tx`. That means the session ended
            // (last pane exited, or the session was killed). Tear down so the
            // client process exits and `HostTty::restore` runs.
            _ = &mut renderer_task => break,
            result = Codec::read_frame(&mut reader) => match result {
                Ok(Some(f)) => f,
                Ok(None) => break,
                Err(_) => break,
            },
        };
        let msg: ClientMsg = match postcard::from_bytes(&frame) {
            Ok(m) => m,
            Err(_) => continue,
        };
        match msg {
            ClientMsg::Input(bytes) => {
                let events = router.classify(bytes.as_ref());
                let mut detach_requested = false;
                for event in events {
                    match event {
                        InputEvent::Mouse(me) => {
                            let _ = session.handle_mouse(me).await;
                        }
                        InputEvent::Key(b) => {
                            // Snap scrollback to live on any keystroke.
                            {
                                let manager = session.window_manager.lock().await;
                                if let Some(p) = manager.active_window().active_pane() {
                                    p.reset_scroll();
                                }
                            }
                            let action = keymap.consume(b);
                            prefix_active.store(keymap.prefix_active(), Ordering::SeqCst);
                            match action {
                                KeymapAction::PassThrough(byte) => {
                                    let _ = session.handle_input_bytes(&[byte]).await;
                                }
                                KeymapAction::Command(cmd) => {
                                    if matches!(cmd, Command::Detach) {
                                        detach_requested = true;
                                        break;
                                    }
                                    let _ = session.handle_command(cmd).await;
                                }
                                KeymapAction::Consumed => {
                                    session.notify.notify_one();
                                }
                            }
                        }
                    }
                }
                if detach_requested {
                    break;
                }
            }
            ClientMsg::Resize(new_size) => {
                let session_for_resize = Arc::clone(&session);
                let _ = tokio::task::spawn_blocking(move || {
                    session_for_resize.handle_resize(client_id, new_size);
                })
                .await;
            }
            ClientMsg::Detach => break,
            ClientMsg::Shutdown => break,
            _ => {}
        }
    }
    cleanup_and_exit(session, client_id, renderer_task).await
}

async fn cleanup_and_exit(
    session: Arc<Session>,
    client_id: u64,
    renderer_task: tokio::task::JoinHandle<()>,
) -> Result<(), DaemonError> {
    let session_for_dereg = Arc::clone(&session);
    let _ = tokio::task::spawn_blocking(move || {
        session_for_dereg.deregister_client(client_id);
    })
    .await;
    renderer_task.abort();
    Ok(())
}

fn default_spawn_spec() -> SpawnSpec {
    let program = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    SpawnSpec {
        program,
        args: vec![],
        env: vec![],
        cwd: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexy_glass_protocol::{PROTOCOL_VERSION, PtySize, SpawnSpec, client_handshake};
    use tokio::io::duplex;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn end_to_end_attach_renders_then_exits() {
        let (server_side, client_side) = duplex(64 * 1024);
        let server = tokio::spawn(async move {
            Connection::serve(server_side, 7, Arc::new(crate::SessionRegistry::new())).await
        });

        let (mut cr, mut cw) = tokio::io::split(client_side);
        let server_hello = client_handshake(&mut cr, &mut cw).await.unwrap();
        assert_eq!(server_hello.version, PROTOCOL_VERSION);

        let attach = ClientMsg::AttachOrCreate {
            name: Some("test".into()),
            create_if_missing: true,
            cmd: Some(SpawnSpec {
                program: "/bin/echo".into(),
                args: vec!["hi".into()],
                env: vec![],
                cwd: None,
            }),
            size: PtySize { rows: 8, cols: 24, pixel_width: 0, pixel_height: 0 },
        };
        let bytes = postcard::to_allocvec(&attach).unwrap();
        Codec::write_frame(&mut cw, &bytes).await.unwrap();

        let mut saw_attached = false;
        let mut saw_output = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
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
                ServerMsg::Attached { .. } => saw_attached = true,
                ServerMsg::Output(_) => saw_output = true,
                ServerMsg::Error(e) => panic!("got error: {e:?}"),
                _ => {}
            }
            if saw_attached && saw_output {
                break;
            }
        }
        assert!(saw_attached, "missing Attached");
        assert!(saw_output, "missing Output");

        server.abort();
    }
}
