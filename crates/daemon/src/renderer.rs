//! Per-client renderer: composes the active window, diffs against last frame,
//! emits ANSI as ServerMsg::Output.

use crate::{error::DaemonError, window_manager::WindowManager};
use bytes::Bytes;
use plexy_glass_emulator::Screen;
use plexy_glass_mux::{Compositor, DiffRenderer, PaneView, StatusLine, WindowEntry};
use plexy_glass_protocol::{Codec, ServerMsg};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWrite;
use tokio::sync::{Mutex, Notify};

const DEBOUNCE: Duration = Duration::from_millis(16);

pub struct Renderer {
    diff: DiffRenderer,
}

impl Renderer {
    pub fn new() -> Self {
        Self {
            diff: DiffRenderer::new(),
        }
    }

    pub async fn run<W>(
        mut self,
        manager: Arc<Mutex<WindowManager>>,
        notify: Arc<Notify>,
        prefix_active: Arc<std::sync::atomic::AtomicBool>,
        mut writer: W,
    ) -> Result<(), DaemonError>
    where
        W: AsyncWrite + Unpin,
    {
        // First render immediately so the client sees an initial frame.
        if let Err(e) = self
            .render_once(&manager, &prefix_active, &mut writer)
            .await
        {
            tracing::warn!(error = %e, "initial render failed");
            return Err(e);
        }
        loop {
            notify.notified().await;
            // Debounce: collect a few notifications during the window. The
            // inner future awaits the same Notify; the timeout drops it.
            let n = Arc::clone(&notify);
            let _ = tokio::time::timeout(DEBOUNCE, async move {
                loop {
                    n.notified().await;
                }
            })
            .await;
            if let Err(e) = self
                .render_once(&manager, &prefix_active, &mut writer)
                .await
            {
                tracing::warn!(error = %e, "render failed; closing renderer");
                return Err(e);
            }
            // Exit cleanly when the session has ended (last pane closed).
            // The final render above already flushed the closing-frame bytes
            // through the wire; emit one short tail sequence so the host
            // shell takes over from a known cursor position with no leaked
            // SGR or hidden-cursor state, then drop the writer.
            if manager.lock().await.is_empty() {
                // SGR reset, then cursor to bottom-left of the host TTY,
                // then a newline so the cursor advances onto a fresh row
                // (the host scrolls if needed). Finally re-show the cursor
                // in case the diff renderer's last frame hid it.
                let host = manager.lock().await.host_size();
                let tail = format!("\x1b[0m\x1b[{};1H\r\n\x1b[?25h", host.rows);
                let msg = ServerMsg::Output(Bytes::from(tail.into_bytes()));
                if let Ok(payload) = postcard::to_allocvec(&msg) {
                    let _ = Codec::write_frame(&mut writer, &payload).await;
                }
                return Ok(());
            }
        }
    }

    async fn render_once<W>(
        &mut self,
        manager: &Arc<Mutex<WindowManager>>,
        prefix_active: &Arc<std::sync::atomic::AtomicBool>,
        writer: &mut W,
    ) -> Result<(), DaemonError>
    where
        W: AsyncWrite + Unpin,
    {
        let bytes = {
            let m = manager.lock().await;
            if m.is_empty() {
                return Ok(());
            }
            let host = m.host_size();
            let viewport = m.viewport();
            let win = m.active_window();
            let layout = win.layout();
            let active_id = win.active();

            let pane_ids = layout.panes();
            let mut owned_screens: Vec<(
                plexy_glass_mux::PaneId,
                plexy_glass_mux::Rect,
                Screen,
                bool,
            )> = Vec::with_capacity(pane_ids.len());
            for id in pane_ids {
                if let Some(pane) = win.pane(id) {
                    let rect = match layout.rect_of(id, viewport) {
                        Some(r) => r,
                        None => continue,
                    };
                    let screen = pane.with_screen(|s| s.clone());
                    owned_screens.push((id, rect, screen, id == active_id));
                }
            }
            let views: Vec<PaneView> = owned_screens
                .iter()
                .map(|(id, rect, screen, active)| PaneView {
                    id: *id,
                    rect: *rect,
                    screen,
                    is_active: *active,
                })
                .collect();

            let windows: Vec<WindowEntry> = m
                .windows()
                .iter()
                .enumerate()
                .map(|(i, w)| WindowEntry {
                    id: w.id,
                    name: w.name.clone(),
                    active: i == m.active_idx(),
                })
                .collect();
            let status = StatusLine {
                windows,
                prefix_active: prefix_active.load(std::sync::atomic::Ordering::SeqCst),
            };

            let virt = Compositor::compose(&views, (host.rows, host.cols), Some(&status), None);
            self.diff.render(&virt)
        };
        let msg = ServerMsg::Output(Bytes::from(bytes));
        let payload = postcard::to_allocvec(&msg)
            .map_err(|e| DaemonError::Io(std::io::Error::other(format!("encode: {e}"))))?;
        Codec::write_frame(writer, &payload).await?;
        Ok(())
    }
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}
