//! Owns all windows for one attached client.

use std::io::Error;
use std::sync::Arc;
use std::time::Duration;

use mouse::{ClickHistory, PaneDrag, ResizeDrag, TabDrag};
use plexy_glass_config::GlyphTier;
use plexy_glass_mux::{Overlay, PaneId, Rect, Selection, SplitDir, WindowId};
use plexy_glass_protocol::{PtySize, SpawnSpec};
// See window.rs: tokio::time::Instant is used so unit tests with
// start_paused = true / time::advance control silence checks without real sleeps.
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;

use crate::declared;
use crate::error::DaemonError;
use crate::pane::Pane;
use crate::popup::{self, Popup};
use crate::window::{CellPx, CompletionEvent, Window};

/// How long a transient status-line message stays visible before it is cleared
/// on the next recompose. Mirrored by the `Session` wake timer.
pub(crate) const STATUS_TTL: Duration = Duration::from_secs(3);

/// Severity of a transient status-line message. Selects both the leading glyph
/// (the primary, color-independent channel) and the palette color the message
/// is painted in, so success vs error is legible even without color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// Neutral notice (onboarding hints, "switched to X", monitor alerts).
    Info,
    /// A positive action completed (copied, reloaded, marked, killed).
    Success,
    /// A non-fatal caveat (e.g. some keymap bindings were skipped).
    Warn,
    /// A failure the user should notice (no such session, reload failed).
    Error,
}

impl Severity {
    /// Palette key resolved against the active palette for the message color.
    pub const fn palette_key(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Success => "ok",
            Self::Warn => "warn",
            Self::Error => "alert",
        }
    }

    /// Leading glyph for the message, by glyph tier. This is the non-color
    /// channel, so the severity reads correctly on a monochrome terminal and on
    /// the `ascii` tier.
    // ponytail: severity glyphs live here (not in the status crate's glyphs.rs)
    // so message styling stays self-contained in the daemon. The daemon→status
    // dependency is one-way, so a `Severity`-keyed table can't live over there.
    pub const fn glyph(self, tier: GlyphTier) -> &'static str {
        match (tier, self) {
            (GlyphTier::Ascii, Self::Info) => "i",
            (GlyphTier::Ascii, Self::Success) => "+",
            (GlyphTier::Ascii, Self::Warn) => "!",
            (GlyphTier::Ascii, Self::Error) => "x",
            (_, Self::Info) => "ℹ",
            (_, Self::Success) => "✓",
            (_, Self::Warn) => "⚠",
            (_, Self::Error) => "✗",
        }
    }
}

/// A transient status-line message and the instant it stops being shown.
struct StatusMessage {
    text: String,
    severity: Severity,
    expires_at: Instant,
}

/// One command-completion to weigh against the desktop-notification policy,
/// collected during a monitor drain. The policy (enabled / threshold /
/// attended) is applied by the render coordinator, which owns the config + the
/// attached-client count.
#[derive(Debug, Clone)]
pub struct PendingNotification {
    pub window_index: usize,
    pub window_name: String,
    pub is_active_window: bool,
    pub event: CompletionEvent,
}

/// One in-band notification request (OSC 9 / OSC 777) drained from a pane,
/// collected during a monitor drain. The policy (enabled / in-band / attended)
/// is applied by the render coordinator, same split as `PendingNotification`.
#[derive(Debug, Clone)]
pub struct InbandNotification {
    pub title: String,
    pub body: String,
    pub window_index: usize,
    pub is_active_window: bool,
}

/// Outcome of one [`WindowManager::update_monitor_flags`] drain.
#[derive(Debug, Default)]
pub struct MonitorDrain {
    /// An alert-message edge fired (the caller schedules the status TTL wake).
    pub alert_edge: bool,
    /// Command-completions to consider notifying about.
    pub notifications: Vec<PendingNotification>,
    /// In-band (OSC 9 / OSC 777) notification requests to consider raising.
    pub in_band: Vec<InbandNotification>,
}

pub struct WindowManager {
    windows: Vec<Window>,
    /// Id of the active window, NOT a `Vec` index. Storing the identity means
    /// structural mutations (reorder, close) don't need to hand-fix an index at
    /// every site; `active_index()` is the single id→slot lookup for the render
    /// and layout paths.
    active: WindowId,
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
    /// The active pane's scroll offset captured at the press that began the
    /// in-flight `selection`. Click-to-reposition only fires when the press
    /// landed on the live (offset 0) view, so the press anchor is in live-grid
    /// space; re-checking only at release would miss a press-on-scrollback that
    /// was wheeled to the bottom before release.
    selection_press_scroll: u32,
    /// True when the in-flight `selection` is an explicit word/line selection
    /// (double/triple-click), so the click dead-zone must NOT treat its small
    /// span as a reposition (a two-character word like `ls` still copies).
    selection_word_line: bool,
    /// Active config shared with every pane this manager spawns. Hot reload
    /// swaps this Arc and walks the panes calling `update_config`.
    config: Arc<plexy_glass_config::Config>,
    /// Border drag-resize in progress. `None` between drags.
    resize_drag: Option<ResizeDrag>,
    /// Tab reorder drag in progress. `None` between drags.
    tab_drag: Option<TabDrag>,
    /// Pane-swap drag in progress. `None` between drags.
    pane_drag: Option<PaneDrag>,
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
    /// serve_attach polls this each iteration of its input loop
    /// and exits when true.
    pub detach_requested: bool,
    /// Previously-active window (by id), for `select_last_window`. Updated on
    /// every window switch; an id so a reorder/close never dangles it.
    last_active_window: Option<WindowId>,
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
    popup: Option<Popup>,
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
            host_cell_px(host_size),
            Arc::clone(&notify),
            death_tx.clone(),
            Arc::clone(&config),
        )?;
        Ok(Self {
            windows: vec![first],
            active: WindowId(0),
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
                program: declared::default_shell(),
                args: Vec::new(),
                env: first_spec.env,
                cwd: None,
            },
            session_cwd: None,
            death_tx,
            selection: None,
            selection_press_scroll: 0,
            selection_word_line: false,
            config,
            resize_drag: None,
            tab_drag: None,
            pane_drag: None,
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
    pub const fn marked_pane(&self) -> Option<PaneId> {
        self.marked_pane
    }

    /// (source, target) pane ids of the active pane-swap drag, if any. Read by
    /// the frame build to draw the source/target highlight.
    pub fn pane_drag_roles(&self) -> Option<(PaneId, Option<PaneId>)> {
        self.pane_drag.as_ref().map(|d| (d.source, d.target))
    }

    /// Clear every in-flight mouse gesture (selection / resize / tab / pane
    /// drag). Shared by `open_popup` (a popup swallows the in-flight Release) and
    /// client teardown (a gone client never sends its Release); either way the
    /// gesture must not linger and fire an unintended swap/resize/reorder on the
    /// NEXT click. Deliberately leaves `click_history` (multi-click timing is
    /// self-expiring and target-scoped, so a stale entry can't cross-fire).
    pub const fn reset_mouse_gestures(&mut self) {
        self.selection = None;
        self.resize_drag = None;
        self.tab_drag = None;
        self.pane_drag = None;
    }

    /// Record the physical status-bar row (or `None` to disable status-bar
    /// click routing) and the pane band's vertical offset (`0` for a bottom
    /// bar, `1` for a top bar). Called by the render coordinator each frame so
    /// mouse hit-testing stays aligned with the compositor's placement.
    pub const fn set_status_layout(&mut self, status_row: Option<u16>, pane_row_offset: u16) {
        self.status_bar_row = status_row;
        self.pane_row_offset = pane_row_offset;
    }

    /// Update the clickable-region table from the latest status snapshot.
    pub fn set_status_hits(&mut self, hits: Vec<plexy_glass_status::StatusHit>) {
        self.status_hits = hits;
    }

    /// The active overlay, if any. Read by the render coordinator and by the
    /// connection layer to decide whether to capture keys.
    pub const fn overlay(&self) -> Option<&Overlay> {
        self.overlay.as_ref()
    }

    /// Set the transient status-line message, expiring `STATUS_TTL` from now.
    /// Replaces any prior message.
    pub fn set_status_message(&mut self, text: String, severity: Severity) {
        self.status_message = Some(StatusMessage {
            text,
            severity,
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

    /// Severity of the currently-set message (a peek that does not clear). The
    /// coordinator reads this just before [`Self::take_active_message`] so it can
    /// style the bar; `Info` is a harmless default when no message is set.
    pub fn active_severity(&self) -> Severity {
        self.status_message
            .as_ref()
            .map_or(Severity::Info, |m| m.severity)
    }

    /// Whether any transient message is currently set (a peek; does not clear).
    /// `Session::handle_mouse` uses it to schedule the TTL-expiry wake for a
    /// message a mouse action set under the WM lock (the sync set path can't
    /// schedule it itself).
    pub const fn has_active_message(&self) -> bool {
        self.status_message.is_some()
    }

    /// Read-only access to the in-flight selection, if any. Used by the
    /// compositor to draw highlight cells.
    pub const fn selection(&self) -> Option<&Selection> {
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
        // Locate the window holding the dead pane. If it is already gone
        // (raced with a synchronous close), just clear a stale mark + notify.
        let Some(win_idx) = self.windows.iter().position(|w| w.pane(pane_id).is_some()) else {
            if self.marked_pane == Some(pane_id) {
                self.marked_pane = None;
            }
            self.notify.notify_one();
            return Ok(());
        };

        // Bug 1: a pane that ran an explicit command (declared `command=` /
        // `$SHELL -c`, flagged at spawn) drops to an interactive `$SHELL` in the
        // SAME slot instead of closing the window. The fallback shell spawns
        // with empty args, so it is not itself respawn-on-exit, and the user
        // later exiting it closes the window normally (respawn-once).
        let respawn = self.windows[win_idx]
            .pane(pane_id)
            .is_some_and(super::pane::Pane::respawn_shell_on_exit);
        if respawn {
            let new_id = self.alloc_pane_id();
            let program = self.default_spec.program.clone();
            let env = self.default_spec.env.clone();
            let notify = Arc::clone(&self.notify);
            let death = self.death_tx.clone();
            let config = Arc::clone(&self.config);
            match self.windows[win_idx].respawn_pane_as_shell(
                pane_id, new_id, program, env, viewport, notify, death, config,
            ) {
                Ok(()) => {
                    // The session-wide mark follows the slot too. The slot is
                    // still occupied (by the fresh shell), so re-point rather
                    // than leave a dangling reference to the dead pane.
                    if self.marked_pane == Some(pane_id) {
                        self.marked_pane = Some(new_id);
                    }
                    self.set_status_message(
                        "command exited — dropped to shell".into(),
                        Severity::Info,
                    );
                    self.notify.notify_one();
                    return Ok(());
                }
                // Spawn failed: fall through to the normal close so the window
                // doesn't wedge with a dead pane in the slot.
                Err(e) => {
                    tracing::warn!(error = %e, "respawn-shell-on-exit failed; closing pane");
                }
            }
        }

        let mut closed_idx: Option<usize> = None;
        {
            let w = &mut self.windows[win_idx];
            let outcome = w.close_pane(pane_id)?;
            if matches!(outcome, plexy_glass_mux::CloseOutcome::TreeEmpty) {
                closed_idx = Some(win_idx);
            } else {
                w.resize(viewport)?;
            }
        }
        if let Some(idx) = closed_idx {
            // Capture the closed window's id before the Vec shuffles.
            let removed_id = self.windows[idx].id;
            self.windows.remove(idx);
            // `active` is an id, so a window closing elsewhere never moves it —
            // only a closed ACTIVE window needs a new focus target. Match the
            // tmux-standard policy: focus lands on the window now at the removed
            // slot (the NEXT window), clamped to the last. The old `idx <=
            // active` decrement landed on the PREVIOUS window, so typing `exit`
            // and pressing kill-window gave opposite focus.
            if self.active == removed_id && !self.windows.is_empty() {
                let slot = idx.min(self.windows.len() - 1);
                self.active = self.windows[slot].id;
            }
            self.fixup_last_active_after_removal(removed_id);
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
    /// Returns a [`MonitorDrain`]: `alert_edge` is `true` if any status alert
    /// message was emitted this drain (so the coordinator can schedule the
    /// message's TTL-expiry repaint wake after it releases the WM lock; the
    /// message is set here under the held lock, see `set_status_message`'s
    /// deadlock note in the coordinator); `notifications` lists every window's
    /// command-completion this drain for the coordinator's desktop-notification
    /// policy to weigh (independent of the per-window `monitor-command` flag);
    /// `in_band` lists every OSC 9 / OSC 777 request drained from every pane
    /// this drain, for the same policy to weigh.
    #[must_use = "schedule the TTL wake on alert_edge and apply the notification policy"]
    pub fn update_monitor_flags(&mut self) -> MonitorDrain {
        let active = self.active_index();
        // Collect edge messages while iterating (the iterator borrows
        // `windows` mutably; `set_status_message` borrows `self`), then emit
        // after the loop. The status line is a single slot, so on simultaneous
        // edges the LAST message wins (accepted, same as any rapid succession).
        let mut message: Option<String> = None;
        // Command-completions to weigh against the notification policy (every
        // window, regardless of the per-window monitor-command flag).
        let mut notifications: Vec<PendingNotification> = Vec::new();
        let mut in_band: Vec<InbandNotification> = Vec::new();
        // Auto-named windows have an empty structural `name`; alert messages
        // must show the DERIVED name, so read the toggle once before the loop
        // (the loop borrows `self.windows` mutably).
        let auto_rename = self.config.auto_rename;
        for (i, w) in self.windows.iter_mut().enumerate() {
            let (acted, belled) = w.drain_pane_alerts();
            // Silence-timing bookkeeping updates every drain for every window
            // regardless of monitor/active state (the silence tick reads
            // `last_output`); output also resets the per-window episode latch.
            w.note_drain_output(acted);
            // Command-completion baselines advance every drain for every
            // window; the flag/edge is recorded only for a monitored non-active
            // window (background completion you can't see), but the completion
            // event is collected for EVERY window (the notification policy is
            // applied by the coordinator).
            let record_done = i != active && w.monitor_command();
            let (completion, done_edge) = w.drain_command_completion(record_done);
            if let Some(event) = completion {
                notifications.push(PendingNotification {
                    window_index: i,
                    window_name: w.display_name(auto_rename),
                    is_active_window: i == active,
                    event,
                });
            }
            // In-band (OSC 9 / OSC 777) requests are collected for EVERY
            // window too, regardless of monitor state; the coordinator applies
            // the notification policy.
            for note in w.drain_pane_notifications() {
                in_band.push(InbandNotification {
                    title: note.title,
                    body: note.body,
                    window_index: i,
                    is_active_window: i == active,
                });
            }
            if i == active {
                w.clear_alerts();
            } else {
                if acted && w.monitor_activity() && w.set_activity() {
                    message = Some(format!(
                        "activity in window {} ({})",
                        i + 1,
                        w.display_name(auto_rename)
                    ));
                }
                if belled && w.monitor_bell() && w.set_bell() {
                    message = Some(format!(
                        "bell in window {} ({})",
                        i + 1,
                        w.display_name(auto_rename)
                    ));
                }
                if let Some(exit) = done_edge {
                    let name = w.display_name(auto_rename);
                    message = Some(match exit {
                        Some(code) => format!("done in window {} ({name}): exit {code}", i + 1),
                        None => format!("done in window {} ({name})", i + 1),
                    });
                }
            }
        }
        let alert_edge = if let Some(text) = message {
            // ponytail: monitor alerts are Info; the text already says what
            // happened. Refining "done: exit N" to Warn/Error is out of scope.
            self.set_status_message(text, Severity::Info);
            true
        } else {
            false
        };
        MonitorDrain {
            alert_edge,
            notifications,
            in_band,
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
        let active = self.active_index();
        let now = Instant::now();
        let mut message: Option<String> = None;
        let auto_rename = self.config.auto_rename;
        for (i, w) in self.windows.iter_mut().enumerate() {
            if i == active {
                continue;
            }
            if w.check_silence(now) {
                message = Some(format!(
                    "silence in window {} ({})",
                    i + 1,
                    w.display_name(auto_rename)
                ));
            }
        }
        if let Some(text) = message {
            self.set_status_message(text, Severity::Info);
            true
        } else {
            false
        }
    }

    pub const fn host_size(&self) -> PtySize {
        self.host_size
    }

    pub fn viewport(&self) -> Rect {
        host_viewport(self.host_size)
    }

    pub fn active_window(&self) -> &Window {
        &self.windows[self.active_index()]
    }

    pub fn active_window_mut(&mut self) -> &mut Window {
        let idx = self.active_index();
        &mut self.windows[idx]
    }

    pub fn windows_mut(&mut self) -> &mut [Window] {
        &mut self.windows
    }

    pub fn set_active_window(&mut self, idx: usize) {
        if idx < self.windows.len() {
            self.active = self.windows[idx].id;
        }
    }

    /// Test seam: pin the program new windows/splits/popups spawn (production
    /// default is the user's `$SHELL`, which unit tests must not depend on).
    #[cfg(test)]
    pub(crate) fn set_default_program(&mut self, program: &str) {
        self.default_spec.program = program.to_string();
    }

    /// Spawn a new window using a caller-supplied spec (declared-template
    /// builds give every window's first pane its own cwd).
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
            host_cell_px(self.host_size),
            Arc::clone(&self.notify),
            self.death_tx.clone(),
            Arc::clone(&self.config),
        )?;
        self.windows.push(window);
        self.active = id;
        Ok(())
    }

    /// Split an existing window's pane at DFS index `target_dfs_idx`
    /// (declared-template builds).
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
            .ok_or_else(|| DaemonError::Io(Error::other(format!("window {window_idx} missing"))))?;
        let leaves = win.layout().dfs_leaves();
        let target_pane = *leaves.get(target_dfs_idx as usize).ok_or_else(|| {
            DaemonError::Io(Error::other(format!(
                "dfs idx {target_dfs_idx} out of range"
            )))
        })?;
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

    /// Pin a window's name (manual rename). Restore also calls this to install
    /// the persisted name, then re-applies the saved `auto_named` afterward, so
    /// pinning here does not clobber a restored auto-named window.
    pub fn set_window_name(&mut self, window_idx: usize, name: String) {
        if let Some(w) = self.windows.get_mut(window_idx) {
            w.set_manual_name(name);
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
    pub const fn popup(&self) -> Option<&Popup> {
        self.popup.as_ref()
    }

    pub const fn has_popup(&self) -> bool {
        self.popup.is_some()
    }

    /// The pane user input goes to: the floating popup's pane while one is
    /// open (it is modal and owns input), otherwise the active window's
    /// active pane. This is THE definition of "where user input goes", so
    /// keep every input-routing decision (byte routing, paste bracketing,
    /// focus events, …) on it so they cannot disagree about the target.
    pub fn input_target_pane(&self) -> Option<&Pane> {
        match &self.popup {
            Some(p) => Some(&p.pane),
            None => Some(self.active_window().active_pane()),
        }
    }

    /// The live cwd of `pane`: its OSC-7 location (`screen.cwd`), falling
    /// back to the active window's home base. The shared per-pane helper
    /// behind `popup_cwd` (active pane) and pipe-pane (the input TARGET pane,
    /// which is the popup's pane while one is open), one definition so the
    /// two cannot drift.
    pub fn pane_cwd(&self, pane: &Pane) -> Option<String> {
        pane.with_screen(|s| s.cwd.clone())
            .and_then(|url| popup::osc7_to_path(&url))
            .or_else(|| self.active_window().home_cwd.clone())
    }

    /// The cwd the popup spawns at: the active pane's live OSC-7 location,
    /// falling back to the window home base. This intentionally diverges from
    /// `split_cwd` (home base only): a popup acts on the current context.
    pub fn popup_cwd(&self) -> Option<String> {
        self.pane_cwd(self.active_window().active_pane())
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
        let size = popup::popup_pty_size(plexy_glass_mux::popup_rect(self.viewport()));
        let id = self.alloc_pane_id();
        let pane = Pane::spawn(
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
        self.reset_mouse_gestures();
        let title = command.unwrap_or_else(|| "popup".to_string());
        self.popup = Some(Popup { pane, title });
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
        if matches!(
            preset,
            LayoutPreset::MainHorizontal | LayoutPreset::MainVertical
        ) && let Some(pos) = panes.iter().position(|p| *p == win.active())
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
        let idx = self.active_index();
        self.set_window_name(idx, name);
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

    /// Move the window at `from` to position `to` (drop-to-position): remove at
    /// `from`, insert at `min(to, len-1)`. Returns `false` (no mutation) for a
    /// single window, an out-of-range `from`, or a no-op.
    pub fn move_window(&mut self, from: usize, to: usize) -> bool {
        let len = self.windows.len();
        if from >= len || len < 2 {
            return false;
        }
        let to = to.min(len - 1);
        if from == to {
            return false;
        }
        let w = self.windows.remove(from);
        self.windows.insert(to, w);
        // `active` and `last_active_window` are ids; the reorder relocates
        // windows but never changes their ids, so both still point at the right
        // window with no fixup.
        self.notify.notify_one();
        true
    }

    /// Move the window with id `id` to position `to`. No-op for an unknown id.
    pub fn move_window_by_id(&mut self, id: WindowId, to: usize) -> bool {
        let Some(from) = self.windows.iter().position(|w| w.id == id) else {
            return false;
        };
        self.move_window(from, to)
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
        w.set_manual_name(name);
        true
    }

    /// Rename the pane with `id` (stores the exact string; trimming already
    /// happened in `handle_tree`). `false` if not found.
    pub fn rename_pane_by_id(&mut self, pane: PaneId, name: String) -> bool {
        for w in &self.windows {
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
        for w in &self.windows {
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
        self.active_index()
    }

    /// Slot of the active window in `self.windows`. The ONE place the active
    /// id→index lookup lives; the render/layout slot-uses route through it.
    fn active_index(&self) -> usize {
        self.windows
            .iter()
            .position(|w| w.id == self.active)
            // invariant: `active` is always a live window's id — every path that
            // removes windows repoints it, and every add sets it to the new id.
            .expect("active is always a live window's id")
    }

    /// The current index of the window being drag-reordered, if any.
    pub fn dragging_window_idx(&self) -> Option<usize> {
        let drag = self.tab_drag.as_ref()?;
        self.windows.iter().position(|w| w.id == drag.source)
    }

    /// Switch the active window to `idx`, recording the current window as the
    /// "last active" so `select_last_window` can toggle back. No-op for an
    /// out-of-range or same index.
    fn switch_to_window(&mut self, idx: usize) {
        let Some(target) = self.windows.get(idx).map(|w| w.id) else {
            return;
        };
        if target == self.active {
            return;
        }
        self.last_active_window = Some(self.active);
        self.active = target;
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
        let cell = host_cell_px(new_size);
        for w in &mut self.windows {
            w.set_cell_px(cell);
            w.resize(viewport)?;
        }
        if let Some(p) = &self.popup {
            p.pane
                .resize(popup::popup_pty_size(plexy_glass_mux::popup_rect(viewport)))?;
        }
        self.notify.notify_one();
        Ok(())
    }

    const fn alloc_pane_id(&mut self) -> PaneId {
        let id = PaneId(self.next_pane_id);
        self.next_pane_id += 1;
        id
    }

    fn close_active_window(&mut self) {
        if !self.windows.is_empty() {
            let removed = self.active_index();
            let removed_id = self.active;
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
                // The active window was just removed; focus follows the same
                // tmux-standard policy as the death-channel path — the window
                // now at the removed slot (the next window), clamped to last.
                let slot = removed.min(self.windows.len() - 1);
                self.active = self.windows[slot].id;
                self.fixup_last_active_after_removal(removed_id);
            }
        }
        // The session ends when its last window closes; a floating popup must
        // not orphan its child (mirrors the death-channel path).
        if self.windows.is_empty() {
            self.close_popup();
        }
    }

    /// Repair `last_active_window` after the window with id `removed` is
    /// dropped: the toggle target is cleared if it *was* the removed window
    /// (its id is gone), and otherwise left alone (an id doesn't shift when the
    /// Vec does). Also clears it if it would now alias the active window
    /// (toggling to the window you are already on is meaningless).
    fn fixup_last_active_after_removal(&mut self, removed: WindowId) {
        if self.last_active_window == Some(removed) {
            self.last_active_window = None;
        }
        if self.last_active_window == Some(self.active) {
            self.last_active_window = None;
        }
    }

    pub const fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }
}
/// Host cell size in pixels (`width, height`), or `(0, 0)` when the terminal
/// reports no pixel dimensions (the emulator then uses its 10×20 fallback).
/// Threaded into each pane's PTY so children scale inline graphics to the real
/// cell box and `CSI 14/16/18t` reports are accurate.
pub(super) fn host_cell_px(host: PtySize) -> CellPx {
    let width = host.pixel_width.checked_div(host.cols).unwrap_or(0);
    let height = host.pixel_height.checked_div(host.rows).unwrap_or(0);
    CellPx { width, height }
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
