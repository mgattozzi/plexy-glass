//! Owns all windows for one attached client.

use crate::{error::DaemonError, window::Window};
use mouse::{ClickHistory, ResizeDrag};
use plexy_glass_mux::{Overlay, PaneId, Rect, Selection, SplitDir, WindowId};
use plexy_glass_protocol::{PtySize, SpawnSpec};
use std::sync::Arc;
use std::time::Duration;
// See window.rs: tokio::time::Instant is used so unit tests with
// start_paused = true / time::advance control silence checks without real sleeps.
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;

/// How long a transient status-line message stays visible before it is cleared
/// on the next recompose. Mirrored by the `Session` wake timer.
pub(crate) const STATUS_TTL: Duration = Duration::from_secs(3);

/// A transient status-line message and the instant it stops being shown.
struct StatusMessage {
    text: String,
    expires_at: Instant,
}

pub struct WindowManager {
    windows: Vec<Window>,
    active: usize,
    next_pane_id: u32,
    next_window_id: u32,
    host_size: PtySize,
    pub notify: Arc<Notify>,
    /// `SpawnSpec` used to create new panes/windows (cloned from the client's
    /// initial `Spawn`).
    default_spec: SpawnSpec,
    /// The session's base cwd (config `session cwd`, tilde-expanded). Seeds the
    /// home base of windows created interactively (`NewWindow`); `None` for
    /// non-declared sessions.
    session_cwd: Option<String>,
    /// Each pane sends its `PaneId` here when its child exits. `None` in tests
    /// where pane lifecycle is driven manually.
    death_tx: Option<mpsc::Sender<PaneId>>,
    /// In-flight mouse selection (left-press → drag → release). `None` between
    /// drags.
    selection: Option<Selection>,
    /// Active config shared with every pane this manager spawns. Hot reload
    /// swaps this Arc and walks the panes calling `update_config`.
    config: Arc<plexy_glass_config::Config>,
    /// Border drag-resize in progress. `None` between drags.
    resize_drag: Option<ResizeDrag>,
    /// Last left-press for multi-click classification.
    click_history: Option<ClickHistory>,
    /// Physical row index where the status bar paints, or `None` if the bar is
    /// hidden. Set by `set_status_layout`.
    status_bar_row: Option<u16>,
    /// Vertical offset of the logical pane band from physical row 0: `0` when
    /// the status bar is at the bottom, `1` when it is at the top. Mouse events
    /// arrive in physical coordinates; this offset translates them into the
    /// layout's logical pane-coordinate space. Set by `set_status_layout`.
    pane_row_offset: u16,
    /// Clickable regions in the current status bar. Refreshed each render
    /// tick via `set_status_hits`.
    status_hits: Vec<plexy_glass_status::StatusHit>,
    /// Set when a status-bar click on the session widget fires `Detach`.
    /// `Connection::serve_attach` polls this each iteration of its input loop
    /// and exits when true.
    pub detach_requested: bool,
    /// Previously-active window index, for `select_last_window`. Updated on
    /// every window switch.
    last_active_window: Option<usize>,
    /// Active interactive overlay (rename prompt / help), or `None`. Session-
    /// shared, mirroring copy mode. While `Some`, the connection routes keys to
    /// `handle_overlay_key` instead of the keymap/shell.
    overlay: Option<Overlay>,
    /// The pane a pane-rename overlay targets, captured when the overlay opens
    /// so a focus change cannot retarget it.
    rename_pane_target: Option<PaneId>,
    /// A transient status-line message (command-prompt feedback), or `None`.
    /// Painted on the bottom content row when no overlay is open; cleared lazily
    /// once expired (see `take_active_message`).
    status_message: Option<StatusMessage>,
    /// Durable command-prompt history (newest last, capped). Cloned into the
    /// command overlay at open time for Up/Down recall; appended on commit.
    command_history: Vec<String>,
    /// The session-wide marked pane (tmux's `select-pane -m`), or `None`. The
    /// target of `join-pane`/`swap-pane`. Runtime-only (not persisted); cleared
    /// when its pane dies (`handle_pane_death`) or is joined.
    marked_pane: Option<PaneId>,
    /// The floating popup pane (transient, modal, never in any layout tree),
    /// or `None`. See `crate::popup`.
    popup: Option<crate::popup::Popup>,
}

/// Maximum retained command-prompt history entries.
const COMMAND_HISTORY_CAP: usize = 100;

impl WindowManager {
    pub fn new(
        first_spec: SpawnSpec,
        host_size: PtySize,
        notify: Arc<Notify>,
        death_tx: Option<mpsc::Sender<PaneId>>,
        config: Arc<plexy_glass_config::Config>,
    ) -> Result<Self, DaemonError> {
        let viewport = host_viewport(host_size);
        let first = Window::spawn_first(
            WindowId(0),
            "shell".into(),
            PaneId(0),
            first_spec.clone(),
            viewport,
            Arc::clone(&notify),
            death_tx.clone(),
            Arc::clone(&config),
        )?;
        Ok(Self {
            windows: vec![first],
            active: 0,
            next_pane_id: 1,
            next_window_id: 1,
            host_size,
            notify,
            // New windows / splits open an interactive shell, NOT a clone of the
            // session's first command. Cloning `first_spec` would mean a session
            // started with (or whose first declared pane runs) e.g. `claude` would
            // re-launch that command for every `new-window`/`split`. Inherit the
            // session's base env, but always run the default shell.
            default_spec: SpawnSpec {
                program: crate::declared::default_shell(),
                args: Vec::new(),
                env: first_spec.env,
                cwd: None,
            },
            session_cwd: None,
            death_tx,
            selection: None,
            config,
            resize_drag: None,
            click_history: None,
            status_bar_row: None,
            pane_row_offset: 0,
            status_hits: Vec::new(),
            detach_requested: false,
            last_active_window: None,
            overlay: None,
            rename_pane_target: None,
            status_message: None,
            command_history: Vec::new(),
            marked_pane: None,
            popup: None,
        })
    }

    /// The session-wide marked pane, if any. Read by the frame build to draw the
    /// marked indicator.
    pub fn marked_pane(&self) -> Option<PaneId> {
        self.marked_pane
    }

    /// Record the physical status-bar row (or `None` to disable status-bar
    /// click routing) and the pane band's vertical offset (`0` for a bottom
    /// bar, `1` for a top bar). Called by the render coordinator each frame so
    /// mouse hit-testing stays aligned with the compositor's placement.
    pub fn set_status_layout(&mut self, status_row: Option<u16>, pane_row_offset: u16) {
        self.status_bar_row = status_row;
        self.pane_row_offset = pane_row_offset;
    }

    /// Update the clickable-region table from the latest status snapshot.
    pub fn set_status_hits(&mut self, hits: Vec<plexy_glass_status::StatusHit>) {
        self.status_hits = hits;
    }

    /// The active overlay, if any. Read by the render coordinator and by the
    /// connection layer to decide whether to capture keys.
    pub fn overlay(&self) -> Option<&Overlay> {
        self.overlay.as_ref()
    }

    /// Set the transient status-line message, expiring `STATUS_TTL` from now.
    /// Replaces any prior message.
    pub fn set_status_message(&mut self, text: String) {
        self.status_message = Some(StatusMessage {
            text,
            expires_at: Instant::now() + STATUS_TTL,
        });
    }

    /// The active (unexpired) status-line text, clearing it in place if it has
    /// expired. Called by the render coordinator each frame.
    pub fn take_active_message(&mut self) -> Option<&str> {
        let expired = self
            .status_message
            .as_ref()
            .is_some_and(|m| Instant::now() >= m.expires_at);
        if expired {
            self.status_message = None;
        }
        self.status_message.as_ref().map(|m| m.text.as_str())
    }

    /// Read-only access to the in-flight selection, if any. Used by the
    /// compositor to draw highlight cells.
    pub fn selection(&self) -> Option<&Selection> {
        self.selection.as_ref()
    }

    /// Close a pane whose child exited. Called by Connection when it
    /// receives a `PaneId` on the death channel.
    pub fn handle_pane_death(&mut self, pane_id: PaneId) -> Result<(), DaemonError> {
        // The popup's pane lives outside every window; its death just closes
        // the popup (the primary dismissal path: the command exited).
        if self.popup.as_ref().map(|p| p.pane.id()) == Some(pane_id) {
            self.popup = None;
            self.notify.notify_one();
            return Ok(());
        }
        let viewport = self.viewport();
        let mut closed_idx: Option<usize> = None;
        for (idx, w) in self.windows.iter_mut().enumerate() {
            if w.pane(pane_id).is_some() {
                let outcome = w.close_pane(pane_id)?;
                if matches!(outcome, plexy_glass_mux::CloseOutcome::TreeEmpty) {
                    closed_idx = Some(idx);
                } else {
                    w.resize(viewport)?;
                }
                break;
            }
        }
        if let Some(idx) = closed_idx {
            self.windows.remove(idx);
            if idx <= self.active && self.active > 0 {
                self.active -= 1;
            }
            if self.active >= self.windows.len() && !self.windows.is_empty() {
                self.active = self.windows.len() - 1;
            }
            self.fixup_last_active_after_removal(idx);
        }
        // The session ends when its last window closes; a floating popup must
        // not orphan its child (nothing else would reap it).
        if self.windows.is_empty() {
            self.close_popup();
        }
        // A dead pane can no longer be a join/swap target.
        if self.marked_pane == Some(pane_id) {
            self.marked_pane = None;
        }
        self.notify.notify_one();
        Ok(())
    }

    /// Drain every pane's activity/bell signal and fold it into the per-window
    /// sticky alert flags. The sole drainer of the pane atomics, called once per
    /// frame by the render coordinator (the status tick task only *reads* the
    /// flags). The current window's alerts are cleared (you're watching it); a
    /// background window with the matching monitor option on gets its sticky flag
    /// set, and a false→true EDGE on that flag fires a status-line alert message.
    ///
    /// Returns `true` if any alert message was emitted this drain, so the
    /// coordinator can schedule the message's TTL-expiry repaint wake after it
    /// releases the WM lock (the message is set here under the held lock, see
    /// `set_status_message`'s deadlock note in the coordinator).
    #[must_use = "schedule the status-message TTL wake when an edge fired"]
    pub fn update_monitor_flags(&mut self) -> bool {
        let active = self.active;
        // Collect edge messages while iterating (the iterator borrows
        // `windows` mutably; `set_status_message` borrows `self`), then emit
        // after the loop. The status line is a single slot, so on simultaneous
        // edges the LAST message wins (accepted, same as any rapid succession).
        let mut message: Option<String> = None;
        for (i, w) in self.windows.iter_mut().enumerate() {
            let (acted, belled) = w.drain_pane_alerts();
            // Silence-timing bookkeeping updates every drain for every window
            // regardless of monitor/active state (the silence tick reads
            // `last_output`); output also resets the per-window episode latch.
            w.note_drain_output(acted);
            // Command-completion baselines advance every drain for every
            // window; the flag/edge is recorded only for a monitored non-active
            // window (background completion you can't see).
            let record_done = i != active && w.monitor_command();
            let done_edge = w.drain_command_completion(record_done);
            if i == active {
                w.clear_alerts();
            } else {
                if acted && w.monitor_activity() && w.set_activity() {
                    message = Some(format!("activity in window {} ({})", i + 1, w.name));
                }
                if belled && w.monitor_bell() && w.set_bell() {
                    message = Some(format!("bell in window {} ({})", i + 1, w.name));
                }
                if let Some(exit) = done_edge {
                    message = Some(match exit {
                        Some(code) => format!("done in window {} ({}): exit {code}", i + 1, w.name),
                        None => format!("done in window {} ({})", i + 1, w.name),
                    });
                }
            }
        }
        if let Some(text) = message {
            self.set_status_message(text);
            true
        } else {
            false
        }
    }

    /// Whether any window currently arms silence monitoring. The session uses
    /// this to spawn the dedicated silence tick task on the first arm and abort
    /// it on the last disarm (armed-only fast path, no idle 1 Hz task).
    pub fn any_silence_monitored(&self) -> bool {
        self.windows.iter().any(|w| w.monitor_silence().is_some())
    }

    /// Silence-tick step: check every monitored NON-active window for the
    /// silence threshold and fold a fresh edge into the sticky `~` flag + a
    /// status message. Returns `true` if any edge fired, so the tick notifies
    /// the coordinator ONLY on an edge (a silent session is by definition not
    /// rendering, so the tick must drive the wake, not ride renders). The
    /// active-window exclusion is required: an idle active window would
    /// otherwise flicker at 1 Hz (tick sets → render clears → tick re-fires).
    #[must_use = "schedule the status-message TTL wake when a silence edge fired"]
    pub fn check_silence_alerts(&mut self) -> bool {
        let active = self.active;
        let now = Instant::now();
        let mut message: Option<String> = None;
        for (i, w) in self.windows.iter_mut().enumerate() {
            if i == active {
                continue;
            }
            if w.check_silence(now) {
                message = Some(format!("silence in window {} ({})", i + 1, w.name));
            }
        }
        if let Some(text) = message {
            self.set_status_message(text);
            true
        } else {
            false
        }
    }

    pub fn host_size(&self) -> PtySize {
        self.host_size
    }

    pub fn viewport(&self) -> Rect {
        host_viewport(self.host_size)
    }

    pub fn active_window(&self) -> &Window {
        &self.windows[self.active]
    }

    pub fn active_window_mut(&mut self) -> &mut Window {
        &mut self.windows[self.active]
    }

    pub fn windows_mut(&mut self) -> &mut [Window] {
        &mut self.windows
    }

    pub fn set_active_window(&mut self, idx: usize) {
        if idx < self.windows.len() {
            self.active = idx;
        }
    }

    /// Test seam: pin the program new windows/splits/popups spawn (production
    /// default is the user's `$SHELL`, which unit tests must not depend on).
    #[cfg(test)]
    pub(crate) fn set_default_program(&mut self, program: &str) {
        self.default_spec.program = program.to_string();
    }

    /// Spawn a new window using a caller-supplied spec (used by session
    /// restore, where every restored window's first pane gets its own cwd).
    pub fn new_window_with_spec(
        &mut self,
        spec: SpawnSpec,
        name: String,
    ) -> Result<(), DaemonError> {
        let id = WindowId(self.next_window_id);
        self.next_window_id += 1;
        let first_pane = self.alloc_pane_id();
        let viewport = self.viewport();
        let window = Window::spawn_first(
            id,
            name,
            first_pane,
            spec,
            viewport,
            Arc::clone(&self.notify),
            self.death_tx.clone(),
            Arc::clone(&self.config),
        )?;
        self.windows.push(window);
        self.active = self.windows.len() - 1;
        Ok(())
    }

    /// Split an existing window's pane at DFS index `target_dfs_idx`. Used by
    /// session restore.
    pub fn split_window_at_dfs(
        &mut self,
        window_idx: usize,
        target_dfs_idx: u32,
        dir: SplitDir,
        spec: SpawnSpec,
    ) -> Result<(), DaemonError> {
        let viewport = self.viewport();
        let new_id = self.alloc_pane_id();
        let win = self
            .windows
            .get_mut(window_idx)
            .ok_or_else(|| DaemonError::Io(std::io::Error::other(format!("window {window_idx} missing"))))?;
        let leaves = win.layout().dfs_leaves();
        let target_pane = *leaves
            .get(target_dfs_idx as usize)
            .ok_or_else(|| DaemonError::Io(std::io::Error::other(format!("dfs idx {target_dfs_idx} out of range"))))?;
        win.split_at(
            target_pane,
            dir,
            new_id,
            spec,
            viewport,
            Arc::clone(&self.notify),
            self.death_tx.clone(),
            Arc::clone(&self.config),
        )
    }

    pub fn set_window_name(&mut self, window_idx: usize, name: String) {
        if let Some(w) = self.windows.get_mut(window_idx) {
            w.name = name;
        }
    }

    /// Set the session base cwd (declared/restored sessions call this so
    /// interactive new windows anchor to it).
    pub fn set_session_cwd(&mut self, cwd: Option<String>) {
        self.session_cwd = cwd;
    }

    /// Set a window's home base by index (used while building/restoring).
    pub fn set_window_home_cwd(&mut self, window_idx: usize, home_cwd: Option<String>) {
        if let Some(w) = self.windows.get_mut(window_idx) {
            w.home_cwd = home_cwd;
        }
    }

    /// The cwd a split / new pane in the active window spawns at: the window's
    /// home base (deterministic, never the active pane's live `cd` location).
    pub fn split_cwd(&self) -> Option<String> {
        self.active_window().home_cwd.clone()
    }

    /// The floating popup, if open.
    pub fn popup(&self) -> Option<&crate::popup::Popup> {
        self.popup.as_ref()
    }

    pub fn has_popup(&self) -> bool {
        self.popup.is_some()
    }

    /// The pane user input goes to: the floating popup's pane while one is
    /// open (it is modal and owns input), otherwise the active window's
    /// active pane. This is THE definition of "where user input goes", so
    /// keep every input-routing decision (byte routing, paste bracketing,
    /// focus events, …) on it so they cannot disagree about the target.
    pub fn input_target_pane(&self) -> Option<&crate::pane::Pane> {
        match &self.popup {
            Some(p) => Some(&p.pane),
            None => self.active_window().active_pane(),
        }
    }

    /// The live cwd of `pane`: its OSC-7 location (`screen.cwd`), falling
    /// back to the active window's home base. The shared per-pane helper
    /// behind `popup_cwd` (active pane) and pipe-pane (the input TARGET pane,
    /// which is the popup's pane while one is open), one definition so the
    /// two cannot drift.
    pub fn pane_cwd(&self, pane: &crate::pane::Pane) -> Option<String> {
        pane.with_screen(|s| s.cwd.clone())
            .and_then(|url| crate::popup::osc7_to_path(&url))
            .or_else(|| self.active_window().home_cwd.clone())
    }

    /// The cwd the popup spawns at: the active pane's live OSC-7 location,
    /// falling back to the window home base. This intentionally diverges from
    /// `split_cwd` (home base only): a popup acts on the current context.
    pub fn popup_cwd(&self) -> Option<String> {
        match self.active_window().active_pane() {
            Some(p) => self.pane_cwd(p),
            None => self.active_window().home_cwd.clone(),
        }
    }

    /// The program new panes, windows, popups, and pipe-pane consumers run
    /// (the user's `$SHELL` in production; pinnable via `set_default_program`
    /// in tests).
    pub fn default_program(&self) -> String {
        self.default_spec.program.clone()
    }

    /// Open the floating popup (last-wins: an existing popup is replaced, but
    /// only once the new pane has actually spawned).
    /// `command` runs via `$SHELL -c`; `None` runs the interactive shell.
    pub fn open_popup(&mut self, command: Option<String>) -> Result<(), DaemonError> {
        // One source of truth: `default_spec.program` is built from
        // `default_shell()` at construction (and pinnable in tests).
        let shell = self.default_spec.program.clone();
        let args = match &command {
            Some(cmd) => vec!["-c".to_string(), cmd.clone()],
            None => Vec::new(),
        };
        let spec = SpawnSpec {
            program: shell,
            args,
            env: self.default_spec.env.clone(),
            cwd: self.popup_cwd(),
        };
        let size = crate::popup::popup_pty_size(plexy_glass_mux::popup_rect(self.viewport()));
        let id = self.alloc_pane_id();
        let pane = crate::pane::Pane::spawn(
            id,
            spec,
            size,
            Arc::clone(&self.notify),
            self.death_tx.clone(),
            Arc::clone(&self.config),
        )?;
        // Last-wins: only replace (and kill) the old popup once the new one
        // actually spawned, so a spawn failure can't destroy the old popup.
        self.close_popup();
        // Rule 0 will swallow the in-flight Release once the popup is open, so
        // an active drag/selection would freeze and bite after close. Drop them.
        self.resize_drag = None;
        self.selection = None;
        let title = command.unwrap_or_else(|| "popup".to_string());
        self.popup = Some(crate::popup::Popup { pane, title });
        self.notify.notify_one();
        Ok(())
    }

    /// Close the floating popup (if open), killing its child. The child's
    /// later death-channel message finds neither a window pane nor a popup
    /// and is a harmless no-op.
    pub fn close_popup(&mut self) {
        if let Some(p) = self.popup.take() {
            p.pane.kill_child();
            self.notify.notify_one();
        }
    }

    /// Rearrange the active window's panes into `preset`. For the main-*
    /// presets the active pane takes the main slot; otherwise DFS order is
    /// kept. A pure rearrangement: panes/PTYs are untouched, then resized to
    /// their new rects. Single-pane windows are a structural no-op but still
    /// record the preset so cycling stays predictable.
    /// Callers must clear zoom first (`handle_command` does, via
    /// `command_clears_zoom`).
    pub fn apply_layout_preset(
        &mut self,
        preset: plexy_glass_mux::LayoutPreset,
    ) -> Result<(), DaemonError> {
        use plexy_glass_mux::LayoutPreset;
        let viewport = self.viewport();
        let win = self.active_window_mut();
        let mut panes = win.layout().dfs_leaves();
        if matches!(preset, LayoutPreset::MainHorizontal | LayoutPreset::MainVertical)
            && let Some(pos) = panes.iter().position(|p| *p == win.active())
        {
            let active = panes.remove(pos);
            panes.insert(0, active);
        }
        win.layout_mut().apply_preset(preset, &panes);
        win.last_preset = Some(preset);
        win.resize(viewport)?;
        self.notify.notify_one();
        Ok(())
    }

    /// Apply the preset after the window's remembered one (wrapping), or the
    /// first preset when none has been applied yet.
    pub fn next_layout(&mut self) -> Result<(), DaemonError> {
        let next = match self.active_window().last_preset {
            Some(p) => p.next(),
            None => plexy_glass_mux::LayoutPreset::ALL[0],
        };
        self.apply_layout_preset(next)
    }

    /// Rename the active window (command-prompt `rename` path). Mirrors the
    /// rename-overlay commit, but the name comes straight from the prompt.
    pub fn rename_active_window(&mut self, name: String) {
        self.set_window_name(self.active, name);
    }

    /// Rename the active pane (command-prompt `rename-pane` path).
    pub fn rename_active_pane(&mut self, name: String) {
        let pid = self.active_window().active();
        if let Some(p) = self.active_window().pane(pid) {
            p.set_name(Some(name));
        }
    }

    // ----- choose-tree by-id actions (used by the connection against any
    // session's WM; each scans `windows` for the id, there is no id-keyed API).

    /// Make the window with `id` active. Clears any zoom first (matching the
    /// keyboard window-switch commands). `false` if no such window.
    pub fn select_window_by_id(&mut self, id: WindowId) -> bool {
        let Some(idx) = self.windows.iter().position(|w| w.id == id) else {
            return false;
        };
        let _ = self.clear_zoom_restore();
        self.set_active_window(idx);
        true
    }

    /// Focus the pane with `id`: make its window active, clear that window's
    /// zoom so the pane is visible, focus it, and resize. `false` if not found.
    pub fn focus_pane_by_id(&mut self, pane: PaneId) -> bool {
        let Some(idx) = self.windows.iter().position(|w| w.pane(pane).is_some()) else {
            return false;
        };
        let viewport = self.viewport();
        self.set_active_window(idx);
        let w = &mut self.windows[idx];
        w.clear_zoom();
        w.focus(pane);
        let _ = w.resize(viewport);
        true
    }

    /// Rename the window with `id`. `false` if not found.
    pub fn rename_window_by_id(&mut self, id: WindowId, name: String) -> bool {
        let Some(w) = self.windows.iter_mut().find(|w| w.id == id) else {
            return false;
        };
        w.name = name;
        true
    }

    /// Rename the pane with `id` (stores the exact string; trimming already
    /// happened in `handle_tree`). `false` if not found.
    pub fn rename_pane_by_id(&mut self, pane: PaneId, name: String) -> bool {
        for w in self.windows.iter() {
            if let Some(p) = w.pane(pane) {
                p.set_name(Some(name));
                return true;
            }
        }
        false
    }

    /// SIGHUP every pane child in the window with `id`; the per-session death
    /// channel performs the structural close (and ends the session if this was
    /// its last window). `false` if not found.
    pub fn kill_window_panes(&mut self, id: WindowId) -> bool {
        let Some(w) = self.windows.iter().find(|w| w.id == id) else {
            return false;
        };
        for pid in w.layout().panes() {
            if let Some(p) = w.pane(pid) {
                p.kill_child();
            }
        }
        true
    }

    /// SIGHUP the pane child with `id`; the death channel closes it. `false` if
    /// not found.
    pub fn kill_pane_child(&mut self, pane: PaneId) -> bool {
        for w in self.windows.iter() {
            if let Some(p) = w.pane(pane) {
                p.kill_child();
                return true;
            }
        }
        false
    }

    pub fn windows(&self) -> &[Window] {
        &self.windows
    }

    pub fn active_idx(&self) -> usize {
        self.active
    }

    /// Switch the active window to `idx`, recording the current window as the
    /// "last active" so `select_last_window` can toggle back. No-op for an
    /// out-of-range or same index.
    fn switch_to_window(&mut self, idx: usize) {
        if idx >= self.windows.len() || idx == self.active {
            return;
        }
        self.last_active_window = Some(self.active);
        self.active = idx;
    }

    /// Clear a zoom overlay (if any) and restore pane sizes. Called before
    /// structural/navigation commands so the overlay never outlives the
    /// layout it hid. No-op when not zoomed.
    fn clear_zoom_restore(&mut self) -> Result<(), DaemonError> {
        if self.active_window_mut().clear_zoom() {
            let viewport = self.viewport();
            self.active_window_mut().resize(viewport)?;
        }
        Ok(())
    }

    pub fn on_host_resize(&mut self, new_size: PtySize) -> Result<(), DaemonError> {
        self.host_size = new_size;
        let viewport = host_viewport(new_size);
        for w in self.windows.iter_mut() {
            w.resize(viewport)?;
        }
        if let Some(p) = &self.popup {
            p.pane
                .resize(crate::popup::popup_pty_size(plexy_glass_mux::popup_rect(viewport)))?;
        }
        self.notify.notify_one();
        Ok(())
    }

    fn alloc_pane_id(&mut self) -> PaneId {
        let id = PaneId(self.next_pane_id);
        self.next_pane_id += 1;
        id
    }

    fn close_active_window(&mut self) {
        if !self.windows.is_empty() {
            let removed = self.active;
            // The marked pane can't survive its window's removal (KillWindow
            // drops every pane synchronously, so the death channel won't clear
            // it).
            if let Some(marked) = self.marked_pane
                && self.windows[removed].pane(marked).is_some()
            {
                self.marked_pane = None;
            }
            // Kill every pane's child + cancel its pipe before the window is
            // dropped. The synchronous window close must not leak shells or
            // pipe-pane consumers (dropping the panes alone never SIGHUPs the
            // children; the reader threads hold the PTY masters open).
            let w = &self.windows[removed];
            for pid in w.layout().panes() {
                if let Some(p) = w.pane(pid) {
                    p.kill_child();
                }
            }
            self.windows.remove(removed);
            if self.windows.is_empty() {
                self.last_active_window = None;
            } else {
                if self.active >= self.windows.len() {
                    self.active = self.windows.len() - 1;
                }
                self.fixup_last_active_after_removal(removed);
            }
        }
        // The session ends when its last window closes; a floating popup must
        // not orphan its child (mirrors the death-channel path).
        if self.windows.is_empty() {
            self.close_popup();
        }
    }

    /// Repair `last_active_window` after the window at `removed` is dropped:
    /// the toggle target is cleared if it *was* the removed window, shifted
    /// down by one if it sat after the removed slot, and left alone otherwise.
    /// Also clears it if it would now alias the active window (toggling to the
    /// window you are already on is meaningless).
    fn fixup_last_active_after_removal(&mut self, removed: usize) {
        self.last_active_window = match self.last_active_window {
            Some(i) if i == removed => None,
            Some(i) if i > removed => Some(i - 1),
            other => other,
        };
        if self.last_active_window == Some(self.active) {
            self.last_active_window = None;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }
}
fn host_viewport(host: PtySize) -> Rect {
    // The pane band reserves one row for the status bar; full pane frames then
    // inset the layout region by one cell on every side (top/bottom/left/right)
    // so the outer frame has cells to occupy. Pane content rects therefore
    // start at (1, 1). The compositor's `pane_row_offset` shifts this physical
    // band down by one more row when the status bar is on top.
    let rows = host.rows.saturating_sub(3).max(1); // 1 status + 2 frame rows
    let cols = host.cols.saturating_sub(2).max(1); // 2 frame cols
    Rect::new(1, 1, rows, cols)
}

mod commands;
mod mouse;
mod overlays;

pub use overlays::OverlayKeyResult;

#[cfg(test)]
mod tests;
