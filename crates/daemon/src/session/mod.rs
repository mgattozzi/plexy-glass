//! A named session: a WindowManager + attached clients + broadcasting renderer.

mod coordinator;
mod restore;

use crate::{error::DaemonError, window_manager::WindowManager};
use coordinator::render_coordinator;
use plexy_glass_mux::{PaneId, VirtualScreen, WindowId};
use plexy_glass_protocol::{NegotiatedKbd, ProtocolError, PtySize, SessionEntry, SpawnSpec};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::SystemTime;
use tokio::sync::{mpsc, watch, Mutex, Notify};
use tokio::task::JoinHandle;

pub struct ClientHandle {
    pub client_id: u64,
    pub size: PtySize,
    pub frame_rx: watch::Receiver<Arc<VirtualScreen>>,
    /// Whether this client's outer terminal currently has focus (`\e[I`/`\e[O`).
    /// Starts `false` because `?1004` reports no initial state on enable, so we
    /// learn it on the first transition. Used for the any-client-focused aggregate.
    pub focused: bool,
    /// Whether this client's keymap prefix is currently armed (mid-chord).
    /// Written by the connection's input loop after every `Keymap::consume`;
    /// read by the render paths for the any-client-armed aggregate that
    /// drives the `prefix-indicator` status widget.
    pub prefix_armed: Arc<AtomicBool>,
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
    /// One-shot wake that repaints an expired status-line message away. Aborted
    /// and replaced each time a new message is set, and aborted on `Drop`.
    status_msg_handle: StdMutex<Option<JoinHandle<()>>>,
}

impl Session {
    /// Snapshot the current active config Arc. Hot reload swaps the
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
                        let pane = w.pane(*pid);
                        // `Screen.cwd` holds the raw OSC-7 URL; persist a plain
                        // path so restore's `SpawnSpec.cwd` is a real directory.
                        let cwd = pane
                            .and_then(|p| p.with_screen(|s| s.cwd.clone()))
                            .and_then(|url| crate::popup::osc7_to_path(&url));
                        let name = pane.and_then(|p| p.name());
                        PaneStateV1 { cwd, name }
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
                    home_cwd: w.home_cwd.clone(),
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
            status_engine_slot: StdMutex::new(status_engine),
            status_tick_handle: StdMutex::new(None),
            status_msg_handle: StdMutex::new(None),
            config_slot: StdMutex::new(config),
            dirty: std::sync::atomic::AtomicBool::new(false),
            persist_notify: Arc::new(Notify::new()),
            persist_handle: StdMutex::new(None),
            death_handle: StdMutex::new(None),
        });
        let coord_handle = tokio::spawn(render_coordinator(Arc::clone(&session), frame_tx));
        // invariant: no other thread holds coordinator_handle at construction time
        *session.coordinator_handle.lock().expect("coordinator lock poisoned") = Some(coord_handle);

        // Spawn the pane-death consumer; it owns the receiver end of the
        // death channel.
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
        if let Some(p) = wm.popup() {
            p.pane.kill_child();
        }
    }

}

/// A point-in-time snapshot of one session's windows/panes, used to build the
/// choose-tree node list at the connection layer.
pub struct SessionTree {
    pub name: String,
    pub active_window: usize,
    pub total_panes: usize,
    pub windows: Vec<WindowTree>,
}

/// One window within a [`SessionTree`]. `panes` is in stable DFS-leaf order.
pub struct WindowTree {
    pub id: WindowId,
    pub name: String,
    pub active_pane: PaneId,
    pub panes: Vec<(PaneId, Option<String>)>,
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

    /// Snapshot this session's windows/panes for the choose-tree overlay. Async
    /// because it locks the WindowManager via `.lock().await` (NEVER
    /// `blocking_lock`: the connection task runs on a runtime worker thread,
    /// where `blocking_lock` panics). Pane order comes from
    /// `layout().dfs_leaves()` (stable).
    pub async fn tree_snapshot(&self) -> SessionTree {
        let m = self.window_manager.lock().await;
        let active_window = m.active_idx();
        let mut total_panes = 0;
        let windows = m
            .windows()
            .iter()
            .map(|w| {
                let ids = w.layout().dfs_leaves();
                total_panes += ids.len();
                let panes = ids
                    .iter()
                    .map(|id| (*id, w.pane(*id).and_then(|p| p.name())))
                    .collect();
                WindowTree { id: w.id, name: w.name.clone(), active_pane: w.active(), panes }
            })
            .collect();
        SessionTree { name: self.name.clone(), active_window, total_panes, windows }
    }

    /// `prefix_armed` is the connection's live prefix flag (shared, not
    /// copied): the input loop keeps storing into the same atomic, so a
    /// client that switches sessions re-registers the SAME flag on the
    /// target and re-arming keeps working after the switch.
    pub fn register_client(
        self: &Arc<Self>,
        size: PtySize,
        prefix_armed: Arc<AtomicBool>,
    ) -> Result<ClientHandle, DaemonError> {
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
                focused: false,
                prefix_armed: Arc::clone(&prefix_armed),
            });
        }
        self.recompute_size_and_notify();
        Ok(ClientHandle {
            client_id,
            size,
            frame_rx: frame_rx_for_caller,
            focused: false,
            prefix_armed,
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
        // Resolve the target panes under the lock, send after dropping it.
        // Three cases: a floating popup is modal (input goes to its child,
        // never the layout panes, sync-panes included); otherwise sync-panes
        // fans out to every layout pane; otherwise the single input target
        // (= the active pane; see `WindowManager::input_target_pane`).
        let targets: Vec<crate::pane::Pane> = {
            let manager = self.window_manager.lock().await;
            if !manager.has_popup() && manager.active_window().sync_input {
                let win = manager.active_window();
                win.layout()
                    .panes()
                    .into_iter()
                    .filter_map(|id| win.pane(id))
                    .cloned()
                    .collect()
            } else {
                manager.input_target_pane().cloned().into_iter().collect()
            }
        };
        for pane in targets {
            pane.send_input(bytes::Bytes::copy_from_slice(bytes)).await.ok();
        }
        self.notify.notify_one();
        Ok(())
    }

    /// Queue a focus-event sequence (`\e[I` in / `\e[O` out) to the focused
    /// pane, `WindowManager::input_target_pane` (the popup's child while one
    /// is open, otherwise the active layout pane), gated on that pane's
    /// ?1004 (`FOCUS_EVENTS`) mode. No-op otherwise.
    pub async fn focus_active_pane(&self, focused: bool) {
        let target = {
            let manager = self.window_manager.lock().await;
            manager.input_target_pane().cloned()
        };
        if let Some(pane) = target {
            let wants =
                pane.with_screen(|s| s.modes.contains(plexy_glass_emulator::Modes::FOCUS_EVENTS));
            if wants {
                let seq: &[u8] = if focused { b"\x1b[I" } else { b"\x1b[O" };
                pane.send_input(bytes::Bytes::from_static(seq)).await.ok();
            }
        }
    }

    /// Forward a color-scheme report (`\e[?997;1n` dark / `;2n` light) to EVERY
    /// pane in EVERY window that subscribed via ?2031.
    pub async fn forward_color_scheme(&self, dark: bool) {
        let seq: &[u8] = if dark { b"\x1b[?997;1n" } else { b"\x1b[?997;2n" };
        let manager = self.window_manager.lock().await;
        for win in manager.windows() {
            for (_id, pane) in win.panes() {
                // Record the scheme on EVERY pane so a later one-shot `\e[?996n`
                // query answers the real preference (not the hardcoded default).
                pane.with_screen_mut(|s| s.set_color_scheme_dark(dark));
                // Push the unsolicited notification only to `?2031` subscribers.
                let wants = pane.with_screen(|s| {
                    s.modes.contains(plexy_glass_emulator::Modes::COLOR_SCHEME_UPDATES)
                });
                if wants {
                    pane.send_input(bytes::Bytes::from_static(seq)).await.ok();
                }
            }
        }
        if let Some(p) = manager.popup() {
            p.pane.with_screen_mut(|s| s.set_color_scheme_dark(dark));
            let wants = p.pane.with_screen(|s| {
                s.modes.contains(plexy_glass_emulator::Modes::COLOR_SCHEME_UPDATES)
            });
            if wants {
                p.pane.send_input(bytes::Bytes::from_static(seq)).await.ok();
            }
        }
    }

    /// The active pane of the active window. Used by the connection input loop
    /// to snapshot the focused pane before/after an input batch so a pane switch
    /// (select-pane, click, choose-tree, ...) can synthesize focus-out/in.
    pub async fn active_pane_id(&self) -> Option<PaneId> {
        let manager = self.window_manager.lock().await;
        Some(manager.active_window().active())
    }

    /// Synthesize a focus transition between two panes after the active pane
    /// changed: queue `\e[O` (focus-out) to `old` and `\e[I` (focus-in) to
    /// `new`, each gated independently on that pane's ?1004 (`FOCUS_EVENTS`)
    /// mode. Panes are searched across ALL windows, since a cross-window switch
    /// leaves the old pane in the previous window and it must still get its
    /// focus-out. A pane that no longer exists (e.g. just killed) is skipped.
    pub async fn synthesize_focus_transition(&self, old: PaneId, new: PaneId) {
        let manager = self.window_manager.lock().await;
        let find = |id: PaneId| manager.windows().iter().find_map(|w| w.pane(id));
        if let Some(p) = find(old)
            && p.with_screen(|s| s.modes.contains(plexy_glass_emulator::Modes::FOCUS_EVENTS))
        {
            p.send_input(bytes::Bytes::from_static(b"\x1b[O")).await.ok();
        }
        if let Some(p) = find(new)
            && p.with_screen(|s| s.modes.contains(plexy_glass_emulator::Modes::FOCUS_EVENTS))
        {
            p.send_input(bytes::Bytes::from_static(b"\x1b[I")).await.ok();
        }
    }

    /// Update one client's focus state and report whether the **aggregate**
    /// focus changed. Any-client-focused rule: the session is focused iff at
    /// least one attached client's outer terminal is. Returns `Some(true)` when
    /// the aggregate transitioned to focused (caller emits `\e[I`), `Some(false)`
    /// when it transitioned to unfocused (caller emits `\e[O`), or `None` when the
    /// aggregate is unchanged (another client already held/lacked focus). A
    /// disconnected client simply drops from the set, so its focus naturally
    /// stops counting on the next transition.
    pub async fn set_client_focus(&self, client_id: u64, focused: bool) -> Option<bool> {
        let mut clients = self.clients.lock().await;
        let any_before = clients.iter().any(|c| c.focused);
        if let Some(c) = clients.iter_mut().find(|c| c.client_id == client_id) {
            c.focused = focused;
        }
        let any_after = clients.iter().any(|c| c.focused);
        (any_before != any_after).then_some(any_after)
    }

    /// Any-client-armed aggregate for the `prefix-indicator` status widget:
    /// true iff at least one attached client's keymap prefix is mid-chord.
    /// Mirrors the any-client-focused rule above.
    pub async fn any_prefix_armed(&self) -> bool {
        let clients = self.clients.lock().await;
        clients.iter().any(|c| c.prefix_armed.load(Ordering::SeqCst))
    }

    /// Re-encode a canonical key event into the active pane's negotiated
    /// keyboard protocol and write the result.
    ///
    /// Decode is per-CONNECTION (the client's outer-terminal protocol,
    /// `client_kbd`) and encode is per-PANE (what the child negotiated); they
    /// compose independently. For a Legacy pane, `raw_bytes` is only forwarded
    /// verbatim when the client is ALSO Legacy. Otherwise the incoming bytes
    /// are rich CSI-u/27-form (the client's outer terminal is Kitty/
    /// modifyOtherKeys) and must be down-converted to legacy. See
    /// `reencode_input`.
    pub async fn handle_key_event(
        &self,
        event: &plexy_glass_mux::KeyEvent,
        raw_bytes: &[u8],
        client_kbd: NegotiatedKbd,
    ) -> Result<(), DaemonError> {
        let manager = self.window_manager.lock().await;
        let win = manager.active_window();
        if win.sync_input {
            for id in win.layout().panes() {
                if let Some(pane) = win.pane(id) {
                    let bytes = encode_for_pane(pane, event, raw_bytes, client_kbd);
                    pane.send_input(bytes::Bytes::from(bytes)).await.ok();
                }
            }
        } else if let Some(pane) = win.active_pane() {
            let bytes = encode_for_pane(pane, event, raw_bytes, client_kbd);
            pane.send_input(bytes::Bytes::from(bytes)).await.ok();
        }
        drop(manager);
        self.notify.notify_one();
        Ok(())
    }

    /// Re-encode a key event for the floating popup's child and write it.
    /// While a popup is open the connection routes PassThrough keys here
    /// instead of `handle_key_event` (the popup is modal).
    pub async fn handle_popup_key_event(
        &self,
        event: &plexy_glass_mux::KeyEvent,
        raw_bytes: &[u8],
        client_kbd: NegotiatedKbd,
    ) -> Result<(), DaemonError> {
        let manager = self.window_manager.lock().await;
        if let Some(p) = manager.popup() {
            let bytes = encode_for_pane(&p.pane, event, raw_bytes, client_kbd);
            let pane = p.pane.clone();
            drop(manager);
            pane.send_input(bytes::Bytes::from(bytes)).await.ok();
            self.notify.notify_one();
        }
        Ok(())
    }

    /// Whether the floating popup is open (connection input-routing check).
    pub async fn popup_active(&self) -> bool {
        self.window_manager.lock().await.has_popup()
    }

    pub async fn handle_command(&self, cmd: plexy_glass_mux::Command) -> Result<(), DaemonError> {
        let mut manager = self.window_manager.lock().await;
        manager.handle_command(cmd)?;
        drop(manager);
        self.notify.notify_one();
        self.mark_dirty();
        Ok(())
    }

    /// Apply a parsed command-prompt command. Parity verbs route through the
    /// existing `handle_command` path; arg-carrying verbs (resize-by-N, renames)
    /// apply directly. Connection-level verbs (`Detach`/`Reload`/`Switch`) are
    /// handled by the caller and reach here only defensively. Returns an
    /// optional confirmation message for the status line.
    pub async fn handle_prompt_command(
        &self,
        cmd: plexy_glass_mux::PromptCommand,
    ) -> Result<Option<String>, DaemonError> {
        use plexy_glass_mux::{Command, FocusTarget, PromptCommand};
        let mapped: Command = match cmd {
            PromptCommand::NewWindow => Command::NewWindow,
            PromptCommand::NextWindow => Command::NextWindow,
            PromptCommand::PrevWindow => Command::PrevWindow,
            PromptCommand::SelectWindow(n) => Command::SelectWindow(n),
            PromptCommand::LastWindow => Command::SelectLastWindow,
            PromptCommand::SplitH => Command::SplitH,
            PromptCommand::SplitV => Command::SplitV,
            PromptCommand::Zoom => Command::ZoomToggle,
            PromptCommand::KillPane => Command::KillPane,
            PromptCommand::KillWindow => Command::KillWindow,
            PromptCommand::CopyMode => Command::EnterCopyMode,
            PromptCommand::ToggleSync => Command::ToggleSyncPanes,
            PromptCommand::Help => Command::ShowHelp,
            PromptCommand::MarkPane => Command::MarkPane,
            PromptCommand::BreakPane => Command::BreakPane,
            PromptCommand::ToggleMonitorActivity => Command::ToggleMonitorActivity,
            PromptCommand::ToggleMonitorBell => Command::ToggleMonitorBell,
            PromptCommand::JoinPane(dir) => Command::JoinPane(dir),
            PromptCommand::SwapPane(t) => {
                Command::SwapPane(matches!(t, plexy_glass_mux::SwapTarget::Next))
            }
            PromptCommand::SwapMarked => Command::SwapMarkedPane,
            PromptCommand::Focus(ft) => match ft {
                FocusTarget::Dir(d) => Command::SelectPane(d),
                FocusTarget::Next => Command::SelectNextPane,
                FocusTarget::Prev => Command::SelectPrevPane,
                FocusTarget::Last => Command::SelectLastPane,
            },
            PromptCommand::Resize(dir, n) => {
                {
                    let mut m = self.window_manager.lock().await;
                    for _ in 0..n {
                        m.handle_command(Command::ResizePane(dir))?;
                    }
                }
                self.notify.notify_one();
                self.mark_dirty();
                return Ok(None);
            }
            PromptCommand::RenameWindow(name) => {
                {
                    let mut m = self.window_manager.lock().await;
                    m.rename_active_window(name);
                }
                self.notify.notify_one();
                self.mark_dirty();
                return Ok(None);
            }
            PromptCommand::RenamePane(name) => {
                {
                    let mut m = self.window_manager.lock().await;
                    m.rename_active_pane(name);
                }
                self.notify.notify_one();
                self.mark_dirty();
                return Ok(None);
            }
            // Handled at the connection layer; defensive no-op here.
            PromptCommand::Detach
            | PromptCommand::Reload
            | PromptCommand::Switch(_)
            | PromptCommand::ChooseSession
            | PromptCommand::ChooseTree
            | PromptCommand::PasteBuffer
            | PromptCommand::ChooseBuffer => {
                return Ok(None);
            }
            PromptCommand::Popup(cmd) => Command::OpenPopup { command: cmd },
            PromptCommand::ClosePopup => Command::ClosePopup,
            PromptCommand::Layout(preset) => Command::SelectLayout(preset),
        };
        self.handle_command(mapped).await?;
        Ok(None)
    }

    /// Show a transient status-line message and schedule a single wake so the
    /// expired message is repainted away even if nothing else changes. Any
    /// prior pending wake is aborted first (mirroring `status_tick_handle`), so
    /// rapid messages neither leak tasks nor fire redundant notifies.
    pub async fn set_status_message(self: &Arc<Self>, text: String) {
        {
            let mut m = self.window_manager.lock().await;
            m.set_status_message(text);
        }
        self.notify.notify_one();

        let prior = {
            // invariant: status_msg_handle mutex held briefly; no .await holding the lock.
            let mut slot = self
                .status_msg_handle
                .lock()
                .expect("status_msg_handle poisoned");
            slot.take()
        };
        if let Some(h) = prior {
            h.abort();
        }
        let weak = Arc::downgrade(self);
        let handle = tokio::spawn(async move {
            // Sleep just past the TTL so the message is definitely expired when
            // the wake-driven recompose runs and clears it.
            tokio::time::sleep(
                crate::window_manager::STATUS_TTL + std::time::Duration::from_millis(50),
            )
            .await;
            if let Some(s) = weak.upgrade() {
                s.notify.notify_one();
            }
        });
        // invariant: status_msg_handle mutex held briefly; no .await holding the lock.
        *self
            .status_msg_handle
            .lock()
            .expect("status_msg_handle poisoned") = Some(handle);
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

/// Pick the encode target for a pane from its negotiated state. Precedence per
/// the spec: Kitty flags > modifyOtherKeys level > Legacy.
pub(crate) fn select_target(
    kitty_flags: u8,
    modify_other_keys: u8,
) -> plexy_glass_keys::KeyboardTarget {
    use plexy_glass_keys::KeyboardTarget;
    if kitty_flags != 0 {
        KeyboardTarget::Kitty(kitty_flags)
    } else if modify_other_keys != 0 {
        KeyboardTarget::ModifyOtherKeys(modify_other_keys)
    } else {
        KeyboardTarget::Legacy
    }
}

/// Pure re-encode decision: given the per-connection `client_kbd` (the protocol
/// the client's OUTER terminal speaks, in which `raw_bytes` are already encoded)
/// and the per-pane negotiated state, produce the bytes to forward to the child.
///
/// Decode (connection) and encode (pane) compose independently:
/// - pane target Kitty/modifyOtherKeys → `encode` to that protocol.
/// - pane target Legacy:
///   - Legacy client → `raw_bytes` verbatim. The incoming bytes are ALREADY
///     legacy, and raw passthrough is lossless while `encode(Legacy)` is lossy
///     for some keys (`legacy_bytes` returns empty for modified Tab/Enter/Escape
///     and unmatched function keys), so passthrough MUST be preserved here.
///   - non-Legacy client (Kitty/modifyOtherKeys outer terminal, so `raw_bytes`
///     are rich CSI-u/27-form) → down-convert via `encode(.., Legacy, ..)`.
///     Forwarding the rich bytes verbatim would break every keystroke for a
///     child that never negotiated those protocols (plain bash/vim/less/python).
fn reencode_input(
    client_kbd: NegotiatedKbd,
    pane_kitty_flags: u8,
    pane_modkeys: u8,
    app_cursor: bool,
    event: &plexy_glass_mux::KeyEvent,
    raw_bytes: &[u8],
) -> Vec<u8> {
    use plexy_glass_keys::KeyboardTarget;
    let target = select_target(pane_kitty_flags, pane_modkeys);
    match target {
        KeyboardTarget::Legacy => {
            if matches!(client_kbd, NegotiatedKbd::Legacy) {
                raw_bytes.to_vec()
            } else {
                plexy_glass_keys::encode(event, KeyboardTarget::Legacy, app_cursor)
            }
        }
        _ => plexy_glass_keys::encode(event, target, app_cursor),
    }
}

/// Read the pane's negotiated keyboard/mode state and re-encode `event` for it,
/// threading the per-connection `client_kbd` so a rich-protocol client into a
/// Legacy pane is down-converted rather than forwarded verbatim. The decision
/// itself lives in the pure `reencode_input` helper (unit-tested directly).
fn encode_for_pane(
    pane: &crate::pane::Pane,
    event: &plexy_glass_mux::KeyEvent,
    raw_bytes: &[u8],
    client_kbd: NegotiatedKbd,
) -> Vec<u8> {
    let (kitty_flags, modkeys, app_cursor) = pane.with_screen(|s| {
        let alt = s.modes.contains(plexy_glass_emulator::Modes::ALT_SCREEN);
        (
            s.kbd.kitty_flags(alt),
            s.kbd.modify_other_keys(),
            s.modes.contains(plexy_glass_emulator::Modes::APP_CURSOR_KEYS),
        )
    });
    reencode_input(client_kbd, kitty_flags, modkeys, app_cursor, event, raw_bytes)
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
        if let Some(handle) = self
            .status_msg_handle
            .lock()
            .expect("status msg handle lock poisoned")
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
            // `dirty` was already swapped to false above; without this the
            // snapshot is silently lost until the next structural change.
            // mark_dirty re-sets the flag AND notifies, so the next
            // `notified().await` returns immediately (stored permit) and the
            // 1500ms debounce sleep paces the retry, so we get a 1.5s retry
            // cadence while the disk stays unwritable, not a busy loop.
            session.mark_dirty();
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
            // Read the sticky flags maintained by the coordinator's
            // update_monitor_flags; the tick task is not the drainer.
            activity: w.activity_flag(),
            bell: w.bell_flag(),
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
    let prefix_active = session.any_prefix_armed().await;
    plexy_glass_status::SnapshotCtx {
        session_name,
        windows,
        active_window: active_idx,
        attached_clients,
        prefix_active,
        active_pane_cwd,
        copy_mode_active,
        sync_active,
        zoom_active,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use restore::restore_cwd;
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
        let _g = crate::test_env::isolate();
        let s = Session::new("main".into(), spec(), size(), cfg()).expect("construct session");
        assert_eq!(s.name, "main");
        assert!(!s.closing.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn handle_prompt_command_applies_effects() {
        let _g = crate::test_env::isolate();
        use plexy_glass_mux::{Direction, PromptCommand};
        let s = Session::new("pc".into(), spec(), size(), cfg()).unwrap();

        // `split h` -> two panes in the active window.
        s.handle_prompt_command(PromptCommand::SplitH).await.unwrap();
        assert_eq!(
            s.window_manager.lock().await.active_window().layout().panes().len(),
            2
        );

        // `rename first` -> active window name.
        s.handle_prompt_command(PromptCommand::RenameWindow("first".into())).await.unwrap();
        assert_eq!(s.window_manager.lock().await.active_window().name, "first");

        // `rename-pane logs` -> active pane name.
        s.handle_prompt_command(PromptCommand::RenamePane("logs".into())).await.unwrap();
        {
            let m = s.window_manager.lock().await;
            let pid = m.active_window().active();
            assert_eq!(
                m.active_window().pane(pid).and_then(|p| p.name()).as_deref(),
                Some("logs")
            );
        }

        // `new` (active -> window 1), then `win 1` (SelectWindow(0)) returns to "first".
        s.handle_prompt_command(PromptCommand::NewWindow).await.unwrap();
        s.handle_prompt_command(PromptCommand::SelectWindow(0)).await.unwrap();
        assert_eq!(s.window_manager.lock().await.active_window().name, "first");

        // `resize l 3` on the split must not error.
        s.handle_prompt_command(PromptCommand::Resize(Direction::Left, 3)).await.unwrap();

        // Connection-level verbs are defensive no-ops here.
        assert!(matches!(s.handle_prompt_command(PromptCommand::Detach).await, Ok(None)));
        assert!(matches!(
            s.handle_prompt_command(PromptCommand::Switch("x".into())).await,
            Ok(None)
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn prompt_popup_maps_to_open_and_close() {
        let _g = crate::test_env::isolate();
        let s = Session::new("t-popup-prompt".into(), spec(), size(), cfg()).unwrap();
        s.handle_prompt_command(plexy_glass_mux::PromptCommand::Popup(Some("sleep 600".into())))
            .await
            .unwrap();
        {
            let m = s.window_manager.lock().await;
            assert_eq!(m.popup().unwrap().title, "sleep 600");
        }
        s.handle_prompt_command(plexy_glass_mux::PromptCommand::ClosePopup).await.unwrap();
        assert!(!s.window_manager.lock().await.has_popup());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn input_bytes_route_to_popup_when_open() {
        let _g = crate::test_env::isolate();
        let s = Session::new("t-popup-input".into(), spec(), size(), cfg()).unwrap();
        s.handle_command(plexy_glass_mux::Command::OpenPopup { command: Some("cat".into()) })
            .await
            .unwrap();
        let mut rx = {
            let m = s.window_manager.lock().await;
            m.popup().unwrap().pane.subscribe_output()
        };
        s.handle_input_bytes(b"popup_gets_this\n").await.unwrap();
        // cat echoes what it reads; the bytes must surface on the POPUP pane.
        let mut seen: Vec<u8> = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            if let Ok(Ok(chunk)) =
                tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await
            {
                seen.extend_from_slice(&chunk);
                if seen.windows(15).any(|w| w == b"popup_gets_this") {
                    break;
                }
            }
        }
        assert!(
            seen.windows(15).any(|w| w == b"popup_gets_this"),
            "popup pane never echoed routed input: {seen:?}"
        );
        // Kill the popup child so it doesn't outlive the test.
        s.handle_command(plexy_glass_mux::Command::ClosePopup).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn focus_events_route_to_popup_when_open() {
        let _g = crate::test_env::isolate();
        let s = Session::new("t-popup-focus".into(), spec(), size(), cfg()).unwrap();
        s.handle_command(plexy_glass_mux::Command::OpenPopup { command: Some("cat".into()) })
            .await
            .unwrap();
        let mut rx = {
            let m = s.window_manager.lock().await;
            let popup = m.popup().unwrap();
            // Subscribe to ?1004 on the POPUP pane; the layout pane stays
            // unsubscribed, so a `\e[I` can only have come via the popup.
            popup
                .pane
                .with_screen_mut(|sc| sc.modes.insert(plexy_glass_emulator::Modes::FOCUS_EVENTS));
            popup.pane.subscribe_output()
        };
        s.focus_active_pane(true).await;
        // The popup runs `$SHELL -c cat`; in canonical mode the PTY echoes the
        // ESC as caret notation (`^[[I`) and cat holds input until a newline,
        // so accept the focus-in sequence in either raw or caret-echoed form.
        let raw: &[u8] = &[0x1b, b'[', b'I'];
        let caret: &[u8] = b"^[[I";
        let hit = |buf: &[u8]| {
            buf.windows(raw.len()).any(|w| w == raw)
                || buf.windows(caret.len()).any(|w| w == caret)
        };
        let mut seen: Vec<u8> = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            if let Ok(Ok(chunk)) =
                tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await
            {
                seen.extend_from_slice(&chunk);
                if hit(&seen) {
                    break;
                }
            }
        }
        assert!(hit(&seen), "popup pane never saw the focus-in sequence: {seen:?}");
        // Kill the popup child so it doesn't outlive the test.
        s.handle_command(plexy_glass_mux::Command::ClosePopup).await.unwrap();
    }

    // Regression: `build_snapshot_ctx` used `blocking_lock` and was driven by the
    // status tick task on a runtime worker thread, which PANICS ("Cannot block
    // the current thread from within a runtime"). It is now async, so calling it
    // from a spawned task (a worker thread on the multi-thread runtime, the exact
    // scenario the tick task hits) must succeed and return real state.
    #[tokio::test(flavor = "multi_thread")]
    async fn build_snapshot_ctx_works_from_spawned_task() {
        let _g = crate::test_env::isolate();
        let s = Session::new("snapctx".into(), spec(), size(), cfg()).unwrap();
        let s2 = Arc::clone(&s);
        let ctx = tokio::spawn(async move { build_snapshot_ctx(&s2).await })
            .await
            .expect("tick-style snapshot task must not panic");
        assert_eq!(ctx.session_name, "snapctx");
        assert_eq!(ctx.windows.len(), 1);
    }

    #[tokio::test]
    async fn build_snapshot_ctx_surfaces_window_alert_flags() {
        let _g = crate::test_env::isolate();
        let s = Session::new("snapalert".into(), spec(), size(), cfg()).unwrap();
        {
            // Add a second window and flag it (the WindowManager's sticky flags
            // are what build_snapshot_ctx reads into the status WindowSummary).
            let mut m = s.window_manager.lock().await;
            m.handle_command(plexy_glass_mux::Command::NewWindow).unwrap();
            m.windows_mut()[0].set_bell();
            m.windows_mut()[0].set_activity();
        }
        let ctx = build_snapshot_ctx(&s).await;
        assert_eq!(ctx.windows.len(), 2);
        assert!(ctx.windows[0].bell, "snapshot surfaces the window's bell flag");
        assert!(ctx.windows[0].activity, "snapshot surfaces the window's activity flag");
        assert!(!ctx.windows[1].bell && !ctx.windows[1].activity, "unflagged window is clean");
    }

    #[tokio::test]
    async fn list_entry_reports_one_window_one_pane_zero_clients() {
        let _g = crate::test_env::isolate();
        let s = Session::new("main".into(), spec(), size(), cfg()).unwrap();
        let entry = tokio::task::spawn_blocking(move || s.list_entry()).await.unwrap();
        assert_eq!(entry.name, "main");
        assert_eq!(entry.windows, 1);
        assert_eq!(entry.panes, 1);
        assert_eq!(entry.clients, 0);
    }

    #[tokio::test]
    async fn tree_snapshot_reports_windows_and_panes() {
        let _g = crate::test_env::isolate();
        let s = Session::new("snap".into(), spec(), size(), cfg()).unwrap();
        {
            // Split the first window so it has two panes, then add a window.
            let mut m = s.window_manager.lock().await;
            m.handle_command(plexy_glass_mux::Command::SplitV).unwrap();
            m.handle_command(plexy_glass_mux::Command::NewWindow).unwrap();
        }
        let st = s.tree_snapshot().await;
        assert_eq!(st.name, "snap");
        assert_eq!(st.windows.len(), 2);
        assert_eq!(st.total_panes, 3, "two panes in window 0, one in window 1");
        assert_eq!(st.windows[0].panes.len(), 2);
        assert_eq!(st.windows[1].panes.len(), 1);
        // NewWindow made window index 1 active.
        assert_eq!(st.active_window, 1);
        // Pane ids in DFS-leaf order; SplitV makes the new pane (1) active in w0.
        assert_eq!(st.windows[0].panes[0].0, PaneId(0));
        assert_eq!(st.windows[0].panes[1].0, PaneId(1));
        assert_eq!(st.windows[0].active_pane, PaneId(1));
        assert_eq!(st.windows[1].panes[0].0, PaneId(2));
        assert_eq!(st.windows[1].active_pane, PaneId(2));
    }

    #[tokio::test]
    async fn register_then_effective_size_matches_single_client() {
        let _g = crate::test_env::isolate();
        let s = Session::new("main".into(), spec(), size(), cfg()).unwrap();
        let s2 = Arc::clone(&s);
        let h = tokio::task::spawn_blocking(move || {
            s2.register_client(
                PtySize { rows: 10, cols: 30, pixel_width: 0, pixel_height: 0 },
                Arc::new(AtomicBool::new(false)),
            )
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
    async fn focus_aggregates_across_clients_any_focused() {
        let _g = crate::test_env::isolate();
        // Any-client-focused: the pane is focused iff at least one client is.
        let s = Session::new("focusagg".into(), spec(), size(), cfg()).unwrap();
        let s2 = Arc::clone(&s);
        let a = tokio::task::spawn_blocking(move || {
            s2.register_client(
                PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
                Arc::new(AtomicBool::new(false)),
            )
        })
        .await
        .unwrap()
        .unwrap();
        let s2 = Arc::clone(&s);
        let b = tokio::task::spawn_blocking(move || {
            s2.register_client(
                PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
                Arc::new(AtomicBool::new(false)),
            )
        })
        .await
        .unwrap()
        .unwrap();
        // Both start unfocused. A gains focus → aggregate false→true (emit focus-in).
        assert_eq!(s.set_client_focus(a.client_id, true).await, Some(true));
        // B gains focus → already focused, no aggregate change.
        assert_eq!(s.set_client_focus(b.client_id, true).await, None);
        // A loses focus → B still focused, no change (no spurious focus-out).
        assert_eq!(s.set_client_focus(a.client_id, false).await, None);
        // B loses focus → aggregate true→false (emit focus-out).
        assert_eq!(s.set_client_focus(b.client_id, false).await, Some(false));
    }

    #[tokio::test]
    async fn any_prefix_armed_aggregates_across_clients() {
        let _g = crate::test_env::isolate();
        // Any-client-armed: the prefix indicator shows iff at least one
        // attached client's keymap prefix is mid-chord.
        let s = Session::new("prefixagg".into(), spec(), size(), cfg()).unwrap();
        let flag_a = Arc::new(AtomicBool::new(false));
        let flag_b = Arc::new(AtomicBool::new(false));
        let s2 = Arc::clone(&s);
        let fa = Arc::clone(&flag_a);
        let _a = tokio::task::spawn_blocking(move || s2.register_client(size(), fa))
            .await
            .unwrap()
            .unwrap();
        let s2 = Arc::clone(&s);
        let fb = Arc::clone(&flag_b);
        let b = tokio::task::spawn_blocking(move || s2.register_client(size(), fb))
            .await
            .unwrap()
            .unwrap();
        // Nobody armed.
        assert!(!s.any_prefix_armed().await);
        // One client arms → aggregate true.
        flag_a.store(true, Ordering::SeqCst);
        assert!(s.any_prefix_armed().await);
        // Arming the other one too keeps it true.
        flag_b.store(true, Ordering::SeqCst);
        assert!(s.any_prefix_armed().await);
        // Both disarm → false.
        flag_a.store(false, Ordering::SeqCst);
        flag_b.store(false, Ordering::SeqCst);
        assert!(!s.any_prefix_armed().await);
        // A departed client's armed flag stops counting.
        flag_b.store(true, Ordering::SeqCst);
        let s2 = Arc::clone(&s);
        let cid_b = b.client_id;
        tokio::task::spawn_blocking(move || s2.deregister_client(cid_b)).await.unwrap();
        assert!(!s.any_prefix_armed().await);
    }

    #[tokio::test]
    async fn smallest_client_wins() {
        let _g = crate::test_env::isolate();
        let s = Session::new("main".into(), spec(), size(), cfg()).unwrap();
        let s2 = Arc::clone(&s);
        let a = tokio::task::spawn_blocking(move || {
            s2.register_client(
                PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
                Arc::new(AtomicBool::new(false)),
            )
        })
        .await
        .unwrap()
        .unwrap();
        let s2 = Arc::clone(&s);
        let b = tokio::task::spawn_blocking(move || {
            s2.register_client(
                PtySize { rows: 10, cols: 30, pixel_width: 0, pixel_height: 0 },
                Arc::new(AtomicBool::new(false)),
            )
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
        let _g = crate::test_env::isolate();
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
        let _g = crate::test_env::isolate();
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
        let _g = crate::test_env::isolate();
        let s = Session::new("main".into(), spec(), size(), cfg()).unwrap();
        s.closing.store(true, Ordering::SeqCst);
        let s2 = Arc::clone(&s);
        let result = tokio::task::spawn_blocking(move || {
            s2.register_client(size(), Arc::new(AtomicBool::new(false)))
        })
        .await
        .unwrap();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn coordinator_publishes_initial_frame() {
        let _g = crate::test_env::isolate();
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
        let _g = crate::test_env::isolate();
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

    #[tokio::test(flavor = "multi_thread")]
    async fn restore_from_round_trips_single_pane_session() {
        let _g = crate::test_env::isolate();
        let original = Session::new("rt".into(), spec(), size(), cfg()).unwrap();
        original.mark_dirty();
        assert!(
            crate::test_env::poll_until(std::time::Duration::from_secs(10), || {
                crate::persist::load_session("rt").ok().flatten().is_some()
            })
            .await,
            "persist debounce never wrote 'rt'"
        );
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
    async fn build_from_template_single_pane() {
        use plexy_glass_config::{PaneNode, PaneTemplate, SessionTemplate, WindowTemplate};
        let _g = crate::test_env::isolate();
        let tmpl = SessionTemplate {
            name: "dev".into(),
            cwd: None,
            windows: vec![WindowTemplate {
                name: "main".into(),
                cwd: None,
                layout: PaneNode::Leaf(PaneTemplate {
                    command: None,
                    cwd: None,
                    name: Some("editor".into()),
                }),
            }],
        };
        let s = Session::build_from_template(&tmpl, size(), cfg()).await.unwrap();
        {
            let wm = s.window_manager.lock().await;
            assert_eq!(wm.windows().len(), 1);
            assert_eq!(wm.windows()[0].name, "main");
            assert_eq!(wm.windows()[0].layout().panes().len(), 1);
        }
        // Deterministic teardown so the spawned shell doesn't outlive the test.
        s.terminate_panes().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn build_from_template_split_and_multiwindow() {
        use plexy_glass_config::{PaneNode, PaneTemplate, SessionTemplate, SplitDirection, WindowTemplate};
        let _g = crate::test_env::isolate();
        let pane = |c: Option<&str>| {
            PaneNode::Leaf(PaneTemplate { command: c.map(str::to_string), cwd: None, name: None })
        };
        let tmpl = SessionTemplate {
            name: "dev".into(),
            cwd: None,
            windows: vec![
                WindowTemplate {
                    name: "split".into(),
                    cwd: None,
                    layout: PaneNode::Split {
                        dir: SplitDirection::Vertical,
                        children: vec![pane(None), pane(None), pane(None)],
                    },
                },
                WindowTemplate { name: "solo".into(), cwd: None, layout: pane(None) },
            ],
        };
        let s = Session::build_from_template(&tmpl, size(), cfg()).await.unwrap();
        {
            let wm = s.window_manager.lock().await;
            assert_eq!(wm.windows().len(), 2);
            assert_eq!(wm.windows()[0].name, "split");
            assert_eq!(wm.windows()[0].layout().panes().len(), 3);
            assert_eq!(wm.windows()[1].name, "solo");
            assert_eq!(wm.windows()[1].layout().panes().len(), 1);
        }
        // Deterministic teardown so the spawned shells don't outlive the test.
        s.terminate_panes().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn build_from_template_window_cwd_seeds_first_pane() {
        use plexy_glass_config::{PaneNode, PaneTemplate, SessionTemplate, WindowTemplate};
        let _g = crate::test_env::isolate();
        let pane = |cwd: Option<&str>| PaneNode::Leaf(PaneTemplate {
            command: None,
            cwd: cwd.map(str::to_string),
            name: None,
        });
        let tmpl = SessionTemplate {
            name: "wcwd".into(),
            cwd: Some("/session".into()),
            windows: vec![
                WindowTemplate { name: "api".into(), cwd: Some("/win/api".into()), layout: pane(None) },
                WindowTemplate { name: "logs".into(), cwd: None, layout: pane(None) },
            ],
        };
        let s = Session::build_from_template(&tmpl, size(), cfg()).await.unwrap();
        let wm = s.window_manager.lock().await;
        // window "api": its first pane spawns at the window cwd.
        assert_eq!(wm.windows()[0].home_cwd.as_deref(), Some("/win/api"));
        // window "logs": no window cwd, so it falls back to the session cwd.
        assert_eq!(wm.windows()[1].home_cwd.as_deref(), Some("/session"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restore_from_round_trips_two_pane_split() {
        let _g = crate::test_env::isolate();
        let original = Session::new("rt2".into(), spec(), size(), cfg()).unwrap();
        original
            .handle_command(plexy_glass_mux::Command::SplitV)
            .await
            .unwrap();
        assert!(
            crate::test_env::poll_until(std::time::Duration::from_secs(10), || {
                crate::persist::load_session("rt2").ok().flatten().is_some()
            })
            .await,
            "persist debounce never wrote 'rt2'"
        );
        drop(original);
        let saved = crate::persist::load_session("rt2")
            .expect("load")
            .expect("file");
        let restored = Session::restore_from(saved, spec(), size(), cfg()).await.unwrap();
        let wm = restored.window_manager.lock().await;
        assert_eq!(wm.windows()[0].layout().panes().len(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restore_preserves_split_ratios() {
        // Build a saved state by hand: two panes side by side at 0.3 / 0.7.
        use crate::persist::{
            LayoutDirV1, LayoutStateV1, PaneStateV1, SessionStateV1, WindowStateV1,
        };
        let _g = crate::test_env::isolate();
        let saved = SessionStateV1 {
            schema: crate::persist::SCHEMA_VERSION,
            name: "t-ratio-restore".into(),
            created: chrono::Utc::now(),
            active_window: 0,
            windows: vec![WindowStateV1 {
                name: "w".into(),
                sync_input: false,
                home_cwd: None,
                active_pane: 0,
                panes: vec![
                    PaneStateV1 { cwd: None, name: None },
                    PaneStateV1 { cwd: None, name: None },
                ],
                layout: LayoutStateV1::Split {
                    dir: LayoutDirV1::Vertical,
                    ratio: 0.3,
                    first: Box::new(LayoutStateV1::Leaf(0)),
                    second: Box::new(LayoutStateV1::Leaf(1)),
                },
            }],
        };
        let s = Session::restore_from(saved, spec(), size(), cfg()).await.unwrap();
        let m = s.window_manager.lock().await;
        let vp = m.viewport();
        let win = m.active_window();
        let leaves = win.layout().dfs_leaves();
        let r0 = win.layout().rect_of(leaves[0], vp).unwrap();
        // 0.3 of the usable width (NOT the 0.5 the replay used to leave):
        // for an 80-col host, viewport is 78 wide → usable 77 → ~23 cols.
        assert!(
            (20..=26).contains(&r0.cols),
            "first pane should be ~30% wide, got {r0:?} of {vp:?}"
        );
    }

    #[tokio::test]
    async fn restore_round_trips_window_home_cwd() {
        let _g = crate::test_env::isolate();
        let original = Session::new("rthome".into(), spec(), size(), cfg()).unwrap();
        let saved = {
            let mut wm = original.window_manager.lock().await;
            wm.set_window_home_cwd(0, Some("/restored/base".into()));
            // `snapshot_for_persist` is sync and takes the locked `WindowManager`.
            original.snapshot_for_persist(&wm)
        };
        assert_eq!(saved.windows[0].home_cwd.as_deref(), Some("/restored/base"));
        let restored = Session::restore_from(saved, spec(), size(), cfg()).await.unwrap();
        let wm = restored.window_manager.lock().await;
        assert_eq!(wm.windows()[0].home_cwd.as_deref(), Some("/restored/base"));
    }

    #[tokio::test]
    async fn snapshot_converts_osc7_cwd_to_plain_path() {
        // Regression: OSC 7 stores the raw `file://host/path` URL on `Screen.cwd`,
        // and persisting that verbatim made restored panes spawn in `$HOME`
        // (`portable-pty` silently falls back for non-directory cwds).
        let _g = crate::test_env::isolate();
        let s = Session::new("t-osc7-snap".into(), spec(), size(), cfg()).unwrap();
        let saved = {
            let wm = s.window_manager.lock().await;
            let pid = wm.active_window().active();
            wm.active_window().pane(pid).unwrap().with_screen_mut(|scr| {
                scr.cwd = Some("file://localhost/tmp/somewhere".into());
            });
            s.snapshot_for_persist(&wm)
        };
        assert_eq!(
            saved.windows[0].panes[0].cwd.as_deref(),
            Some("/tmp/somewhere"),
            "persisted pane cwd must be a plain path, not an OSC-7 URL"
        );
    }

    #[test]
    fn restore_cwd_strips_legacy_osc7_urls() {
        // Legacy persist files (pre-fix) carry raw OSC-7 URLs; the restore
        // seam must convert them so SpawnSpec.cwd is a real directory path.
        assert_eq!(restore_cwd(Some("file:///tmp")).as_deref(), Some("/tmp"));
        assert_eq!(
            restore_cwd(Some("file://localhost/tmp/x")).as_deref(),
            Some("/tmp/x")
        );
        assert_eq!(
            restore_cwd(Some("/plain/path")).as_deref(),
            Some("/plain/path")
        );
        // Malformed -> None (daemon-cwd fallback), not a bogus path.
        assert_eq!(restore_cwd(Some("file://nohostnopath")), None);
        assert_eq!(restore_cwd(None), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restore_reanchors_session_cwd_for_new_windows() {
        // Regression: `restore_from` never called `set_session_cwd`, so after a
        // restore `Ctrl+a c` (NewWindow anchors to `session_cwd`) lost its
        // anchor. Window 0's saved home base is the persisted proxy for it.
        use crate::persist::{LayoutStateV1, PaneStateV1, SessionStateV1, WindowStateV1};
        let _g = crate::test_env::isolate();
        let saved = SessionStateV1 {
            schema: crate::persist::SCHEMA_VERSION,
            name: "t-restore-anchor".into(),
            created: chrono::Utc::now(),
            active_window: 0,
            windows: vec![WindowStateV1 {
                name: "w".into(),
                sync_input: false,
                home_cwd: Some("/tmp".into()),
                active_pane: 0,
                panes: vec![PaneStateV1 { cwd: None, name: None }],
                layout: LayoutStateV1::Leaf(0),
            }],
        };
        let s = Session::restore_from(saved, spec(), size(), cfg()).await.unwrap();
        s.handle_command(plexy_glass_mux::Command::NewWindow).await.unwrap();
        let wm = s.window_manager.lock().await;
        // NewWindow stamps session_cwd onto the new window's home base; if
        // the anchor was restored, the second window inherits it.
        assert_eq!(
            wm.windows()[1].home_cwd.as_deref(),
            Some("/tmp"),
            "restored session must re-anchor session_cwd for NewWindow"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restore_from_round_trips_pane_name() {
        let _g = crate::test_env::isolate();
        let original = Session::new("rtn".into(), spec(), size(), cfg()).unwrap();
        {
            let wm = original.window_manager.lock().await;
            let pid = wm.active_window().active();
            wm.active_window().pane(pid).unwrap().set_name(Some("logs".into()));
        }
        original.mark_dirty();
        assert!(
            crate::test_env::poll_until(std::time::Duration::from_secs(10), || {
                crate::persist::load_session("rtn").ok().flatten().is_some()
            })
            .await,
            "persist debounce never wrote 'rtn'"
        );
        drop(original);
        let saved = crate::persist::load_session("rtn").expect("load").expect("file");
        assert_eq!(saved.windows[0].panes[0].name.as_deref(), Some("logs"));
        let restored = Session::restore_from(saved, spec(), size(), cfg()).await.unwrap();
        let wm = restored.window_manager.lock().await;
        let pid = wm.active_window().active();
        assert_eq!(
            wm.active_window().pane(pid).unwrap().name().as_deref(),
            Some("logs"),
            "pane name survives save + restore"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn split_command_writes_persisted_layout() {
        let _g = crate::test_env::isolate();
        let s = Session::new("p5-split".into(), spec(), size(), cfg()).unwrap();
        s.handle_command(plexy_glass_mux::Command::SplitV).await.unwrap();
        assert!(
            crate::test_env::poll_until(std::time::Duration::from_secs(10), || {
                crate::persist::load_session("p5-split").ok().flatten().is_some()
            })
            .await,
            "persist debounce never wrote 'p5-split'"
        );
        let loaded = crate::persist::load_session("p5-split")
            .expect("load")
            .expect("file");
        assert_eq!(loaded.windows[0].panes.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kill_closes_split_unix_socket_to_client() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let _g = crate::test_env::isolate();
        let s = Session::new("sp".into(), spec(), size(), cfg()).unwrap();
        let handle = tokio::task::block_in_place(|| {
            s.register_client(size(), Arc::new(AtomicBool::new(false)))
        })
        .unwrap();
        let frame_rx = handle.frame_rx.clone();

        // Real bidirectional socket, split exactly like serve_attach does.
        let (client_sock, server_sock) = tokio::net::UnixStream::pair().unwrap();
        let (mut server_read, server_write) = tokio::io::split(server_sock);

        let renderer = crate::renderer::Renderer::new();
        // No session switch in this test; keep the sender alive so the switch
        // arm simply never fires.
        let (_switch_tx, switch_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer_task = tokio::spawn(async move {
            let _ = renderer.run(frame_rx, switch_rx, server_write).await;
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
        let _g = crate::test_env::isolate();
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
        let _g = crate::test_env::isolate();
        let s = Session::new("bc".into(), spec(), size(), cfg()).unwrap();
        s.mark_dirty();
        assert!(
            crate::test_env::poll_until(std::time::Duration::from_secs(10), || {
                crate::persist::load_session("bc").ok().flatten().is_some()
            })
            .await,
            "persist debounce never wrote 'bc'"
        );
        assert!(crate::persist::load_session("bc").unwrap().is_some());
        crate::persist::delete_session("bc").unwrap();
        // Close, then try hard to make the persist task re-save.
        s.begin_close();
        s.begin_close(); // idempotent: must not panic
        s.mark_dirty();
        s.persist_notify.notify_one();
        // Negative assertion: proving absence requires a fixed wait. We sleep
        // long enough for the debounce (1500ms) + one extra cycle to fire if
        // begin_close failed to suppress it, then assert no file was written.
        tokio::time::sleep(std::time::Duration::from_millis(1800)).await;
        assert!(
            crate::persist::load_session("bc").unwrap().is_none(),
            "persist task re-saved the file after begin_close"
        );
        s.terminate_panes().await; // exercise the path; child dies
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mark_dirty_eventually_writes_file() {
        let _g = crate::test_env::isolate();
        let s = Session::new("dirty-test".into(), spec(), size(), cfg()).unwrap();
        s.mark_dirty();
        assert!(
            crate::test_env::poll_until(std::time::Duration::from_secs(10), || {
                crate::persist::load_session("dirty-test").ok().flatten().is_some()
            })
            .await,
            "persist debounce never wrote 'dirty-test'"
        );
        let loaded = crate::persist::load_session("dirty-test")
            .expect("load")
            .expect("file should exist");
        assert_eq!(loaded.name, "dirty-test");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn persist_failure_resets_dirty_and_retries() {
        let _g = crate::test_env::isolate();
        let s = Session::new("persist-retry".into(), spec(), size(), cfg()).unwrap();
        // Inject a write failure: occupy the sessions-dir path with a FILE so
        // `save_session`'s `create_dir_all` fails (ENOTDIR-class error).
        let dir = crate::persist::sessions_dir();
        std::fs::create_dir_all(dir.parent().expect("sessions dir has a parent")).unwrap();
        std::fs::write(&dir, b"not a directory").unwrap();
        s.mark_dirty();
        // Poll until the failed attempt has run and re-set dirty=true
        // (the old code left it false, losing the snapshot).
        assert!(
            crate::test_env::poll_until(std::time::Duration::from_secs(10), || {
                s.dirty.load(std::sync::atomic::Ordering::Relaxed)
            })
            .await,
            "failed persist must re-set dirty so the snapshot is retried"
        );
        // Heal the path. The failure handler self-notified, so the loop
        // retries on its own and we don't need another `mark_dirty`.
        std::fs::remove_file(&dir).unwrap();
        assert!(
            crate::test_env::poll_until(std::time::Duration::from_secs(10), || {
                crate::persist::load_session("persist-retry").ok().flatten().is_some()
            })
            .await,
            "retry after heal should have persisted the session"
        );
        let loaded = crate::persist::load_session("persist-retry")
            .expect("load")
            .expect("retry after heal should have persisted the session");
        assert_eq!(loaded.name, "persist-retry");
        assert!(
            !s.dirty.load(std::sync::atomic::Ordering::Relaxed),
            "successful retry should leave dirty clear"
        );
    }

    #[tokio::test]
    async fn snapshot_for_persist_captures_single_pane_session() {
        let _g = crate::test_env::isolate();
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
        let _g = crate::test_env::isolate();
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

#[cfg(test)]
mod reencode_tests {
    use super::{reencode_input, select_target};
    use plexy_glass_keys::{encode, KeyboardTarget};
    use plexy_glass_mux::{Key, KeyEvent, Modifiers};
    use plexy_glass_protocol::NegotiatedKbd;

    #[test]
    fn target_precedence_and_encoding() {
        let e = KeyEvent::new(Key::Char('i'), Modifiers::CTRL | Modifiers::SHIFT);
        // Kitty flags present -> CSI-u (wins over modkeys).
        assert_eq!(encode(&e, select_target(1, 2), false), b"\x1b[105;6u");
        // No Kitty, modifyOtherKeys level 2 -> 27-form.
        assert_eq!(encode(&e, select_target(0, 2), false), b"\x1b[27;6;105~");
        // Neither -> Legacy.
        assert!(matches!(select_target(0, 0), KeyboardTarget::Legacy));
    }

    // BLOCKER regression: a Kitty-capable client's OUTER terminal emits CSI-u
    // for every key (`a`->\e[97u, Ctrl+a->\e[97;5u). Forwarding those bytes
    // verbatim into a default un-negotiated (Legacy) pane breaks every
    // keystroke. The re-encode stage must DOWN-CONVERT to legacy.
    #[test]
    fn kitty_client_into_legacy_pane_downconverts() {
        // Plain `a`: \e[97u -> "a".
        let a = KeyEvent::new(Key::Char('a'), Modifiers::empty());
        assert_eq!(
            reencode_input(NegotiatedKbd::Kitty(31), 0, 0, false, &a, b"\x1b[97u"),
            b"a",
        );
        // Ctrl+a: \e[97;5u -> 0x01 (SOH), the legacy control byte.
        let ctrl_a = KeyEvent::new(Key::Char('a'), Modifiers::CTRL);
        assert_eq!(
            reencode_input(NegotiatedKbd::Kitty(31), 0, 0, false, &ctrl_a, b"\x1b[97;5u"),
            vec![0x01],
        );
    }

    // A genuinely Legacy client into a Legacy pane keeps lossless raw
    // passthrough: the incoming bytes are already legacy, and passthrough
    // preserves anything the parser couldn't model.
    #[test]
    fn legacy_client_into_legacy_pane_passes_raw() {
        let a = KeyEvent::plain(Key::Char('a'));
        assert_eq!(
            reencode_input(NegotiatedKbd::Legacy, 0, 0, false, &a, b"a"),
            b"a",
        );
        // A byte the parser couldn't model losslessly must pass through
        // unchanged. The event here is irrelevant for a Legacy client; only the
        // raw bytes matter.
        let raw = b"\x1b[1;2R"; // e.g. an unmodeled DSR-ish report
        assert_eq!(
            reencode_input(NegotiatedKbd::Legacy, 0, 0, false, &a, raw),
            raw,
        );
    }

    // A Kitty client into a Kitty pane re-encodes to the pane's Kitty form
    // (mirrors target_precedence_and_encoding's Kitty case).
    #[test]
    fn kitty_client_into_kitty_pane_reencodes() {
        let e = KeyEvent::new(Key::Char('i'), Modifiers::CTRL | Modifiers::SHIFT);
        // Non-zero pane kitty flags mean we go through
        // `encode(.., Kitty(flags), ..)`, and `raw_bytes` are ignored on the
        // encode path.
        assert_eq!(
            reencode_input(NegotiatedKbd::Kitty(31), 1, 0, false, &e, b"\x1b[105;6u"),
            encode(&e, KeyboardTarget::Kitty(1), false),
        );
    }
}
