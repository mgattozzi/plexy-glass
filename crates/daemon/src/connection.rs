//! One attached client. Phase 3 rewrites this to drive a `WindowManager`.

use crate::{
    error::DaemonError,
    renderer::Renderer,
    window_manager::WindowManager,
};
use bytes::Bytes;
use plexy_glass_mux::{Keymap, KeymapAction};
use plexy_glass_protocol::{
    ClientMsg, Codec, ProtocolError, ServerMsg, server_handshake,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex, Notify, mpsc};

pub struct Connection;

impl Connection {
    pub async fn serve<S>(stream: S, daemon_pid: u32) -> Result<(), DaemonError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut reader, mut writer) = tokio::io::split(stream);
        server_handshake(&mut reader, &mut writer, daemon_pid).await?;

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

        let notify = Arc::new(Notify::new());
        let (death_tx, mut death_rx) = mpsc::channel::<plexy_glass_mux::PaneId>(16);
        let manager = match WindowManager::new(spec, size, Arc::clone(&notify), Some(death_tx)) {
            Ok(m) => m,
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
        let manager = Arc::new(Mutex::new(manager));

        send_msg(&mut writer, &ServerMsg::Spawned).await?;

        let prefix_active = Arc::new(AtomicBool::new(false));
        let renderer = Renderer::new();
        let renderer_task = tokio::spawn({
            let manager = Arc::clone(&manager);
            let notify = Arc::clone(&notify);
            let prefix_active = Arc::clone(&prefix_active);
            async move { renderer.run(manager, notify, prefix_active, writer).await }
        });

        let mut keymap = Keymap::default_tmux();
        let mut router = crate::InputRouter::new();
        'outer: loop {
            tokio::select! {
                biased;
                Some(pane_id) = death_rx.recv() => {
                    let mut mgr = manager.lock().await;
                    let _ = mgr.handle_pane_death(pane_id);
                    if mgr.is_empty() {
                        break 'outer;
                    }
                }
                frame = Codec::read_frame(&mut reader) => {
                    let frame = match frame {
                        Ok(Some(f)) => f,
                        Ok(None) => break,
                        Err(_) => break,
                    };
                    let msg: ClientMsg = match postcard::from_bytes(&frame) {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    match msg {
                        ClientMsg::Input(bytes) => {
                            let events = router.classify(bytes.as_ref());
                            for event in events {
                                match event {
                                    crate::InputEvent::Mouse(me) => {
                                        let mut mgr = manager.lock().await;
                                        let _ = mgr.handle_mouse(me).await;
                                    }
                                    crate::InputEvent::Key(b) => {
                                        // Snap scroll-back to live on any keystroke.
                                        {
                                            let mgr = manager.lock().await;
                                            if let Some(p) = mgr.active_window().active_pane() {
                                                p.reset_scroll();
                                            }
                                        }
                                        let action = keymap.consume(b);
                                        prefix_active.store(keymap.prefix_active(), Ordering::SeqCst);
                                        match action {
                                            KeymapAction::PassThrough(byte) => {
                                                let manager = manager.lock().await;
                                                if let Some(pane) = manager.active_window().active_pane() {
                                                    let _ = pane
                                                        .send_input(Bytes::copy_from_slice(&[byte]))
                                                        .await;
                                                }
                                            }
                                            KeymapAction::Command(cmd) => {
                                                if matches!(cmd, plexy_glass_mux::Command::Detach) {
                                                    break 'outer;
                                                }
                                                let mut manager = manager.lock().await;
                                                if let Err(e) = manager.handle_command(cmd) {
                                                    tracing::warn!(?cmd, error = %e, "command failed");
                                                }
                                                notify.notify_one();
                                            }
                                            KeymapAction::Consumed => {
                                                notify.notify_one();
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        ClientMsg::Resize(size) => {
                            let mut manager = manager.lock().await;
                            let _ = manager.on_host_resize(size);
                        }
                        ClientMsg::Shutdown => break,
                        _ => {}
                    }
                }
            }
        }

        // Wake the renderer one last time so it can flush its closing frame
        // (the host-TTY-restoring sequences the line editor wrote on exit)
        // before noticing `manager.is_empty()` and returning gracefully. The
        // wait_child thread joins the reader thread before signaling death,
        // so by the time we get here the emulator has already absorbed every
        // byte the child produced on its way out.
        notify.notify_one();
        drop(manager);
        let _ = renderer_task.await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use plexy_glass_protocol::{PROTOCOL_VERSION, PtySize, SpawnSpec, client_handshake};
    use tokio::io::duplex;

    #[tokio::test]
    async fn end_to_end_renders_then_exits() {
        let (server_side, client_side) = duplex(64 * 1024);
        let server = tokio::spawn(async move { Connection::serve(server_side, 7).await });

        let (mut cr, mut cw) = tokio::io::split(client_side);
        let server_hello = client_handshake(&mut cr, &mut cw).await.unwrap();
        assert_eq!(server_hello.version, PROTOCOL_VERSION);

        let spawn = ClientMsg::Spawn {
            cmd: SpawnSpec {
                program: "/bin/echo".into(),
                args: vec!["hi".into()],
                env: vec![],
                cwd: None,
            },
            size: PtySize {
                rows: 8,
                cols: 24,
                pixel_width: 0,
                pixel_height: 0,
            },
        };
        let bytes = postcard::to_allocvec(&spawn).unwrap();
        Codec::write_frame(&mut cw, &bytes).await.unwrap();

        let mut saw_spawned = false;
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
                ServerMsg::Spawned => saw_spawned = true,
                ServerMsg::Output(_) => saw_output = true,
                ServerMsg::Error(e) => panic!("got error: {e:?}"),
                _ => {}
            }
            if saw_spawned && saw_output {
                break;
            }
        }
        assert!(saw_spawned, "missing Spawned");
        assert!(saw_output, "missing Output");

        server.abort();
    }
}
