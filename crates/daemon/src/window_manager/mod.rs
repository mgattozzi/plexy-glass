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
mod tests;
