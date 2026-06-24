//! Session-scoped interactive overlays: a modal text-input (rename) and a
//! scrollable display (help). Pure logic: the daemon owns the state on its
//! `WindowManager`, captures keys at the connection layer (mirroring copy
//! mode), and the compositor renders the overlay. This module only decides how
//! one key event mutates an overlay and what the caller should do next.

use crate::command_prompt::{self, Completion};
use crate::{Direction, Key, KeyEvent, Modifiers};

/// What a rename overlay targets. The concrete window/pane is resolved by the
/// daemon at open time, not stored here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameTarget {
    Window,
    Pane,
}

/// One row in the session picker. `name` is the switch target and the filter
/// key; `label` is the display string (e.g. "work — 2 win, 3 panes"); and
/// `is_current` marks the session this client is attached to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerEntry {
    pub name: String,
    pub label: String,
    pub is_current: bool,
}

/// Indices into `entries` whose `name` matches `filter`, via a case-insensitive
/// substring test (`to_lowercase().contains`, correct for multi-codepoint
/// lowercase expansions). An empty filter yields every index in order; because
/// `entries` are pre-sorted ascending by name at open time, the result preserves
/// that a–z order. Shared by the picker key handler and the compositor.
pub fn picker_filtered_indices(entries: &[PickerEntry], filter: &str) -> Vec<usize> {
    if filter.is_empty() {
        return (0..entries.len()).collect();
    }
    let needle = filter.to_lowercase();
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.name.to_lowercase().contains(&needle))
        .map(|(i, _)| i)
        .collect()
}

/// An active overlay. `None` (on the holder) means no overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    /// A single-line text prompt editing a name.
    Rename { target: RenameTarget, buf: String },
    /// A scrollable read-only page (e.g. the keybinding list). `scroll` is the
    /// top line index; the renderer clamps it to the content length.
    Help { scroll: u16 },
    /// A single-line command prompt (`Ctrl+a :`). `history` is a read-only copy
    /// of the session's command history (newest last) for Up/Down recall;
    /// `hist_idx` is `None` while editing a fresh line. `completions` is a
    /// session-name snapshot used to Tab-complete a `switch ` argument.
    Command {
        buf: String,
        history: Vec<String>,
        hist_idx: Option<usize>,
        completions: Vec<String>,
    },
    /// An fzf-style session picker. `entries` is a snapshot (sorted by name);
    /// `filter` is the live substring query; `selected` indexes into the
    /// *filtered* view (always clamped). Commit switches to the selected
    /// session.
    SessionPicker {
        entries: Vec<PickerEntry>,
        filter: String,
        selected: usize,
    },
    /// A fully-expanded session → window → pane tree (`choose-tree`). The daemon
    /// drives it via `crate::tree::handle_tree` directly (not through
    /// `overlay::handle`), because its actions are cross-session and need
    /// the registry; the state lives here so the compositor can render it.
    Tree(crate::tree::TreeState),
    /// The choose-buffer overlay. Driven by `crate::buffer::handle_buffers` at the
    /// daemon layer (its actions need the registry's paste buffers).
    BufferPicker(crate::buffer::BufferPickerState),
    /// The structured history palette. Driven by `crate::history::handle_history`
    /// at the daemon layer (the jump is cross-session and needs the registry).
    History(crate::history::HistoryState),
    /// Hint mode: labelled spans in the focused pane. Driven by
    /// `crate::hint::handle_hint` at the daemon layer (commit needs the
    /// clipboard + registry).
    Hint(crate::hint::HintState),
}

/// The caller's follow-up after feeding a key to an overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayAction {
    /// State changed; recompose the frame.
    Redraw,
    /// A rename was confirmed with this (already-trimmed) text.
    Commit(String),
    /// The overlay was dismissed with no effect.
    Cancel,
    /// Key ignored; nothing changed.
    None,
}

/// Lines scrolled by a page-style key. The handler does not know the viewport
/// height, so it uses a fixed step and the renderer clamps the result.
const PAGE_STEP: u16 = 10;

/// Apply one key event to `overlay`. Pure: mutates `overlay` in place and
/// returns the action for the caller to act on.
pub fn handle(event: &KeyEvent, overlay: &mut Overlay) -> OverlayAction {
    match overlay {
        Overlay::Rename { buf, .. } => handle_rename(event, buf),
        Overlay::Help { scroll } => handle_help(event, scroll),
        Overlay::Command { buf, history, hist_idx, completions } => {
            handle_command_prompt(event, buf, history, hist_idx, completions)
        }
        Overlay::SessionPicker { entries, filter, selected } => {
            handle_session_picker(event, entries, filter, selected)
        }
        // Tree / buffer-picker / history / hint overlays are handled by the
        // daemon via their own pure handlers (actions need the registry); these
        // arms only keep the match exhaustive and are never reached.
        Overlay::Tree(_) | Overlay::BufferPicker(_) | Overlay::History(_) | Overlay::Hint(_) => {
            OverlayAction::None
        }
    }
}

fn handle_rename(event: &KeyEvent, buf: &mut String) -> OverlayAction {
    match (event.mods, event.key) {
        // Esc cancels. Note: unlike the help overlay, `q` is a normal character
        // here and must NOT dismiss.
        (m, Key::Escape) if m.is_empty() => OverlayAction::Cancel,
        (_, Key::Enter) | (_, Key::KeypadEnter) => OverlayAction::Commit(buf.trim().to_string()),
        (m, Key::Backspace) if m.is_empty() => {
            if buf.pop().is_some() {
                OverlayAction::Redraw
            } else {
                OverlayAction::None
            }
        }
        // Printable scalar (plain or shifted). Reject control combos so e.g.
        // Ctrl+C doesn't insert a stray glyph.
        (m, Key::Char(c)) if m.is_empty() || m == Modifiers::SHIFT => {
            buf.push(c);
            OverlayAction::Redraw
        }
        _ => OverlayAction::None,
    }
}

fn handle_help(event: &KeyEvent, scroll: &mut u16) -> OverlayAction {
    use crate::Direction;
    // Dismiss keys (mirrors copy mode's escape chain).
    if event.mods.is_empty() && matches!(event.key, Key::Escape | Key::Char('q') | Key::Enter) {
        return OverlayAction::Cancel;
    }
    let before = *scroll;
    let next = match (event.mods, event.key) {
        (m, Key::Char('j')) | (m, Key::Arrow(Direction::Down)) if m.is_empty() => {
            scroll.saturating_add(1)
        }
        (m, Key::Char('k')) | (m, Key::Arrow(Direction::Up)) if m.is_empty() => {
            scroll.saturating_sub(1)
        }
        (m, Key::PageDown) if m.is_empty() => scroll.saturating_add(PAGE_STEP),
        (m, Key::PageUp) if m.is_empty() => scroll.saturating_sub(PAGE_STEP),
        (m, Key::Char('d')) if m == Modifiers::CTRL => scroll.saturating_add(PAGE_STEP),
        (m, Key::Char('u')) if m == Modifiers::CTRL => scroll.saturating_sub(PAGE_STEP),
        (m, Key::Char('g')) | (m, Key::Home) if m.is_empty() => 0,
        // `G` arrives shifted; jump to the bottom (renderer clamps to content).
        (m, Key::Char('G')) if m == Modifiers::SHIFT => u16::MAX,
        (m, Key::End) if m.is_empty() => u16::MAX,
        _ => return OverlayAction::None,
    };
    *scroll = next;
    if next == before {
        OverlayAction::None
    } else {
        OverlayAction::Redraw
    }
}

fn handle_command_prompt(
    event: &KeyEvent,
    buf: &mut String,
    history: &[String],
    hist_idx: &mut Option<usize>,
    completions: &[String],
) -> OverlayAction {
    match (event.mods, event.key) {
        (m, Key::Escape) if m.is_empty() => OverlayAction::Cancel,
        // Empty/whitespace line cancels; otherwise commit the trimmed line.
        (_, Key::Enter) | (_, Key::KeypadEnter) => {
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                OverlayAction::Cancel
            } else {
                OverlayAction::Commit(trimmed.to_string())
            }
        }
        (m, Key::Backspace) if m.is_empty() => {
            *hist_idx = None;
            if buf.pop().is_some() {
                OverlayAction::Redraw
            } else {
                OverlayAction::None
            }
        }
        // Ctrl+U clears the line (arrives as Char('u') + CTRL, like the help
        // overlay's Ctrl+u page-up).
        (m, Key::Char('u')) if m == Modifiers::CTRL => {
            *hist_idx = None;
            if buf.is_empty() {
                OverlayAction::None
            } else {
                buf.clear();
                OverlayAction::Redraw
            }
        }
        (m, Key::Tab) if m.is_empty() => complete_in_place(buf, completions),
        (m, Key::Arrow(Direction::Up)) if m.is_empty() => history_recall(buf, history, hist_idx, true),
        (m, Key::Arrow(Direction::Down)) if m.is_empty() => {
            history_recall(buf, history, hist_idx, false)
        }
        // Printable scalar (plain or shifted). Reject control combos.
        (m, Key::Char(c)) if m.is_empty() || m == Modifiers::SHIFT => {
            *hist_idx = None;
            buf.push(c);
            OverlayAction::Redraw
        }
        _ => OverlayAction::None,
    }
}

/// Tab-complete the verb (first token) or, after `switch `, the session-name
/// argument. Mutates `buf` in place; returns whether anything changed.
fn complete_in_place(buf: &mut String, completions: &[String]) -> OverlayAction {
    let trimmed_start = buf.trim_start();
    let leading_ws = buf.len() - trimmed_start.len();

    if let Some(rest) = trimmed_start.strip_prefix("switch ") {
        // Complete the trailing session-name token against the snapshot.
        let arg = rest.trim_start();
        let cands: Vec<&str> = completions.iter().map(String::as_str).collect();
        match command_prompt::complete(arg, &cands) {
            Completion::Unique(s) | Completion::Partial(s) => {
                let keep = buf.len() - arg.len();
                buf.truncate(keep);
                buf.push_str(&s);
                OverlayAction::Redraw
            }
            Completion::None => OverlayAction::None,
        }
    } else if !trimmed_start.contains(char::is_whitespace) {
        // Single token, no trailing space yet → complete the verb.
        match command_prompt::complete(trimmed_start, command_prompt::VERBS) {
            Completion::Unique(s) => {
                buf.truncate(leading_ws);
                buf.push_str(&s);
                buf.push(' '); // bare verb → ready for an argument
                OverlayAction::Redraw
            }
            Completion::Partial(s) => {
                buf.truncate(leading_ws);
                buf.push_str(&s);
                OverlayAction::Redraw
            }
            Completion::None => OverlayAction::None,
        }
    } else {
        OverlayAction::None
    }
}

/// Walk command history. `older = true` for Up (toward older entries, starting
/// at the newest), `false` for Down (toward newer; past the newest restores a
/// fresh empty line). `history` is newest-last.
fn history_recall(
    buf: &mut String,
    history: &[String],
    hist_idx: &mut Option<usize>,
    older: bool,
) -> OverlayAction {
    if history.is_empty() {
        return OverlayAction::None;
    }
    if older {
        let new_idx = match *hist_idx {
            None => history.len() - 1,
            Some(0) => 0,
            Some(i) => i - 1,
        };
        *hist_idx = Some(new_idx);
        *buf = history[new_idx].clone();
        OverlayAction::Redraw
    } else {
        match *hist_idx {
            None => OverlayAction::None,
            Some(i) if i + 1 < history.len() => {
                *hist_idx = Some(i + 1);
                *buf = history[i + 1].clone();
                OverlayAction::Redraw
            }
            Some(_) => {
                *hist_idx = None;
                buf.clear();
                OverlayAction::Redraw
            }
        }
    }
}

/// fzf-style picker key handling: printable chars filter, arrows / Ctrl+n /
/// Ctrl+p navigate, Home/End jump, Enter commits the selected session's name,
/// Esc cancels. `selected` indexes the filtered view and is clamped on every
/// filter change.
fn handle_session_picker(
    event: &KeyEvent,
    entries: &[PickerEntry],
    filter: &mut String,
    selected: &mut usize,
) -> OverlayAction {
    let filtered_len = picker_filtered_indices(entries, filter).len();
    match (event.mods, event.key) {
        (m, Key::Escape) if m.is_empty() => OverlayAction::Cancel,
        (_, Key::Enter) | (_, Key::KeypadEnter) => {
            let filtered = picker_filtered_indices(entries, filter);
            match filtered.get(*selected) {
                Some(&idx) => OverlayAction::Commit(entries[idx].name.clone()),
                None => OverlayAction::None,
            }
        }
        (m, Key::Arrow(Direction::Up)) if m.is_empty() => move_picker(selected, filtered_len, false),
        (m, Key::Char('p')) if m == Modifiers::CTRL => move_picker(selected, filtered_len, false),
        (m, Key::Arrow(Direction::Down)) if m.is_empty() => move_picker(selected, filtered_len, true),
        (m, Key::Char('n')) if m == Modifiers::CTRL => move_picker(selected, filtered_len, true),
        (m, Key::Home) if m.is_empty() => set_picker(selected, 0),
        (m, Key::End) if m.is_empty() => set_picker(selected, filtered_len.saturating_sub(1)),
        (m, Key::Backspace) if m.is_empty() => {
            if filter.pop().is_some() {
                let len = picker_filtered_indices(entries, filter).len();
                *selected = (*selected).min(len.saturating_sub(1));
                OverlayAction::Redraw
            } else {
                OverlayAction::None
            }
        }
        (m, Key::Char('u')) if m == Modifiers::CTRL => {
            if filter.is_empty() {
                OverlayAction::None
            } else {
                filter.clear();
                *selected = 0;
                OverlayAction::Redraw
            }
        }
        (m, Key::Char(c)) if m.is_empty() || m == Modifiers::SHIFT => {
            filter.push(c);
            *selected = 0; // filter changed; reset selection to the top
            OverlayAction::Redraw
        }
        _ => OverlayAction::None,
    }
}

fn move_picker(selected: &mut usize, len: usize, down: bool) -> OverlayAction {
    if len == 0 {
        return OverlayAction::None;
    }
    let max = len - 1;
    let new = if down {
        (*selected + 1).min(max)
    } else {
        selected.saturating_sub(1)
    };
    if new != *selected {
        *selected = new;
        OverlayAction::Redraw
    } else {
        OverlayAction::None
    }
}

fn set_picker(selected: &mut usize, target: usize) -> OverlayAction {
    if *selected != target {
        *selected = target;
        OverlayAction::Redraw
    } else {
        OverlayAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(mods: Modifiers, key: Key) -> KeyEvent {
        KeyEvent::new(key, mods)
    }

    fn cmd() -> Overlay {
        Overlay::Command {
            buf: String::new(),
            history: Vec::new(),
            hist_idx: None,
            completions: Vec::new(),
        }
    }

    fn cmd_with(history: Vec<&str>, completions: Vec<&str>) -> Overlay {
        Overlay::Command {
            buf: String::new(),
            history: history.into_iter().map(String::from).collect(),
            hist_idx: None,
            completions: completions.into_iter().map(String::from).collect(),
        }
    }

    fn buf_of(o: &Overlay) -> &str {
        let Overlay::Command { buf, .. } = o else { panic!("expected command overlay") };
        buf
    }

    #[test]
    fn command_types_and_commits_trimmed() {
        let mut o = cmd();
        for c in "  new  ".chars() {
            handle(&ev(Modifiers::empty(), Key::Char(c)), &mut o);
        }
        assert_eq!(
            handle(&ev(Modifiers::empty(), Key::Enter), &mut o),
            OverlayAction::Commit("new".into())
        );
    }

    #[test]
    fn command_empty_enter_cancels() {
        let mut o = cmd();
        assert_eq!(
            handle(&ev(Modifiers::empty(), Key::Enter), &mut o),
            OverlayAction::Cancel
        );
        // Whitespace-only too.
        let mut o = cmd();
        handle(&ev(Modifiers::empty(), Key::Char(' ')), &mut o);
        assert_eq!(
            handle(&ev(Modifiers::empty(), Key::Enter), &mut o),
            OverlayAction::Cancel
        );
    }

    #[test]
    fn command_backspace_and_ctrl_u() {
        let mut o = Overlay::Command {
            buf: "split h".into(),
            history: Vec::new(),
            hist_idx: None,
            completions: Vec::new(),
        };
        handle(&ev(Modifiers::empty(), Key::Backspace), &mut o);
        assert_eq!(buf_of(&o), "split ");
        assert_eq!(
            handle(&ev(Modifiers::CTRL, Key::Char('u')), &mut o),
            OverlayAction::Redraw
        );
        assert_eq!(buf_of(&o), "");
        // Ctrl+U on an empty buffer is a no-op.
        assert_eq!(
            handle(&ev(Modifiers::CTRL, Key::Char('u')), &mut o),
            OverlayAction::None
        );
    }

    #[test]
    fn command_escape_cancels() {
        let mut o = cmd();
        assert_eq!(
            handle(&ev(Modifiers::empty(), Key::Escape), &mut o),
            OverlayAction::Cancel
        );
    }

    #[test]
    fn command_tab_completes_unique_verb_with_space() {
        let mut o = cmd();
        handle(&ev(Modifiers::empty(), Key::Char('z')), &mut o);
        handle(&ev(Modifiers::empty(), Key::Tab), &mut o);
        assert_eq!(buf_of(&o), "zoom ");
    }

    #[test]
    fn command_tab_completes_partial_prefix() {
        let mut o = cmd();
        // "ren" -> "rename" (shared by rename / rename-pane), no trailing space.
        for c in "ren".chars() {
            handle(&ev(Modifiers::empty(), Key::Char(c)), &mut o);
        }
        handle(&ev(Modifiers::empty(), Key::Tab), &mut o);
        assert_eq!(buf_of(&o), "rename");
    }

    #[test]
    fn command_tab_completes_switch_session_name() {
        let mut o = cmd_with(vec![], vec!["work", "web"]);
        for c in "switch we".chars() {
            handle(&ev(Modifiers::empty(), Key::Char(c)), &mut o);
        }
        handle(&ev(Modifiers::empty(), Key::Tab), &mut o);
        assert_eq!(buf_of(&o), "switch web");
    }

    #[test]
    fn command_tab_completion_preserves_leading_whitespace() {
        let mut o = cmd();
        for c in " zo".chars() {
            handle(&ev(Modifiers::empty(), Key::Char(c)), &mut o);
        }
        handle(&ev(Modifiers::empty(), Key::Tab), &mut o);
        assert_eq!(buf_of(&o), " zoom ", "leading whitespace is preserved on completion");
    }

    #[test]
    fn command_tab_completes_bare_switch_to_prefix() {
        let mut o = cmd_with(vec![], vec!["work", "web"]);
        for c in "switch".chars() {
            handle(&ev(Modifiers::empty(), Key::Char(c)), &mut o);
        }
        handle(&ev(Modifiers::empty(), Key::Tab), &mut o);
        // No trailing space yet → the verb branch bootstraps the "switch " prefix.
        assert_eq!(buf_of(&o), "switch ");
    }

    #[test]
    fn command_history_up_down() {
        let mut o = cmd_with(vec!["new", "split h"], vec![]);
        // Up -> newest ("split h"), Up again -> older ("new"), clamp.
        handle(&ev(Modifiers::empty(), Key::Arrow(Direction::Up)), &mut o);
        assert_eq!(buf_of(&o), "split h");
        handle(&ev(Modifiers::empty(), Key::Arrow(Direction::Up)), &mut o);
        assert_eq!(buf_of(&o), "new");
        handle(&ev(Modifiers::empty(), Key::Arrow(Direction::Up)), &mut o);
        assert_eq!(buf_of(&o), "new"); // clamped at oldest
        // Down -> newer ("split h"), Down again -> fresh empty line.
        handle(&ev(Modifiers::empty(), Key::Arrow(Direction::Down)), &mut o);
        assert_eq!(buf_of(&o), "split h");
        handle(&ev(Modifiers::empty(), Key::Arrow(Direction::Down)), &mut o);
        assert_eq!(buf_of(&o), "");
    }

    #[test]
    fn command_down_on_fresh_line_is_noop() {
        let mut o = cmd_with(vec!["new"], vec![]);
        assert_eq!(
            handle(&ev(Modifiers::empty(), Key::Arrow(Direction::Down)), &mut o),
            OverlayAction::None
        );
    }

    fn entry(name: &str) -> PickerEntry {
        PickerEntry { name: name.into(), label: name.into(), is_current: false }
    }

    #[test]
    fn picker_filter_empty_returns_all_in_order() {
        let es = vec![entry("alpha"), entry("beta"), entry("gamma")];
        assert_eq!(picker_filtered_indices(&es, ""), vec![0, 1, 2]);
    }

    #[test]
    fn picker_filter_case_insensitive_substring() {
        let es = vec![entry("Work"), entry("web"), entry("personal")];
        // "we" matches "web" only; "e" matches Work? no, it's a substring of name.
        assert_eq!(picker_filtered_indices(&es, "we"), vec![1]);
        assert_eq!(picker_filtered_indices(&es, "W"), vec![0, 1]); // Work, web (case-insensitive)
        assert_eq!(picker_filtered_indices(&es, "PERSON"), vec![2]);
    }

    #[test]
    fn picker_filter_no_match_is_empty() {
        let es = vec![entry("alpha"), entry("beta")];
        assert!(picker_filtered_indices(&es, "zzz").is_empty());
    }

    #[test]
    fn picker_filter_non_ascii() {
        let es = vec![entry("café"), entry("CAFÉ-2"), entry("tea")];
        assert_eq!(picker_filtered_indices(&es, "café"), vec![0, 1]);
    }

    fn picker(names: &[&str]) -> Overlay {
        Overlay::SessionPicker {
            entries: names.iter().map(|n| entry(n)).collect(),
            filter: String::new(),
            selected: 0,
        }
    }

    fn picker_state(o: &Overlay) -> (&str, usize) {
        let Overlay::SessionPicker { filter, selected, .. } = o else {
            panic!("expected session picker")
        };
        (filter, *selected)
    }

    #[test]
    fn picker_navigation_clamps_both_ends() {
        let mut o = picker(&["a", "b", "c"]);
        // Up at top is a no-op.
        assert_eq!(
            handle(&ev(Modifiers::empty(), Key::Arrow(Direction::Up)), &mut o),
            OverlayAction::None
        );
        handle(&ev(Modifiers::empty(), Key::Arrow(Direction::Down)), &mut o);
        handle(&ev(Modifiers::CTRL, Key::Char('n')), &mut o);
        assert_eq!(picker_state(&o).1, 2);
        // Down at bottom is a no-op.
        assert_eq!(
            handle(&ev(Modifiers::empty(), Key::Arrow(Direction::Down)), &mut o),
            OverlayAction::None
        );
        handle(&ev(Modifiers::CTRL, Key::Char('p')), &mut o);
        assert_eq!(picker_state(&o).1, 1);
        handle(&ev(Modifiers::empty(), Key::Home), &mut o);
        assert_eq!(picker_state(&o).1, 0);
        handle(&ev(Modifiers::empty(), Key::End), &mut o);
        assert_eq!(picker_state(&o).1, 2);
    }

    #[test]
    fn picker_typing_filters_and_resets_selection() {
        let mut o = picker(&["work", "web", "personal"]);
        handle(&ev(Modifiers::empty(), Key::End), &mut o); // selected -> 2
        handle(&ev(Modifiers::empty(), Key::Char('w')), &mut o);
        let (f, sel) = picker_state(&o);
        assert_eq!(f, "w");
        assert_eq!(sel, 0, "filter change resets selection to top");
        // "we" now matches only "web": navigating down stays put.
        handle(&ev(Modifiers::empty(), Key::Char('e')), &mut o);
        assert_eq!(picker_state(&o).0, "we");
        assert_eq!(
            handle(&ev(Modifiers::empty(), Key::Arrow(Direction::Down)), &mut o),
            OverlayAction::None
        );
    }

    #[test]
    fn picker_backspace_reclamps_selection() {
        let mut o = picker(&["alpha", "alpine", "beta"]);
        for c in "alp".chars() {
            handle(&ev(Modifiers::empty(), Key::Char(c)), &mut o);
        }
        // "alp" matches alpha, alpine; select the second.
        handle(&ev(Modifiers::empty(), Key::Arrow(Direction::Down)), &mut o);
        assert_eq!(picker_state(&o).1, 1);
        // Narrow to "alph" -> only "alpha"; selection must clamp to 0.
        handle(&ev(Modifiers::empty(), Key::Char('h')), &mut o);
        // (typing reset it to 0 anyway); backspace back to "alp" keeps it valid.
        handle(&ev(Modifiers::empty(), Key::Backspace), &mut o);
        assert!(picker_state(&o).1 <= 1);
    }

    #[test]
    fn picker_ctrl_u_clears_filter() {
        let mut o = picker(&["a", "b"]);
        for c in "xyz".chars() {
            handle(&ev(Modifiers::empty(), Key::Char(c)), &mut o);
        }
        assert_eq!(
            handle(&ev(Modifiers::CTRL, Key::Char('u')), &mut o),
            OverlayAction::Redraw
        );
        assert_eq!(picker_state(&o).0, "");
    }

    #[test]
    fn picker_enter_commits_selected_name() {
        let mut o = picker(&["work", "web", "personal"]);
        handle(&ev(Modifiers::empty(), Key::Arrow(Direction::Down)), &mut o); // -> "web"
        assert_eq!(
            handle(&ev(Modifiers::empty(), Key::Enter), &mut o),
            OverlayAction::Commit("web".into())
        );
    }

    #[test]
    fn picker_enter_on_empty_filtered_list_is_noop() {
        let mut o = picker(&["work", "web"]);
        for c in "zzz".chars() {
            handle(&ev(Modifiers::empty(), Key::Char(c)), &mut o);
        }
        assert_eq!(
            handle(&ev(Modifiers::empty(), Key::Enter), &mut o),
            OverlayAction::None
        );
    }

    #[test]
    fn picker_escape_cancels() {
        let mut o = picker(&["a"]);
        assert_eq!(
            handle(&ev(Modifiers::empty(), Key::Escape), &mut o),
            OverlayAction::Cancel
        );
    }

    #[test]
    fn picker_enter_resolves_filtered_index_not_absolute() {
        // After filtering, selected=0 must map to the first *filtered* entry,
        // not entries[0].
        let mut o = picker(&["alpha", "beta", "gamma"]);
        for c in "amm".chars() {
            // "gamma" contains "amm"
            handle(&ev(Modifiers::empty(), Key::Char(c)), &mut o);
        }
        assert_eq!(
            handle(&ev(Modifiers::empty(), Key::Enter), &mut o),
            OverlayAction::Commit("gamma".into())
        );
    }

    fn rename() -> Overlay {
        Overlay::Rename { target: RenameTarget::Window, buf: String::new() }
    }

    #[test]
    fn rename_appends_printable_chars() {
        let mut o = rename();
        assert_eq!(handle(&ev(Modifiers::empty(), Key::Char('h')), &mut o), OverlayAction::Redraw);
        handle(&ev(Modifiers::SHIFT, Key::Char('I')), &mut o);
        let Overlay::Rename { buf, .. } = &o else { panic!("expected rename") };
        assert_eq!(buf, "hI");
    }

    #[test]
    fn rename_backspace_pops_and_is_noop_when_empty() {
        let mut o = Overlay::Rename { target: RenameTarget::Pane, buf: "ab".into() };
        assert_eq!(handle(&ev(Modifiers::empty(), Key::Backspace), &mut o), OverlayAction::Redraw);
        handle(&ev(Modifiers::empty(), Key::Backspace), &mut o);
        let Overlay::Rename { buf, .. } = &o else { panic!() };
        assert!(buf.is_empty());
        // Backspace on empty: no change.
        assert_eq!(handle(&ev(Modifiers::empty(), Key::Backspace), &mut o), OverlayAction::None);
    }

    #[test]
    fn rename_enter_commits_trimmed() {
        let mut o = Overlay::Rename { target: RenameTarget::Window, buf: "  build  ".into() };
        assert_eq!(
            handle(&ev(Modifiers::empty(), Key::Enter), &mut o),
            OverlayAction::Commit("build".into())
        );
    }

    #[test]
    fn rename_escape_cancels_but_q_is_a_character() {
        let mut o = rename();
        // 'q' must be typed, not treated as dismiss.
        assert_eq!(handle(&ev(Modifiers::empty(), Key::Char('q')), &mut o), OverlayAction::Redraw);
        let Overlay::Rename { buf, .. } = &o else { panic!() };
        assert_eq!(buf, "q");
        assert_eq!(handle(&ev(Modifiers::empty(), Key::Escape), &mut o), OverlayAction::Cancel);
    }

    #[test]
    fn rename_ignores_control_combos() {
        let mut o = rename();
        assert_eq!(handle(&ev(Modifiers::CTRL, Key::Char('c')), &mut o), OverlayAction::None);
        let Overlay::Rename { buf, .. } = &o else { panic!() };
        assert!(buf.is_empty());
    }

    #[test]
    fn help_scrolls_and_saturates_at_top() {
        let mut o = Overlay::Help { scroll: 0 };
        // At the top, scrolling up is a no-op.
        assert_eq!(handle(&ev(Modifiers::empty(), Key::Char('k')), &mut o), OverlayAction::None);
        assert_eq!(handle(&ev(Modifiers::empty(), Key::Char('j')), &mut o), OverlayAction::Redraw);
        let Overlay::Help { scroll } = &o else { panic!() };
        assert_eq!(*scroll, 1);
    }

    #[test]
    fn help_page_and_jump_keys() {
        let mut o = Overlay::Help { scroll: 0 };
        handle(&ev(Modifiers::empty(), Key::PageDown), &mut o);
        let Overlay::Help { scroll } = &o else { panic!() };
        assert_eq!(*scroll, PAGE_STEP);
        handle(&ev(Modifiers::SHIFT, Key::Char('G')), &mut o);
        let Overlay::Help { scroll } = &o else { panic!() };
        assert_eq!(*scroll, u16::MAX);
        handle(&ev(Modifiers::empty(), Key::Char('g')), &mut o);
        let Overlay::Help { scroll } = &o else { panic!() };
        assert_eq!(*scroll, 0);
    }

    #[test]
    fn help_dismiss_keys() {
        for key in [Key::Escape, Key::Char('q'), Key::Enter] {
            let mut o = Overlay::Help { scroll: 3 };
            assert_eq!(handle(&ev(Modifiers::empty(), key), &mut o), OverlayAction::Cancel);
        }
    }
}
