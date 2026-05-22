//! A named session: a WindowManager + attached clients + broadcasting renderer.

use crate::{error::DaemonError, window_manager::WindowManager};
use plexy_glass_mux::{PaneId, VirtualScreen};
use plexy_glass_protocol::{ProtocolError, PtySize, SessionEntry, SpawnSpec};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::SystemTime;
use tokio::sync::{mpsc, watch, Mutex, Notify};
use tokio::task::JoinHandle;

async fn render_coordinator(
    session: Arc<Session>,
    frame_tx: watch::Sender<Arc<VirtualScreen>>,
) {
    use plexy_glass_emulator::Screen;
    use plexy_glass_mux::{Compositor, PaneView, StatusLine};
    use std::time::Duration;
    const DEBOUNCE: Duration = Duration::from_millis(16);

    loop {
        session.notify.notified().await;
        // Debounce a few notifications.
        let n = Arc::clone(&session.notify);
        let _ = tokio::time::timeout(DEBOUNCE, async move {
            loop {
                n.notified().await;
            }
        })
        .await;

        let frame = {
            let m = session.window_manager.lock().await;
            if m.is_empty() {
                let host = m.host_size();
                let virt = build_session_end_frame(host);
                let _ = frame_tx.send(Arc::new(virt));
                break;
            }
            let host = m.host_size();
            let viewport = m.viewport();
            let win = m.active_window();
            let layout = win.layout();
            let active_id = win.active();

            let pane_ids = layout.panes();
            let mut owned: Vec<(
                plexy_glass_mux::PaneId,
                plexy_glass_mux::Rect,
                Screen,
                bool,
                u32,
                Option<plexy_glass_mux::CopyMode>,
            )> = Vec::with_capacity(pane_ids.len());
            for id in pane_ids {
                if let Some(pane) = win.pane(id) {
                    let rect = match layout.rect_of(id, viewport) {
                        Some(r) => r,
                        None => continue,
                    };
                    let screen = pane.with_screen(|s| s.clone());
                    let scroll = pane.scroll_offset();
                    let copy_mode = pane.with_copy_mode(|cm| cm.clone());
                    owned.push((id, rect, screen, id == active_id, scroll, copy_mode));
                }
            }
            let views: Vec<PaneView> = owned
                .iter()
                .map(|(id, rect, screen, active, scroll, cm)| PaneView {
                    id: *id,
                    rect: *rect,
                    screen,
                    is_active: *active,
                    scroll_offset: *scroll,
                    copy_mode: cm.as_ref(),
                })
                .collect();

            // Build event-driven widget context, refresh, snapshot.
            let session_name = session.name.clone();
            let attached_clients = session.clients.lock().await.len() as u8;
            let windows_data: Vec<plexy_glass_status::WindowSummary> = m
                .windows()
                .iter()
                .enumerate()
                .map(|(i, w)| plexy_glass_status::WindowSummary {
                    name: w.name.clone(),
                    active: i == m.active_idx(),
                })
                .collect();
            let active_pane_cwd = m
                .active_window()
                .active_pane()
                .and_then(|p| p.with_screen(|s| s.cwd.clone()));
            let copy_mode_active = m
                .active_window()
                .active_pane()
                .map(|p| p.is_in_copy_mode())
                .unwrap_or(false);
            let ctx = plexy_glass_status::EvalContext {
                session_name: &session_name,
                windows: &windows_data,
                active_window: m.active_idx(),
                attached_clients,
                prefix_active: false,
                active_pane_cwd: active_pane_cwd.as_deref(),
                copy_mode_active,
            };
            session.status_engine.refresh_event_driven(&ctx).await;
            // Also flush any interval widgets whose deadline has passed. On
            // the first render this populates widgets the tick task hasn't
            // had a chance to evaluate yet (initial next_due is None, so
            // they're all considered due); on subsequent renders it's a
            // cheap no-op when the tick task is keeping up.
            let _ = session.status_engine.refresh_due_intervals(&ctx).await;
            let snap = session.status_engine.snapshot().await;
            let status = StatusLine {
                left: snap.left.into_iter().flatten().collect(),
                middle: snap.middle.into_iter().flatten().collect(),
                right: snap.right.into_iter().flatten().collect(),
            };
            let selection = m.selection().cloned();

            Compositor::compose(
                &views,
                (host.rows, host.cols),
                Some(&status),
                selection.as_ref(),
            )
        };
        let _ = frame_tx.send(Arc::new(frame));
    }
    session.closing.store(true, Ordering::SeqCst);
    // frame_tx drops here; subscribers will see frame_rx.changed() return Err
    // and exit their loops, which closes their sockets and lets clients restore.
}

fn build_session_end_frame(host: PtySize) -> plexy_glass_mux::VirtualScreen {
    plexy_glass_mux::VirtualScreen::blank(host.rows, host.cols)
}

pub struct ClientHandle {
    pub client_id: u64,
    pub size: PtySize,
    pub frame_rx: watch::Receiver<Arc<VirtualScreen>>,
}

pub struct Session {
    pub name: String,
    pub created: SystemTime,
    pub window_manager: Mutex<WindowManager>,
    pub clients: Mutex<Vec<ClientHandle>>,
    pub notify: Arc<Notify>,
    /// Receiver template: clone into a new `ClientHandle`'s `frame_rx`.
    /// The matching `Sender` lives inside the coordinator task; it drops
    /// when the coordinator exits, signalling end-of-session to all clients.
    pub frame_rx_template: watch::Receiver<Arc<VirtualScreen>>,
    pub death_tx: mpsc::Sender<PaneId>,
    pub closing: AtomicBool,
    next_client_id: AtomicU64,
    coordinator_handle: StdMutex<Option<JoinHandle<()>>>,
    /// Holds the death channel receiver until Task 13 wires up the consumer.
    pub pending_death_rx: Mutex<Option<mpsc::Receiver<PaneId>>>,
    pub status_engine: Arc<plexy_glass_status::EngineInner>,
    status_tick_handle: StdMutex<Option<JoinHandle<()>>>,
    pub config: Arc<plexy_glass_config::Config>,
}

impl Session {
    pub fn new(
        name: String,
        initial_cmd: SpawnSpec,
        first_size: PtySize,
        config: Arc<plexy_glass_config::Config>,
    ) -> Result<Arc<Self>, DaemonError> {
        let notify = Arc::new(Notify::new());
        let (death_tx, death_rx) = mpsc::channel::<PaneId>(16);
        let window_manager = WindowManager::new(
            initial_cmd,
            first_size,
            Arc::clone(&notify),
            Some(death_tx.clone()),
            Arc::clone(&config),
        )?;
        let initial_frame = Arc::new(VirtualScreen::blank(first_size.rows, first_size.cols));
        let (frame_tx, frame_rx_template) = watch::channel(initial_frame);
        let engine = plexy_glass_status::StatusEngine::new(&config.status, &config.palette);
        let status_engine = engine.inner();
        let session = Arc::new(Self {
            name,
            created: SystemTime::now(),
            window_manager: Mutex::new(window_manager),
            clients: Mutex::new(Vec::new()),
            notify,
            frame_rx_template,
            death_tx,
            closing: AtomicBool::new(false),
            next_client_id: AtomicU64::new(0),
            coordinator_handle: StdMutex::new(None),
            pending_death_rx: Mutex::new(Some(death_rx)),
            status_engine,
            status_tick_handle: StdMutex::new(None),
            config,
        });
        let coord_handle = tokio::spawn(render_coordinator(Arc::clone(&session), frame_tx));
        // invariant: no other thread holds coordinator_handle at construction time
        *session.coordinator_handle.lock().expect("coordinator lock poisoned") = Some(coord_handle);

        // Take the receiver out of `pending_death_rx` and spawn the consumer.
        // invariant: pending_death_rx is Some immediately after Session construction
        let death_rx = session
            .pending_death_rx
            .try_lock()
            .expect("pending_death_rx lock: no contention at construction time")
            .take()
            .expect("invariant: pending_death_rx is Some after Session::new");
        let session_for_death = Arc::clone(&session);
        tokio::spawn(async move {
            let mut death_rx = death_rx;
            while let Some(pane_id) = death_rx.recv().await {
                let mut m = session_for_death.window_manager.lock().await;
                let _ = m.handle_pane_death(pane_id);
                let now_empty = m.is_empty();
                drop(m);
                session_for_death.notify.notify_one();
                if now_empty {
                    break;
                }
            }
        });

        // Spawn the status tick task. Capture a `Weak<Session>` so the task
        // doesn't keep the session alive on its own; when the registry
        // drops the session's last strong `Arc` (`kill -n NAME`), the upgrade
        // below returns `None` and the closure produces an empty snapshot.
        // The surrounding tick task will be aborted by `Drop::drop` on
        // `Session`, but until then a missing session still yields a valid
        // (if empty) ctx.
        let session_weak = Arc::downgrade(&session);
        let tick_handle = engine.spawn_tick_task(
            Arc::clone(&session.notify),
            move || match session_weak.upgrade() {
                Some(s) => build_snapshot_ctx(&s),
                None => plexy_glass_status::SnapshotCtx {
                    session_name: String::new(),
                    windows: Vec::new(),
                    active_window: 0,
                    attached_clients: 0,
                    prefix_active: false,
                    active_pane_cwd: None,
                    copy_mode_active: false,
                },
            },
        );
        // invariant: no other thread holds status_tick_handle at construction time
        *session.status_tick_handle.lock().expect("status tick handle lock poisoned") =
            Some(tick_handle);

        Ok(session)
    }

    pub fn list_entry(&self) -> SessionEntry {
        let m = self.window_manager.blocking_lock();
        let clients = self.clients.blocking_lock().len() as u8;
        let windows = m.windows().len() as u8;
        let panes = m
            .windows()
            .iter()
            .map(|w| w.layout().panes().len() as u8)
            .sum();
        SessionEntry {
            name: self.name.clone(),
            windows,
            panes,
            clients,
            created: self.created,
        }
    }

    pub fn register_client(self: &Arc<Self>, size: PtySize) -> Result<ClientHandle, DaemonError> {
        if self.closing.load(Ordering::SeqCst) {
            return Err(DaemonError::Protocol(ProtocolError::SessionNotFound {
                name: self.name.clone(),
            }));
        }
        let client_id = self.next_client_id.fetch_add(1, Ordering::SeqCst);
        let frame_rx_for_caller = self.frame_rx_template.clone();
        let frame_rx_for_session = self.frame_rx_template.clone();
        {
            let mut clients = self.clients.blocking_lock();
            clients.push(ClientHandle {
                client_id,
                size,
                frame_rx: frame_rx_for_session,
            });
        }
        self.recompute_size_and_notify();
        Ok(ClientHandle {
            client_id,
            size,
            frame_rx: frame_rx_for_caller,
        })
    }

    pub fn deregister_client(&self, client_id: u64) {
        {
            let mut clients = self.clients.blocking_lock();
            clients.retain(|c| c.client_id != client_id);
        }
        self.recompute_size_and_notify();
    }

    pub fn effective_size(&self) -> PtySize {
        let clients = self.clients.blocking_lock();
        if clients.is_empty() {
            let m = self.window_manager.blocking_lock();
            return m.host_size();
        }
        let rows = clients.iter().map(|c| c.size.rows).min().unwrap_or(1);
        let cols = clients.iter().map(|c| c.size.cols).min().unwrap_or(1);
        let pw = clients.iter().map(|c| c.size.pixel_width).min().unwrap_or(0);
        let ph = clients.iter().map(|c| c.size.pixel_height).min().unwrap_or(0);
        PtySize { rows, cols, pixel_width: pw, pixel_height: ph }
    }

    pub async fn handle_input_bytes(&self, bytes: &[u8]) -> Result<(), DaemonError> {
        let manager = self.window_manager.lock().await;
        let win = manager.active_window();
        if win.sync_input {
            for id in win.layout().panes() {
                if let Some(pane) = win.pane(id) {
                    pane.send_input(bytes::Bytes::copy_from_slice(bytes)).await.ok();
                }
            }
        } else if let Some(pane) = win.active_pane() {
            pane.send_input(bytes::Bytes::copy_from_slice(bytes)).await.ok();
        }
        drop(manager);
        self.notify.notify_one();
        Ok(())
    }

    pub async fn handle_command(&self, cmd: plexy_glass_mux::Command) -> Result<(), DaemonError> {
        let mut manager = self.window_manager.lock().await;
        manager.handle_command(cmd)?;
        drop(manager);
        self.notify.notify_one();
        Ok(())
    }

    pub async fn handle_mouse(
        &self,
        event: plexy_glass_mux::MouseEvent,
    ) -> Result<(), DaemonError> {
        let mut manager = self.window_manager.lock().await;
        manager.handle_mouse(event).await?;
        drop(manager);
        self.notify.notify_one();
        Ok(())
    }

    pub fn handle_resize(&self, client_id: u64, new_size: PtySize) {
        {
            let mut clients = self.clients.blocking_lock();
            if let Some(c) = clients.iter_mut().find(|c| c.client_id == client_id) {
                c.size = new_size;
            }
        }
        self.recompute_size_and_notify();
    }

    fn recompute_size_and_notify(&self) {
        let new_size = self.effective_size();
        let mut m = self.window_manager.blocking_lock();
        if m.host_size() != new_size {
            let _ = m.on_host_resize(new_size);
        }
        drop(m);
        self.notify.notify_one();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Abort the background tasks so they don't outlive the Session.
        // The status tick task captures Weak<Session>, so by the time we
        // reach Drop the only place that can revive the session is gone.
        if let Some(handle) = self
            .status_tick_handle
            .lock()
            .expect("status tick handle lock poisoned")
            .take()
        {
            handle.abort();
        }
        if let Some(handle) = self
            .coordinator_handle
            .lock()
            .expect("coordinator handle lock poisoned")
            .take()
        {
            handle.abort();
        }
    }
}

/// Build an owned snapshot of session state for the status tick closure.
/// Runs synchronously inside the tick task; uses `blocking_lock` for the
/// async mutexes since the tick task lives on the multi-thread runtime.
fn build_snapshot_ctx(session: &Arc<Session>) -> plexy_glass_status::SnapshotCtx {
    let manager = session.window_manager.blocking_lock();
    let session_name = session.name.clone();
    let attached_clients = session.clients.blocking_lock().len() as u8;
    let active_idx = manager.active_idx();
    let windows: Vec<plexy_glass_status::WindowSummary> = manager
        .windows()
        .iter()
        .enumerate()
        .map(|(i, w)| plexy_glass_status::WindowSummary {
            name: w.name.clone(),
            active: i == active_idx,
        })
        .collect();
    let active_pane_cwd = manager
        .active_window()
        .active_pane()
        .and_then(|p| p.with_screen(|s| s.cwd.clone()));
    let copy_mode_active = manager
        .active_window()
        .active_pane()
        .map(|p| p.is_in_copy_mode())
        .unwrap_or(false);
    plexy_glass_status::SnapshotCtx {
        session_name,
        windows,
        active_window: active_idx,
        attached_clients,
        prefix_active: false,
        active_pane_cwd,
        copy_mode_active,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexy_glass_protocol::SpawnSpec;
    use std::sync::atomic::Ordering;

    fn spec() -> SpawnSpec {
        SpawnSpec {
            program: "/bin/sh".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        }
    }

    fn size() -> PtySize {
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }
    }

    fn cfg() -> Arc<plexy_glass_config::Config> {
        Arc::new(plexy_glass_config::built_in_default())
    }

    #[tokio::test]
    async fn session_construct_succeeds() {
        let s = Session::new("main".into(), spec(), size(), cfg()).expect("construct session");
        assert_eq!(s.name, "main");
        assert!(!s.closing.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn list_entry_reports_one_window_one_pane_zero_clients() {
        let s = Session::new("main".into(), spec(), size(), cfg()).unwrap();
        let entry = tokio::task::spawn_blocking(move || s.list_entry()).await.unwrap();
        assert_eq!(entry.name, "main");
        assert_eq!(entry.windows, 1);
        assert_eq!(entry.panes, 1);
        assert_eq!(entry.clients, 0);
    }

    #[tokio::test]
    async fn register_then_effective_size_matches_single_client() {
        let s = Session::new("main".into(), spec(), size(), cfg()).unwrap();
        let s2 = Arc::clone(&s);
        let h = tokio::task::spawn_blocking(move || {
            s2.register_client(PtySize { rows: 10, cols: 30, pixel_width: 0, pixel_height: 0 })
        })
        .await
        .unwrap()
        .unwrap();
        let s2 = Arc::clone(&s);
        let eff = tokio::task::spawn_blocking(move || s2.effective_size()).await.unwrap();
        assert_eq!((eff.rows, eff.cols), (10, 30));
        let s2 = Arc::clone(&s);
        let cid = h.client_id;
        tokio::task::spawn_blocking(move || s2.deregister_client(cid)).await.unwrap();
    }

    #[tokio::test]
    async fn smallest_client_wins() {
        let s = Session::new("main".into(), spec(), size(), cfg()).unwrap();
        let s2 = Arc::clone(&s);
        let a = tokio::task::spawn_blocking(move || {
            s2.register_client(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
        })
        .await
        .unwrap()
        .unwrap();
        let s2 = Arc::clone(&s);
        let b = tokio::task::spawn_blocking(move || {
            s2.register_client(PtySize { rows: 10, cols: 30, pixel_width: 0, pixel_height: 0 })
        })
        .await
        .unwrap()
        .unwrap();
        let s2 = Arc::clone(&s);
        let eff = tokio::task::spawn_blocking(move || s2.effective_size()).await.unwrap();
        assert_eq!((eff.rows, eff.cols), (10, 30));
        let s2 = Arc::clone(&s);
        let cid_b = b.client_id;
        tokio::task::spawn_blocking(move || s2.deregister_client(cid_b)).await.unwrap();
        let s2 = Arc::clone(&s);
        let eff2 = tokio::task::spawn_blocking(move || s2.effective_size()).await.unwrap();
        assert_eq!((eff2.rows, eff2.cols), (24, 80));
        let s2 = Arc::clone(&s);
        let cid_a = a.client_id;
        tokio::task::spawn_blocking(move || s2.deregister_client(cid_a)).await.unwrap();
    }

    #[tokio::test]
    async fn handle_input_bytes_sends_to_active_pane() {
        let spec = SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };
        let s = Session::new("test".into(), spec, size(), cfg()).unwrap();
        s.handle_input_bytes(b"hello\n").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let m = s.window_manager.lock().await;
        let pane = m.active_window().active_pane().unwrap();
        let saw = pane.with_screen(|screen| {
            (0..screen.active.num_cols())
                .filter_map(|c| {
                    screen.active.get_cell(0, c).map(|cell| cell.grapheme.as_str().to_string())
                })
                .collect::<Vec<_>>()
                .join("")
        });
        assert!(saw.contains("hello"), "expected 'hello' in active grid; got {saw:?}");
        let _ = pane.send_input(bytes::Bytes::from_static(&[0x04])).await;
    }

    #[tokio::test]
    async fn handle_input_bytes_broadcasts_when_sync_active() {
        let spec = SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };
        let s = Session::new("test".into(), spec, size(), cfg()).unwrap();
        // Split into two panes and enable sync-input mode.
        s.handle_command(plexy_glass_mux::Command::SplitV).await.unwrap();
        s.handle_command(plexy_glass_mux::Command::ToggleSyncPanes).await.unwrap();
        // Broadcast input to both panes.
        s.handle_input_bytes(b"hello\n").await.unwrap();
        // Give children time to echo.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let m = s.window_manager.lock().await;
        let win = m.active_window();
        let panes = win.layout().panes();
        assert_eq!(panes.len(), 2, "expected two panes after split");
        for id in &panes {
            let pane = win.pane(*id).expect("pane must exist");
            let saw = pane.with_screen(|screen| {
                (0..screen.active.num_cols())
                    .filter_map(|c| {
                        screen.active.get_cell(0, c).map(|cell| cell.grapheme.as_str().to_string())
                    })
                    .collect::<Vec<_>>()
                    .join("")
            });
            assert!(saw.contains("hello"), "pane {id:?} missing 'hello' broadcast: {saw:?}");
        }
        // Cleanup: send EOF to each pane.
        for id in &panes {
            if let Some(p) = win.pane(*id) {
                let _ = p.send_input(bytes::Bytes::from_static(&[0x04])).await;
            }
        }
    }

    #[tokio::test]
    async fn closing_session_refuses_register() {
        let s = Session::new("main".into(), spec(), size(), cfg()).unwrap();
        s.closing.store(true, Ordering::SeqCst);
        let s2 = Arc::clone(&s);
        let result =
            tokio::task::spawn_blocking(move || s2.register_client(size())).await.unwrap();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn coordinator_publishes_initial_frame() {
        let s = Session::new("test".into(), spec(), size(), cfg()).unwrap();
        let mut rx = s.frame_rx_template.clone();
        s.notify.notify_one();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            rx.changed(),
        )
        .await;
        assert!(result.is_ok(), "expected a frame within 1s");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn coordinator_emits_tail_frame_when_last_pane_dies() {
        let spec = SpawnSpec {
            program: "/bin/echo".into(),
            args: vec!["hi".into()],
            env: vec![],
            cwd: None,
        };
        let s = Session::new("test".into(), spec, size(), cfg()).unwrap();
        // Wait up to 5s for the session to close (echo exits, then the death consumer
        // reports it, then the coordinator observes is_empty and sets closing=true).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if s.closing.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(s.closing.load(Ordering::SeqCst), "session did not converge to closing");
    }
}
