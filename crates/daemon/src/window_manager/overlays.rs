use super::{COMMAND_HISTORY_CAP, WindowManager};
use plexy_glass_mux::{
    BufferAction, BufferEntry, BufferOutcome, BufferPickerState, KeyEvent, Overlay, OverlayAction,
    OverlayHandler, PickerEntry, RenameTarget, TreeAction, TreeMode, TreeNode, TreeOutcome,
    TreeState, handle_buffers, handle_tree,
};

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

impl WindowManager {
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
}
