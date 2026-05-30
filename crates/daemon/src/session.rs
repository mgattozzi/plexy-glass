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

        // Kill teardown: when the session is closing, emit a final blank
        // frame and exit so frame_tx drops and attached clients detach.
        if session.closing.load(Ordering::SeqCst) {
            let host = { session.window_manager.lock().await.host_size() };
            let _ = frame_tx.send(Arc::new(build_session_end_frame(host)));
            break;
        }

        let frame = {
            let mut m = session.window_manager.lock().await;
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
            let zoomed = win.zoomed;

            // When zoomed, render ONLY the zoomed pane at the full viewport;
            // otherwise render every pane at its layout rect.
            let pane_ids: Vec<plexy_glass_mux::PaneId> = match zoomed {
                Some(zid) => vec![zid],
                None => layout.panes(),
            };
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
                    let rect = if zoomed == Some(id) {
                        viewport
                    } else {
                        match layout.rect_of(id, viewport) {
                            Some(r) => r,
                            None => continue,
                        }
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
            let sync_active = m.active_window().sync_input;
            let zoom_active = m.active_window().is_zoomed();
            let ctx = plexy_glass_status::EvalContext {
                session_name: &session_name,
                windows: &windows_data,
                active_window: m.active_idx(),
                attached_clients,
                prefix_active: false,
                active_pane_cwd: active_pane_cwd.as_deref(),
                copy_mode_active,
                sync_active,
                zoom_active,
            };
            let engine = session.status_engine_snapshot();
            engine.refresh_event_driven(&ctx).await;
            // Also flush any interval widgets whose deadline has passed. On
            // the first render this populates widgets the tick task hasn't
            // had a chance to evaluate yet (initial next_due is None, so
            // they're all considered due); on subsequent renders it's a
            // cheap no-op when the tick task is keeping up.
            let _ = engine.refresh_due_intervals(&ctx).await;
            let snap = engine.snapshot().await;
            // Push clickable regions to the window manager so the next
            // status-bar click can dispatch the matching command (M10).
            let hits = snap.click_hits();
            let host_size = m.host_size();
            // Honor the configured status-bar position for both the click row
            // and the compositor placement.
            let placement = match session.config_snapshot().status.position {
                plexy_glass_config::Position::Top => plexy_glass_mux::StatusPlacement::Top,
                plexy_glass_config::Position::Bottom => plexy_glass_mux::StatusPlacement::Bottom,
            };
            let (status_row, pane_row_offset) = match placement {
                plexy_glass_mux::StatusPlacement::Top => (0u16, 1u16),
                plexy_glass_mux::StatusPlacement::Bottom => {
                    (host_size.rows.saturating_sub(1), 0u16)
                }
            };
            m.set_status_layout(Some(status_row), pane_row_offset);
            m.set_status_hits(hits);
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
                placement,
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
    status_engine_slot: StdMutex<Arc<plexy_glass_status::EngineInner>>,
    status_tick_handle: StdMutex<Option<JoinHandle<()>>>,
    config_slot: StdMutex<Arc<plexy_glass_config::Config>>,
    /// JoinHandle for the death-consumer task. It pins a strong `Arc` (blocked
    /// on `death_rx.recv()`), so teardown must abort it explicitly, since `Drop`
    /// can never run while it holds the `Arc`.
    death_handle: StdMutex<Option<JoinHandle<()>>>,
    /// True iff structural state changed since the last successful save.
    /// Set by `mark_dirty`; cleared by the persist task before snapshotting.
    pub dirty: std::sync::atomic::AtomicBool,
    /// Wake the persist task. Multiple writers OK; single waiter.
    pub persist_notify: Arc<Notify>,
    /// JoinHandle for the persist task; aborted in `Drop`.
    persist_handle: StdMutex<Option<JoinHandle<()>>>,
}

impl Session {
    /// Snapshot the current active config Arc. Hot reload (Task 8) swaps the
    /// inner Arc; callers should call this each time they need a current view
    /// of the config rather than caching across awaits.
    pub fn config_snapshot(&self) -> Arc<plexy_glass_config::Config> {
        // invariant: config_slot mutex is held briefly; no .await holding the lock.
        self.config_slot.lock().expect("config_slot poisoned").clone()
    }

    /// Snapshot the current status engine Arc. Hot reload swaps the inner
    /// Arc when the status config changes.
    pub fn status_engine_snapshot(&self) -> Arc<plexy_glass_status::EngineInner> {
        // invariant: status_engine_slot mutex is held briefly; no .await holding the lock.
        self.status_engine_slot.lock().expect("status_engine_slot poisoned").clone()
    }

    /// Build a `SessionStateV1` reflecting current in-memory state. Caller
    /// must hold the `window_manager` lock; the snapshot is point-in-time
    /// consistent with that lock window. Serialization happens off-lock.
    pub fn snapshot_for_persist(
        &self,
        wm: &WindowManager,
    ) -> crate::persist::SessionStateV1 {
        use crate::persist::{
            LayoutDirV1, LayoutStateV1, PaneStateV1, SCHEMA_VERSION, SessionStateV1, WindowStateV1,
        };
        let windows = wm
            .windows()
            .iter()
            .map(|w| {
                let layout_tree = w.layout();
                let leaves = layout_tree.dfs_leaves();
                let panes: Vec<PaneStateV1> = leaves
                    .iter()
                    .map(|pid| {
                        let cwd = w
                            .pane(*pid)
                            .and_then(|p| p.with_screen(|s| s.cwd.clone()));
                        PaneStateV1 { cwd }
                    })
                    .collect();
                let layout = layout_tree
                    .map_layout(
                        |_pane_id, idx| LayoutStateV1::Leaf(idx),
                        |dir, ratio, first, second| LayoutStateV1::Split {
                            dir: match dir {
                                plexy_glass_mux::SplitDir::Vertical => LayoutDirV1::Vertical,
                                plexy_glass_mux::SplitDir::Horizontal => LayoutDirV1::Horizontal,
                            },
                            ratio,
                            first: Box::new(first),
                            second: Box::new(second),
                        },
                    )
                    // invariant: WindowManager never holds a window with an empty layout.
                    .unwrap_or(LayoutStateV1::Leaf(0));
                let active_pane_id = w.active();
                let active_pane = leaves
                    .iter()
                    .position(|p| *p == active_pane_id)
                    .map(|i| i as u32)
                    .unwrap_or(0);
                WindowStateV1 {
                    name: w.name.clone(),
                    sync_input: w.sync_input,
                    active_pane,
                    panes,
                    layout,
                }
            })
            .collect();
        SessionStateV1 {
            schema: SCHEMA_VERSION,
            name: self.name.clone(),
            created: chrono::DateTime::<chrono::Utc>::from(self.created),
            active_window: wm.active_idx(),
            windows,
        }
    }

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
            status_engine_slot: StdMutex::new(status_engine),
            status_tick_handle: StdMutex::new(None),
            config_slot: StdMutex::new(config),
            dirty: std::sync::atomic::AtomicBool::new(false),
            persist_notify: Arc::new(Notify::new()),
            persist_handle: StdMutex::new(None),
            death_handle: StdMutex::new(None),
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
        let death_task = tokio::spawn(async move {
            let mut death_rx = death_rx;
            while let Some(pane_id) = death_rx.recv().await {
                let mut m = session_for_death.window_manager.lock().await;
                let _ = m.handle_pane_death(pane_id);
                let now_empty = m.is_empty();
                drop(m);
                session_for_death.notify.notify_one();
                session_for_death.mark_dirty();
                if now_empty {
                    break;
                }
            }
        });
        // invariant: no other thread holds death_handle at construction time.
        *session.death_handle.lock().expect("death handle lock poisoned") = Some(death_task);

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
            move || {
                let weak = session_weak.clone();
                async move {
                    match weak.upgrade() {
                        Some(s) => build_snapshot_ctx(&s).await,
                        None => empty_snapshot_ctx(),
                    }
                }
            },
        );
        // invariant: no other thread holds status_tick_handle at construction time
        *session.status_tick_handle.lock().expect("status tick handle lock poisoned") =
            Some(tick_handle);

        // Spawn the persist task. Uses `Weak<Session>` so it exits naturally
        // when the registry drops the last strong `Arc`.
        let persist_weak = Arc::downgrade(&session);
        let persist_task = tokio::spawn(persist_loop(persist_weak));
        // invariant: no other thread holds persist_handle at construction time
        *session.persist_handle.lock().expect("persist_handle lock poisoned") =
            Some(persist_task);

        Ok(session)
    }

    /// Mark structural state changed. The persist task picks this up,
    /// debounces 1500ms, and writes the latest snapshot to disk.
    pub fn mark_dirty(&self) {
        self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        self.persist_notify.notify_one();
    }

    /// Deterministically tear the session down. Idempotent. Stops the persist
    /// task FIRST so it cannot re-save during teardown, aborts the
    /// death-consumer (blocked on recv, pins an Arc) and status-tick task,
    /// then wakes the coordinator so it observes `closing`, emits a final
    /// blank frame, and exits (dropping `frame_tx` so attached clients detach).
    /// Pane children are terminated separately via `terminate_panes`.
    pub fn begin_close(&self) {
        self.closing.store(true, Ordering::SeqCst);
        // NB: the persist task is NOT aborted here, it must be stopped
        // *and awaited* (see `stop_persist`) before `kill` deletes the file,
        // because `JoinHandle::abort` is cooperative and `save_session` has no
        // await point, so a fire-and-forget abort could let an in-flight save
        // re-create the file after deletion. `closing` (set above) makes the
        // task bail at its next poll; `stop_persist` guarantees it has fully
        // stopped. Drop still aborts persist as a backstop.
        if let Some(h) = self
            .death_handle
            .lock()
            .expect("death handle lock poisoned")
            .take()
        {
            h.abort();
        }
        if let Some(h) = self
            .status_tick_handle
            .lock()
            .expect("status tick handle lock poisoned")
            .take()
        {
            h.abort();
        }
        self.notify.notify_one();
    }

    /// Abort the persist task and WAIT for it to fully stop. Must be called
    /// (and awaited) before deleting a session's saved file on `kill`: a bare
    /// `abort()` is cooperative and cannot interrupt a synchronous
    /// `save_session`, so without this await an in-flight save could land the
    /// file back on disk *after* `delete_session` removed it.
    pub async fn stop_persist(&self) {
        let handle = self
            .persist_handle
            .lock()
            .expect("persist handle lock poisoned")
            .take();
        if let Some(h) = handle {
            h.abort();
            // Await the task's termination (Err = cancelled, Ok = it finished
            // its current save first). Either way it is dead afterwards.
            let _ = h.await;
        }
    }

    /// Terminate every pane's child process. Async because it needs the
    /// window-manager lock. Safe to call after `begin_close`. Dropping panes
    /// alone does not SIGHUP children (the reader thread holds the PTY), so
    /// this is required for `kill` to actually end the children.
    pub async fn terminate_panes(&self) {
        let wm = self.window_manager.lock().await;
        for w in wm.windows() {
            for id in w.layout().panes() {
                if let Some(p) = w.pane(id) {
                    p.kill_child();
                }
            }
        }
    }

    /// Build a Session from a saved on-disk state. The base shell is the
    /// same as `new`; we then replay structural changes (splits, extra
    /// windows, names, sync_input, focus) to reach the saved layout.
    /// Each restored pane spawns the caller-supplied `base_spec` with cwd
    /// set from the saved state. Split ratios reset to 50/50 (a v1
    /// limitation; users can mouse-drag to restore).
    pub async fn restore_from(
        saved: crate::persist::SessionStateV1,
        base_spec: plexy_glass_protocol::SpawnSpec,
        size: PtySize,
        config: Arc<plexy_glass_config::Config>,
    ) -> Result<Arc<Self>, DaemonError> {
        let first_window = saved.windows.first().ok_or_else(|| {
            DaemonError::Io(std::io::Error::other("restored session has zero windows"))
        })?;
        let first_pane_saved = first_window.panes.first().ok_or_else(|| {
            DaemonError::Io(std::io::Error::other("restored window has zero panes"))
        })?;
        let mut first_spec = base_spec.clone();
        first_spec.cwd = first_pane_saved.cwd.clone();

        let session = Self::new(saved.name.clone(), first_spec, size, Arc::clone(&config))?;
        {
            let mut wm = session.window_manager.lock().await;
            // Window 0 already exists from Session::new with its first pane, so
            // restore its name + remaining panes via replay.
            wm.set_window_name(0, first_window.name.clone());
            replay_window_layout(&mut wm, 0, first_window, &base_spec)?;
            for (wi, w) in saved.windows.iter().enumerate().skip(1) {
                let first_pane = w.panes.first().ok_or_else(|| {
                    DaemonError::Io(std::io::Error::other(format!(
                        "restored window {wi} has zero panes"
                    )))
                })?;
                let mut spec_for_first = base_spec.clone();
                spec_for_first.cwd = first_pane.cwd.clone();
                wm.new_window_with_spec(spec_for_first, w.name.clone())?;
                replay_window_layout(&mut wm, wi, w, &base_spec)?;
            }
            // Restore per-window flags + active-pane focus.
            for (i, saved_w) in saved.windows.iter().enumerate() {
                if let Some(win) = wm.windows_mut().get_mut(i) {
                    win.sync_input = saved_w.sync_input;
                    let leaves = win.layout().dfs_leaves();
                    if let Some(pid) = leaves.get(saved_w.active_pane as usize) {
                        win.focus(*pid);
                    }
                }
            }
            let active = saved
                .active_window
                .min(wm.windows().len().saturating_sub(1));
            wm.set_active_window(active);
        }
        // Round-trip: re-save the restored shape (also catches any drift
        // between the saved file and what we actually built).
        session.mark_dirty();
        Ok(session)
    }
}

/// Replay a saved layout for `window_idx`. The window's first pane is
/// already present; we walk the saved layout depth-first, splitting the
/// existing structure at each Split node to spawn the next pane.
fn replay_window_layout(
    wm: &mut WindowManager,
    window_idx: usize,
    saved: &crate::persist::WindowStateV1,
    base_spec: &plexy_glass_protocol::SpawnSpec,
) -> Result<(), DaemonError> {
    let mut ops: Vec<ReplayOp> = Vec::new();
    collect_replay_ops(&saved.layout, 0, &mut ops);
    for op in ops {
        let mut spec = base_spec.clone();
        spec.cwd = saved
            .panes
            .get(op.new_pane_dfs_idx as usize)
            .and_then(|p| p.cwd.clone());
        wm.split_window_at_dfs(window_idx, op.target_dfs_idx, op.dir, spec)?;
    }
    Ok(())
}

struct ReplayOp {
    /// DFS index of the existing pane being split.
    target_dfs_idx: u32,
    /// DFS index that the newly-spawned pane will occupy AFTER the split.
    new_pane_dfs_idx: u32,
    dir: plexy_glass_mux::SplitDir,
}

fn collect_replay_ops(node: &crate::persist::LayoutStateV1, base_dfs: u32, out: &mut Vec<ReplayOp>) {
    use crate::persist::{LayoutDirV1, LayoutStateV1};
    match node {
        LayoutStateV1::Leaf(_) => {}
        LayoutStateV1::Split { dir, first, second, .. } => {
            let target = leftmost_leaf_dfs(first, base_dfs);
            let first_size = count_leaves(first);
            let new_pane = base_dfs + first_size;
            out.push(ReplayOp {
                target_dfs_idx: target,
                new_pane_dfs_idx: new_pane,
                dir: match dir {
                    LayoutDirV1::Vertical => plexy_glass_mux::SplitDir::Vertical,
                    LayoutDirV1::Horizontal => plexy_glass_mux::SplitDir::Horizontal,
                },
            });
            collect_replay_ops(first, base_dfs, out);
            collect_replay_ops(second, base_dfs + first_size, out);
        }
    }
}

fn leftmost_leaf_dfs(node: &crate::persist::LayoutStateV1, base: u32) -> u32 {
    match node {
        crate::persist::LayoutStateV1::Leaf(_) => base,
        crate::persist::LayoutStateV1::Split { first, .. } => leftmost_leaf_dfs(first, base),
    }
}

fn count_leaves(node: &crate::persist::LayoutStateV1) -> u32 {
    match node {
        crate::persist::LayoutStateV1::Leaf(_) => 1,
        crate::persist::LayoutStateV1::Split { first, second, .. } => {
            count_leaves(first) + count_leaves(second)
        }
    }
}

impl Session {
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
        // Lock-order discipline: every dual-lock site must take window_manager
        // BEFORE clients (see render_coordinator / build_snapshot_ctx). So we
        // must NOT hold the clients guard while acquiring window_manager, since
        // that would be a clients->WM order, inverting against the WM->clients
        // sites and risking an AB-BA deadlock (esp. at last-client-detach, the
        // empty branch below). Read what we need from clients, release that
        // guard, then take window_manager separately.
        let sizes: Option<PtySize> = {
            let clients = self.clients.blocking_lock();
            if clients.is_empty() {
                None
            } else {
                Some(PtySize {
                    rows: clients.iter().map(|c| c.size.rows).min().unwrap_or(1),
                    cols: clients.iter().map(|c| c.size.cols).min().unwrap_or(1),
                    pixel_width: clients.iter().map(|c| c.size.pixel_width).min().unwrap_or(0),
                    pixel_height: clients.iter().map(|c| c.size.pixel_height).min().unwrap_or(0),
                })
            }
        };
        match sizes {
            Some(s) => s,
            // No clients: fall back to the current host size. The clients guard
            // is already released, so this takes window_manager alone.
            None => self.window_manager.blocking_lock().host_size(),
        }
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
        self.mark_dirty();
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
        // Mouse drives border drag-resize + status-bar commands; both change
        // structural state. handle_input_bytes is unrelated.
        self.mark_dirty();
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
        let resized = m.host_size() != new_size;
        if resized {
            let _ = m.on_host_resize(new_size);
        }
        drop(m);
        self.notify.notify_one();
        // Resize may have clamped split ratios at min-size, so persist the new
        // shape.
        if resized {
            self.mark_dirty();
        }
    }

    /// Replace this session's active config Arc, rebuild the status engine
    /// + tick task, and push the new config Arc to every live pane.
    ///
    /// Order of operations matters:
    /// 1. swap the config slot first so `build_snapshot_ctx` and any other
    ///    consumer that reads `config_snapshot()` after this call sees the new
    ///    config;
    /// 2. abort the old tick task before spawning the new one, so we don't
    ///    leak tasks;
    /// 3. install the new status engine + tick handle;
    /// 4. wake the render coordinator so the new engine/palette take effect
    ///    on the next frame;
    /// 5. push the new config to each Pane so OSC color queries (T3) use
    ///    the new palette.
    pub async fn swap_config(self: &Arc<Self>, new_config: Arc<plexy_glass_config::Config>) {
        // (1) Update the config slot first.
        {
            // invariant: config_slot mutex is held briefly; no .await holding the lock.
            let mut slot = self.config_slot.lock().expect("config_slot poisoned");
            *slot = Arc::clone(&new_config);
        }

        // Build a fresh `StatusEngine` + tick task.
        let new_engine =
            plexy_glass_status::StatusEngine::new(&new_config.status, &new_config.palette);
        let new_inner = new_engine.inner();

        // (2) Abort the old tick before spawning a new one.
        {
            // invariant: status_tick_handle mutex held briefly; no .await holding the lock.
            let mut slot = self
                .status_tick_handle
                .lock()
                .expect("status_tick_handle poisoned");
            if let Some(old_tick) = slot.take() {
                old_tick.abort();
            }
        }

        // (3) Install the new engine.
        {
            // invariant: status_engine_slot mutex held briefly; no .await holding the lock.
            let mut slot = self
                .status_engine_slot
                .lock()
                .expect("status_engine_slot poisoned");
            *slot = new_inner;
        }

        let session_weak = Arc::downgrade(self);
        let tick_handle = new_engine.spawn_tick_task(
            Arc::clone(&self.notify),
            move || {
                let weak = session_weak.clone();
                async move {
                    match weak.upgrade() {
                        Some(s) => build_snapshot_ctx(&s).await,
                        None => empty_snapshot_ctx(),
                    }
                }
            },
        );
        {
            // invariant: status_tick_handle mutex held briefly; no .await holding the lock.
            let mut slot = self
                .status_tick_handle
                .lock()
                .expect("status_tick_handle poisoned");
            *slot = Some(tick_handle);
        }

        // (4) Wake the render coordinator so the new engine + palette apply
        // immediately on the next frame.
        self.notify.notify_one();

        // (5) Push the new config to every Pane so reader threads pick up
        // the new palette for OSC color queries (T3 stored config on Pane).
        let manager = self.window_manager.lock().await;
        for win in manager.windows() {
            for id in win.layout().panes() {
                if let Some(pane) = win.pane(id) {
                    pane.update_config(Arc::clone(&new_config));
                }
            }
        }
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
        if let Some(handle) = self
            .persist_handle
            .lock()
            .expect("persist handle lock poisoned")
            .take()
        {
            handle.abort();
        }
        if let Some(handle) = self
            .death_handle
            .lock()
            .expect("death handle lock poisoned")
            .take()
        {
            handle.abort();
        }
    }
}

/// Background persist task. Awaits the session's `persist_notify`, sleeps
/// 1.5s to coalesce a burst of changes, then if `dirty` is still set,
/// snapshots state + writes atomically. Exits when the session is dropped.
async fn persist_loop(weak: std::sync::Weak<Session>) {
    loop {
        let Some(session) = weak.upgrade() else { return };
        if session.closing.load(Ordering::SeqCst) {
            return;
        }
        let notify = Arc::clone(&session.persist_notify);
        drop(session);
        notify.notified().await;
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let Some(session) = weak.upgrade() else { return };
        // Never resurrect a file after kill: bail if the session is closing.
        if session.closing.load(Ordering::SeqCst) {
            return;
        }
        if !session.dirty.swap(false, std::sync::atomic::Ordering::Relaxed) {
            continue;
        }
        let snap = {
            let wm = session.window_manager.lock().await;
            session.snapshot_for_persist(&wm)
        };
        if let Err(e) = crate::persist::save_session(&snap) {
            tracing::warn!(error = %e, name = %session.name, "session persist failed");
        }
    }
}

/// An empty `SnapshotCtx` for the case where the `Weak<Session>` held by the
/// status tick task can no longer upgrade, i.e. the session has been dropped.
/// The tick task is normally aborted on Drop, but a tick may have already
/// started; in that case we return a benign default so widgets render as if
/// no session were attached.
fn empty_snapshot_ctx() -> plexy_glass_status::SnapshotCtx {
    plexy_glass_status::SnapshotCtx {
        session_name: String::new(),
        windows: Vec::new(),
        active_window: 0,
        attached_clients: 0,
        prefix_active: false,
        active_pane_cwd: None,
        copy_mode_active: false,
        sync_active: false,
        zoom_active: false,
    }
}

/// Build an owned snapshot of session state for the status tick closure.
/// MUST be async (not `blocking_lock`): the tick task runs on a runtime
/// worker thread, where `tokio::sync::Mutex::blocking_lock` panics
/// ("Cannot block the current thread from within a runtime"). Using the
/// async lock is also runtime-agnostic (works on current-thread test runtimes).
async fn build_snapshot_ctx(session: &Arc<Session>) -> plexy_glass_status::SnapshotCtx {
    let manager = session.window_manager.lock().await;
    let session_name = session.name.clone();
    let attached_clients = session.clients.lock().await.len() as u8;
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
    let sync_active = manager.active_window().sync_input;
    let zoom_active = manager.active_window().is_zoomed();
    plexy_glass_status::SnapshotCtx {
        session_name,
        windows,
        active_window: active_idx,
        attached_clients,
        prefix_active: false,
        active_pane_cwd,
        copy_mode_active,
        sync_active,
        zoom_active,
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

    // Regression: `build_snapshot_ctx` used `blocking_lock` and was driven by the
    // status tick task on a runtime worker thread, which PANICS ("Cannot block
    // the current thread from within a runtime"). It is now async, so calling it
    // from a spawned task (a worker thread on the multi-thread runtime, the exact
    // scenario the tick task hits) must succeed and return real state.
    #[tokio::test(flavor = "multi_thread")]
    async fn build_snapshot_ctx_works_from_spawned_task() {
        let s = Session::new("snapctx".into(), spec(), size(), cfg()).unwrap();
        let s2 = Arc::clone(&s);
        let ctx = tokio::spawn(async move { build_snapshot_ctx(&s2).await })
            .await
            .expect("tick-style snapshot task must not panic");
        assert_eq!(ctx.session_name, "snapctx");
        assert_eq!(ctx.windows.len(), 1);
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

    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        old_xdg: Option<std::ffi::OsString>,
        _tmp: tempfile::TempDir,
    }

    fn test_isolate_state_dir() -> EnvGuard {
        // Crate-wide lock: serializes against persist/registry env-mutating tests.
        let lock = crate::STATE_ENV_LOCK.lock().expect("env mutex poisoned");
        let tmp = tempfile::tempdir().expect("tempdir");
        let old_xdg = std::env::var_os("XDG_STATE_HOME");
        // SAFETY: env mutation guarded by `ENV_LOCK` for the guard's lifetime.
        unsafe {
            std::env::set_var("XDG_STATE_HOME", tmp.path());
        }
        EnvGuard { _lock: lock, old_xdg, _tmp: tmp }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: `ENV_LOCK` is held for `self`'s lifetime.
            unsafe {
                match &self.old_xdg {
                    Some(v) => std::env::set_var("XDG_STATE_HOME", v),
                    None => std::env::remove_var("XDG_STATE_HOME"),
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restore_from_round_trips_single_pane_session() {
        let _g = test_isolate_state_dir();
        let original = Session::new("rt".into(), spec(), size(), cfg()).unwrap();
        original.mark_dirty();
        tokio::time::sleep(std::time::Duration::from_millis(1800)).await;
        drop(original);
        let saved = crate::persist::load_session("rt")
            .expect("load")
            .expect("file");
        let restored = Session::restore_from(saved, spec(), size(), cfg()).await.unwrap();
        let wm = restored.window_manager.lock().await;
        assert_eq!(wm.windows().len(), 1);
        assert_eq!(wm.windows()[0].layout().panes().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restore_from_round_trips_two_pane_split() {
        let _g = test_isolate_state_dir();
        let original = Session::new("rt2".into(), spec(), size(), cfg()).unwrap();
        original
            .handle_command(plexy_glass_mux::Command::SplitV)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1800)).await;
        drop(original);
        let saved = crate::persist::load_session("rt2")
            .expect("load")
            .expect("file");
        let restored = Session::restore_from(saved, spec(), size(), cfg()).await.unwrap();
        let wm = restored.window_manager.lock().await;
        assert_eq!(wm.windows()[0].layout().panes().len(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn split_command_writes_persisted_layout() {
        let _g = test_isolate_state_dir();
        let s = Session::new("p5-split".into(), spec(), size(), cfg()).unwrap();
        s.handle_command(plexy_glass_mux::Command::SplitV).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1800)).await;
        let loaded = crate::persist::load_session("p5-split")
            .expect("load")
            .expect("file");
        assert_eq!(loaded.windows[0].panes.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kill_closes_split_unix_socket_to_client() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let _g = test_isolate_state_dir();
        let s = Session::new("sp".into(), spec(), size(), cfg()).unwrap();
        let handle = tokio::task::block_in_place(|| s.register_client(size())).unwrap();
        let frame_rx = handle.frame_rx.clone();

        // Real bidirectional socket, split exactly like serve_attach does.
        let (client_sock, server_sock) = tokio::net::UnixStream::pair().unwrap();
        let (mut server_read, server_write) = tokio::io::split(server_sock);

        let renderer = crate::renderer::Renderer::new();
        let mut renderer_task = tokio::spawn(async move {
            let _ = renderer.run(frame_rx, server_write).await;
        });

        // Mini serve_attach: hold the read half, break when the renderer ends,
        // then drop the read half (mimics serve_attach returning).
        let conn = tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            loop {
                tokio::select! {
                    biased;
                    _ = &mut renderer_task => break,
                    r = server_read.read(&mut buf) => {
                        if matches!(r, Ok(0) | Err(_)) { break; }
                    }
                }
            }
            // `server_read` drops here.
        });

        s.begin_close();
        s.terminate_panes().await;

        let (mut cr, mut cw) = tokio::io::split(client_sock);
        // Keep a writer so we don't close our own side prematurely.
        let _ = cw.write_all(b"").await;
        let mut buf = vec![0u8; 64 * 1024];
        let got_eof = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                match cr.read(&mut buf).await {
                    Ok(0) => break true,
                    Ok(_) => continue,
                    Err(_) => break true,
                }
            }
        })
        .await;
        let _ = conn.await;
        assert!(
            got_eof.is_ok() && got_eof.unwrap(),
            "split unix socket to client never closed after kill"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn begin_close_drops_frame_tx_so_clients_detach() {
        let _g = test_isolate_state_dir();
        let s = Session::new("fx".into(), spec(), size(), cfg()).unwrap();
        // A client's renderer watches this; when the coordinator drops
        // frame_tx, changed() returns Err and the renderer (hence client)
        // tears down.
        let mut frame_rx = s.frame_rx_template.clone();
        s.begin_close();
        s.terminate_panes().await;
        // The frame channel must close (all senders dropped) promptly.
        let closed = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if frame_rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
        assert!(closed.is_ok(), "frame_tx never dropped after begin_close");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn begin_close_is_idempotent_and_stops_persist() {
        let _g = test_isolate_state_dir();
        let s = Session::new("bc".into(), spec(), size(), cfg()).unwrap();
        s.mark_dirty();
        tokio::time::sleep(std::time::Duration::from_millis(1800)).await;
        assert!(crate::persist::load_session("bc").unwrap().is_some());
        crate::persist::delete_session("bc").unwrap();
        // Close, then try hard to make the persist task re-save.
        s.begin_close();
        s.begin_close(); // idempotent: must not panic
        s.mark_dirty();
        s.persist_notify.notify_one();
        tokio::time::sleep(std::time::Duration::from_millis(1800)).await;
        assert!(
            crate::persist::load_session("bc").unwrap().is_none(),
            "persist task re-saved the file after begin_close"
        );
        s.terminate_panes().await; // exercise the path; child dies
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mark_dirty_eventually_writes_file() {
        let _g = test_isolate_state_dir();
        let s = Session::new("dirty-test".into(), spec(), size(), cfg()).unwrap();
        s.mark_dirty();
        tokio::time::sleep(std::time::Duration::from_millis(1800)).await;
        let loaded = crate::persist::load_session("dirty-test")
            .expect("load")
            .expect("file should exist");
        assert_eq!(loaded.name, "dirty-test");
    }

    #[tokio::test]
    async fn snapshot_for_persist_captures_single_pane_session() {
        let s = Session::new("snap1".into(), spec(), size(), cfg()).unwrap();
        let wm = s.window_manager.lock().await;
        let snap = s.snapshot_for_persist(&wm);
        assert_eq!(snap.name, "snap1");
        assert_eq!(snap.schema, crate::persist::SCHEMA_VERSION);
        assert_eq!(snap.windows.len(), 1);
        assert_eq!(snap.windows[0].panes.len(), 1);
        assert!(matches!(
            snap.windows[0].layout,
            crate::persist::LayoutStateV1::Leaf(0)
        ));
    }

    #[tokio::test]
    async fn snapshot_for_persist_captures_two_pane_split() {
        let s = Session::new("snap2".into(), spec(), size(), cfg()).unwrap();
        {
            let mut wm = s.window_manager.lock().await;
            wm.handle_command(plexy_glass_mux::Command::SplitV).unwrap();
        }
        let wm = s.window_manager.lock().await;
        let snap = s.snapshot_for_persist(&wm);
        assert_eq!(snap.windows[0].panes.len(), 2);
        match &snap.windows[0].layout {
            crate::persist::LayoutStateV1::Split { dir, first, second, .. } => {
                assert_eq!(*dir, crate::persist::LayoutDirV1::Vertical);
                assert!(matches!(**first, crate::persist::LayoutStateV1::Leaf(0)));
                assert!(matches!(**second, crate::persist::LayoutStateV1::Leaf(1)));
            }
            other => panic!("expected Split, got {other:?}"),
        }
    }
}
