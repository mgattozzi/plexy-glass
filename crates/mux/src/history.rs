//! Pure core for the structured history palette: a flat, filterable finder over
//! command blocks across all sessions. The daemon assembles the entries (a
//! registry walk) and adapts [`HistoryOutcome`] to its `OverlayKeyResult`; this
//! core has no daemon dependency, so it builds and tests standalone (like
//! `tree.rs`). Built on the shared `finder` core: printables type into the
//! filter (resetting selection to the top), arrows / Ctrl-k / Ctrl-j move,
//! Ctrl-U clears, Enter jumps, Esc cancels.

use crate::finder::{self, FilterList, FinderKey};
use crate::{KeyEvent, PaneId, WindowId};

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
    /// Filter + selection cursor (shared finder core).
    pub finder: FilterList,
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
            finder: FilterList::new(),
        }
    }

    /// The per-entry lowercased haystacks (command + capped output), in order.
    fn haystacks(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.haystack.as_str()).collect()
    }

    /// Absolute indices of entries matching the live filter, in order.
    pub fn visible_indices(&self) -> Vec<usize> {
        finder::filtered_indices(&self.haystacks(), &self.finder.filter)
    }

    /// The absolute index of the selected entry, or `None` when nothing matches.
    pub fn selected(&self) -> Option<usize> {
        self.finder.selected(&self.haystacks())
    }

    /// The live filter text.
    pub fn filter(&self) -> &str {
        &self.finder.filter
    }
}

/// Apply one key. An empty filtered view can only `Cancel` (Accept returns
/// `None` because nothing is selected).
pub fn handle_history(event: &KeyEvent, state: &mut HistoryState) -> HistoryOutcome {
    // Build the haystacks from the entries field directly (not via a &self
    // method) so the finder field can be borrowed mutably alongside.
    let hs: Vec<&str> = state.entries.iter().map(|e| e.haystack.as_str()).collect();
    let redraw = |changed: bool| {
        if changed {
            HistoryOutcome::Redraw
        } else {
            HistoryOutcome::None
        }
    };
    match finder::classify(event) {
        FinderKey::Cancel => HistoryOutcome::Cancel,
        FinderKey::Accept => match state.finder.selected(&hs) {
            Some(i) => {
                let e = &state.entries[i];
                HistoryOutcome::Jump(HistoryTarget {
                    session: e.session.clone(),
                    window: e.window,
                    pane: e.pane,
                    prompt_line: e.prompt_line,
                    command: e.command.clone(),
                })
            }
            None => HistoryOutcome::None,
        },
        FinderKey::Up => redraw(state.finder.up()),
        FinderKey::Down => redraw(state.finder.down(&hs)),
        FinderKey::Home => redraw(state.finder.home()),
        FinderKey::End => redraw(state.finder.end(&hs)),
        FinderKey::Clear => redraw(state.finder.clear()),
        FinderKey::Backspace => redraw(state.finder.backspace(&hs)),
        FinderKey::Char(c) => {
            state.finder.push(c);
            HistoryOutcome::Redraw
        }
        FinderKey::Pass => HistoryOutcome::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Direction, Key, Modifiers};

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
        s.finder.filter = "REFUSED".into();
        assert_eq!(
            s.visible_indices(),
            vec![1],
            "matches output, case-insensitive"
        );
        s.finder.filter = "docker".into();
        assert_eq!(s.visible_indices(), vec![0], "matches command");
        s.finder.filter = "zzz".into();
        assert!(s.visible_indices().is_empty());
    }

    #[test]
    fn typing_filters_and_enter_jumps() {
        let mut s = state();
        for c in "cargo".chars() {
            assert_eq!(handle_history(&chr(c), &mut s), HistoryOutcome::Redraw);
        }
        // Only entry 1 matches; the cursor reset to the top of the filtered view
        // (index 0) which maps to absolute entry 1.
        assert_eq!(s.selected(), Some(1));
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
        s.finder.filter = "zzz".into();
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
    fn arrows_and_ctrl_jk_move_within_visible() {
        let mut s = state();
        assert_eq!(
            handle_history(&key(Key::Arrow(Direction::Down)), &mut s),
            HistoryOutcome::Redraw
        );
        assert_eq!(s.selected(), Some(1));
        handle_history(&key(Key::Arrow(Direction::Down)), &mut s); // clamp at end
        assert_eq!(s.selected(), Some(1));
        assert_eq!(handle_history(&ctrl('k'), &mut s), HistoryOutcome::Redraw);
        assert_eq!(s.selected(), Some(0), "Ctrl-k moves up");
        assert_eq!(handle_history(&ctrl('j'), &mut s), HistoryOutcome::Redraw);
        assert_eq!(s.selected(), Some(1), "Ctrl-j moves down");
    }

    #[test]
    fn home_and_end_jump_to_ends() {
        let mut s = state();
        assert_eq!(
            handle_history(&key(Key::End), &mut s),
            HistoryOutcome::Redraw
        );
        assert_eq!(s.selected(), Some(1));
        assert_eq!(
            handle_history(&key(Key::Home), &mut s),
            HistoryOutcome::Redraw
        );
        assert_eq!(s.selected(), Some(0));
    }

    #[test]
    fn backspace_pops_filter_and_reclamps() {
        let mut s = state();
        s.finder.filter = "cargo".into();
        // pop to "carg" still matches only entry 1.
        assert_eq!(
            handle_history(&key(Key::Backspace), &mut s),
            HistoryOutcome::Redraw
        );
        assert_eq!(s.finder.filter, "carg");
        assert_eq!(s.selected(), Some(1));
    }

    #[test]
    fn ctrl_u_clears_filter() {
        let mut s = state();
        for c in "cargo".chars() {
            handle_history(&chr(c), &mut s);
        }
        assert_eq!(handle_history(&ctrl('u'), &mut s), HistoryOutcome::Redraw);
        assert_eq!(s.finder.filter, "");
    }
}
