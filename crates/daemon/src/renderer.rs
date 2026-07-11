//! Per-client renderer task: diffs frames against last frame, emits ANSI as ServerMsg::Output.

use std::future;
use std::io::Error;
use std::sync::Arc;

use plexy_glass_mux::{DiffRenderer, VirtualScreen};
use plexy_glass_protocol::errors::CodecError;
use plexy_glass_protocol::{Codec, ServerMsg};
use tokio::io::AsyncWrite;
use tokio::sync::{mpsc, watch};
use tracing::warn;

use crate::error::DaemonError;

/// Out-of-band signals from the connection's input loop to its renderer task.
/// The renderer owns the writer, so anything the input loop wants to SEND to the
/// client (or invalidate) goes through here.
#[derive(Debug)]
pub enum RenderInject {
    Msg(ServerMsg),
    Invalidate,
}

pub struct Renderer {
    diff: DiffRenderer,
}

impl Renderer {
    pub fn new() -> Self {
        Self {
            diff: DiffRenderer::new(),
        }
    }

    /// Set this client's negotiated inline-graphics capabilities so the diff
    /// renderer emits only image protocols the client's terminal supports.
    pub const fn set_graphics_caps(&mut self, caps: plexy_glass_mux::GraphicsCaps) {
        self.diff.set_graphics_caps(caps);
    }

    pub async fn run<W>(
        mut self,
        mut frame_rx: watch::Receiver<Arc<VirtualScreen>>,
        mut switch_rx: mpsc::UnboundedReceiver<watch::Receiver<Arc<VirtualScreen>>>,
        mut inject_rx: mpsc::UnboundedReceiver<RenderInject>,
        mut writer: W,
    ) -> Result<(), DaemonError>
    where
        W: AsyncWrite + Unpin,
    {
        // Send the initial frame immediately so the client sees state on attach.
        {
            let initial = frame_rx.borrow_and_update().clone();
            let bytes = self.diff.render(&initial);
            self.send_output(&mut writer, bytes).await?;
        }
        // `switch_rx` stays open for the connection's lifetime; once the sender
        // drops we stop polling that arm (a `pending` future never resolves)
        // rather than treating it as end-of-session.
        let mut switch_open = true;
        loop {
            tokio::select! {
                biased;
                // A session switch takes effect before draining frame changes.
                maybe = async {
                    if switch_open {
                        switch_rx.recv().await
                    } else {
                        future::pending().await
                    }
                } => {
                    match maybe {
                        Some(new_rx) => {
                            frame_rx = new_rx;
                            // The new session's screen is unrelated to the old
                            // one's; force a full repaint to wipe stale cells.
                            self.diff.invalidate();
                            let frame = frame_rx.borrow_and_update().clone();
                            let bytes = self.diff.render(&frame);
                            self.send_output(&mut writer, bytes).await?;
                        }
                        None => switch_open = false,
                    }
                }
                changed = frame_rx.changed() => {
                    match changed {
                        Ok(()) => {
                            let frame = frame_rx.borrow_and_update().clone();
                            let bytes = self.diff.render(&frame);
                            self.send_output(&mut writer, bytes).await?;
                        }
                        // Session ended (`frame_tx` dropped), teardown unchanged.
                        Err(_) => return Ok(()),
                    }
                }
                inj = inject_rx.recv() => {
                    match inj {
                        Some(RenderInject::Msg(msg)) => self.write_msg(&mut writer, &msg).await?,
                        // Force a full repaint AND push it now. `invalidate()`
                        // only sets the dirty flag; without an immediate render
                        // the client (which just cleared its screen for the
                        // picker) stays blank until the next coordinator tick
                        // (~status.refresh, 5s idle). Mirror the switch arm.
                        // Force a full repaint AND push it now. `invalidate()`
                        // only sets the dirty flag; without an immediate render
                        // the client (which just cleared its screen for the
                        // picker) stays blank until the next coordinator tick
                        // (~status.refresh, 5s idle). Mirror the switch arm.
                        Some(RenderInject::Invalidate) => {
                            self.diff.invalidate();
                            let frame = frame_rx.borrow_and_update().clone();
                            let bytes = self.diff.render(&frame);
                            self.send_output(&mut writer, bytes).await?;
                        }
                        // Input loop gone; connection is ending, `renderer_task`
                        // will be aborted by the caller.
                        None => {}
                    }
                }
            }
        }
    }

    async fn send_output<W>(&mut self, writer: &mut W, bytes: Vec<u8>) -> Result<(), DaemonError>
    where
        W: AsyncWrite + Unpin,
    {
        let msg = ServerMsg::Output(bytes::Bytes::from(bytes));
        self.write_msg(writer, &msg).await
    }

    /// Serialize + frame any `ServerMsg` onto this connection's writer. The
    /// single write path for both regular output frames and out-of-band
    /// injected messages (e.g. `OpenSessionPicker`) — do not add a second one.
    async fn write_msg<W>(&mut self, writer: &mut W, msg: &ServerMsg) -> Result<(), DaemonError>
    where
        W: AsyncWrite + Unpin,
    {
        let payload = postcard::to_allocvec(msg)
            .map_err(|e| DaemonError::Io(Error::other(format!("encode: {e}"))))?;
        match Codec::write_frame(writer, &payload).await {
            Ok(()) => Ok(()),
            // A render frame larger than the transport cap — almost always a
            // first-frame transmit of one or more inline images too big for
            // even MAX_FRAME_BYTES. Dropping the frame keeps the client alive
            // (the over-cap image just doesn't paint) instead of tearing the
            // connection down, which is what the old `?` did. Invalidate so the
            // NEXT frame is a full repaint from a consistent state: without it,
            // the diff would believe it already sent bytes it actually dropped
            // and the pane could never recover.
            Err(CodecError::FrameTooLarge { max, got }) => {
                warn!(
                    max,
                    got,
                    "render frame exceeds MAX_FRAME_BYTES; dropping it (an inline \
                     image too large for one transport frame). The session stays \
                     up; the image will not render."
                );
                self.diff.invalidate();
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use plexy_glass_emulator::Cell;
    use tokio::io;
    use tokio::io::AsyncRead;
    use tokio::time::timeout;

    use super::*;

    fn screen_with(ch: &str) -> Arc<VirtualScreen> {
        let mut s = VirtualScreen::blank(2, 4);
        s.put(
            0,
            0,
            Cell {
                grapheme: ch.into(),
                ..Cell::default()
            },
        );
        Arc::new(s)
    }

    async fn next_output<R: AsyncRead + Unpin>(reader: &mut R) -> String {
        let frame = Codec::read_frame(reader).await.unwrap().unwrap();
        let msg: ServerMsg = postcard::from_bytes(&frame).unwrap();
        match msg {
            ServerMsg::Output(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            other => panic!("expected Output, got {other:?}"),
        }
    }

    // A session switch must rebind the renderer's frame source and emit a full
    // repaint of the new session, even though the new watch channel has not
    // "changed" since it was created.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn switch_rebinds_frame_source_and_full_repaints() {
        let (_tx_a, rx_a) = watch::channel(screen_with("A"));
        let (_tx_b, rx_b) = watch::channel(screen_with("B"));
        let (switch_tx, switch_rx) = mpsc::unbounded_channel();
        let (_inject_tx, inject_rx) = mpsc::unbounded_channel();
        // The renderer writes to `server_sock`; we read its output from the
        // opposite duplex endpoint `client_sock`.
        let (server_sock, mut client_sock) = io::duplex(64 * 1024);

        let task = tokio::spawn(async move {
            let _ = Renderer::new()
                .run(rx_a, switch_rx, inject_rx, server_sock)
                .await;
        });

        // Initial frame is session A.
        assert!(
            next_output(&mut client_sock).await.contains('A'),
            "initial frame is A"
        );

        // Hand the renderer session B's frame stream.
        switch_tx.send(rx_b).unwrap();
        assert!(
            next_output(&mut client_sock).await.contains('B'),
            "post-switch frame is B"
        );

        task.abort();
    }

    // `RenderInject::Invalidate` must EMIT a full frame immediately, not just
    // set the diff's dirty flag. Regression: the old arm only called
    // `invalidate()`, so an Esc-cancel / reselect left the client blank until
    // the next coordinator tick (~5s idle). No `frame_rx` change here — the
    // frame must arrive purely from the inject.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invalidate_emits_a_frame_immediately() {
        let (_tx_a, rx_a) = watch::channel(screen_with("A"));
        let (_switch_tx, switch_rx) = mpsc::unbounded_channel();
        let (inject_tx, inject_rx) = mpsc::unbounded_channel();
        let (server_sock, mut client_sock) = io::duplex(64 * 1024);

        let task = tokio::spawn(async move {
            let _ = Renderer::new()
                .run(rx_a, switch_rx, inject_rx, server_sock)
                .await;
        });

        // Drain the initial attach frame.
        assert!(next_output(&mut client_sock).await.contains('A'));

        inject_tx.send(RenderInject::Invalidate).unwrap();
        // A full repaint of the (unchanged) frame must arrive promptly.
        let out = timeout(Duration::from_secs(2), next_output(&mut client_sock))
            .await
            .expect("Invalidate must emit a frame without waiting for a coordinator tick");
        assert!(out.contains('A'), "invalidate repaint contains the frame");

        task.abort();
    }

    // Dropping the switch sender must not terminate the renderer; it keeps
    // serving the current frame stream.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_switch_sender_does_not_end_renderer() {
        let (tx_a, rx_a) = watch::channel(screen_with("A"));
        let (switch_tx, switch_rx) = mpsc::unbounded_channel();
        let (_inject_tx, inject_rx) = mpsc::unbounded_channel();
        let (server_sock, mut client_sock) = io::duplex(64 * 1024);

        let task = tokio::spawn(async move {
            let _ = Renderer::new()
                .run(rx_a, switch_rx, inject_rx, server_sock)
                .await;
        });
        assert!(next_output(&mut client_sock).await.contains('A'));

        drop(switch_tx); // switch arm parks; renderer stays alive
        tx_a.send(screen_with("Z")).unwrap();
        assert!(
            next_output(&mut client_sock).await.contains('Z'),
            "still serving frames after sender drop"
        );

        task.abort();
    }
}
