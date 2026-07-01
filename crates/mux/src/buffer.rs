//! Pure model and key handler for the `choose-buffer` overlay: a list of paste
//! buffers the user can paste or delete. The daemon owns the snapshot and
//! performs the actions; this module decides how one key mutates the list and
//! what the caller must do next. Returns a crate-local [`BufferOutcome`] (NOT
//! `OverlayAction`), so it has no dependency on the overlay enum and can be
//! built/tested in isolation. Mirrors `tree.rs`.

use crate::{Direction, Key, KeyEvent, Modifiers};

/// One row in the choose-buffer overlay. `name` is the buffer id (the paste /
/// delete key); `preview` is a one-line, control-stripped, width-truncated
/// excerpt of the buffer's content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferEntry {
    pub name: String,
    pub preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferPickerState {
    pub entries: Vec<BufferEntry>,
    pub selected: usize,
}

/// What the caller (connection layer) must perform after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BufferAction {
    Paste(String),
    Delete(String),
}

/// Crate-local follow-up. The daemon adapts this into `OverlayKeyResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BufferOutcome {
    None,
    Redraw,
    Cancel,
    /// Perform the action. For `Paste` the caller closes the overlay; for
    /// `Delete` the entry was already pruned and the overlay stays open.
    Act(BufferAction),
}

/// Apply one key. `Esc` always closes (even an empty chooser, which, unlike the
/// transient empty tree, is a state the user can dwell in and must escape from).
/// Every other key is a no-op when `entries` is empty.
pub fn handle_buffers(event: &KeyEvent, state: &mut BufferPickerState) -> BufferOutcome {
    if event.mods.is_empty() && event.key == Key::Escape {
        return BufferOutcome::Cancel;
    }
    if state.entries.is_empty() {
        return BufferOutcome::None;
    }
    let last = state.entries.len() - 1;
    match (event.mods, event.key) {
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
        (_, Key::Enter | Key::KeypadEnter) => {
            BufferOutcome::Act(BufferAction::Paste(state.entries[state.selected].name.clone()))
        }
        (m, Key::Char('d')) if m.is_empty() => {
            let name = state.entries[state.selected].name.clone();
            state.entries.remove(state.selected);
            clamp_sel(state);
            BufferOutcome::Act(BufferAction::Delete(name))
        }
        _ => BufferOutcome::None,
    }
}

fn move_sel(state: &mut BufferPickerState, down: bool) -> BufferOutcome {
    let last = state.entries.len() - 1;
    let new = if down {
        (state.selected + 1).min(last)
    } else {
        state.selected.saturating_sub(1)
    };
    set_sel(state, new)
}

fn set_sel(state: &mut BufferPickerState, target: usize) -> BufferOutcome {
    let clamped = target.min(state.entries.len().saturating_sub(1));
    if clamped == state.selected {
        BufferOutcome::None
    } else {
        state.selected = clamped;
        BufferOutcome::Redraw
    }
}

fn clamp_sel(state: &mut BufferPickerState) {
    if state.entries.is_empty() {
        state.selected = 0;
    } else {
        state.selected = state.selected.min(state.entries.len() - 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(mods: Modifiers, key: Key) -> KeyEvent {
        KeyEvent::new(key, mods)
    }

    fn entry(name: &str) -> BufferEntry {
        BufferEntry { name: name.into(), preview: format!("preview of {name}") }
    }

    fn sample() -> BufferPickerState {
        BufferPickerState {
            entries: vec![entry("buffer2"), entry("buffer1"), entry("buffer0")],
            selected: 0,
        }
    }

    #[test]
    fn navigation_clamps_both_ends() {
        let mut s = sample();
        assert_eq!(handle_buffers(&ev(Modifiers::empty(), Key::Arrow(Direction::Up)), &mut s), BufferOutcome::None);
        for _ in 0..10 {
            handle_buffers(&ev(Modifiers::empty(), Key::Char('j')), &mut s);
        }
        assert_eq!(s.selected, 2);
        assert_eq!(handle_buffers(&ev(Modifiers::empty(), Key::Char('j')), &mut s), BufferOutcome::None);
        handle_buffers(&ev(Modifiers::empty(), Key::Home), &mut s);
        assert_eq!(s.selected, 0);
        handle_buffers(&ev(Modifiers::empty(), Key::End), &mut s);
        assert_eq!(s.selected, 2);
    }

    #[test]
    fn shifted_g_arrives_without_modifier() {
        let mut s = sample();
        assert_eq!(handle_buffers(&ev(Modifiers::empty(), Key::Char('G')), &mut s), BufferOutcome::Redraw);
        assert_eq!(s.selected, 2);
    }

    #[test]
    fn enter_pastes_selected() {
        let mut s = sample();
        handle_buffers(&ev(Modifiers::empty(), Key::Char('j')), &mut s); // select buffer1
        assert_eq!(
            handle_buffers(&ev(Modifiers::empty(), Key::Enter), &mut s),
            BufferOutcome::Act(BufferAction::Paste("buffer1".into()))
        );
    }

    #[test]
    fn delete_prunes_and_reclamps() {
        let mut s = sample();
        handle_buffers(&ev(Modifiers::empty(), Key::End), &mut s); // selected = 2 (buffer0)
        let out = handle_buffers(&ev(Modifiers::empty(), Key::Char('d')), &mut s);
        assert_eq!(out, BufferOutcome::Act(BufferAction::Delete("buffer0".into())));
        assert_eq!(s.entries.len(), 2);
        assert_eq!(s.selected, 1, "selection re-clamped after removing the last row");
        assert!(s.entries.iter().all(|e| e.name != "buffer0"));
    }

    #[test]
    fn empty_list_ignores_keys_but_escape_closes() {
        let mut s = BufferPickerState { entries: vec![], selected: 0 };
        for key in [Key::Char('j'), Key::Char('G'), Key::Enter, Key::Char('d')] {
            assert_eq!(handle_buffers(&ev(Modifiers::empty(), key), &mut s), BufferOutcome::None);
        }
        // Esc must still close an empty chooser (not a no-op trap).
        assert_eq!(
            handle_buffers(&ev(Modifiers::empty(), Key::Escape), &mut s),
            BufferOutcome::Cancel
        );
    }

    #[test]
    fn escape_cancels() {
        let mut s = sample();
        assert_eq!(handle_buffers(&ev(Modifiers::empty(), Key::Escape), &mut s), BufferOutcome::Cancel);
    }
}
