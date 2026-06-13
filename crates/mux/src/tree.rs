//! Pure model and key handler for the `choose-tree` overlay: a session →
//! window → pane tree the user can switch to, kill, rename, collapse
//! (`h`/`l`), or narrow with an incremental filter (`/`). The daemon owns the
//! state and performs the actions; this module only decides how one key
//! mutates the tree and what the caller must do next. Returns a tree-local
//! [`TreeOutcome`] (NOT `OverlayAction`), so it has no dependency on the
//! overlay enum and can be built/tested in isolation.

use std::collections::HashSet;

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

    /// The node's collapse identity. Sessions and windows are collapsible;
    /// panes have no children and therefore no key.
    pub fn key(&self) -> Option<NodeKey> {
        match self.kind() {
            TreeKind::Session => Some(NodeKey::Session(self.session.clone())),
            TreeKind::Window => self
                .window
                .map(|window| NodeKey::Window { session: self.session.clone(), window }),
            TreeKind::Pane => None,
        }
    }
}

/// Identity of a collapsible node, derived from the node's fields (never the
/// row index) so it survives in-overlay mutations and subtree prunes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NodeKey {
    Session(String),
    Window { session: String, window: WindowId },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TreeMode {
    #[default]
    Navigate,
    ConfirmKill,
    Rename { buf: String },
    /// Incremental filter entry; the live pattern is [`TreeState::filter`].
    Filter,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TreeState {
    pub nodes: Vec<TreeNode>,
    pub selected: usize,
    pub mode: TreeMode,
    /// Collapsed session/window nodes. Pruning a collapsed node (kill) leaves its
    /// key behind, which is a benign leak: a stale key never matches a live node
    /// again and the set dies with the overlay.
    pub collapsed: HashSet<NodeKey>,
    /// Case-insensitive substring filter over row labels; empty = no filter.
    pub filter: String,
}

impl TreeState {
    /// A fresh fully-expanded, unfiltered tree over `nodes`.
    pub fn new(nodes: Vec<TreeNode>) -> Self {
        Self { nodes, ..Self::default() }
    }

    /// Indices of the currently visible rows, in order. A row is visible iff
    /// no ancestor is collapsed AND (the filter is empty, or its label matches
    /// the filter as a case-insensitive substring, or a descendant's label does,
    /// so that ancestors of matches stay visible and the path is preserved).
    pub fn visible_indices(&self) -> Vec<usize> {
        let n = self.nodes.len();
        let mut keep = vec![true; n];
        if !self.filter.is_empty() {
            let needle = self.filter.to_lowercase();
            let matched: Vec<bool> = self
                .nodes
                .iter()
                .map(|nd| nd.label.to_lowercase().contains(&needle))
                .collect();
            for i in 0..n {
                keep[i] = matched[i] || {
                    // Any matching descendant: the contiguous deeper run after i.
                    let depth = self.nodes[i].depth;
                    (i + 1..n)
                        .take_while(|&j| self.nodes[j].depth > depth)
                        .any(|j| matched[j])
                };
            }
        }
        let mut out = Vec::new();
        // Ancestor stack: (depth, is that ancestor collapsed?).
        let mut stack: Vec<(u8, bool)> = Vec::new();
        for (i, nd) in self.nodes.iter().enumerate() {
            while stack.last().is_some_and(|&(d, _)| d >= nd.depth) {
                stack.pop();
            }
            if keep[i] && !stack.iter().any(|&(_, collapsed)| collapsed) {
                out.push(i);
            }
            let collapsed_here = nd.key().is_some_and(|k| self.collapsed.contains(&k));
            stack.push((nd.depth, collapsed_here));
        }
        out
    }
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
    /// Rename a session. Unlike the window/pane renames the tree is NOT
    /// optimistically mutated: the daemon commits on success only (see
    /// [`handle_rename`]'s session arm).
    RenameSession { old: String, new: String },
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

/// `"{name} — {windows} win, {panes} panes"`, the single source of truth for a
/// session row's text. Extracted verbatim from the daemon's `build_tree_nodes`
/// (which delegates here) so a rename re-stamp produces the exact label a
/// reopen would show.
pub fn session_label(name: &str, windows: usize, panes: usize) -> String {
    format!("{name} \u{2014} {windows} win, {panes} panes")
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
        TreeMode::Filter => handle_filter(event, state),
    }
}

fn handle_navigate(event: &KeyEvent, state: &mut TreeState) -> TreeOutcome {
    let vis = state.visible_indices();
    // Selection always references a visible row except when the filter matches
    // nothing, so the `pos.is_some()` guards keep actions unreachable then.
    let pos = vis.iter().position(|&i| i == state.selected);
    let last = vis.len().saturating_sub(1);
    match (event.mods, event.key) {
        (m, Key::Escape) if m.is_empty() => TreeOutcome::Cancel,
        (m, Key::Arrow(Direction::Up)) if m.is_empty() => move_sel(state, &vis, pos, false),
        (m, Key::Char('k')) if m.is_empty() => move_sel(state, &vis, pos, false),
        (m, Key::Char('p')) if m == Modifiers::CTRL => move_sel(state, &vis, pos, false),
        (m, Key::Arrow(Direction::Down)) if m.is_empty() => move_sel(state, &vis, pos, true),
        (m, Key::Char('j')) if m.is_empty() => move_sel(state, &vis, pos, true),
        (m, Key::Char('n')) if m == Modifiers::CTRL => move_sel(state, &vis, pos, true),
        (m, Key::Home) if m.is_empty() => select_visible(state, &vis, 0),
        (m, Key::Char('g')) if m.is_empty() => select_visible(state, &vis, 0),
        (m, Key::End) if m.is_empty() => select_visible(state, &vis, last),
        // 'G' arrives as (empty, 'G') from the byte parser; accept SHIFT too.
        (m, Key::Char('G')) if m.is_empty() || m == Modifiers::SHIFT => {
            select_visible(state, &vis, last)
        }
        (m, Key::Char('h') | Key::Arrow(Direction::Left)) if m.is_empty() && pos.is_some() => {
            collapse_selected(state)
        }
        (m, Key::Char('l') | Key::Arrow(Direction::Right)) if m.is_empty() && pos.is_some() => {
            expand_selected(state)
        }
        (m, Key::Char('/')) if m.is_empty() => {
            state.mode = TreeMode::Filter;
            TreeOutcome::Redraw
        }
        (_, Key::Enter | Key::KeypadEnter) if pos.is_some() => {
            let n = &state.nodes[state.selected];
            TreeOutcome::Act(TreeAction::Switch {
                session: n.session.clone(),
                window: n.window,
                pane: n.pane,
            })
        }
        (m, Key::Char('x')) if m.is_empty() && pos.is_some() => {
            state.mode = TreeMode::ConfirmKill;
            TreeOutcome::Redraw
        }
        (m, Key::Char('r')) if m.is_empty() && pos.is_some() => {
            // All kinds, sessions included; the edit buffer is primed with the
            // bare current name (same as window/pane rename).
            let n = &state.nodes[state.selected];
            state.mode = TreeMode::Rename { buf: n.name.clone() };
            TreeOutcome::Redraw
        }
        _ => TreeOutcome::None,
    }
}

/// `h`/`Left`: collapse the selected session/window. On a pane row, fold its
/// WINDOW and move the selection to that window row (vim-ish "fold up").
/// Collapsing an already-collapsed node is a no-op.
fn collapse_selected(state: &mut TreeState) -> TreeOutcome {
    let n = &state.nodes[state.selected];
    match n.kind() {
        TreeKind::Session | TreeKind::Window => {
            // invariant: session/window nodes always have a key.
            let key = n.key().expect("session/window node has a NodeKey");
            if state.collapsed.insert(key) {
                TreeOutcome::Redraw
            } else {
                TreeOutcome::None
            }
        }
        TreeKind::Pane => {
            let session = n.session.clone();
            // invariant: a Pane node always carries its parent WindowId.
            let window = n.window.expect("pane node has WindowId");
            state.collapsed.insert(NodeKey::Window { session: session.clone(), window });
            // Land on the window row: the nearest preceding row of that window.
            let parent = (0..state.selected).rev().find(|&i| {
                let p = &state.nodes[i];
                p.kind() == TreeKind::Window && p.window == Some(window) && p.session == session
            });
            match parent {
                Some(p) => state.selected = p,
                None => clamp_sel(state),
            }
            TreeOutcome::Redraw
        }
    }
}

/// `l`/`Right`: expand the selected session/window; no-op on panes and on
/// nodes that are not collapsed.
fn expand_selected(state: &mut TreeState) -> TreeOutcome {
    match state.nodes[state.selected].key() {
        Some(key) if state.collapsed.remove(&key) => TreeOutcome::Redraw,
        _ => TreeOutcome::None,
    }
}

/// Filter-entry mode: printables append to the live pattern (the view narrows
/// per keystroke), Backspace pops, Enter returns to Navigate keeping the
/// filter, Esc returns to Navigate clearing it.
fn handle_filter(event: &KeyEvent, state: &mut TreeState) -> TreeOutcome {
    match (event.mods, event.key) {
        (m, Key::Escape) if m.is_empty() => {
            state.filter.clear();
            state.mode = TreeMode::Navigate;
            clamp_sel(state);
            TreeOutcome::Redraw
        }
        (_, Key::Enter | Key::KeypadEnter) => {
            state.mode = TreeMode::Navigate;
            TreeOutcome::Redraw
        }
        (m, Key::Backspace) if m.is_empty() => {
            if state.filter.pop().is_some() {
                clamp_sel(state);
                TreeOutcome::Redraw
            } else {
                TreeOutcome::None
            }
        }
        (m, Key::Char(c)) if m.is_empty() || m == Modifiers::SHIFT => {
            state.filter.push(c);
            clamp_sel(state);
            TreeOutcome::Redraw
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
            match n.kind() {
                TreeKind::Window => {
                    n.name = trimmed.clone();
                    n.label = window_label(n.index, &trimmed);
                    TreeOutcome::Act(TreeAction::RenameWindow {
                        session: n.session.clone(),
                        // invariant: a Window node always carries a WindowId.
                        window: n.window.expect("window node has WindowId"),
                        name: trimmed,
                    })
                }
                TreeKind::Pane => {
                    n.name = trimmed.clone();
                    n.label = pane_label(n.index, &trimmed);
                    TreeOutcome::Act(TreeAction::RenamePane {
                        session: n.session.clone(),
                        // invariant: a Pane node always carries a PaneId.
                        pane: n.pane.expect("pane node has PaneId"),
                        name: trimmed,
                    })
                }
                // Session rename does NOT optimistically mutate the tree,
                // unlike window/pane: the session name is stamped on every
                // descendant row (`node.session`) and inside collapsed
                // `NodeKey`s, so the commit happens daemon-side ON SUCCESS
                // ONLY (re-stamp the row label via `session_label`, rewrite
                // descendants, re-key collapsed entries). On failure there is
                // nothing to revert.
                TreeKind::Session => {
                    if trimmed == n.name {
                        // Unchanged is a no-op: the registry would reject
                        // old == new as a name collision.
                        return TreeOutcome::Redraw;
                    }
                    TreeOutcome::Act(TreeAction::RenameSession {
                        old: n.session.clone(),
                        new: trimmed,
                    })
                }
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

/// Move the selection one step over the VISIBLE rows. `pos` is the selection's
/// current position in `vis` (None when it is hidden, in which case we fall
/// back to the first visible row).
fn move_sel(state: &mut TreeState, vis: &[usize], pos: Option<usize>, down: bool) -> TreeOutcome {
    let target = match pos {
        Some(p) if down => (p + 1).min(vis.len().saturating_sub(1)),
        Some(p) => p.saturating_sub(1),
        None => 0,
    };
    select_visible(state, vis, target)
}

/// Select the row at visible position `pos` (clamped to the last visible row).
/// No-op when nothing is visible or the selection does not move.
fn select_visible(state: &mut TreeState, vis: &[usize], pos: usize) -> TreeOutcome {
    let Some(&idx) = vis.get(pos.min(vis.len().saturating_sub(1))) else {
        return TreeOutcome::None;
    };
    if idx != state.selected {
        state.selected = idx;
        TreeOutcome::Redraw
    } else {
        TreeOutcome::None
    }
}

/// Clamp the selection to a visible row: keep it if it already is one, else
/// the nearest visible row at or before it, else the first visible row. With
/// nothing visible (a filter matching no row) the selection parks at 0; action
/// keys are guarded on visibility so a hidden row can never be acted on.
fn clamp_sel(state: &mut TreeState) {
    let vis = state.visible_indices();
    if vis.contains(&state.selected) {
        return;
    }
    match vis.iter().rev().find(|&&i| i <= state.selected).or(vis.first()) {
        Some(&i) => state.selected = i,
        None => state.selected = 0,
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
            (None, None) => session_label(name, 1, 1),
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

    // session A { window "win" { pane 1, pane 2 } }, session B { window "beta" { pane 1 } }
    fn sample() -> TreeState {
        TreeState {
            nodes: vec![
                node("A", None, None, 0, "A", 0),
                node("A", Some(0), None, 1, "win", 1),
                node("A", Some(0), Some(0), 2, "", 1),
                node("A", Some(0), Some(1), 2, "", 2),
                node("B", None, None, 0, "B", 0),
                node("B", Some(0), None, 1, "beta", 1),
                node("B", Some(0), Some(0), 2, "", 1),
            ],
            ..TreeState::default()
        }
    }

    #[test]
    fn labels_format() {
        assert_eq!(window_label(2, "build"), "2: build");
        assert_eq!(pane_label(3, ""), "pane 3");
        assert_eq!(pane_label(3, "logs"), "pane 3: logs");
    }

    #[test]
    fn session_label_matches_daemon_format() {
        // Format parity with what the daemon's `build_tree_nodes` built before
        // this helper existed: `format!("{} \u{2014} {} win, {} panes", ...)`.
        assert_eq!(session_label("main", 1, 2), "main \u{2014} 1 win, 2 panes");
        assert_eq!(session_label("main", 1, 2), "main — 1 win, 2 panes");
    }

    // ── collapse / expand ────────────────────────────────────────────────────

    #[test]
    fn collapse_session_hides_descendants_and_expand_restores() {
        let mut s = sample();
        assert_eq!(handle_tree(&ev(Modifiers::empty(), Key::Char('h')), &mut s), TreeOutcome::Redraw);
        assert_eq!(s.visible_indices(), vec![0, 4, 5, 6]);
        assert!(s.collapsed.contains(&NodeKey::Session("A".into())));
        // Already collapsed → no-op.
        assert_eq!(handle_tree(&ev(Modifiers::empty(), Key::Char('h')), &mut s), TreeOutcome::None);
        assert_eq!(handle_tree(&ev(Modifiers::empty(), Key::Char('l')), &mut s), TreeOutcome::Redraw);
        assert_eq!(s.visible_indices(), vec![0, 1, 2, 3, 4, 5, 6]);
        assert!(s.collapsed.is_empty());
    }

    #[test]
    fn collapse_window_hides_panes_via_left_arrow() {
        let mut s = sample();
        s.selected = 1;
        assert_eq!(
            handle_tree(&ev(Modifiers::empty(), Key::Arrow(Direction::Left)), &mut s),
            TreeOutcome::Redraw
        );
        assert_eq!(s.visible_indices(), vec![0, 1, 4, 5, 6]);
        assert!(s.collapsed.contains(&NodeKey::Window { session: "A".into(), window: WindowId(0) }));
        // Expand via Right.
        assert_eq!(
            handle_tree(&ev(Modifiers::empty(), Key::Arrow(Direction::Right)), &mut s),
            TreeOutcome::Redraw
        );
        assert_eq!(s.visible_indices().len(), 7);
    }

    #[test]
    fn pane_h_folds_its_window_and_moves_selection_to_it() {
        let mut s = sample();
        s.selected = 2; // pane under A's window
        assert_eq!(handle_tree(&ev(Modifiers::empty(), Key::Char('h')), &mut s), TreeOutcome::Redraw);
        assert_eq!(s.selected, 1, "selection moved to the folded window row");
        assert_eq!(s.visible_indices(), vec![0, 1, 4, 5, 6]);
    }

    #[test]
    fn expand_on_pane_is_noop() {
        let mut s = sample();
        s.selected = 2;
        assert_eq!(handle_tree(&ev(Modifiers::empty(), Key::Char('l')), &mut s), TreeOutcome::None);
    }

    #[test]
    fn navigation_moves_over_visible_rows_only() {
        let mut s = sample();
        s.selected = 1;
        handle_tree(&ev(Modifiers::empty(), Key::Char('h')), &mut s); // fold A's window
        // j from the window skips its hidden panes straight to session B.
        handle_tree(&ev(Modifiers::empty(), Key::Char('j')), &mut s);
        assert_eq!(s.selected, 4);
        handle_tree(&ev(Modifiers::empty(), Key::Char('k')), &mut s);
        assert_eq!(s.selected, 1);
        handle_tree(&ev(Modifiers::empty(), Key::Char('G')), &mut s);
        assert_eq!(s.selected, 6, "G → last visible");
        handle_tree(&ev(Modifiers::empty(), Key::Char('g')), &mut s);
        assert_eq!(s.selected, 0, "g → first visible");
        handle_tree(&ev(Modifiers::empty(), Key::End), &mut s);
        assert_eq!(s.selected, 6);
        handle_tree(&ev(Modifiers::empty(), Key::Home), &mut s);
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn ctrl_n_p_navigate_like_j_k() {
        let mut s = sample();
        s.selected = 0;
        handle_tree(&ev(Modifiers::CTRL, Key::Char('n')), &mut s);
        assert_eq!(s.selected, 1, "Ctrl+n moves down like j");
        handle_tree(&ev(Modifiers::CTRL, Key::Char('p')), &mut s);
        assert_eq!(s.selected, 0, "Ctrl+p moves up like k");
    }

    // ── filter ──────────────────────────────────────────────────────────────

    fn type_str(s: &mut TreeState, text: &str) {
        for c in text.chars() {
            handle_tree(&ev(Modifiers::empty(), Key::Char(c)), s);
        }
    }

    #[test]
    fn filter_keeps_matches_and_their_ancestors() {
        let mut s = sample();
        handle_tree(&ev(Modifiers::empty(), Key::Char('/')), &mut s);
        assert_eq!(s.mode, TreeMode::Filter);
        type_str(&mut s, "pane 2");
        // "pane 2" matches only A's second pane; its window + session stay.
        assert_eq!(s.visible_indices(), vec![0, 1, 3]);
    }

    #[test]
    fn collapse_suppresses_a_filter_match_under_it() {
        // Collapse + filter compete: a row whose label matches the filter but
        // whose ancestor is collapsed stays HIDDEN (collapse wins).
        let mut s = sample();
        s.selected = 0;
        handle_tree(&ev(Modifiers::empty(), Key::Char('h')), &mut s); // collapse session A
        handle_tree(&ev(Modifiers::empty(), Key::Char('/')), &mut s);
        type_str(&mut s, "win"); // matches index 1 (A's window), a descendant of A
        assert!(
            !s.visible_indices().contains(&1),
            "a filter match under a collapsed ancestor must stay hidden"
        );
        // Expand A and the filter match reappears.
        handle_tree(&ev(Modifiers::empty(), Key::Enter), &mut s); // keep filter, → Navigate
        s.selected = 0;
        handle_tree(&ev(Modifiers::empty(), Key::Char('l')), &mut s); // expand A
        assert!(
            s.visible_indices().contains(&1),
            "expanding the ancestor reveals the filter match"
        );
    }

    #[test]
    fn filter_is_case_insensitive_and_narrows_live() {
        let mut s = sample();
        handle_tree(&ev(Modifiers::empty(), Key::Char('/')), &mut s);
        type_str(&mut s, "BET");
        // Each keystroke re-narrows; "BET" matches "1: beta" only.
        assert_eq!(s.visible_indices(), vec![4, 5]);
        assert_eq!(s.selected, 4, "selection clamped to a visible row");
        // A non-matching key empties the view; Backspace restores it live.
        type_str(&mut s, "x");
        assert!(s.visible_indices().is_empty());
        handle_tree(&ev(Modifiers::empty(), Key::Backspace), &mut s);
        assert_eq!(s.visible_indices(), vec![4, 5]);
    }

    #[test]
    fn filter_enter_keeps_filter_esc_clears_it() {
        let mut s = sample();
        handle_tree(&ev(Modifiers::empty(), Key::Char('/')), &mut s);
        type_str(&mut s, "beta");
        handle_tree(&ev(Modifiers::empty(), Key::Enter), &mut s);
        assert_eq!(s.mode, TreeMode::Navigate);
        assert_eq!(s.filter, "beta", "Enter keeps the filter");
        assert_eq!(s.visible_indices(), vec![4, 5]);
        handle_tree(&ev(Modifiers::empty(), Key::Char('/')), &mut s);
        handle_tree(&ev(Modifiers::empty(), Key::Escape), &mut s);
        assert_eq!(s.mode, TreeMode::Navigate);
        assert!(s.filter.is_empty(), "Esc clears the filter");
        assert_eq!(s.visible_indices().len(), 7);
    }

    #[test]
    fn actions_under_filter_hit_the_selected_visible_row() {
        let mut s = sample();
        handle_tree(&ev(Modifiers::empty(), Key::Char('/')), &mut s);
        type_str(&mut s, "beta");
        handle_tree(&ev(Modifiers::empty(), Key::Enter), &mut s);
        // Visible: [4 (B), 5 (1: beta)]; selection clamped to 4; j lands on 5.
        handle_tree(&ev(Modifiers::empty(), Key::Char('j')), &mut s);
        assert_eq!(s.selected, 5);
        handle_tree(&ev(Modifiers::empty(), Key::Char('x')), &mut s);
        let out = handle_tree(&ev(Modifiers::empty(), Key::Char('y')), &mut s);
        assert_eq!(
            out,
            TreeOutcome::Act(TreeAction::KillWindow { session: "B".into(), window: WindowId(0) })
        );
    }

    #[test]
    fn navigation_visible_aware_under_filter() {
        let mut s = sample();
        handle_tree(&ev(Modifiers::empty(), Key::Char('/')), &mut s);
        type_str(&mut s, "beta");
        handle_tree(&ev(Modifiers::empty(), Key::Enter), &mut s);
        handle_tree(&ev(Modifiers::empty(), Key::Char('G')), &mut s);
        assert_eq!(s.selected, 5, "G → last visible under filter");
        handle_tree(&ev(Modifiers::empty(), Key::Char('g')), &mut s);
        assert_eq!(s.selected, 4, "g → first visible under filter");
    }

    #[test]
    fn post_kill_clamp_lands_on_a_visible_row() {
        let mut s = sample();
        s.selected = 5;
        handle_tree(&ev(Modifiers::empty(), Key::Char('h')), &mut s); // fold B's window
        s.selected = 4; // session B
        handle_tree(&ev(Modifiers::empty(), Key::Char('x')), &mut s);
        handle_tree(&ev(Modifiers::empty(), Key::Char('y')), &mut s); // kill B
        assert_eq!(s.nodes.len(), 4);
        assert_eq!(s.selected, 3, "clamped to the nearest visible row");
        assert!(s.visible_indices().contains(&s.selected));
    }

    // ── session rename ──────────────────────────────────────────────────────

    #[test]
    fn session_r_enters_rename_primed_and_emits_action_without_mutation() {
        let mut s = sample();
        let before = s.nodes.clone();
        assert_eq!(handle_tree(&ev(Modifiers::empty(), Key::Char('r')), &mut s), TreeOutcome::Redraw);
        assert_eq!(s.mode, TreeMode::Rename { buf: "A".into() }, "primed with the session name");
        handle_tree(&ev(Modifiers::empty(), Key::Backspace), &mut s);
        type_str(&mut s, "Z");
        let out = handle_tree(&ev(Modifiers::empty(), Key::Enter), &mut s);
        assert_eq!(
            out,
            TreeOutcome::Act(TreeAction::RenameSession { old: "A".into(), new: "Z".into() })
        );
        assert_eq!(s.nodes, before, "tree NOT optimistically mutated for a session rename");
        assert_eq!(s.mode, TreeMode::Navigate);
    }

    #[test]
    fn session_rename_empty_or_unchanged_is_noop() {
        let mut s = sample();
        // Empty input: same as window/pane rename, a no-op edit.
        handle_tree(&ev(Modifiers::empty(), Key::Char('r')), &mut s);
        handle_tree(&ev(Modifiers::empty(), Key::Backspace), &mut s);
        assert_eq!(handle_tree(&ev(Modifiers::empty(), Key::Enter), &mut s), TreeOutcome::Redraw);
        // Unchanged: no action (the registry would reject old == new).
        handle_tree(&ev(Modifiers::empty(), Key::Char('r')), &mut s);
        assert_eq!(handle_tree(&ev(Modifiers::empty(), Key::Enter), &mut s), TreeOutcome::Redraw);
        assert_eq!(s.mode, TreeMode::Navigate);
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
        let mut s = TreeState::default();
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
