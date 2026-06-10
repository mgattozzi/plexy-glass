//! Owns all windows for one attached client.

use crate::{error::DaemonError, window::Window};
use plexy_glass_mux::{
    BorderHit, BorderSide, BufferAction, BufferEntry, BufferOutcome, BufferPickerState, Command,
    KeyEvent, MouseButton, MouseEncoding, MouseEvent, MouseKind, Overlay, OverlayAction,
    OverlayHandler, PaneId, PickerEntry, Rect, RenameTarget, Selection, SelectionKind, SplitDir,
    TreeAction, TreeMode, TreeNode, TreeOutcome, TreeState, WindowId, encode_for_child,
    extract_text, handle_buffers, handle_tree,
};
use plexy_glass_protocol::{PtySize, SpawnSpec};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Notify, mpsc};

/// How long a transient status-line message stays visible before it is cleared
/// on the next recompose. Mirrored by the `Session` wake timer.
pub(crate) const STATUS_TTL: Duration = Duration::from_secs(3);

/// A transient status-line message and the instant it stops being shown.
struct StatusMessage {
    text: String,
    expires_at: Instant,
}

/// How the caller should follow up after feeding a key to the active overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayKeyResult {
    /// Key ignored; nothing changed.
    Ignored,
    /// Overlay state changed (typing / scroll / cancel); recompose only.
    Redraw,
    /// A rename committed and changed a name; recompose AND persist.
    Committed,
    /// A command-prompt line was committed. The connection layer parses and
    /// dispatches it (it may switch sessions / detach / reload, which need
    /// connection-scoped state). The string is the raw, trimmed command line.
    Command(String),
    /// A session was chosen in the picker. The connection layer switches this
    /// client to the named session (via the same path as `switch <name>`).
    SwitchSession(String),
    /// A choose-tree action. The connection layer performs it against the
    /// registry (cross-session kill/rename) or re-points this client (switch).
    Tree(TreeAction),
    /// A choose-buffer action. The connection layer pastes the named buffer into
    /// the active pane or deletes it from the registry's paste buffers.
    Buffer(BufferAction),
}

/// Active border drag-resize. Cleared on Release. While `Some`, all mouse
/// events go to `handle_resize_drag_event`. Fields read by M5 wiring.
#[allow(dead_code)] // fields populated by M5
struct ResizeDrag {
    adjacent_pane: PaneId,
    side: BorderSide,
    last_pos: (u16, u16),
}

/// Last left-press metadata for multi-click classification (double-click =
/// Word, triple-click = Line). Resets when the click target changes or the
/// 400ms window expires. Fields read by M6/M7 wiring.
#[allow(dead_code)] // fields populated by M6/M7
struct ClickHistory {
    pane: PaneId,
    row: u16,
    col: u16,
    button: MouseButton,
    at: Instant,
    count: u8,
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
    /// (Task 8) swaps this Arc and walks the panes calling `update_config`.
    config: Arc<plexy_glass_config::Config>,
    /// Border drag-resize in progress (M5). `None` between drags.
    resize_drag: Option<ResizeDrag>,
    /// Last left-press for multi-click classification (M6/M7).
    #[allow(dead_code)] // read by M6/M7
    click_history: Option<ClickHistory>,
    /// Physical row index where the status bar paints, or `None` if the bar is
    /// hidden. Set by `set_status_layout` (M10).
    status_bar_row: Option<u16>,
    /// Vertical offset of the logical pane band from physical row 0: `0` when
    /// the status bar is at the bottom, `1` when it is at the top. Mouse events
    /// arrive in physical coordinates; this offset translates them into the
    /// layout's logical pane-coordinate space. Set by `set_status_layout`.
    pane_row_offset: u16,
    /// Clickable regions in the current status bar. Refreshed each render
    /// tick via `set_status_hits`. M10.
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

    /// Open a rename prompt seeded with the active window's current name.
    pub fn open_rename_window(&mut self) {
        let buf = self.active_window().name.clone();
        self.overlay = Some(Overlay::Rename { target: RenameTarget::Window, buf });
        self.rename_pane_target = None;
    }

    /// Open a rename prompt for the active pane, capturing its id so a later
    /// focus change cannot retarget the commit.
    pub fn open_rename_pane(&mut self) {
        let pid = self.active_window().active();
        let buf = self
            .active_window()
            .pane(pid)
            .and_then(|p| p.name())
            .unwrap_or_default();
        self.overlay = Some(Overlay::Rename { target: RenameTarget::Pane, buf });
        self.rename_pane_target = Some(pid);
    }

    /// Open the scrollable help overlay.
    pub fn open_help(&mut self) {
        self.overlay = Some(Overlay::Help { scroll: 0 });
        self.rename_pane_target = None;
    }

    /// Open the command prompt. `completions` is a snapshot of live session
    /// names for Tab-completing a `switch ` argument. History is cloned from the
    /// durable list so Up/Down recall survives reopening within the session.
    pub fn open_command_prompt(&mut self, completions: Vec<String>) {
        self.overlay = Some(Overlay::Command {
            buf: String::new(),
            history: self.command_history.clone(),
            hist_idx: None,
            completions,
        });
        self.rename_pane_target = None;
    }

    /// Open the session picker over a snapshot of live sessions (sorted by name,
    /// the current one marked). Selection switches via the connection layer.
    pub fn open_session_picker(&mut self, entries: Vec<PickerEntry>) {
        self.overlay = Some(Overlay::SessionPicker {
            entries,
            filter: String::new(),
            selected: 0,
        });
        self.rename_pane_target = None;
    }

    /// Open the choose-tree overlay over a pre-built node snapshot (assembled by
    /// the connection layer from every live session). Navigation/actions are
    /// driven by `tree::handle_tree`; cross-session effects are dispatched at the
    /// connection layer.
    pub fn open_tree(&mut self, nodes: Vec<TreeNode>) {
        self.overlay = Some(Overlay::Tree(TreeState {
            nodes,
            selected: 0,
            mode: TreeMode::Navigate,
        }));
        self.rename_pane_target = None;
    }

    /// Open the choose-buffer overlay over a snapshot of the paste buffers.
    pub fn open_buffer_picker(&mut self, entries: Vec<BufferEntry>) {
        self.overlay = Some(Overlay::BufferPicker(BufferPickerState { entries, selected: 0 }));
        self.rename_pane_target = None;
    }

    fn close_overlay(&mut self) {
        self.overlay = None;
        self.rename_pane_target = None;
    }

    /// Feed one key to the active overlay. On commit, applies the rename to the
    /// active window or the captured pane; an empty (whitespace-only) name is a
    /// no-op rename. The return tells the caller how to follow up: `Ignored`
    /// (nothing), `Redraw` (recompose only), or `Committed` (recompose AND
    /// persist, a name actually changed).
    pub fn handle_overlay_key(&mut self, event: &KeyEvent) -> OverlayKeyResult {
        // The tree overlay is driven by the pure `handle_tree`; its actions are
        // cross-session and dispatched at the connection layer. `Switch` and
        // `Cancel` close the overlay here; `Kill*`/`Rename*` keep it open (the
        // handler already updated the in-memory model optimistically).
        if let Some(Overlay::Tree(state)) = self.overlay.as_mut() {
            return match handle_tree(event, state) {
                TreeOutcome::None => OverlayKeyResult::Ignored,
                TreeOutcome::Redraw => OverlayKeyResult::Redraw,
                TreeOutcome::Cancel => {
                    self.close_overlay();
                    OverlayKeyResult::Redraw
                }
                TreeOutcome::Act(action @ TreeAction::Switch { .. }) => {
                    self.close_overlay();
                    OverlayKeyResult::Tree(action)
                }
                TreeOutcome::Act(action) => OverlayKeyResult::Tree(action),
            };
        }
        // Choose-buffer: Paste/Cancel close; Delete keeps the overlay open (the
        // handler already pruned the row).
        if let Some(Overlay::BufferPicker(state)) = self.overlay.as_mut() {
            return match handle_buffers(event, state) {
                BufferOutcome::None => OverlayKeyResult::Ignored,
                BufferOutcome::Redraw => OverlayKeyResult::Redraw,
                BufferOutcome::Cancel => {
                    self.close_overlay();
                    OverlayKeyResult::Redraw
                }
                BufferOutcome::Act(action @ BufferAction::Paste(_)) => {
                    self.close_overlay();
                    OverlayKeyResult::Buffer(action)
                }
                BufferOutcome::Act(action) => OverlayKeyResult::Buffer(action),
            };
        }
        let (action, target, is_command, is_picker) = {
            let Some(overlay) = self.overlay.as_mut() else {
                return OverlayKeyResult::Ignored;
            };
            let action = OverlayHandler::handle(event, overlay);
            let target = match overlay {
                Overlay::Rename { target, .. } => Some(*target),
                _ => None,
            };
            let is_command = matches!(overlay, Overlay::Command { .. });
            let is_picker = matches!(overlay, Overlay::SessionPicker { .. });
            (action, target, is_command, is_picker)
        };
        match action {
            OverlayAction::None => OverlayKeyResult::Ignored,
            OverlayAction::Redraw => OverlayKeyResult::Redraw,
            OverlayAction::Cancel => {
                self.close_overlay();
                OverlayKeyResult::Redraw
            }
            OverlayAction::Commit(name) if is_picker => {
                // The picker committed a session name; the connection switches.
                self.close_overlay();
                OverlayKeyResult::SwitchSession(name)
            }
            OverlayAction::Commit(text) if is_command => {
                // Command prompt: record history (coalescing consecutive dups,
                // capped) and hand the raw line to the connection to dispatch.
                if self.command_history.last() != Some(&text) {
                    self.command_history.push(text.clone());
                    if self.command_history.len() > COMMAND_HISTORY_CAP {
                        let excess = self.command_history.len() - COMMAND_HISTORY_CAP;
                        self.command_history.drain(0..excess);
                    }
                }
                self.close_overlay();
                OverlayKeyResult::Command(text)
            }
            OverlayAction::Commit(text) => {
                let mut changed = false;
                if !text.is_empty() {
                    match target {
                        Some(RenameTarget::Window) => {
                            self.set_window_name(self.active, text);
                            changed = true;
                        }
                        Some(RenameTarget::Pane) => {
                            if let Some(pid) = self.rename_pane_target
                                && let Some(p) = self.active_window().pane(pid)
                            {
                                p.set_name(Some(text));
                                changed = true;
                            }
                        }
                        None => {}
                    }
                }
                self.close_overlay();
                if changed {
                    OverlayKeyResult::Committed
                } else {
                    OverlayKeyResult::Redraw
                }
            }
        }
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
    /// sticky alert flags. This is the sole drainer of the pane atomics, called
    /// once per frame by the render coordinator (the status tick task only
    /// *reads* the flags). The current window's alerts are cleared (you're
    /// watching it); a background window with the matching monitor option on
    /// gets its sticky flag set.
    pub fn update_monitor_flags(&mut self) {
        let active = self.active;
        for (i, w) in self.windows.iter_mut().enumerate() {
            let (acted, belled) = w.drain_pane_alerts();
            if i == active {
                w.clear_alerts();
            } else {
                if acted && w.monitor_activity() {
                    w.set_activity();
                }
                if belled && w.monitor_bell() {
                    w.set_bell();
                }
            }
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

    /// The cwd the popup spawns at: the active pane's live OSC-7 location,
    /// falling back to the window home base. This intentionally diverges from
    /// `split_cwd` (home base only): a popup acts on the current context.
    pub fn popup_cwd(&self) -> Option<String> {
        self.active_window()
            .active_pane()
            .and_then(|p| p.with_screen(|s| s.cwd.clone()))
            .and_then(|url| crate::popup::osc7_to_path(&url))
            .or_else(|| self.active_window().home_cwd.clone())
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

    pub fn handle_command(&mut self, cmd: Command) -> Result<(), DaemonError> {
        let viewport = self.viewport();
        // Any structural / navigation command clears a zoom overlay first
        // (zoom is a view of one pane; changing the layout or focus ends it).
        if command_clears_zoom(&cmd) {
            self.clear_zoom_restore()?;
        }
        match cmd {
            Command::SplitV => {
                let new_id = self.alloc_pane_id();
                let mut spec = self.default_spec.clone();
                spec.cwd = self.split_cwd();
                let notify = Arc::clone(&self.notify);
                let death = self.death_tx.clone();
                let config = Arc::clone(&self.config);
                self.active_window_mut().split(
                    SplitDir::Vertical,
                    new_id,
                    spec,
                    viewport,
                    notify,
                    death,
                    config,
                )?;
            }
            Command::SplitH => {
                let new_id = self.alloc_pane_id();
                let mut spec = self.default_spec.clone();
                spec.cwd = self.split_cwd();
                let notify = Arc::clone(&self.notify);
                let death = self.death_tx.clone();
                let config = Arc::clone(&self.config);
                self.active_window_mut().split(
                    SplitDir::Horizontal,
                    new_id,
                    spec,
                    viewport,
                    notify,
                    death,
                    config,
                )?;
            }
            Command::SelectNextPane => self.active_window_mut().select_next(),
            Command::SelectPrevPane => self.active_window_mut().select_prev(),
            Command::SelectPane(dir) => {
                let _ = self.active_window_mut().select_direction(dir, viewport);
            }
            Command::KillPane => {
                // KillPane drops the active pane synchronously (no death-channel
                // round-trip), so clear a mark that pointed at it here, otherwise
                // it would dangle until some later event.
                let killed = self.active_window().active();
                let outcome = self.active_window_mut().close_active()?;
                if matches!(outcome, plexy_glass_mux::CloseOutcome::TreeEmpty) {
                    self.close_active_window();
                } else {
                    // Surviving panes may now occupy a larger rect after the
                    // layout collapses; resize their PTYs to match.
                    self.active_window_mut().resize(viewport)?;
                }
                if self.marked_pane == Some(killed) {
                    self.marked_pane = None;
                }
            }
            Command::NewWindow => {
                let id = WindowId(self.next_window_id);
                self.next_window_id += 1;
                let first_pane = self.alloc_pane_id();
                let mut spec = self.default_spec.clone();
                let home = self.session_cwd.clone();
                spec.cwd = home.clone();
                let n = id.raw();
                let mut window = Window::spawn_first(
                    id,
                    format!("shell{n}"),
                    first_pane,
                    spec,
                    viewport,
                    Arc::clone(&self.notify),
                    self.death_tx.clone(),
                    Arc::clone(&self.config),
                )?;
                window.home_cwd = home;
                self.windows.push(window);
                self.last_active_window = Some(self.active);
                self.active = self.windows.len() - 1;
            }
            Command::NextWindow => {
                if !self.windows.is_empty() {
                    let idx = (self.active + 1) % self.windows.len();
                    self.switch_to_window(idx);
                }
            }
            Command::PrevWindow => {
                if !self.windows.is_empty() {
                    let idx = if self.active == 0 {
                        self.windows.len() - 1
                    } else {
                        self.active - 1
                    };
                    self.switch_to_window(idx);
                }
            }
            Command::SelectWindow(n) => {
                self.switch_to_window(usize::from(n));
            }
            Command::SelectLastWindow => {
                if let Some(prev) = self.last_active_window {
                    self.switch_to_window(prev);
                }
            }
            Command::SelectLastPane => {
                if let Some(p) = self.active_window().last_pane() {
                    self.active_window_mut().focus(p);
                }
            }
            Command::MarkPane => {
                let a = self.active_window().active();
                self.marked_pane = if self.marked_pane == Some(a) { None } else { Some(a) };
            }
            Command::BreakPane => {
                if self.active_window().layout().panes().len() < 2 {
                    self.set_status_message("only pane in window".into());
                } else {
                    let active = self.active_window().active();
                    // invariant: the active pane is always in its window.
                    let pane = self
                        .active_window_mut()
                        .detach_pane(active)
                        .expect("active pane present");
                    self.active_window_mut().resize(viewport)?; // surviving source
                    let name =
                        pane.name().unwrap_or_else(|| format!("shell{}", self.next_window_id));
                    let id = WindowId(self.next_window_id);
                    self.next_window_id += 1;
                    let mut w = Window::from_pane(id, name, pane);
                    w.resize(viewport)?;
                    self.windows.push(w);
                    self.last_active_window = Some(self.active);
                    self.active = self.windows.len() - 1;
                }
            }
            Command::SwapPane(next) => {
                let active = self.active_window().active();
                if let Some(other) = self.active_window().neighbor_leaf(next) {
                    let w = self.active_window_mut();
                    w.layout_mut().swap_panes(active, other);
                    w.resize(viewport)?;
                }
            }
            Command::JoinPane(dir) => {
                if let Some(marked) = self.marked_pane {
                    let act_idx = self.active;
                    let act_pane = self.windows[act_idx].active();
                    if marked == act_pane {
                        self.set_status_message("marked pane is the active pane".into());
                    } else if let Some(src_idx) =
                        self.windows.iter().position(|w| w.pane(marked).is_some())
                    {
                        let act_wid = self.windows[act_idx].id;
                        if let Some(pane) = self.windows[src_idx].detach_pane(marked) {
                            if self.windows[src_idx].is_layout_empty() {
                                self.windows.remove(src_idx);
                                // invariant: the active window is never the emptied
                                // source, `marked != act_pane` (guarded above) means
                                // the active window keeps act_pane and stays alive.
                                self.active = self
                                    .windows
                                    .iter()
                                    .position(|w| w.id == act_wid)
                                    .expect("active window survives a join");
                                self.fixup_last_active_after_removal(src_idx);
                            } else {
                                // Source survives with a promoted layout, so resize it.
                                self.windows[src_idx].resize(viewport)?;
                            }
                            let act_idx = self.active;
                            let act_pane = self.windows[act_idx].active();
                            self.windows[act_idx]
                                .adopt_split(act_pane, dir, pane, viewport)?;
                            self.marked_pane = None;
                        }
                    } else {
                        self.marked_pane = None; // marked pane vanished
                    }
                } else {
                    self.set_status_message("no marked pane".into());
                }
            }
            Command::SwapMarkedPane => {
                if let Some(marked) = self.marked_pane {
                    let a = self.active_window().active();
                    if marked != a {
                        if self.active_window().pane(marked).is_some() {
                            let w = self.active_window_mut();
                            w.layout_mut().swap_panes(a, marked);
                            w.resize(viewport)?;
                        } else {
                            self.set_status_message(
                                "marked pane is in another window — use join".into(),
                            );
                        }
                    }
                } else {
                    self.set_status_message("no marked pane".into());
                }
            }
            Command::ToggleMonitorActivity => {
                let on = self.active_window_mut().toggle_monitor_activity();
                self.set_status_message(
                    format!("monitor-activity {}", if on { "on" } else { "off" }),
                );
            }
            Command::ToggleMonitorBell => {
                let on = self.active_window_mut().toggle_monitor_bell();
                self.set_status_message(format!("monitor-bell {}", if on { "on" } else { "off" }));
            }
            Command::ResizePane(dir) => {
                let active = self.active_window().active();
                const STEP: i32 = 3;
                let (axis, delta) = match dir {
                    plexy_glass_mux::Direction::Left => (SplitDir::Vertical, -STEP),
                    plexy_glass_mux::Direction::Right => (SplitDir::Vertical, STEP),
                    plexy_glass_mux::Direction::Up => (SplitDir::Horizontal, -STEP),
                    plexy_glass_mux::Direction::Down => (SplitDir::Horizontal, STEP),
                };
                self.active_window_mut()
                    .layout_mut()
                    .resize_split(active, axis, delta, viewport);
                self.active_window_mut().resize(viewport)?;
            }
            Command::KillWindow => self.close_active_window(),
            Command::RenameWindow => self.open_rename_window(),
            Command::RenamePane => self.open_rename_pane(),
            Command::ShowHelp => self.open_help(),
            Command::ZoomToggle => {
                self.active_window_mut().toggle_zoom();
                // The zoom-aware resize handles both directions: it sizes a
                // newly-zoomed pane to the full viewport, or restores every
                // pane to its layout rect on un-zoom.
                self.active_window_mut().resize(viewport)?;
            }
            Command::ToggleSyncPanes => {
                let win = self.active_window_mut();
                win.sync_input = !win.sync_input;
            }
            Command::Detach | Command::Cancel => {}
            Command::ReloadConfig => {
                // Handled by Connection::serve_attach (needs registry access).
            }
            Command::CommandPrompt => {
                // Opened at the connection layer (needs the live session list
                // for Tab-completion); see `Connection::serve_attach`.
            }
            Command::ChooseSession => {
                // Opened at the connection layer (needs the live session list);
                // see `Connection::serve_attach`.
            }
            Command::ChooseTree => {
                // Opened at the connection layer (needs the live session list);
                // see `Connection::serve_attach`.
            }
            Command::PasteBuffer | Command::ChooseBuffer => {
                // Handled at the connection layer (needs the registry's paste
                // buffers); see Connection::serve_attach.
            }
            Command::EnterCopyMode => {
                if let Some(pane) = self.active_window().active_pane() {
                    let (total_lines, pane_rows, start_line, start_col) = pane.with_screen(|s| {
                        let scrollback_len = s.scrollback.len() as u32;
                        let active_rows = u32::from(s.active.num_rows());
                        let total = scrollback_len + active_rows;
                        let start_line = scrollback_len + u32::from(s.cursor.row);
                        let start_col = s.cursor.col;
                        let pane_rows = s.active.num_rows();
                        (total, pane_rows, start_line, start_col)
                    });
                    pane.enter_copy_mode(total_lines, pane_rows, start_line, start_col);
                }
            }
            Command::OpenPopup { command } => {
                self.open_popup(command)?;
            }
            Command::ClosePopup => {
                self.close_popup();
            }
            Command::SelectLayout(preset) => {
                self.apply_layout_preset(preset)?;
            }
            Command::NextLayout => {
                self.next_layout()?;
            }
        }
        self.notify.notify_one();
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

    /// Dispatch one decoded mouse event through the precedence ladder
    /// (Rule 0: modal popup, see docs/superpowers/specs/2026-06-09-popup-panes-design.md;
    /// then docs/superpowers/specs/2026-05-22-full-mouse-design.md §6).
    pub async fn handle_mouse(&mut self, event: MouseEvent) -> Result<(), DaemonError> {
        // Rule 0: a floating popup owns the mouse entirely while open. A click
        // in the box interior is forwarded to the child (translated to interior
        // coordinates) when it enabled mouse reporting; everything else (border,
        // outside, status bar) is swallowed. Modal by design.
        if let Some(popup) = self.popup.as_ref() {
            let event = self.to_pane_coords(event);
            let rect = plexy_glass_mux::popup_rect(self.viewport());
            let interior = rect.rows >= 3
                && rect.cols >= 3
                && event.row > rect.row
                && event.row < rect.row + rect.rows - 1
                && event.col > rect.col
                && event.col < rect.col + rect.cols - 1;
            if !interior {
                return Ok(());
            }
            if !popup.pane.with_screen(|s| s.modes.any_mouse_mode_active()) {
                return Ok(());
            }
            let mut local = event;
            local.row = event.row - rect.row - 1;
            local.col = event.col - rect.col - 1;
            let encoding = popup.pane.with_screen(|s| mouse_encoding_for(s.modes));
            let bytes = encode_for_child(local, encoding);
            let pane = popup.pane.clone();
            let _ = pane.send_input(bytes::Bytes::from(bytes)).await;
            return Ok(());
        }
        // Rule 2 (first): status-bar row hit. The bar lives outside the pane
        // band, so test it against the *physical* row before translating. A
        // drag in progress still consumes everything, including moves that
        // stray onto the status row.
        if self.resize_drag.is_none() && self.is_status_bar_row(event.row) {
            return self.handle_status_bar_event(event).await;
        }
        // Everything below addresses panes/borders, which live in the layout's
        // logical coordinate space. Translate away the status-bar offset (1 row
        // when the bar is on top; 0 otherwise, leaving bottom placement byte
        // for byte unchanged).
        let event = self.to_pane_coords(event);
        // Rule 1: resize-drag in progress consumes everything until release.
        if self.resize_drag.is_some() {
            return self.handle_resize_drag_event(event).await;
        }
        // Rule 3: border hit on left press.
        if matches!(event.kind, MouseKind::Press)
            && event.button == MouseButton::Left
            && let Some(hit) = self.layout_border_at(event.row, event.col)
        {
            return self.begin_resize_drag(hit, event.row, event.col).await;
        }
        // Rule 4: copy-mode pane.
        let viewport = self.viewport();
        let Some(pane_id) = self
            .active_window()
            .layout()
            .pane_at_coord(viewport, event.row, event.col)
        else {
            return Ok(());
        };
        if self.pane_is_in_copy_mode(pane_id) {
            return self.handle_copy_mode_mouse(pane_id, event).await;
        }
        // Rule 4.5: a left-press on a *non-active* pane focuses it and is
        // consumed, even when the pane's app has mouse mode on. Without this,
        // panes running mouse-reporting apps (less, hx, TUIs) would forward the
        // click via Rule 5 and never become focusable. Mirrors the focus-only
        // behavior `handle_left_press` gives plain panes.
        if matches!(event.kind, MouseKind::Press)
            && event.button == MouseButton::Left
            && pane_id != self.active_window().active()
        {
            self.active_window_mut().focus(pane_id);
            self.notify.notify_one();
            return Ok(());
        }
        // Rule 5: pane has child-app mouse-mode on → passthrough.
        if self.pane_has_any_mouse_mode(pane_id) {
            return self.forward_mouse_to_pane(pane_id, event).await;
        }
        // Rule 6: default daemon handlers.
        self.handle_default_mouse(pane_id, event, viewport).await
    }

    // ----- Precedence-ladder helpers (M2 stubs filled in by M4-M10) -----

    fn is_status_bar_row(&self, row: u16) -> bool {
        self.status_bar_row == Some(row)
    }

    /// Translate a physical mouse event into the layout's logical pane
    /// coordinates by removing the status-bar offset. A no-op when the bar is
    /// at the bottom (offset 0).
    fn to_pane_coords(&self, mut event: MouseEvent) -> MouseEvent {
        event.row = event.row.saturating_sub(self.pane_row_offset);
        event
    }

    async fn handle_status_bar_event(
        &mut self,
        event: MouseEvent,
    ) -> Result<(), DaemonError> {
        if !matches!(event.kind, MouseKind::Press) || event.button != MouseButton::Left {
            return Ok(());
        }
        let Some(hit) = self
            .status_hits
            .iter()
            .find(|h| h.col_range.contains(&event.col))
            .cloned()
        else {
            return Ok(());
        };
        use plexy_glass_status::ClickAction;
        match hit.action {
            ClickAction::SelectWindow(idx) => {
                // SelectWindow takes u8; clamp on overflow (unlikely with
                // realistic window counts).
                let n = u8::try_from(idx).unwrap_or(u8::MAX);
                self.handle_command(Command::SelectWindow(n))?;
            }
            ClickAction::ToggleSyncPanes => {
                self.handle_command(Command::ToggleSyncPanes)?;
            }
            ClickAction::ExitCopyMode => {
                if let Some(pane) = self.active_window().active_pane().cloned() {
                    pane.exit_copy_mode();
                }
                self.notify.notify_one();
            }
            ClickAction::Detach => {
                self.detach_requested = true;
                self.notify.notify_one();
            }
            ClickAction::NoOp => {}
        }
        Ok(())
    }

    fn layout_border_at(&self, row: u16, col: u16) -> Option<BorderHit> {
        self.active_window()
            .layout()
            .border_at(self.viewport(), row, col)
    }

    async fn begin_resize_drag(
        &mut self,
        hit: BorderHit,
        row: u16,
        col: u16,
    ) -> Result<(), DaemonError> {
        self.resize_drag = Some(ResizeDrag {
            adjacent_pane: hit.adjacent_pane,
            side: hit.side,
            last_pos: (row, col),
        });
        Ok(())
    }

    async fn handle_resize_drag_event(
        &mut self,
        event: MouseEvent,
    ) -> Result<(), DaemonError> {
        let Some(drag) = self.resize_drag.as_mut() else {
            return Ok(());
        };
        match event.kind {
            MouseKind::Move => {
                let delta = match drag.side {
                    BorderSide::Right => event.col as i16 - drag.last_pos.1 as i16,
                    BorderSide::Bottom => event.row as i16 - drag.last_pos.0 as i16,
                };
                if delta == 0 {
                    return Ok(());
                }
                let pane = drag.adjacent_pane;
                let side = drag.side;
                let viewport = self.viewport();
                let applied = self
                    .active_window_mut()
                    .layout_mut()
                    .adjust_split(pane, side, delta, viewport);
                if applied != 0 {
                    // Step last_pos by the actually-applied delta so we don't
                    // accumulate slip when the drag bottoms out at min-size.
                    let drag = self.resize_drag.as_mut().expect("just held above");
                    match side {
                        BorderSide::Right => {
                            drag.last_pos.1 = (drag.last_pos.1 as i16 + applied) as u16;
                        }
                        BorderSide::Bottom => {
                            drag.last_pos.0 = (drag.last_pos.0 as i16 + applied) as u16;
                        }
                    }
                    self.active_window_mut().resize(viewport)?;
                    self.notify.notify_one();
                }
                Ok(())
            }
            MouseKind::Release => {
                self.resize_drag = None;
                let viewport = self.viewport();
                self.active_window_mut().resize(viewport)?;
                self.notify.notify_one();
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn pane_is_in_copy_mode(&self, pane: PaneId) -> bool {
        self.active_window()
            .pane(pane)
            .map(|p| p.is_in_copy_mode())
            .unwrap_or(false)
    }

    async fn handle_copy_mode_mouse(
        &mut self,
        pane_id: PaneId,
        event: MouseEvent,
    ) -> Result<(), DaemonError> {
        let click_count = self.classify_click_count(pane_id, &event);
        let Some(pane) = self.active_window().pane(pane_id).cloned() else {
            return Ok(());
        };
        // The handler mutates copy-mode state; we need both with_screen + with_copy_mode_mut.
        let action: Option<plexy_glass_mux::CopyModeAction> = pane.with_screen(|screen| {
            pane.with_copy_mode_mut(|cm| cm.handle_mouse(&event, click_count, screen))
        });
        if let Some(action) = action {
            use plexy_glass_mux::CopyModeAction;
            match action {
                CopyModeAction::Render => self.notify.notify_one(),
                CopyModeAction::Exit => {
                    pane.exit_copy_mode();
                    self.notify.notify_one();
                }
                CopyModeAction::Yank(text) => {
                    tokio::spawn(async move {
                        let _ = crate::osc_actions::write_clipboard(text.as_bytes()).await;
                    });
                    pane.exit_copy_mode();
                    self.notify.notify_one();
                }
            }
        }
        Ok(())
    }

    /// Classify the current left-press as count=1/2/3 based on time + target
    /// match against `click_history`. Updates `click_history` and returns
    /// the new count. Non-left-press events return 1 without updating.
    fn classify_click_count(&mut self, pane: PaneId, event: &MouseEvent) -> u8 {
        if !matches!(event.kind, MouseKind::Press) || event.button != MouseButton::Left {
            return 1;
        }
        let now = Instant::now();
        let same_target = match &self.click_history {
            Some(h) => {
                h.pane == pane
                    && h.row == event.row
                    && h.col == event.col
                    && h.button == MouseButton::Left
                    && now.saturating_duration_since(h.at)
                        < std::time::Duration::from_millis(400)
            }
            None => false,
        };
        let count = if same_target {
            self.click_history
                .as_ref()
                .map(|h| h.count.saturating_add(1).min(3))
                .unwrap_or(1)
        } else {
            1
        };
        self.click_history = Some(ClickHistory {
            pane,
            row: event.row,
            col: event.col,
            button: MouseButton::Left,
            at: now,
            count,
        });
        count
    }

    fn pane_has_any_mouse_mode(&self, pane_id: PaneId) -> bool {
        self.active_window()
            .pane(pane_id)
            .map(|p| p.with_screen(|s| s.modes.any_mouse_mode_active()))
            .unwrap_or(false)
    }

    async fn forward_mouse_to_pane(
        &mut self,
        pane_id: PaneId,
        event: MouseEvent,
    ) -> Result<(), DaemonError> {
        if let Some(pane) = self.active_window().pane(pane_id).cloned() {
            let encoding = pane.with_screen(|s| mouse_encoding_for(s.modes));
            let bytes = encode_for_child(event, encoding);
            let _ = pane.send_input(bytes::Bytes::from(bytes)).await;
        }
        Ok(())
    }

    async fn handle_default_mouse(
        &mut self,
        pane_id: PaneId,
        event: MouseEvent,
        viewport: Rect,
    ) -> Result<(), DaemonError> {
        match event.kind {
            MouseKind::Press if event.button == MouseButton::Left => {
                self.handle_left_press(pane_id, event, viewport).await?;
            }
            MouseKind::Release if event.button == MouseButton::Left => {
                self.handle_left_release().await?;
            }
            MouseKind::Move if event.button == MouseButton::Left => {
                self.handle_left_drag(event, viewport);
            }
            MouseKind::Press if event.button == MouseButton::Middle => {
                self.handle_middle_press(pane_id).await?;
            }
            MouseKind::Wheel { delta } => {
                self.handle_wheel(pane_id, delta);
            }
            _ => {}
        }
        self.notify.notify_one();
        Ok(())
    }

    /// Middle-click pastes from the system clipboard. Bracketed-paste-aware:
    /// if the active pane's emulator has `Modes::BRACKETED_PASTE` on, the
    /// pasted bytes are wrapped with `\x1b[200~ ... \x1b[201~` so inner apps
    /// can distinguish paste from typed input.
    async fn handle_middle_press(&mut self, pane_id: PaneId) -> Result<(), DaemonError> {
        let bytes = crate::osc_actions::read_clipboard().await;
        if bytes.is_empty() {
            return Ok(());
        }
        let bracketed = self
            .active_window()
            .pane(pane_id)
            .map(|p| {
                p.with_screen(|s| {
                    s.modes
                        .contains(plexy_glass_emulator::Modes::BRACKETED_PASTE)
                })
            })
            .unwrap_or(false);
        let to_send = if bracketed {
            let mut v = Vec::with_capacity(bytes.len() + 12);
            v.extend_from_slice(b"\x1b[200~");
            v.extend_from_slice(&bytes);
            v.extend_from_slice(b"\x1b[201~");
            v
        } else {
            bytes
        };
        if let Some(pane) = self.active_window().pane(pane_id).cloned() {
            let _ = pane.send_input(bytes::Bytes::from(to_send)).await;
        }
        Ok(())
    }

    async fn handle_left_press(
        &mut self,
        pane_id: PaneId,
        event: MouseEvent,
        viewport: Rect,
    ) -> Result<(), DaemonError> {
        // Click in a non-active pane → focus only.
        if pane_id != self.active_window().active() {
            self.active_window_mut().focus(pane_id);
            return Ok(());
        }

        let pane_rect = self
            .active_window()
            .layout()
            .rect_of(pane_id, viewport)
            .unwrap_or(viewport);
        let local_row = event.row.saturating_sub(pane_rect.row);
        let local_col = event.col.saturating_sub(pane_rect.col);

        // Shift+left-click EXTENDS the existing selection in this pane
        // instead of starting a new one (M7).
        if event.modifiers.shift
            && self
                .selection
                .as_ref()
                .map(|s| s.source_pane == pane_id)
                .unwrap_or(false)
        {
            if let Some(sel) = self.selection.as_mut() {
                sel.extend(local_row, local_col, pane_rect);
            }
            return Ok(());
        }

        // OSC 8 hyperlink under the cell? Open in the OS browser; suppress
        // selection start.
        let url = self.active_window().pane(pane_id).and_then(|p| {
            p.with_screen(|s| {
                s.active
                    .get_cell(local_row, local_col)
                    .and_then(|cell| cell.hyperlink_id)
                    .and_then(|id| s.hyperlinks.get(id).map(str::to_owned))
            })
        });
        if let Some(url) = url {
            tokio::spawn(async move {
                let _ = crate::osc_actions::open_url(&url).await;
            });
            return Ok(());
        }

        // OSC 133 click-to-position. If a prompt mark on the current row is
        // before the click column, walk the shell cursor with arrow keys.
        if let Some(pane) = self.active_window().pane(pane_id).cloned()
            && crate::osc_actions::click_to_position(&pane, local_col).await?
        {
            return Ok(());
        }

        // Multi-click classification (M7): double = Word, triple = Line.
        let count = self.classify_click_count(pane_id, &event);
        let new_sel = if count >= 3 {
            self.active_window()
                .pane(pane_id)
                .and_then(|p| p.with_screen(|s| plexy_glass_mux::line_at(pane_id, s, local_row)))
        } else if count == 2 {
            self.active_window().pane(pane_id).and_then(|p| {
                p.with_screen(|s| plexy_glass_mux::word_at(pane_id, s, local_row, local_col))
            })
        } else {
            None
        };
        self.selection = new_sel.or_else(|| {
            Some(Selection::start(
                pane_id,
                local_row,
                local_col,
                SelectionKind::Char,
            ))
        });
        Ok(())
    }

    fn handle_left_drag(&mut self, event: MouseEvent, viewport: Rect) {
        let Some(source_pane) = self.selection.as_ref().map(|s| s.source_pane) else {
            return;
        };
        let Some(pane_rect) = self.active_window().layout().rect_of(source_pane, viewport) else {
            return;
        };
        let local_row = event.row.saturating_sub(pane_rect.row);
        let local_col = event.col.saturating_sub(pane_rect.col);
        if let Some(sel) = self.selection.as_mut() {
            sel.extend(local_row, local_col, pane_rect);
        }
    }

    async fn handle_left_release(&mut self) -> Result<(), DaemonError> {
        let Some(sel) = self.selection.take() else {
            return Ok(());
        };
        if sel.is_empty() {
            return Ok(());
        }
        if let Some(pane) = self.active_window().pane(sel.source_pane) {
            let text = pane.with_screen(|s| extract_text(&sel, s));
            if !text.is_empty() {
                tokio::spawn(async move {
                    let _ = crate::osc_actions::write_clipboard(text.as_bytes()).await;
                });
            }
        }
        Ok(())
    }

    fn handle_wheel(&mut self, pane_id: PaneId, delta: i16) {
        let Some(pane) = self.active_window().pane(pane_id) else {
            return;
        };
        let max_offset = pane.scrollback_len();
        // Wheel-up = positive delta = scroll INTO older history.
        pane.scroll_by(delta.into(), max_offset);
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

/// Derive the wire encoding from a pane's mouse-related modes. `?1006` (SGR)
/// takes precedence; otherwise the most-specific legacy mode is used.
fn mouse_encoding_for(modes: plexy_glass_emulator::Modes) -> MouseEncoding {
    use plexy_glass_emulator::Modes;
    if modes.contains(Modes::MOUSE_SGR) {
        MouseEncoding::Sgr
    } else if modes.contains(Modes::MOUSE_ANY) {
        MouseEncoding::AnyEvent
    } else if modes.contains(Modes::MOUSE_BTN) || modes.contains(Modes::MOUSE_BTN_EVENT) {
        // ?1000 and ?1002 share the legacy button-event wire encoding.
        MouseEncoding::ButtonEvent
    } else {
        // ?9 (X10) or no explicit mode: X10 click-only form.
        MouseEncoding::X10
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

/// Whether a command should clear an active zoom overlay before running.
/// Structural (split/kill/new-window) and navigation (window/pane switch,
/// resize) commands end zoom; `ZoomToggle`, sync-toggle, copy-mode, detach,
/// cancel, and reload do not.
fn command_clears_zoom(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::SplitV
            | Command::SplitH
            | Command::KillPane
            | Command::KillWindow
            | Command::NewWindow
            | Command::NextWindow
            | Command::PrevWindow
            | Command::SelectWindow(_)
            | Command::SelectNextPane
            | Command::SelectPrevPane
            | Command::SelectPane(_)
            | Command::ResizePane(_)
            | Command::SelectLastWindow
            | Command::SelectLastPane
            | Command::BreakPane
            | Command::JoinPane(_)
            | Command::SwapPane(_)
            | Command::SwapMarkedPane
            | Command::SelectLayout(_)
            | Command::NextLayout
    )
}


#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> SpawnSpec {
        SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        }
    }

    fn cfg() -> Arc<plexy_glass_config::Config> {
        Arc::new(plexy_glass_config::built_in_default())
    }

    #[tokio::test]
    async fn new_creates_one_window_one_pane() {
        let notify = Arc::new(Notify::new());
        let m = WindowManager::new(
            spec(),
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        assert_eq!(m.windows().len(), 1);
        assert_eq!(m.active_window().active(), PaneId(0));
    }

    #[tokio::test]
    async fn new_window_default_spec_is_shell_not_first_command() {
        // A session whose first pane runs a non-shell command (here `/bin/cat`,
        // standing in for `claude`) must still open the SHELL for new windows and
        // splits, not re-run that command. Regression: `default_spec` used to be a
        // clone of `first_spec`.
        let notify = Arc::new(Notify::new());
        let m = WindowManager::new(
            spec(), // program = `/bin/cat` (a non-shell first command)
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        assert_eq!(m.default_spec.program, crate::declared::default_shell());
        assert_ne!(m.default_spec.program, "/bin/cat");
        assert!(m.default_spec.args.is_empty());
    }

    #[tokio::test]
    async fn status_message_set_and_lazy_expire() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        assert_eq!(m.take_active_message(), None);
        m.set_status_message("no session: foo".into());
        assert_eq!(m.take_active_message(), Some("no session: foo"));
        // Force expiry and confirm it clears in place on the next read.
        m.status_message.as_mut().unwrap().expires_at = Instant::now() - Duration::from_secs(1);
        assert_eq!(m.take_active_message(), None);
        assert!(m.status_message.is_none(), "expired message cleared in place");
        // A newer message replaces the prior one.
        m.set_status_message("first".into());
        m.set_status_message("second".into());
        assert_eq!(m.take_active_message(), Some("second"));
    }

    #[tokio::test]
    async fn splitv_makes_two_panes() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
        m.handle_command(Command::SplitV).unwrap();
        assert_eq!(m.active_window().layout().panes().len(), 2);
        assert_eq!(m.active_window().active(), PaneId(1));
    }

    #[tokio::test]
    async fn new_window_adds_and_activates() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
        m.handle_command(Command::NewWindow).unwrap();
        assert_eq!(m.windows().len(), 2);
        assert_eq!(m.active_idx(), 1);
    }

    #[tokio::test]
    async fn select_pane_right_after_split() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
        m.handle_command(Command::SplitV).unwrap();
        // After SplitV, the new pane (`PaneId(1)`) is active.
        assert_eq!(m.active_window().active(), PaneId(1));
        // Going left should reach PaneId(0).
        m.handle_command(Command::SelectPane(plexy_glass_mux::Direction::Left))
            .unwrap();
        assert_eq!(m.active_window().active(), PaneId(0));
        // Going right should return to PaneId(1).
        m.handle_command(Command::SelectPane(plexy_glass_mux::Direction::Right))
            .unwrap();
        assert_eq!(m.active_window().active(), PaneId(1));
    }

    #[tokio::test]
    async fn select_pane_up_down_after_horizontal_split() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
        m.handle_command(Command::SplitH).unwrap();
        // After SplitH, the new pane (`PaneId(1)`) is active.
        assert_eq!(m.active_window().active(), PaneId(1));
        // Going up should reach PaneId(0).
        m.handle_command(Command::SelectPane(plexy_glass_mux::Direction::Up))
            .unwrap();
        assert_eq!(m.active_window().active(), PaneId(0));
        // Going down should return to PaneId(1).
        m.handle_command(Command::SelectPane(plexy_glass_mux::Direction::Down))
            .unwrap();
        assert_eq!(m.active_window().active(), PaneId(1));
    }

    #[tokio::test]
    async fn click_in_other_pane_changes_focus() {
        use plexy_glass_mux::{MouseButton, MouseEvent, MouseKind, MouseModifiers};

        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
        m.handle_command(Command::SplitV).unwrap();
        // After SplitV, the new pane (`PaneId(1)`) is active on the right.
        assert_eq!(m.active_window().active(), PaneId(1));

        // Click in the LEFT half (`PaneId(0)`), which should change focus there.
        let event = MouseEvent {
            kind: MouseKind::Press,
            button: MouseButton::Left,
            modifiers: MouseModifiers::default(),
            row: 5,
            col: 2,
        };
        m.handle_mouse(event).await.unwrap();
        assert_eq!(m.active_window().active(), PaneId(0));
    }

    #[tokio::test]
    async fn next_window_cycles() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
        m.handle_command(Command::NewWindow).unwrap();
        m.handle_command(Command::NextWindow).unwrap();
        assert_eq!(m.active_idx(), 0);
        m.handle_command(Command::NextWindow).unwrap();
        assert_eq!(m.active_idx(), 1);
    }

    #[tokio::test]
    async fn split_cwd_is_window_home_base_not_active_pane_cwd() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
        m.set_window_home_cwd(0, Some("/home/base".into()));
        // The active pane reports a DIFFERENT live cwd via OSC 7, and it must be ignored.
        if let Some(pane) = m.active_window().active_pane() {
            pane.with_screen_mut(|s| s.cwd = Some("file:///somewhere/else".to_string()));
        }
        // A split spawns at the window home base, never the active pane's cd location.
        assert_eq!(m.split_cwd().as_deref(), Some("/home/base"));
        // And the split still succeeds structurally.
        m.handle_command(Command::SplitV).unwrap();
        assert_eq!(m.active_window().layout().panes().len(), 2);
    }

    #[tokio::test]
    async fn toggle_sync_panes_flips_the_flag() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        assert!(!m.active_window().sync_input);
        m.handle_command(Command::ToggleSyncPanes).unwrap();
        assert!(m.active_window().sync_input);
        m.handle_command(Command::ToggleSyncPanes).unwrap();
        assert!(!m.active_window().sync_input);
    }

    fn okey(k: plexy_glass_mux::Key) -> plexy_glass_mux::KeyEvent {
        plexy_glass_mux::KeyEvent::plain(k)
    }

    fn type_str(m: &mut WindowManager, s: &str) {
        for c in s.chars() {
            m.handle_overlay_key(&okey(plexy_glass_mux::Key::Char(c)));
        }
    }

    #[tokio::test]
    async fn session_picker_overlay_commits_switch_session() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        m.open_session_picker(vec![
            PickerEntry { name: "main".into(), label: "main".into(), is_current: true },
            PickerEntry { name: "work".into(), label: "work".into(), is_current: false },
        ]);
        assert!(matches!(
            m.overlay(),
            Some(plexy_glass_mux::Overlay::SessionPicker { .. })
        ));
        // Down to "work", then Enter → SwitchSession("work"); overlay closes.
        m.handle_overlay_key(&okey(plexy_glass_mux::Key::Arrow(plexy_glass_mux::Direction::Down)));
        let r = m.handle_overlay_key(&okey(plexy_glass_mux::Key::Enter));
        assert!(matches!(r, OverlayKeyResult::SwitchSession(ref n) if n == "work"));
        assert!(m.overlay().is_none(), "picker closes on commit");
    }

    #[tokio::test]
    async fn rename_window_overlay_commits_name() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        let orig = m.active_window().name.clone();
        m.handle_command(Command::RenameWindow).unwrap();
        // The prompt seeds with the current name for in-place editing.
        match m.overlay() {
            Some(plexy_glass_mux::Overlay::Rename { buf, .. }) => assert_eq!(*buf, orig),
            other => panic!("expected a rename overlay, got {other:?}"),
        }
        // Clear the seeded name, then type a fresh one.
        for _ in 0..orig.chars().count() {
            m.handle_overlay_key(&okey(plexy_glass_mux::Key::Backspace));
        }
        type_str(&mut m, "build");
        // A real rename reports `Committed` so the connection persists it.
        assert_eq!(
            m.handle_overlay_key(&okey(plexy_glass_mux::Key::Enter)),
            OverlayKeyResult::Committed
        );
        assert!(m.overlay().is_none(), "overlay closed on commit");
        assert_eq!(m.active_window().name, "build");
    }

    #[tokio::test]
    async fn rename_window_escape_cancels_without_change() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        let orig = m.active_window().name.clone();
        m.handle_command(Command::RenameWindow).unwrap();
        type_str(&mut m, "zzz");
        m.handle_overlay_key(&okey(plexy_glass_mux::Key::Escape));
        assert!(m.overlay().is_none());
        assert_eq!(m.active_window().name, orig, "cancel leaves the name unchanged");
    }

    #[tokio::test]
    async fn rename_pane_overlay_sets_pane_name() {
        let mut m = make_two_pane_manager().await;
        let active = m.active_window().active();
        m.handle_command(Command::RenamePane).unwrap();
        type_str(&mut m, "logs");
        m.handle_overlay_key(&okey(plexy_glass_mux::Key::Enter));
        assert!(m.overlay().is_none());
        assert_eq!(
            m.active_window().pane(active).and_then(|p| p.name()).as_deref(),
            Some("logs")
        );
    }

    #[tokio::test]
    async fn empty_rename_commit_is_noop_but_closes() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        let orig = m.active_window().name.clone();
        m.handle_command(Command::RenameWindow).unwrap();
        // Clear the seeded name entirely, then commit empty.
        for _ in 0..orig.chars().count() {
            m.handle_overlay_key(&okey(plexy_glass_mux::Key::Backspace));
        }
        // Empty commit closes the overlay but changes nothing, so it must NOT
        // report Committed (no needless persist).
        assert_eq!(
            m.handle_overlay_key(&okey(plexy_glass_mux::Key::Enter)),
            OverlayKeyResult::Redraw
        );
        assert!(m.overlay().is_none());
        assert_eq!(m.active_window().name, orig, "empty commit does not rename");
    }

    #[tokio::test]
    async fn show_help_opens_and_dismisses() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        m.handle_command(Command::ShowHelp).unwrap();
        assert!(matches!(m.overlay(), Some(plexy_glass_mux::Overlay::Help { .. })));
        m.handle_overlay_key(&okey(plexy_glass_mux::Key::Char('q')));
        assert!(m.overlay().is_none());
    }

    async fn make_two_pane_manager() -> WindowManager {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        m.set_default_program("/bin/sh"); // splits must not depend on `$SHELL`
        m.handle_command(Command::SplitV).unwrap();
        m
    }

    #[tokio::test]
    async fn select_layout_puts_active_pane_in_the_main_slot() {
        let mut m = make_two_pane_manager().await;
        m.handle_command(Command::SplitH).unwrap(); // 3 panes
        let active = m.active_window().active();
        m.handle_command(Command::SelectLayout(plexy_glass_mux::LayoutPreset::MainVertical))
            .unwrap();
        let vp = m.viewport();
        let rects: Vec<(PaneId, plexy_glass_mux::Rect)> = m
            .active_window()
            .layout()
            .dfs_leaves()
            .into_iter()
            .map(|p| (p, m.active_window().layout().rect_of(p, vp).unwrap()))
            .collect();
        let (widest, widest_rect) = rects
            .iter()
            .max_by_key(|(_, r)| r.cols)
            .copied()
            .unwrap();
        assert_eq!(widest, active, "active pane takes the main slot: {rects:?}");
        assert!(
            widest_rect.cols > vp.cols / 2,
            "main pane has the major share: {widest_rect:?}"
        );
        // Focus unchanged; PTYs resized to the new rects.
        assert_eq!(m.active_window().active(), active);
        let (rows, cols) = m
            .active_window()
            .pane(active)
            .unwrap()
            .with_screen(|s| (s.active.num_rows(), s.active.num_cols()));
        assert_eq!((rows, cols), (widest_rect.rows, widest_rect.cols));
    }

    #[tokio::test]
    async fn next_layout_cycles_in_order_and_remembers() {
        use plexy_glass_mux::LayoutPreset;
        let mut m = make_two_pane_manager().await;
        m.handle_command(Command::NextLayout).unwrap();
        assert_eq!(m.active_window().last_preset, Some(LayoutPreset::EvenHorizontal));
        m.handle_command(Command::NextLayout).unwrap();
        assert_eq!(m.active_window().last_preset, Some(LayoutPreset::EvenVertical));
        // A manual split does NOT reset the cycle position.
        m.handle_command(Command::SplitV).unwrap();
        m.handle_command(Command::NextLayout).unwrap();
        assert_eq!(m.active_window().last_preset, Some(LayoutPreset::MainHorizontal));
    }

    #[tokio::test]
    async fn select_layout_clears_zoom() {
        let mut m = make_two_pane_manager().await;
        m.handle_command(Command::ZoomToggle).unwrap();
        assert!(m.active_window().is_zoomed());
        m.handle_command(Command::SelectLayout(plexy_glass_mux::LayoutPreset::Tiled))
            .unwrap();
        assert!(!m.active_window().is_zoomed());
    }

    #[tokio::test]
    async fn select_layout_single_pane_is_noop_but_remembers() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        m.handle_command(Command::SelectLayout(plexy_glass_mux::LayoutPreset::Tiled))
            .unwrap();
        assert_eq!(m.active_window().layout().panes().len(), 1);
        assert_eq!(m.active_window().last_preset, Some(plexy_glass_mux::LayoutPreset::Tiled));
    }

    fn gutter_col_for(m: &WindowManager) -> u16 {
        let vp = m.viewport();
        m.active_window()
            .layout()
            .rect_of(PaneId(0), vp)
            .map(|r| r.col + r.cols)
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn border_press_starts_resize_drag() {
        let mut m = make_two_pane_manager().await;
        let gutter = gutter_col_for(&m);
        let event = MouseEvent {
            kind: MouseKind::Press,
            button: MouseButton::Left,
            modifiers: plexy_glass_mux::MouseModifiers::default(),
            row: 5,
            col: gutter,
        };
        m.handle_mouse(event).await.unwrap();
        assert!(m.resize_drag.is_some());
    }

    #[tokio::test]
    async fn classify_click_count_increments_within_window() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        let event = MouseEvent {
            kind: MouseKind::Press,
            button: MouseButton::Left,
            modifiers: plexy_glass_mux::MouseModifiers::default(),
            row: 5,
            col: 5,
        };
        assert_eq!(m.classify_click_count(PaneId(0), &event), 1);
        assert_eq!(m.classify_click_count(PaneId(0), &event), 2);
        assert_eq!(m.classify_click_count(PaneId(0), &event), 3);
        // Clamps at 3.
        assert_eq!(m.classify_click_count(PaneId(0), &event), 3);
    }

    #[tokio::test]
    async fn classify_click_count_resets_on_target_change() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        let mut event = MouseEvent {
            kind: MouseKind::Press,
            button: MouseButton::Left,
            modifiers: plexy_glass_mux::MouseModifiers::default(),
            row: 5,
            col: 5,
        };
        assert_eq!(m.classify_click_count(PaneId(0), &event), 1);
        event.col = 10; // different target
        assert_eq!(m.classify_click_count(PaneId(0), &event), 1);
    }

    #[tokio::test]
    async fn resize_drag_release_clears_state() {
        let mut m = make_two_pane_manager().await;
        let gutter = gutter_col_for(&m);
        m.handle_mouse(MouseEvent {
            kind: MouseKind::Press,
            button: MouseButton::Left,
            modifiers: plexy_glass_mux::MouseModifiers::default(),
            row: 5,
            col: gutter,
        })
        .await
        .unwrap();
        assert!(m.resize_drag.is_some());
        m.handle_mouse(MouseEvent {
            kind: MouseKind::Release,
            button: MouseButton::Left,
            modifiers: plexy_glass_mux::MouseModifiers::default(),
            row: 5,
            col: gutter,
        })
        .await
        .unwrap();
        assert!(m.resize_drag.is_none());
    }

    // Regression: a pane running an app that enabled mouse reporting (less, hx,
    // a TUI) must still be focusable by left-clicking it. Previously Rule 5
    // forwarded *every* event to the child before the focus-on-click rule could
    // run, so a click on such a pane never changed focus.
    #[tokio::test]
    async fn left_click_focuses_non_active_pane_even_with_app_mouse_mode() {
        let mut m = make_two_pane_manager().await;
        let active = m.active_window().active();
        let other = if active == PaneId(0) { PaneId(1) } else { PaneId(0) };
        // Simulate less/hx: the non-active pane's app enabled mouse reporting.
        m.active_window()
            .pane(other)
            .unwrap()
            .with_screen_mut(|s| s.modes.insert(plexy_glass_emulator::Modes::MOUSE_BTN));
        assert!(m.pane_has_any_mouse_mode(other));
        // Click in the middle of the other pane (avoiding borders), translating
        // the logical rect back to physical coords via the status-bar offset.
        let vp = m.viewport();
        let rect = m.active_window().layout().rect_of(other, vp).unwrap();
        let event = MouseEvent {
            kind: MouseKind::Press,
            button: MouseButton::Left,
            modifiers: plexy_glass_mux::MouseModifiers::default(),
            row: rect.row + rect.rows / 2 + m.pane_row_offset,
            col: rect.col + rect.cols / 2,
        };
        m.handle_mouse(event).await.unwrap();
        assert_eq!(
            m.active_window().active(),
            other,
            "left-clicking a pane with app mouse mode on should focus it"
        );
    }

    // SP4: with the status bar on top, mouse events (physical coords) must be
    // shifted up by one row before hitting the logical pane/border layout.
    // A physical click on the logical gutter row is *inside* a pane; the
    // border lives one physical row lower.
    #[tokio::test]
    async fn status_top_offset_shifts_border_hit_row() {
        use plexy_glass_mux::{MouseButton, MouseEvent, MouseKind, MouseModifiers};

        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
        m.handle_command(Command::SplitH).unwrap(); // stacked top/bottom panes
        let vp = m.viewport();
        let r0 = m.active_window().layout().rect_of(PaneId(0), vp).unwrap();
        let r1 = m.active_window().layout().rect_of(PaneId(1), vp).unwrap();
        let top = if r0.row <= r1.row { r0 } else { r1 };
        let gutter_row = top.row + top.rows; // logical border row between panes

        // Status bar on top → pane band shifted down one physical row.
        m.set_status_layout(Some(0), 1);
        let press = |row| MouseEvent {
            kind: MouseKind::Press,
            button: MouseButton::Left,
            modifiers: MouseModifiers::default(),
            row,
            col: 4,
        };

        // Physical gutter_row → logical gutter_row-1 (inside top pane): no drag.
        m.handle_mouse(press(gutter_row)).await.unwrap();
        assert!(
            m.resize_drag.is_none(),
            "physical gutter row is inside a pane under top placement"
        );
        // One physical row lower → logical border row: starts a resize drag.
        m.handle_mouse(press(gutter_row + 1)).await.unwrap();
        assert!(
            m.resize_drag.is_some(),
            "physical gutter_row+1 maps to the logical border under top placement"
        );
    }

    // K8: deterministic resize propagation (no PTY/timing). Proves
    // on_host_resize → Window::resize → Pane::resize → emulator resize for
    // every pane. The flaky e2e resize tests were the only prior coverage.
    #[tokio::test]
    async fn on_host_resize_propagates_to_all_panes() {
        let mut m = make_two_pane_manager().await; // 24x80, vertical split
        m.on_host_resize(PtySize { rows: 40, cols: 120, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let vp = m.viewport();
        // The layout region is inset by the pane frame: full width (120) minus
        // the two outer frame columns.
        assert_eq!(vp.cols, 118, "viewport width did not update (120 - 2 frame cols)");
        let win = m.active_window();
        let panes = win.layout().panes();
        assert_eq!(panes.len(), 2);
        for id in panes {
            let rect = win.layout().rect_of(id, vp).expect("rect");
            let (er, ec) = win
                .pane(id)
                .unwrap()
                .with_screen(|s| (s.active.num_rows(), s.active.num_cols()));
            assert_eq!(er, rect.rows, "pane {id:?} emulator rows != layout rect");
            assert_eq!(ec, rect.cols, "pane {id:?} emulator cols != layout rect");
        }
    }

    // K9: mouse precedence ladder. The wheel rung routes to the active pane.
    #[tokio::test]
    async fn wheel_event_routes_to_active_pane_without_panic() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        let pane = m.active_window().layout().panes()[0];
        let before = m.active_window().pane(pane).unwrap().scroll_offset();
        m.handle_mouse(MouseEvent {
            kind: MouseKind::Wheel { delta: 3 },
            button: MouseButton::None,
            modifiers: plexy_glass_mux::MouseModifiers::default(),
            row: 2,
            col: 2,
        })
        .await
        .unwrap();
        // No scrollback yet → offset clamps to 0; the rung routed without error.
        let after = m.active_window().pane(pane).unwrap().scroll_offset();
        assert!(after >= before);
    }

    // K9: mouse precedence ladder. A status-bar click dispatches its action.
    #[tokio::test]
    async fn status_bar_click_dispatches_select_window() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
        m.handle_command(Command::NewWindow).unwrap(); // 2 windows, active = 1
        assert_eq!(m.active_idx(), 1);
        m.set_status_layout(Some(23), 0);
        m.set_status_hits(vec![plexy_glass_status::StatusHit {
            col_range: 0..5,
            action: plexy_glass_status::ClickAction::SelectWindow(0),
        }]);
        m.handle_mouse(MouseEvent {
            kind: MouseKind::Press,
            button: MouseButton::Left,
            modifiers: plexy_glass_mux::MouseModifiers::default(),
            row: 23,
            col: 2,
        })
        .await
        .unwrap();
        assert_eq!(m.active_idx(), 0, "status-bar click did not select window 0");
    }

    // ---- SP1: pane zoom ----

    #[tokio::test]
    async fn zoom_toggle_sets_and_clears_zoomed() {
        let mut m = make_two_pane_manager().await;
        let active = m.active_window().active();
        m.handle_command(Command::ZoomToggle).unwrap();
        assert_eq!(m.active_window().zoomed, Some(active));
        m.handle_command(Command::ZoomToggle).unwrap();
        assert!(m.active_window().zoomed.is_none());
    }

    #[tokio::test]
    async fn splitting_clears_zoom() {
        let mut m = make_two_pane_manager().await;
        m.handle_command(Command::ZoomToggle).unwrap();
        assert!(m.active_window().is_zoomed());
        m.handle_command(Command::SplitV).unwrap();
        assert!(!m.active_window().is_zoomed(), "split must clear zoom");
    }

    #[tokio::test]
    async fn zoom_resizes_pane_to_full_then_restores() {
        let mut m = make_two_pane_manager().await; // 24x80 vertical split
        let active = m.active_window().active();
        let vp = m.viewport();
        m.handle_command(Command::ZoomToggle).unwrap();
        let zoomed_cols = m
            .active_window()
            .pane(active)
            .unwrap()
            .with_screen(|s| s.active.num_cols());
        assert_eq!(zoomed_cols, vp.cols, "zoomed pane should span full width");
        m.handle_command(Command::ZoomToggle).unwrap();
        let restored_cols = m
            .active_window()
            .pane(active)
            .unwrap()
            .with_screen(|s| s.active.num_cols());
        assert!(restored_cols < vp.cols, "unzoom should restore the split width");
    }

    // ---- SP2: pane resize keybindings ----

    #[tokio::test]
    async fn resize_pane_right_widens_active_pane() {
        let mut m = make_two_pane_manager().await; // vertical split; active = second (right)
        let vp = m.viewport();
        let active = m.active_window().active();
        let before = m.active_window().layout().rect_of(active, vp).unwrap().cols;
        m.handle_command(Command::ResizePane(plexy_glass_mux::Direction::Right)).unwrap();
        let after = m.active_window().layout().rect_of(active, vp).unwrap().cols;
        assert!(after >= before, "active pane should not shrink when growing right");
    }

    #[tokio::test]
    async fn resize_single_pane_is_noop() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        m.handle_command(Command::ResizePane(plexy_glass_mux::Direction::Left)).unwrap();
        assert_eq!(m.active_window().layout().panes().len(), 1);
    }

    // ---- SP3: last-window / last-pane ----

    #[tokio::test]
    async fn last_window_toggle_returns_to_previous() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
        m.handle_command(Command::NewWindow).unwrap(); // active = 1, last = 0
        assert_eq!(m.active_idx(), 1);
        m.handle_command(Command::SelectLastWindow).unwrap();
        assert_eq!(m.active_idx(), 0, "should return to window 0");
        m.handle_command(Command::SelectLastWindow).unwrap();
        assert_eq!(m.active_idx(), 1, "toggling again returns to window 1");
    }

    #[tokio::test]
    async fn last_window_noop_when_single() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        m.handle_command(Command::SelectLastWindow).unwrap();
        assert_eq!(m.active_idx(), 0);
    }

    #[tokio::test]
    async fn last_pane_returns_to_previous() {
        let mut m = make_two_pane_manager().await; // 2 panes, active = second
        let second = m.active_window().active();
        let panes = m.active_window().layout().panes();
        let first = *panes.iter().find(|p| **p != second).unwrap();
        m.active_window_mut().focus(first);
        m.handle_command(Command::SelectLastPane).unwrap();
        assert_eq!(m.active_window().active(), second, "last-pane returns to previous pane");
    }

    // SP6: killing the active window must keep `last_active_window` valid.
    // Regression for a stale index that pointed past the shifted window list.
    #[tokio::test]
    async fn kill_window_keeps_last_active_index_valid() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
        m.handle_command(Command::NewWindow).unwrap(); // W1, active=1, last=0
        m.handle_command(Command::NewWindow).unwrap(); // W2, active=2, last=1
        m.handle_command(Command::SelectWindow(0)).unwrap(); // active=0, last=Some(2) -> W2
        assert_eq!(m.active_idx(), 0);
        let w2_id = m.windows()[2].id;
        // Kill W0 (active). windows shift to [W1, W2]; last (was index 2)
        // must follow W2 to its new index 1, not dangle at 2.
        m.handle_command(Command::KillWindow).unwrap();
        assert_eq!(m.windows().len(), 2);
        m.handle_command(Command::SelectLastWindow).unwrap();
        assert_eq!(m.windows()[m.active_idx()].id, w2_id, "toggle lands on the original W2");
    }

    // SP6: a window removed by pane-death (its last pane exited) must shift
    // `last_active_window` the same way `active` is shifted.
    #[tokio::test]
    async fn pane_death_window_removal_keeps_last_active_valid() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
        m.handle_command(Command::NewWindow).unwrap(); // W1 (pane 1)
        m.handle_command(Command::NewWindow).unwrap(); // W2 (pane 2)
        m.handle_command(Command::SelectWindow(0)).unwrap(); // active=0, last=Some(2) -> W2
        let w2_id = m.windows()[2].id;
        // W1's sole pane (PaneId(1)) dies, so window index 1 is removed.
        m.handle_pane_death(PaneId(1)).unwrap();
        assert_eq!(m.windows().len(), 2);
        // last (was index 2, after the removed index 1) must follow W2 to
        // index 1 so the toggle still reaches it.
        m.handle_command(Command::SelectLastWindow).unwrap();
        assert_eq!(m.windows()[m.active_idx()].id, w2_id, "toggle lands on the original W2");
    }

    // ----- choose-tree -----

    fn mk_mgr() -> WindowManager {
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            Arc::new(Notify::new()),
            None,
            cfg(),
        )
        .unwrap();
        m.set_default_program("/bin/sh"); // new windows/splits must not depend on `$SHELL`
        m
    }

    fn tnode(session: &str, window: Option<u32>, pane: Option<u32>, depth: u8) -> TreeNode {
        TreeNode {
            session: session.into(),
            window: window.map(WindowId),
            pane: pane.map(PaneId),
            depth,
            label: String::new(),
            name: String::new(),
            index: 0,
            is_current: false,
        }
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(plexy_glass_mux::Key::Char(c), plexy_glass_mux::Modifiers::empty())
    }

    #[tokio::test]
    async fn open_tree_then_enter_switches_and_closes() {
        let mut m = mk_mgr();
        m.open_tree(vec![tnode("work", None, None, 0), tnode("work", Some(0), None, 1)]);
        assert!(matches!(m.overlay(), Some(Overlay::Tree(_))));
        let r = m
            .handle_overlay_key(&KeyEvent::new(plexy_glass_mux::Key::Enter, plexy_glass_mux::Modifiers::empty()));
        assert!(matches!(r, OverlayKeyResult::Tree(TreeAction::Switch { .. })));
        assert!(m.overlay().is_none(), "switch closes the overlay");
    }

    #[tokio::test]
    async fn tree_kill_keeps_overlay_open() {
        let mut m = mk_mgr();
        m.open_tree(vec![
            tnode("work", None, None, 0),
            tnode("work", Some(0), None, 1),
            tnode("work", Some(0), Some(5), 2),
        ]);
        m.handle_overlay_key(&key('j'));
        m.handle_overlay_key(&key('j')); // select the pane node
        assert!(matches!(m.handle_overlay_key(&key('x')), OverlayKeyResult::Redraw));
        let r = m.handle_overlay_key(&key('y'));
        assert!(matches!(
            r,
            OverlayKeyResult::Tree(TreeAction::KillPane { pane: PaneId(5), .. })
        ));
        assert!(m.overlay().is_some(), "kill keeps the overlay open");
    }

    #[tokio::test]
    async fn by_id_helpers_hit_and_miss() {
        let mut m = mk_mgr();
        m.handle_command(Command::SplitV).unwrap(); // window 0: panes {0,1}, active 1
        m.handle_command(Command::NewWindow).unwrap(); // window 1 (pane 2), active window 1

        // Real cross-window focus: from window 1, focus pane 0 (in window 0).
        assert!(m.focus_pane_by_id(PaneId(0)));
        assert_eq!(m.active_idx(), 0, "focus switched to pane 0's window");
        assert_eq!(m.windows()[0].active(), PaneId(0), "pane 0 became its window's active pane");
        assert!(!m.focus_pane_by_id(PaneId(999)));

        assert!(m.select_window_by_id(WindowId(1)));
        assert_eq!(m.active_idx(), 1);
        assert!(!m.select_window_by_id(WindowId(99)));

        assert!(m.rename_window_by_id(WindowId(0), "renamed".into()));
        assert_eq!(m.windows()[0].name, "renamed");
        assert!(!m.rename_window_by_id(WindowId(99), "x".into()));

        // Rename a pane and read the stored name back.
        assert!(m.rename_pane_by_id(PaneId(0), "p".into()));
        assert_eq!(m.windows()[0].pane(PaneId(0)).unwrap().name(), Some("p".to_string()));
        assert!(!m.rename_pane_by_id(PaneId(999), "p".into()));

        assert!(m.kill_window_panes(WindowId(1)));
        assert!(!m.kill_window_panes(WindowId(99)));
        assert!(!m.kill_pane_child(PaneId(999)));
    }

    #[tokio::test]
    async fn by_id_helpers_clear_zoom() {
        let mut m = mk_mgr();
        m.handle_command(Command::SplitV).unwrap(); // window 0: panes {0,1}, active 1

        // `focus_pane_by_id` clears the TARGET window's zoom so the pane is visible.
        m.handle_command(Command::ZoomToggle).unwrap();
        assert!(m.windows()[0].is_zoomed());
        assert!(m.focus_pane_by_id(PaneId(0)));
        assert!(!m.windows()[0].is_zoomed(), "focus_pane_by_id unzooms the target window");

        // `select_window_by_id` clears the leaving window's zoom before switching.
        m.handle_command(Command::NewWindow).unwrap(); // window 1 active
        m.select_window_by_id(WindowId(0)); // back to window 0
        m.handle_command(Command::ZoomToggle).unwrap();
        assert!(m.windows()[0].is_zoomed());
        m.select_window_by_id(WindowId(1));
        assert!(!m.windows()[0].is_zoomed(), "select_window_by_id unzooms before switching");
    }

    // ----- break / join / swap -----

    #[tokio::test]
    async fn mark_pane_toggles_and_death_clears() {
        let mut m = mk_mgr();
        m.handle_command(Command::SplitV).unwrap(); // panes 0,1; active 1
        assert_eq!(m.marked_pane(), None);
        m.handle_command(Command::MarkPane).unwrap();
        assert_eq!(m.marked_pane(), Some(PaneId(1)));
        m.handle_command(Command::MarkPane).unwrap();
        assert_eq!(m.marked_pane(), None, "re-marking the active pane toggles off");
        m.handle_command(Command::MarkPane).unwrap();
        assert_eq!(m.marked_pane(), Some(PaneId(1)));
        m.handle_pane_death(PaneId(1)).unwrap();
        assert_eq!(m.marked_pane(), None, "a dead pane is unmarked");
    }

    #[tokio::test]
    async fn break_pane_moves_active_into_new_window() {
        let mut m = mk_mgr();
        m.handle_command(Command::SplitV).unwrap(); // window 0: panes 0,1; active 1
        m.handle_command(Command::BreakPane).unwrap();
        assert_eq!(m.windows().len(), 2);
        assert_eq!(m.windows()[0].layout().panes(), vec![PaneId(0)]);
        assert_eq!(m.active_idx(), 1);
        assert_eq!(m.active_window().active(), PaneId(1));
        assert_eq!(m.last_active_window, Some(0));
    }

    #[tokio::test]
    async fn break_pane_single_pane_is_noop_with_status() {
        let mut m = mk_mgr();
        m.handle_command(Command::BreakPane).unwrap();
        assert_eq!(m.windows().len(), 1, "single pane is not broken out");
        assert_eq!(m.take_active_message(), Some("only pane in window"));
    }

    #[tokio::test]
    async fn swap_pane_exchanges_with_neighbor() {
        let mut m = mk_mgr();
        m.handle_command(Command::SplitV).unwrap(); // panes 0,1; active 1
        let vp = m.viewport();
        let before = m.active_window().layout().rect_of(PaneId(1), vp).unwrap();
        m.handle_command(Command::SwapPane(false)).unwrap(); // swap with previous (pane 0)
        assert_eq!(m.active_window().active(), PaneId(1), "active id unchanged");
        let after = m.active_window().layout().rect_of(PaneId(1), vp).unwrap();
        assert_ne!(before, after, "active pane's content moved to the other slot");
    }

    #[tokio::test]
    async fn join_pane_moves_marked_and_removes_empty_source() {
        let mut m = mk_mgr(); // W0 (WindowId 0), pane 0
        m.handle_command(Command::NewWindow).unwrap(); // W1 (WindowId 1), pane 1, active
        m.handle_command(Command::SelectWindow(0)).unwrap(); // active W0
        m.handle_command(Command::MarkPane).unwrap(); // marked = pane 0
        m.handle_command(Command::SelectWindow(1)).unwrap(); // active W1
        m.handle_command(Command::JoinPane(SplitDir::Vertical)).unwrap();
        assert_eq!(m.windows().len(), 1, "emptied source window removed");
        let panes = m.active_window().layout().panes();
        assert!(panes.contains(&PaneId(0)) && panes.contains(&PaneId(1)));
        assert_eq!(m.active_window().active(), PaneId(0), "joined pane is active");
        assert_eq!(m.marked_pane(), None, "mark cleared after join");
    }

    #[tokio::test]
    async fn join_pane_fixes_active_and_last_active_after_source_removal() {
        let mut m = mk_mgr(); // W0 idx0
        m.handle_command(Command::NewWindow).unwrap(); // W1 idx1
        m.handle_command(Command::NewWindow).unwrap(); // W2 idx2, active
        m.handle_command(Command::SelectWindow(0)).unwrap(); // active W0
        m.handle_command(Command::MarkPane).unwrap(); // marked = pane 0 (W0, a lower index)
        m.handle_command(Command::SelectWindow(1)).unwrap(); // active W1
        m.handle_command(Command::SelectWindow(2)).unwrap(); // active W2, last = W1
        let w2_id = m.windows()[m.active_idx()].id;
        let w1_id = m.windows()[1].id;
        m.handle_command(Command::JoinPane(SplitDir::Vertical)).unwrap(); // W0 empties → removed
        assert_eq!(m.windows()[m.active_idx()].id, w2_id, "active stays W2 across removal");
        m.handle_command(Command::SelectLastWindow).unwrap();
        assert_eq!(m.windows()[m.active_idx()].id, w1_id, "last-active follows W1 across removal");
    }

    #[tokio::test]
    async fn join_pane_source_survives_when_multipane() {
        let mut m = mk_mgr(); // W0 idx0 pane0
        m.handle_command(Command::NewWindow).unwrap(); // W1 idx1 pane1, active
        m.handle_command(Command::SplitV).unwrap(); // W1 panes {1,2}, active 2
        let half = m.windows()[1].pane(PaneId(1)).unwrap().with_screen(|s| s.active.num_cols());
        m.handle_command(Command::MarkPane).unwrap(); // marked = pane 2 (W1)
        m.handle_command(Command::SelectWindow(0)).unwrap(); // active W0
        m.handle_command(Command::JoinPane(SplitDir::Vertical)).unwrap();
        assert_eq!(m.windows().len(), 2, "multi-pane source survives");
        let active_panes = m.active_window().layout().panes();
        assert!(active_panes.contains(&PaneId(0)) && active_panes.contains(&PaneId(2)));
        assert_eq!(m.windows()[1].layout().panes(), vec![PaneId(1)], "source keeps its other pane");
        let full = m.windows()[1].pane(PaneId(1)).unwrap().with_screen(|s| s.active.num_cols());
        assert!(full > half, "surviving source pane was resized wider ({half} -> {full})");
        assert_eq!(m.marked_pane(), None);
    }

    #[tokio::test]
    async fn swap_marked_pane_same_window_then_cross_window_status() {
        let mut m = mk_mgr();
        m.handle_command(Command::SplitV).unwrap(); // W0 panes 0,1; active 1
        let vp = m.viewport();
        m.handle_command(Command::MarkPane).unwrap(); // marked = 1
        m.handle_command(Command::SelectPane(plexy_glass_mux::Direction::Left)).unwrap(); // active 0
        let before = m.active_window().layout().rect_of(PaneId(1), vp).unwrap();
        m.handle_command(Command::SwapMarkedPane).unwrap();
        let after = m.active_window().layout().rect_of(PaneId(1), vp).unwrap();
        assert_ne!(before, after, "same-window swap exchanged slots");
        assert_eq!(m.marked_pane(), Some(PaneId(1)), "mark preserved across swap");

        // Cross-window: marked stays in W0; move active to a new window → status.
        m.handle_command(Command::NewWindow).unwrap();
        m.handle_command(Command::SwapMarkedPane).unwrap();
        assert_eq!(
            m.take_active_message(),
            Some("marked pane is in another window — use join")
        );
    }

    #[tokio::test]
    async fn kill_pane_and_kill_window_clear_mark() {
        // KillPane on the marked pane clears the mark (no death-channel round-trip).
        let mut m = mk_mgr();
        m.handle_command(Command::SplitV).unwrap(); // panes 0,1; active 1
        m.handle_command(Command::MarkPane).unwrap(); // marked = 1 (active)
        m.handle_command(Command::KillPane).unwrap();
        assert_eq!(m.marked_pane(), None, "killing the marked pane clears the mark");

        // KillWindow clears a mark on any pane in the removed window.
        let mut m = mk_mgr();
        m.handle_command(Command::SplitV).unwrap(); // W0: panes 0,1
        m.handle_command(Command::NewWindow).unwrap(); // W1, active
        m.handle_command(Command::SelectWindow(0)).unwrap(); // active W0
        m.handle_command(Command::MarkPane).unwrap(); // marks a W0 pane
        m.handle_command(Command::KillWindow).unwrap(); // removes W0
        assert_eq!(m.marked_pane(), None, "killing the marked pane's window clears the mark");
    }

    #[tokio::test]
    async fn structural_commands_clear_zoom() {
        for cmd in [Command::SwapPane(false), Command::BreakPane] {
            let mut m = mk_mgr();
            m.handle_command(Command::SplitV).unwrap();
            m.handle_command(Command::ZoomToggle).unwrap();
            assert!(m.active_window().is_zoomed());
            m.handle_command(cmd.clone()).unwrap();
            assert!(!m.active_window().is_zoomed(), "{cmd:?} must clear zoom");
        }
        // Join/SwapMarked need a same-window marked pane.
        for cmd in [Command::JoinPane(SplitDir::Vertical), Command::SwapMarkedPane] {
            let mut m = mk_mgr();
            m.handle_command(Command::SplitV).unwrap(); // panes 0,1; active 1
            m.handle_command(Command::SelectPane(plexy_glass_mux::Direction::Left)).unwrap(); // active 0
            m.handle_command(Command::MarkPane).unwrap(); // marked 0
            m.handle_command(Command::SelectPane(plexy_glass_mux::Direction::Right)).unwrap(); // active 1
            m.handle_command(Command::ZoomToggle).unwrap();
            assert!(m.active_window().is_zoomed());
            m.handle_command(cmd.clone()).unwrap();
            assert!(!m.active_window().is_zoomed(), "{cmd:?} must clear zoom");
        }
    }

    #[tokio::test]
    async fn swap_pane_next_and_prev_are_directional() {
        let setup = || {
            let mut m = mk_mgr();
            m.handle_command(Command::SplitV).unwrap();
            m.handle_command(Command::SplitV).unwrap(); // leaves [0,1,2], active 2
            m
        };
        // prev: active 2 swaps with pane 1.
        let mut m = setup();
        let vp = m.viewport();
        let r1 = m.active_window().layout().rect_of(PaneId(1), vp).unwrap();
        m.handle_command(Command::SwapPane(false)).unwrap();
        assert_eq!(m.active_window().layout().rect_of(PaneId(2), vp).unwrap(), r1, "prev swaps 2<->1");
        // next: active 2 wraps to pane 0.
        let mut m = setup();
        let r0 = m.active_window().layout().rect_of(PaneId(0), vp).unwrap();
        m.handle_command(Command::SwapPane(true)).unwrap();
        assert_eq!(m.active_window().layout().rect_of(PaneId(2), vp).unwrap(), r0, "next wraps 2<->0");
    }

    #[tokio::test]
    async fn swap_pane_single_pane_is_noop() {
        let mut m = mk_mgr();
        m.handle_command(Command::SwapPane(true)).unwrap();
        assert_eq!(m.active_window().layout().panes(), vec![PaneId(0)]);
        assert_eq!(m.active_window().active(), PaneId(0));
    }

    #[tokio::test]
    async fn break_pane_names_window_from_pane_name() {
        let mut m = mk_mgr();
        m.handle_command(Command::SplitV).unwrap(); // panes 0,1; active 1
        assert!(m.rename_pane_by_id(PaneId(1), "editor".into()));
        m.handle_command(Command::BreakPane).unwrap();
        assert_eq!(m.windows().len(), 2);
        assert_eq!(m.active_window().name, "editor", "new window named from the pane");
    }

    #[tokio::test]
    async fn join_pane_same_window_reorders() {
        let mut m = mk_mgr();
        m.handle_command(Command::SplitV).unwrap(); // panes 0,1; active 1
        m.handle_command(Command::SelectPane(plexy_glass_mux::Direction::Left)).unwrap(); // active 0
        m.handle_command(Command::MarkPane).unwrap(); // marked 0
        m.handle_command(Command::SelectPane(plexy_glass_mux::Direction::Right)).unwrap(); // active 1
        m.handle_command(Command::JoinPane(SplitDir::Horizontal)).unwrap();
        assert_eq!(m.windows().len(), 1, "same-window join keeps one window");
        let panes = m.active_window().layout().panes();
        assert_eq!(panes.len(), 2);
        assert!(panes.contains(&PaneId(0)) && panes.contains(&PaneId(1)));
        assert_eq!(m.active_window().active(), PaneId(0), "joined pane is active");
        assert_eq!(m.marked_pane(), None);
    }

    #[tokio::test]
    async fn join_pane_marked_equals_active_is_status_noop() {
        let mut m = mk_mgr();
        m.handle_command(Command::SplitV).unwrap(); // active 1
        m.handle_command(Command::MarkPane).unwrap(); // marked = active = 1
        let before = m.active_window().layout().panes().len();
        m.handle_command(Command::JoinPane(SplitDir::Vertical)).unwrap();
        assert_eq!(m.active_window().layout().panes().len(), before, "no structural change");
        assert_eq!(m.take_active_message(), Some("marked pane is the active pane"));
    }

    #[tokio::test]
    async fn mark_survives_break_then_swap_is_cross_window() {
        let mut m = mk_mgr();
        m.handle_command(Command::SplitV).unwrap(); // panes 0,1; active 1
        m.handle_command(Command::MarkPane).unwrap(); // marked = 1 (active)
        m.handle_command(Command::BreakPane).unwrap(); // pane 1 → new window
        assert_eq!(m.marked_pane(), Some(PaneId(1)), "mark survives a break (pane still lives)");
        m.handle_command(Command::SelectWindow(0)).unwrap(); // active W0 (pane 0)
        m.handle_command(Command::SwapMarkedPane).unwrap();
        assert_eq!(
            m.take_active_message(),
            Some("marked pane is in another window — use join")
        );
    }

    #[tokio::test]
    async fn buffer_picker_paste_closes_delete_stays_open() {
        let mut m = mk_mgr();
        m.open_buffer_picker(vec![
            plexy_glass_mux::BufferEntry { name: "buffer1".into(), preview: "a".into() },
            plexy_glass_mux::BufferEntry { name: "buffer0".into(), preview: "b".into() },
        ]);
        assert!(matches!(m.overlay(), Some(Overlay::BufferPicker(_))));
        // `d` deletes the selected buffer and keeps the overlay open.
        let r = m.handle_overlay_key(&key('d'));
        assert!(
            matches!(&r, OverlayKeyResult::Buffer(plexy_glass_mux::BufferAction::Delete(n)) if n == "buffer1"),
            "got {r:?}"
        );
        assert!(m.overlay().is_some(), "delete keeps the overlay open");
        // Enter pastes the now-selected buffer and closes.
        let r = m.handle_overlay_key(&KeyEvent::new(
            plexy_glass_mux::Key::Enter,
            plexy_glass_mux::Modifiers::empty(),
        ));
        assert!(
            matches!(&r, OverlayKeyResult::Buffer(plexy_glass_mux::BufferAction::Paste(n)) if n == "buffer0"),
            "got {r:?}"
        );
        assert!(m.overlay().is_none(), "paste closes the overlay");
    }

    // ----- activity / bell monitoring -----

    #[tokio::test]
    async fn toggle_monitor_commands_flip_and_message() {
        let mut m = mk_mgr();
        // Defaults: `monitor_activity` off, `monitor_bell` on.
        assert!(!m.active_window().monitor_activity());
        assert!(m.active_window().monitor_bell());
        m.handle_command(Command::ToggleMonitorActivity).unwrap();
        assert_eq!(m.take_active_message(), Some("monitor-activity on"));
        assert!(m.active_window().monitor_activity());
        m.handle_command(Command::ToggleMonitorActivity).unwrap();
        assert_eq!(m.take_active_message(), Some("monitor-activity off"));
        m.handle_command(Command::ToggleMonitorBell).unwrap();
        assert_eq!(m.take_active_message(), Some("monitor-bell off"));
        assert!(!m.active_window().monitor_bell());
    }

    #[tokio::test]
    async fn update_monitor_flags_clears_active_window_alerts() {
        let mut m = mk_mgr();
        m.active_window_mut().set_bell(); // a stale alert on the (current) window
        m.active_window_mut().set_activity();
        m.update_monitor_flags();
        assert!(!m.active_window().bell_flag(), "current window's bell cleared");
        assert!(!m.active_window().activity_flag(), "current window's activity cleared");
    }

    #[tokio::test]
    async fn update_monitor_flags_sets_background_activity_then_clears_on_switch() {
        let mut m = mk_mgr(); // window 0
        m.handle_command(Command::NewWindow).unwrap(); // window 1 (active)
        m.handle_command(Command::ToggleMonitorActivity).unwrap(); // monitor on for window 1
        m.handle_command(Command::SelectWindow(0)).unwrap(); // window 0 active; window 1 background
        // Generate output in the background window's pane (cat echoes it).
        let pid = m.windows()[1].layout().panes()[0];
        m.windows()[1]
            .pane(pid)
            .unwrap()
            .send_input(bytes::Bytes::from_static(b"x\n"))
            .await
            .unwrap();
        // Drain into the sticky flag (the coordinator's per-frame step).
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            m.update_monitor_flags();
            if m.windows()[1].activity_flag() {
                break;
            }
            if Instant::now() > deadline {
                panic!("background activity never flagged");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!m.active_window().activity_flag(), "the current window is never flagged");
        // Switching to the flagged window clears it on the next update.
        m.handle_command(Command::SelectWindow(1)).unwrap();
        m.update_monitor_flags();
        assert!(!m.windows()[1].activity_flag(), "flag cleared once the window is current");
    }

    #[tokio::test]
    async fn update_monitor_flags_sets_background_bell_from_a_real_bel() {
        // Exercises the full emulator-BEL → pane-bell → window-flag chain with a
        // real BEL (not a faked atomic). monitor_bell is on by default.
        //
        // Window 1 must run the test's `cat` spec, NOT `Command::NewWindow`
        // (which deliberately spawns `$SHELL`): cat echoes the typed BEL byte
        // verbatim within milliseconds, whereas an interactive login shell
        // only emits a BEL if its line editor happens to beep on ^G, and that
        // comes after fork/exec + sourcing the user's rc files, which under
        // full-suite load occasionally exceeded the 5s deadline (the old
        // flake) and breaks outright under a different/misconfigured $SHELL.
        let mut m = mk_mgr(); // window 0
        m.new_window_with_spec(spec(), "w1".into()).unwrap(); // window 1 (active), runs `cat`
        m.handle_command(Command::SelectWindow(0)).unwrap(); // window 0 active; window 1 background
        let pid = m.windows()[1].layout().panes()[0];
        // The trailing newline flushes cat's line so it OUTPUTS the raw BEL (the
        // line-discipline echo may render the input as "^G").
        m.windows()[1]
            .pane(pid)
            .unwrap()
            .send_input(bytes::Bytes::from_static(b"\x07\n"))
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            m.update_monitor_flags();
            if m.windows()[1].bell_flag() {
                break;
            }
            if Instant::now() > deadline {
                panic!("background bell never flagged window 1");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!m.active_window().bell_flag(), "the current window is never bell-flagged");
    }

    #[tokio::test]
    async fn open_popup_sets_state_with_derived_size() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
        assert!(!m.has_popup());
        m.handle_command(Command::OpenPopup { command: None }).unwrap();
        assert!(m.has_popup());
        let rect = plexy_glass_mux::popup_rect(m.viewport());
        let (rows, cols) = m
            .popup()
            .unwrap()
            .pane
            .with_screen(|s| (s.active.num_rows(), s.active.num_cols()));
        assert_eq!((rows, cols), (rect.rows - 2, rect.cols - 2));
        assert_eq!(m.popup().unwrap().title, "popup");
    }

    #[tokio::test]
    async fn open_popup_is_last_wins_and_close_clears() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
        m.handle_command(Command::OpenPopup { command: Some("sleep 600".into()) }).unwrap();
        assert_eq!(m.popup().unwrap().title, "sleep 600");
        m.handle_command(Command::OpenPopup { command: None }).unwrap();
        assert_eq!(m.popup().unwrap().title, "popup", "second open replaces the first");
        m.handle_command(Command::ClosePopup).unwrap();
        assert!(!m.has_popup());
        // Idempotent.
        m.handle_command(Command::ClosePopup).unwrap();
        assert!(!m.has_popup());
    }

    #[tokio::test]
    async fn input_target_pane_prefers_popup() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
        let layout_pane = m.active_window().active();
        assert_eq!(
            m.input_target_pane().map(|p| p.id()),
            Some(layout_pane),
            "no popup: input targets the active layout pane"
        );
        m.handle_command(Command::OpenPopup { command: None }).unwrap();
        let popup_id = m.popup().unwrap().pane.id();
        assert_eq!(
            m.input_target_pane().map(|p| p.id()),
            Some(popup_id),
            "popup open: it is modal and owns user input"
        );
        m.handle_command(Command::ClosePopup).unwrap();
        assert_eq!(
            m.input_target_pane().map(|p| p.id()),
            Some(layout_pane),
            "popup closed: input returns to the active layout pane"
        );
    }

    #[tokio::test]
    async fn paste_gate_reads_the_popup_pane_mode_not_the_layout_pane() {
        // Regression: the Ctrl+a ] / choose-buffer paste gate used to read
        // `BRACKETED_PASTE` from the active LAYOUT pane even while a popup was
        // open, but the bytes go to the popup (`handle_input_bytes` is
        // popup-first), so the wrong pane's mode decided the wrapping. The
        // gate decision must be computed from
        // `input_target_pane().wants_bracketed_paste()`.
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
        m.handle_command(Command::OpenPopup { command: None }).unwrap();
        // The popup app turns bracketed paste ON; the layout pane has it OFF.
        m.popup().unwrap().pane.with_screen_mut(|s| {
            s.modes.insert(plexy_glass_emulator::Modes::BRACKETED_PASTE);
        });
        let layout = m.active_window().active_pane().unwrap();
        assert!(!layout.wants_bracketed_paste(), "layout pane: mode off");
        assert!(
            m.input_target_pane().unwrap().wants_bracketed_paste(),
            "while a popup is open, the paste gate's input is the POPUP's mode"
        );
        m.handle_command(Command::ClosePopup).unwrap();
        assert!(
            !m.input_target_pane().unwrap().wants_bracketed_paste(),
            "popup closed: the gate reads the layout pane's mode again"
        );
    }

    #[tokio::test]
    async fn popup_cwd_prefers_live_osc7_then_home_base() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        m.set_window_home_cwd(0, Some("/home/base".into()));
        // No live OSC-7 cwd → home base.
        assert_eq!(m.popup_cwd().as_deref(), Some("/home/base"));
        // Live OSC-7 cwd wins (documented divergence from split_cwd).
        if let Some(pane) = m.active_window().active_pane() {
            pane.with_screen_mut(|s| s.cwd = Some("file:///live/here".to_string()));
        }
        assert_eq!(m.popup_cwd().as_deref(), Some("/live/here"));
        assert_eq!(m.split_cwd().as_deref(), Some("/home/base"), "splits unaffected");
    }

    #[tokio::test]
    async fn popup_pane_death_closes_popup_only() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
        m.handle_command(Command::OpenPopup { command: None }).unwrap();
        let popup_id = m.popup().unwrap().pane.id();
        m.handle_pane_death(popup_id).unwrap();
        assert!(!m.has_popup());
        assert_eq!(m.windows().len(), 1, "layout untouched by popup death");
    }

    #[tokio::test]
    async fn last_window_death_also_closes_popup() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
        m.handle_command(Command::OpenPopup { command: None }).unwrap();
        // The only layout pane dies → session is ending; popup must not orphan.
        m.handle_pane_death(PaneId(0)).unwrap();
        assert!(m.is_empty());
        assert!(!m.has_popup());
    }

    #[tokio::test]
    async fn kill_window_emptying_session_also_closes_popup() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
        m.handle_command(Command::OpenPopup { command: None }).unwrap();
        m.handle_command(Command::KillWindow).unwrap();
        assert!(m.is_empty());
        assert!(!m.has_popup());
    }

    #[tokio::test]
    async fn host_resize_resizes_popup_pane() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
            cfg(),
        )
        .unwrap();
        m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
        m.handle_command(Command::OpenPopup { command: None }).unwrap();
        m.on_host_resize(PtySize { rows: 40, cols: 120, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let rect = plexy_glass_mux::popup_rect(m.viewport());
        let (rows, cols) = m
            .popup()
            .unwrap()
            .pane
            .with_screen(|s| (s.active.num_rows(), s.active.num_cols()));
        assert_eq!((rows, cols), (rect.rows - 2, rect.cols - 2));
    }

    #[tokio::test]
    async fn popup_swallows_clicks_outside_and_keeps_focus() {
        let mut m = make_two_pane_manager().await;
        let focused_before = m.active_window().active();
        let other = if focused_before == PaneId(0) { PaneId(1) } else { PaneId(0) };
        m.handle_command(Command::OpenPopup { command: None }).unwrap();
        // Click squarely inside the OTHER layout pane (outside the popup box).
        // Geometry check: viewport is (1,1,21,78), so the popup box spans rows
        // 3..=18, cols 9..=70. SplitV focuses the new right pane, so `other` is
        // the left pane at col 1, left of the popup's left edge (col 9). (Even
        // if `other` were the right pane, row rect.row+1 == 2 is above the box.)
        let vp = m.viewport();
        let rect = m.active_window().layout().rect_of(other, vp).unwrap();
        let popup_box = plexy_glass_mux::popup_rect(vp);
        assert!(
            rect.col < popup_box.col || rect.row + 1 < popup_box.row,
            "test premise: click target must be outside the popup box"
        );
        m.handle_mouse(MouseEvent {
            kind: MouseKind::Press,
            button: MouseButton::Left,
            modifiers: plexy_glass_mux::MouseModifiers::default(),
            row: rect.row + 1 + m.pane_row_offset,
            col: rect.col,
        })
        .await
        .unwrap();
        assert_eq!(m.active_window().active(), focused_before, "popup is modal: no focus change");
        assert!(m.has_popup(), "popup still open");
        assert!(m.resize_drag.is_none(), "no border drag starts under a popup");
    }

    #[tokio::test]
    async fn popup_swallows_interior_click_when_child_has_no_mouse_mode() {
        let mut m = make_two_pane_manager().await;
        let focused_before = m.active_window().active();
        m.handle_command(Command::OpenPopup { command: None }).unwrap();
        // Box center is genuinely interior: for a (1,1,21,78) viewport the box
        // is rows 3..=18 / cols 9..=70, so (11, 40) sits inside the border. It
        // also happens to sit on the SplitV gutter, and without Rule 0 this press
        // would start a resize drag, so the drag/focus asserts below bite.
        let rect = plexy_glass_mux::popup_rect(m.viewport());
        m.handle_mouse(MouseEvent {
            kind: MouseKind::Press,
            button: MouseButton::Left,
            modifiers: plexy_glass_mux::MouseModifiers::default(),
            row: rect.row + rect.rows / 2 + m.pane_row_offset,
            col: rect.col + rect.cols / 2,
        })
        .await
        .unwrap();
        assert!(m.has_popup());
        assert!(m.selection().is_none(), "no layout selection starts under a popup");
        assert!(m.resize_drag.is_none(), "no border drag starts under a popup");
        assert_eq!(m.active_window().active(), focused_before, "no focus change under a popup");
    }

    #[tokio::test]
    async fn open_popup_clears_in_flight_resize_drag() {
        let mut m = make_two_pane_manager().await;
        let gutter = gutter_col_for(&m);
        // Start a border drag...
        m.handle_mouse(MouseEvent {
            kind: MouseKind::Press,
            button: MouseButton::Left,
            modifiers: plexy_glass_mux::MouseModifiers::default(),
            row: 5,
            col: gutter,
        })
        .await
        .unwrap();
        assert!(m.resize_drag.is_some(), "premise: drag started");
        // ...then a popup opens (e.g. via a keybinding) mid-drag.
        m.handle_command(Command::OpenPopup { command: None }).unwrap();
        assert!(m.resize_drag.is_none(), "popup open must drop the frozen drag");
        assert!(m.selection().is_none());
    }
}
