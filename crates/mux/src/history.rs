//! Pure core for the structured history palette: a flat, filterable finder over
//! command blocks across all sessions. The daemon assembles the entries (a
//! registry walk) and adapts [`HistoryOutcome`] to its `OverlayKeyResult`; this
//! core has no daemon dependency, so it builds and tests standalone (like
//! `tree.rs`). Fuzzy-finder input model: printables type into the filter,
//! arrows / Ctrl-P/N move, Enter jumps, Esc cancels.

use crate::{Direction, Key, KeyEvent, Modifiers, PaneId, WindowId};

/// One block in the palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub session: String,
    pub window: WindowId,
    pub window_idx: u32,
    pub pane: PaneId,
    pub prompt_line: u32,
    pub command: String,
    pub exit: Option<i32>,
    pub duration: Option<u32>,
    /// Pre-lowercased "command\noutput" (output capped), the filter haystack.
    pub haystack: String,
}

/// Where a chosen block lives, plus its command for the jump-time drift re-find.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryTarget {
    pub session: String,
    pub window: WindowId,
    pub pane: PaneId,
    pub prompt_line: u32,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryState {
    /// Pre-sorted: current pane first, newest-first within each pane.
    pub entries: Vec<HistoryEntry>,
    /// Index into `entries`, kept on a VISIBLE row by every mutator.
    pub selected: usize,
    /// Live query (original case; matched lowercased against `haystack`).
    pub filter: String,
}

/// history.rs-local follow-up; the daemon adapts it to `OverlayKeyResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryOutcome {
    None,
    Redraw,
    Cancel,
    Jump(HistoryTarget),
}

impl HistoryState {
    pub const fn new(entries: Vec<HistoryEntry>) -> Self {
        Self {
            entries,
            selected: 0,
            filter: String::new(),
        }
    }

    /// Indices of entries whose haystack contains the (lowercased) filter,
    /// preserving the pre-sorted order. Empty filter = all.
    pub fn visible_indices(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.entries.len()).collect();
        }
        let needle = self.filter.to_lowercase();
        (0..self.entries.len())
            .filter(|&i| self.entries[i].haystack.contains(&needle))
            .collect()
    }

    /// Keep `selected` on a visible row after the visible set changes.
    fn clamp_to_visible(&mut self, vis: &[usize]) {
        if !vis.is_empty() && !vis.contains(&self.selected) {
            self.selected = vis[0];
        }
    }
}

/// Apply one key. No-op on an empty entry list (every action is guarded by a
/// visible row existing, so an empty result set can only `Cancel`).
pub fn handle_history(event: &KeyEvent, state: &mut HistoryState) -> HistoryOutcome {
    let vis = state.visible_indices();
    let pos = vis.iter().position(|&i| i == state.selected);
    let last = vis.len().saturating_sub(1);
    match (event.mods, event.key) {
        (m, Key::Escape) if m.is_empty() => HistoryOutcome::Cancel,
        (m, Key::Arrow(Direction::Up)) if m.is_empty() => move_sel(state, &vis, pos, false),
        (m, Key::Char('p')) if m == Modifiers::CTRL => move_sel(state, &vis, pos, false),
        (m, Key::Arrow(Direction::Down)) if m.is_empty() => move_sel(state, &vis, pos, true),
        (m, Key::Char('n')) if m == Modifiers::CTRL => move_sel(state, &vis, pos, true),
        (m, Key::Home) if m.is_empty() => select_visible(state, &vis, 0),
        (m, Key::End) if m.is_empty() => select_visible(state, &vis, last),
        (_, Key::Enter | Key::KeypadEnter) if pos.is_some() => {
            let e = &state.entries[state.selected];
            HistoryOutcome::Jump(HistoryTarget {
                session: e.session.clone(),
                window: e.window,
                pane: e.pane,
                prompt_line: e.prompt_line,
                command: e.command.clone(),
            })
        }
        (m, Key::Backspace) if m.is_empty() => {
            if state.filter.pop().is_some() {
                let vis = state.visible_indices();
                state.clamp_to_visible(&vis);
                HistoryOutcome::Redraw
            } else {
                HistoryOutcome::None
            }
        }
        (m, Key::Char(c)) if m.is_empty() || m == Modifiers::SHIFT => {
            state.filter.push(c);
            let vis = state.visible_indices();
            state.clamp_to_visible(&vis);
            HistoryOutcome::Redraw
        }
        _ => HistoryOutcome::None,
    }
}

fn move_sel(
    state: &mut HistoryState,
    vis: &[usize],
    pos: Option<usize>,
    down: bool,
) -> HistoryOutcome {
    if vis.is_empty() {
        return HistoryOutcome::None;
    }
    let cur = pos.unwrap_or(0);
    let next = if down {
        (cur + 1).min(vis.len() - 1)
    } else {
        cur.saturating_sub(1)
    };
    state.selected = vis[next];
    HistoryOutcome::Redraw
}

fn select_visible(state: &mut HistoryState, vis: &[usize], idx: usize) -> HistoryOutcome {
    if let Some(&i) = vis.get(idx) {
        state.selected = i;
        HistoryOutcome::Redraw
    } else {
        HistoryOutcome::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(session: &str, cmd: &str, out: &str, line: u32) -> HistoryEntry {
        HistoryEntry {
            session: session.into(),
            window: WindowId(0),
            window_idx: 0,
            pane: PaneId(0),
            prompt_line: line,
            command: cmd.into(),
            exit: Some(0),
            duration: None,
            haystack: format!("{cmd}\n{out}").to_lowercase(),
        }
    }
    fn key(k: Key) -> KeyEvent {
        KeyEvent::plain(k)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(Key::Char(c), Modifiers::CTRL)
    }
    fn chr(c: char) -> KeyEvent {
        KeyEvent::plain(Key::Char(c))
    }

    fn state() -> HistoryState {
        HistoryState::new(vec![
            entry("api", "docker compose up", "started", 10),
            entry("web", "cargo test", "connection refused", 4),
        ])
    }

    #[test]
    fn visible_filters_command_and_output_case_insensitive() {
        let mut s = state();
        s.filter = "REFUSED".into();
        assert_eq!(
            s.visible_indices(),
            vec![1],
            "matches output, case-insensitive"
        );
        s.filter = "docker".into();
        assert_eq!(s.visible_indices(), vec![0], "matches command");
        s.filter = "zzz".into();
        assert!(s.visible_indices().is_empty());
    }

    #[test]
    fn typing_filters_and_enter_jumps() {
        let mut s = state();
        for c in "cargo".chars() {
            assert_eq!(handle_history(&chr(c), &mut s), HistoryOutcome::Redraw);
        }
        assert_eq!(s.selected, 1, "selection clamped to the only visible row");
        match handle_history(&key(Key::Enter), &mut s) {
            HistoryOutcome::Jump(t) => {
                assert_eq!(t.command, "cargo test");
                assert_eq!(t.session, "web");
            }
            other => panic!("expected Jump, got {other:?}"),
        }
    }

    #[test]
    fn enter_with_empty_result_is_none() {
        let mut s = state();
        s.filter = "zzz".into();
        assert_eq!(
            handle_history(&key(Key::Enter), &mut s),
            HistoryOutcome::None
        );
    }

    #[test]
    fn esc_cancels() {
        let mut s = state();
        assert_eq!(
            handle_history(&key(Key::Escape), &mut s),
            HistoryOutcome::Cancel
        );
    }

    #[test]
    fn arrows_and_ctrl_np_move_within_visible() {
        let mut s = state();
        assert_eq!(
            handle_history(&key(Key::Arrow(Direction::Down)), &mut s),
            HistoryOutcome::Redraw
        );
        assert_eq!(s.selected, 1);
        handle_history(&key(Key::Arrow(Direction::Down)), &mut s); // clamp at end
        assert_eq!(s.selected, 1);
        assert_eq!(handle_history(&ctrl('p'), &mut s), HistoryOutcome::Redraw);
        assert_eq!(s.selected, 0, "Ctrl-P moves up");
        assert_eq!(handle_history(&ctrl('n'), &mut s), HistoryOutcome::Redraw);
        assert_eq!(s.selected, 1, "Ctrl-N moves down");
    }

    #[test]
    fn home_and_end_jump_to_ends() {
        let mut s = state();
        // Start at 0; End → last visible (1); Home → first visible (0).
        assert_eq!(
            handle_history(&key(Key::End), &mut s),
            HistoryOutcome::Redraw
        );
        assert_eq!(s.selected, 1);
        assert_eq!(
            handle_history(&key(Key::Home), &mut s),
            HistoryOutcome::Redraw
        );
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn backspace_pops_filter_and_reclamps() {
        let mut s = state();
        s.filter = "cargo".into();
        s.selected = 1;
        // pop to "carg" still matches only entry 1.
        assert_eq!(
            handle_history(&key(Key::Backspace), &mut s),
            HistoryOutcome::Redraw
        );
        assert_eq!(s.filter, "carg");
        assert_eq!(s.selected, 1);
    }
}
