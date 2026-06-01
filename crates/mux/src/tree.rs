//! Pure model and key handler for the `choose-tree` overlay: a fully-expanded
//! session → window → pane tree the user can switch to, kill, or rename. The
//! daemon owns the state and performs the actions; this module only decides how
//! one key mutates the tree and what the caller must do next. Returns a
//! tree-local [`TreeOutcome`] (NOT `OverlayAction`), so it has no dependency on
//! the overlay enum and can be built/tested in isolation.

use crate::{Direction, Key, KeyEvent, Modifiers, PaneId, WindowId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeKind {
    Session,
    Window,
    Pane,
}

/// One node in the snapshot. Pre-order DFS: session, then its windows, then each
/// window's panes. `label` is the full display text (no indent, no marker);
/// `name` is the bare editable name; `index` is the window's 1-based index or the
/// pane's 1-based DFS index (0 for a session). Rename mutates `name` and rebuilds
/// `label` via [`window_label`]/[`pane_label`] so the optimistic row text matches
/// what a reopen would show. `is_current` marks the current session's path only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeNode {
    pub session: String,
    pub window: Option<WindowId>,
    pub pane: Option<PaneId>,
    pub depth: u8,
    pub label: String,
    pub name: String,
    pub index: u32,
    pub is_current: bool,
}

impl TreeNode {
    pub fn kind(&self) -> TreeKind {
        match (self.window, self.pane) {
            (_, Some(_)) => TreeKind::Pane,
            (Some(_), None) => TreeKind::Window,
            (None, None) => TreeKind::Session,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeMode {
    Navigate,
    ConfirmKill,
    Rename { buf: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeState {
    pub nodes: Vec<TreeNode>,
    pub selected: usize,
    pub mode: TreeMode,
}

/// What the caller (daemon connection layer) must perform after a tree key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeAction {
    Switch { session: String, window: Option<WindowId>, pane: Option<PaneId> },
    KillSession(String),
    KillWindow { session: String, window: WindowId },
    KillPane { session: String, pane: PaneId },
    RenameWindow { session: String, window: WindowId, name: String },
    RenamePane { session: String, pane: PaneId, name: String },
}

/// tree.rs-local follow-up. The daemon adapts this into `OverlayAction`/
/// `OverlayKeyResult` at the overlay boundary; keeping it local is what lets the
/// pure core build and test without touching any exhaustive overlay match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeOutcome {
    /// Key ignored; nothing changed.
    None,
    /// State changed; recompose the frame.
    Redraw,
    /// The overlay was dismissed.
    Cancel,
    /// Perform this action. For `Switch` the caller closes the overlay; for the
    /// `Kill*`/`Rename*` actions the model is already updated and the overlay
    /// stays open so the user can act on several nodes.
    Act(TreeAction),
}

/// `"{index}: {name}"`, the single source of truth for a window row's text,
/// used by the connection at snapshot time and by [`handle_tree`] on rename.
pub fn window_label(index: u32, name: &str) -> String {
    format!("{index}: {name}")
}

/// `"pane {index}"`, or `"pane {index}: {name}"` when the pane is named.
pub fn pane_label(index: u32, name: &str) -> String {
    if name.is_empty() {
        format!("pane {index}")
    } else {
        format!("pane {index}: {name}")
    }
}

/// Remove the node at `idx` and (for a session/window) its DFS subtree: the
/// contiguous following run with `depth >` the removed node's depth. Pre-order
/// DFS guarantees a subtree is a contiguous run.
fn prune_subtree(nodes: &mut Vec<TreeNode>, idx: usize) {
    if idx >= nodes.len() {
        return;
    }
    let base_depth = nodes[idx].depth;
    let mut end = idx + 1;
    while end < nodes.len() && nodes[end].depth > base_depth {
        end += 1;
    }
    nodes.drain(idx..end);
}

/// Apply one key to the tree. Pure: mutates `state` in place and returns the
/// follow-up. Every key is a no-op when `state.nodes` is empty (reachable after
/// killing the last session).
pub fn handle_tree(event: &KeyEvent, state: &mut TreeState) -> TreeOutcome {
    if state.nodes.is_empty() {
        return TreeOutcome::None;
    }
    match &state.mode {
        TreeMode::Navigate => handle_navigate(event, state),
        TreeMode::ConfirmKill => handle_confirm_kill(event, state),
        TreeMode::Rename { .. } => handle_rename(event, state),
    }
}

fn handle_navigate(event: &KeyEvent, state: &mut TreeState) -> TreeOutcome {
    let last = state.nodes.len() - 1;
    match (event.mods, event.key) {
        (m, Key::Escape) if m.is_empty() => TreeOutcome::Cancel,
        (m, Key::Arrow(Direction::Up)) if m.is_empty() => move_sel(state, false),
        (m, Key::Char('k')) if m.is_empty() => move_sel(state, false),
        (m, Key::Char('p')) if m == Modifiers::CTRL => move_sel(state, false),
        (m, Key::Arrow(Direction::Down)) if m.is_empty() => move_sel(state, true),
        (m, Key::Char('j')) if m.is_empty() => move_sel(state, true),
        (m, Key::Char('n')) if m == Modifiers::CTRL => move_sel(state, true),
        (m, Key::Home) if m.is_empty() => set_sel(state, 0),
        (m, Key::Char('g')) if m.is_empty() => set_sel(state, 0),
        (m, Key::End) if m.is_empty() => set_sel(state, last),
        // 'G' arrives as (empty, 'G') from the byte parser; accept SHIFT too.
        (m, Key::Char('G')) if m.is_empty() || m == Modifiers::SHIFT => set_sel(state, last),
        (_, Key::Enter) | (_, Key::KeypadEnter) => {
            let n = &state.nodes[state.selected];
            TreeOutcome::Act(TreeAction::Switch {
                session: n.session.clone(),
                window: n.window,
                pane: n.pane,
            })
        }
        (m, Key::Char('x')) if m.is_empty() => {
            state.mode = TreeMode::ConfirmKill;
            TreeOutcome::Redraw
        }
        (m, Key::Char('r')) if m.is_empty() => {
            let n = &state.nodes[state.selected];
            match n.kind() {
                TreeKind::Window | TreeKind::Pane => {
                    state.mode = TreeMode::Rename { buf: n.name.clone() };
                    TreeOutcome::Redraw
                }
                // Session rename is a non-goal here; the footer hides the `r` hint on
                // session nodes, so this key is never advertised there.
                TreeKind::Session => TreeOutcome::None,
            }
        }
        _ => TreeOutcome::None,
    }
}

fn handle_confirm_kill(event: &KeyEvent, state: &mut TreeState) -> TreeOutcome {
    match (event.mods, event.key) {
        (m, Key::Char('y')) if m.is_empty() => {
            let n = &state.nodes[state.selected];
            let action = match n.kind() {
                TreeKind::Session => TreeAction::KillSession(n.session.clone()),
                TreeKind::Window => TreeAction::KillWindow {
                    session: n.session.clone(),
                    // invariant: a Window node always carries a WindowId.
                    window: n.window.expect("window node has WindowId"),
                },
                TreeKind::Pane => TreeAction::KillPane {
                    session: n.session.clone(),
                    // invariant: a Pane node always carries a PaneId.
                    pane: n.pane.expect("pane node has PaneId"),
                },
            };
            prune_subtree(&mut state.nodes, state.selected);
            clamp_sel(state);
            state.mode = TreeMode::Navigate;
            TreeOutcome::Act(action)
        }
        (m, Key::Char('n')) if m.is_empty() => {
            state.mode = TreeMode::Navigate;
            TreeOutcome::Redraw
        }
        (m, Key::Escape) if m.is_empty() => {
            state.mode = TreeMode::Navigate;
            TreeOutcome::Redraw
        }
        _ => TreeOutcome::None,
    }
}

fn handle_rename(event: &KeyEvent, state: &mut TreeState) -> TreeOutcome {
    let TreeMode::Rename { buf } = &mut state.mode else {
        return TreeOutcome::None;
    };
    match (event.mods, event.key) {
        (m, Key::Escape) if m.is_empty() => {
            state.mode = TreeMode::Navigate;
            TreeOutcome::Redraw
        }
        (_, Key::Enter) | (_, Key::KeypadEnter) => {
            let trimmed = buf.trim().to_string();
            state.mode = TreeMode::Navigate;
            if trimmed.is_empty() {
                return TreeOutcome::Redraw;
            }
            let n = &mut state.nodes[state.selected];
            n.name = trimmed.clone();
            n.label = match n.kind() {
                TreeKind::Window => window_label(n.index, &trimmed),
                TreeKind::Pane => pane_label(n.index, &trimmed),
                TreeKind::Session => n.label.clone(),
            };
            match n.kind() {
                TreeKind::Window => TreeOutcome::Act(TreeAction::RenameWindow {
                    session: n.session.clone(),
                    window: n.window.expect("window node has WindowId"),
                    name: trimmed,
                }),
                TreeKind::Pane => TreeOutcome::Act(TreeAction::RenamePane {
                    session: n.session.clone(),
                    pane: n.pane.expect("pane node has PaneId"),
                    name: trimmed,
                }),
                TreeKind::Session => TreeOutcome::Redraw,
            }
        }
        (m, Key::Backspace) if m.is_empty() => {
            if buf.pop().is_some() {
                TreeOutcome::Redraw
            } else {
                TreeOutcome::None
            }
        }
        (m, Key::Char(c)) if m.is_empty() || m == Modifiers::SHIFT => {
            buf.push(c);
            TreeOutcome::Redraw
        }
        _ => TreeOutcome::None,
    }
}

fn move_sel(state: &mut TreeState, down: bool) -> TreeOutcome {
    let last = state.nodes.len() - 1;
    let new = if down {
        (state.selected + 1).min(last)
    } else {
        state.selected.saturating_sub(1)
    };
    set_sel(state, new)
}

fn set_sel(state: &mut TreeState, target: usize) -> TreeOutcome {
    let clamped = target.min(state.nodes.len().saturating_sub(1));
    if clamped != state.selected {
        state.selected = clamped;
        TreeOutcome::Redraw
    } else {
        TreeOutcome::None
    }
}

fn clamp_sel(state: &mut TreeState) {
    if state.nodes.is_empty() {
        state.selected = 0;
    } else {
        state.selected = state.selected.min(state.nodes.len() - 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(mods: Modifiers, key: Key) -> KeyEvent {
        KeyEvent::new(key, mods)
    }

    fn node(
        session: &str,
        window: Option<u32>,
        pane: Option<u32>,
        depth: u8,
        name: &str,
        index: u32,
    ) -> TreeNode {
        let label = match (window, pane) {
            (_, Some(_)) => pane_label(index, name),
            (Some(_), None) => window_label(index, name),
            (None, None) => format!("{name} — 1 win, 1 panes"),
        };
        TreeNode {
            session: session.into(),
            window: window.map(WindowId),
            pane: pane.map(PaneId),
            depth,
            label,
            name: name.into(),
            index,
            is_current: false,
        }
    }

    // session A { window 0 { pane 0, pane 1 } }, session B { window 0 { pane 0 } }
    fn sample() -> TreeState {
        TreeState {
            nodes: vec![
                node("A", None, None, 0, "A", 0),
                node("A", Some(0), None, 1, "win", 1),
                node("A", Some(0), Some(0), 2, "", 1),
                node("A", Some(0), Some(1), 2, "", 2),
                node("B", None, None, 0, "B", 0),
                node("B", Some(0), None, 1, "win", 1),
                node("B", Some(0), Some(0), 2, "", 1),
            ],
            selected: 0,
            mode: TreeMode::Navigate,
        }
    }

    #[test]
    fn labels_format() {
        assert_eq!(window_label(2, "build"), "2: build");
        assert_eq!(pane_label(3, ""), "pane 3");
        assert_eq!(pane_label(3, "logs"), "pane 3: logs");
    }

    #[test]
    fn navigation_clamps_both_ends() {
        let mut s = sample();
        assert_eq!(
            handle_tree(&ev(Modifiers::empty(), Key::Arrow(Direction::Up)), &mut s),
            TreeOutcome::None
        );
        for _ in 0..20 {
            handle_tree(&ev(Modifiers::empty(), Key::Char('j')), &mut s);
        }
        assert_eq!(s.selected, 6);
        assert_eq!(
            handle_tree(&ev(Modifiers::empty(), Key::Char('j')), &mut s),
            TreeOutcome::None
        );
        handle_tree(&ev(Modifiers::empty(), Key::Home), &mut s);
        assert_eq!(s.selected, 0);
        handle_tree(&ev(Modifiers::empty(), Key::End), &mut s);
        assert_eq!(s.selected, 6);
    }

    #[test]
    fn shifted_g_arrives_without_modifier() {
        let mut s = sample();
        assert_eq!(
            handle_tree(&ev(Modifiers::empty(), Key::Char('G')), &mut s),
            TreeOutcome::Redraw
        );
        assert_eq!(s.selected, 6);
    }

    #[test]
    fn enter_emits_switch_per_depth() {
        let mut s = sample();
        assert_eq!(
            handle_tree(&ev(Modifiers::empty(), Key::Enter), &mut s),
            TreeOutcome::Act(TreeAction::Switch { session: "A".into(), window: None, pane: None })
        );
        s.selected = 1;
        assert_eq!(
            handle_tree(&ev(Modifiers::empty(), Key::Enter), &mut s),
            TreeOutcome::Act(TreeAction::Switch {
                session: "A".into(),
                window: Some(WindowId(0)),
                pane: None
            })
        );
        s.selected = 3;
        assert_eq!(
            handle_tree(&ev(Modifiers::empty(), Key::Enter), &mut s),
            TreeOutcome::Act(TreeAction::Switch {
                session: "A".into(),
                window: Some(WindowId(0)),
                pane: Some(PaneId(1))
            })
        );
    }

    #[test]
    fn confirm_kill_prunes_subtree() {
        let mut s = sample();
        s.selected = 1; // window node with two pane children
        assert_eq!(
            handle_tree(&ev(Modifiers::empty(), Key::Char('x')), &mut s),
            TreeOutcome::Redraw
        );
        let out = handle_tree(&ev(Modifiers::empty(), Key::Char('y')), &mut s);
        assert_eq!(
            out,
            TreeOutcome::Act(TreeAction::KillWindow { session: "A".into(), window: WindowId(0) })
        );
        assert_eq!(s.nodes.len(), 4);
        assert_eq!(s.mode, TreeMode::Navigate);
    }

    #[test]
    fn confirm_kill_n_aborts() {
        let mut s = sample();
        handle_tree(&ev(Modifiers::empty(), Key::Char('x')), &mut s);
        assert_eq!(
            handle_tree(&ev(Modifiers::empty(), Key::Char('n')), &mut s),
            TreeOutcome::Redraw
        );
        assert_eq!(s.mode, TreeMode::Navigate);
        assert_eq!(s.nodes.len(), 7);
    }

    #[test]
    fn kill_session_prunes_whole_session() {
        let mut s = sample();
        s.selected = 0;
        handle_tree(&ev(Modifiers::empty(), Key::Char('x')), &mut s);
        let out = handle_tree(&ev(Modifiers::empty(), Key::Char('y')), &mut s);
        assert_eq!(out, TreeOutcome::Act(TreeAction::KillSession("A".into())));
        assert_eq!(s.nodes.len(), 3);
        assert!(s.nodes.iter().all(|n| n.session == "B"));
    }

    #[test]
    fn rename_session_is_noop() {
        let mut s = sample();
        s.selected = 0;
        assert_eq!(
            handle_tree(&ev(Modifiers::empty(), Key::Char('r')), &mut s),
            TreeOutcome::None
        );
        assert_eq!(s.mode, TreeMode::Navigate);
    }

    #[test]
    fn rename_window_edits_and_rebuilds_label() {
        let mut s = sample();
        s.selected = 1;
        handle_tree(&ev(Modifiers::empty(), Key::Char('r')), &mut s);
        assert!(matches!(s.mode, TreeMode::Rename { .. }));
        for _ in 0..3 {
            handle_tree(&ev(Modifiers::empty(), Key::Backspace), &mut s);
        }
        handle_tree(&ev(Modifiers::empty(), Key::Char('e')), &mut s);
        handle_tree(&ev(Modifiers::empty(), Key::Char('d')), &mut s);
        let out = handle_tree(&ev(Modifiers::empty(), Key::Enter), &mut s);
        assert_eq!(
            out,
            TreeOutcome::Act(TreeAction::RenameWindow {
                session: "A".into(),
                window: WindowId(0),
                name: "ed".into()
            })
        );
        assert_eq!(s.nodes[1].name, "ed");
        assert_eq!(s.nodes[1].label, "1: ed", "label keeps its index prefix");
    }

    #[test]
    fn rename_empty_is_noop_edit() {
        let mut s = sample();
        s.selected = 2; // pane
        handle_tree(&ev(Modifiers::empty(), Key::Char('r')), &mut s);
        handle_tree(&ev(Modifiers::empty(), Key::Char(' ')), &mut s);
        assert_eq!(
            handle_tree(&ev(Modifiers::empty(), Key::Enter), &mut s),
            TreeOutcome::Redraw
        );
        assert_eq!(s.mode, TreeMode::Navigate);
    }

    #[test]
    fn rename_pane_label_uses_pane_format() {
        let mut s = sample();
        s.selected = 2; // pane index 1, no name
        handle_tree(&ev(Modifiers::empty(), Key::Char('r')), &mut s);
        handle_tree(&ev(Modifiers::empty(), Key::Char('l')), &mut s);
        let out = handle_tree(&ev(Modifiers::empty(), Key::Enter), &mut s);
        assert_eq!(
            out,
            TreeOutcome::Act(TreeAction::RenamePane {
                session: "A".into(),
                pane: PaneId(0),
                name: "l".into()
            })
        );
        assert_eq!(s.nodes[2].label, "pane 1: l");
    }

    #[test]
    fn empty_tree_ignores_all_keys() {
        let mut s = TreeState { nodes: vec![], selected: 0, mode: TreeMode::Navigate };
        for key in [
            Key::Char('j'),
            Key::Char('k'),
            Key::Char('G'),
            Key::Enter,
            Key::Char('x'),
            Key::Char('r'),
        ] {
            assert_eq!(handle_tree(&ev(Modifiers::empty(), key), &mut s), TreeOutcome::None);
        }
    }

    #[test]
    fn escape_cancels() {
        let mut s = sample();
        assert_eq!(
            handle_tree(&ev(Modifiers::empty(), Key::Escape), &mut s),
            TreeOutcome::Cancel
        );
    }
}
