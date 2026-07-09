//! Shared pure core for the flat filter-finders (command palette, history
//! palette, session picker): a substring filter plus a selection cursor over
//! the *filtered* view (the fzf model — the cursor resets to the top when the
//! filter narrows on a keystroke, and clamps on backspace). Each overlay owns
//! its item type, haystack construction, outcome enum, and rendering; only
//! these mechanics live here. No daemon dependency, so it tests standalone.

use crate::{Direction, Key, KeyEvent, Modifiers};

/// Indices of `haystacks` that contain `filter` (lowercased) as a substring,
/// in input order. Empty filter = every index. The haystacks are matched
/// **as-is** — only `filter` is lowercased — so callers lowercase their
/// haystacks once at construction for case-insensitive search.
pub fn filtered_indices<S: AsRef<str>>(haystacks: &[S], filter: &str) -> Vec<usize> {
    if filter.is_empty() {
        return (0..haystacks.len()).collect();
    }
    let needle = filter.to_lowercase();
    (0..haystacks.len())
        .filter(|&i| haystacks[i].as_ref().contains(&needle))
        .collect()
}

/// Filter text plus a cursor over the *filtered* view.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FilterList {
    /// Live query (original case; matched lowercased against the haystacks).
    pub filter: String,
    /// Index into the filtered view. Kept in `[0, filtered_len)` (or 0 when
    /// the filtered view is empty) by every mutator.
    pub cursor: usize,
}

impl FilterList {
    pub const fn new() -> Self {
        Self {
            filter: String::new(),
            cursor: 0,
        }
    }

    fn filtered_len<S: AsRef<str>>(&self, haystacks: &[S]) -> usize {
        filtered_indices(haystacks, &self.filter).len()
    }

    /// The absolute index into `haystacks` currently selected, or `None` when
    /// the filtered view is empty.
    pub fn selected<S: AsRef<str>>(&self, haystacks: &[S]) -> Option<usize> {
        filtered_indices(haystacks, &self.filter)
            .get(self.cursor)
            .copied()
    }

    /// Move up one row. Returns whether the cursor changed.
    pub const fn up(&mut self) -> bool {
        if self.cursor > 0 {
            self.cursor -= 1;
            true
        } else {
            false
        }
    }

    /// Move down one row (clamped to the last filtered row).
    pub fn down<S: AsRef<str>>(&mut self, haystacks: &[S]) -> bool {
        let len = self.filtered_len(haystacks);
        if len == 0 {
            return false;
        }
        let max = len - 1;
        if self.cursor < max {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    pub const fn home(&mut self) -> bool {
        if self.cursor != 0 {
            self.cursor = 0;
            true
        } else {
            false
        }
    }

    pub fn end<S: AsRef<str>>(&mut self, haystacks: &[S]) -> bool {
        let target = self.filtered_len(haystacks).saturating_sub(1);
        if self.cursor == target {
            false
        } else {
            self.cursor = target;
            true
        }
    }

    /// Append a char to the filter and reset the cursor to the top (fzf model).
    pub fn push(&mut self, c: char) {
        self.filter.push(c);
        self.cursor = 0;
    }

    /// Pop the last filter char and clamp the cursor to the new filtered length.
    /// Returns whether anything changed.
    pub fn backspace<S: AsRef<str>>(&mut self, haystacks: &[S]) -> bool {
        if self.filter.pop().is_some() {
            let len = self.filtered_len(haystacks);
            self.cursor = self.cursor.min(len.saturating_sub(1));
            true
        } else {
            false
        }
    }

    /// Clear the filter (Ctrl-U). Returns whether anything changed.
    pub fn clear(&mut self) -> bool {
        if self.filter.is_empty() {
            false
        } else {
            self.filter.clear();
            self.cursor = 0;
            true
        }
    }
}

/// One classified finder key. `Pass` = not a finder key (the caller ignores it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinderKey {
    Up,
    Down,
    Home,
    End,
    Char(char),
    Backspace,
    Clear,
    Accept,
    Cancel,
    Pass,
}

/// Map a key event to a finder action. Navigation is arrows + `Ctrl-k` (up) /
/// `Ctrl-j` (down); `Home`/`End`, `Backspace`, `Ctrl-U` clear, `Enter` accept,
/// `Esc` cancel; a plain or shifted printable filters. Everything else `Pass`es.
pub fn classify(event: &KeyEvent) -> FinderKey {
    match (event.mods, event.key) {
        (m, Key::Escape) if m.is_empty() => FinderKey::Cancel,
        (_, Key::Enter | Key::KeypadEnter) => FinderKey::Accept,
        (m, Key::Arrow(Direction::Up)) if m.is_empty() => FinderKey::Up,
        (m, Key::Char('k')) if m == Modifiers::CTRL => FinderKey::Up,
        (m, Key::Arrow(Direction::Down)) if m.is_empty() => FinderKey::Down,
        (m, Key::Char('j')) if m == Modifiers::CTRL => FinderKey::Down,
        (m, Key::Home) if m.is_empty() => FinderKey::Home,
        (m, Key::End) if m.is_empty() => FinderKey::End,
        (m, Key::Char('u')) if m == Modifiers::CTRL => FinderKey::Clear,
        (m, Key::Backspace) if m.is_empty() => FinderKey::Backspace,
        (m, Key::Char(c)) if m.is_empty() || m == Modifiers::SHIFT => FinderKey::Char(c),
        _ => FinderKey::Pass,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Key, KeyEvent, Modifiers};

    fn ev(mods: Modifiers, key: Key) -> KeyEvent {
        KeyEvent::new(key, mods)
    }

    // Pre-lowercased haystacks, per the filtered_indices contract.
    fn hs() -> Vec<&'static str> {
        vec![
            "split horizontal",
            "split vertical",
            "zoom pane",
            "reload config",
        ]
    }

    #[test]
    fn filter_empty_returns_all_in_order() {
        assert_eq!(filtered_indices(&hs(), ""), vec![0, 1, 2, 3]);
    }

    #[test]
    fn filter_substring_case_insensitive_needle() {
        // "SPLIT" (upper) matches the lowercased haystacks (needle is lowered).
        assert_eq!(filtered_indices(&hs(), "SPLIT"), vec![0, 1]);
        assert_eq!(filtered_indices(&hs(), "zoom"), vec![2]);
        assert_eq!(filtered_indices(&hs(), "zzz"), Vec::<usize>::new());
    }

    #[test]
    fn push_resets_cursor_to_top() {
        let mut f = FilterList::new();
        f.cursor = 2;
        f.push('s');
        assert_eq!(
            f.cursor, 0,
            "typing resets selection to the top (fzf model)"
        );
        assert_eq!(f.filter, "s");
    }

    #[test]
    fn selected_maps_cursor_to_absolute_filtered_index() {
        let mut f = FilterList::new();
        f.filter = "split".into();
        // Filtered view is [0, 1]; cursor 1 -> absolute index 1.
        f.cursor = 1;
        assert_eq!(f.selected(&hs()), Some(1));
    }

    #[test]
    fn down_clamps_at_last_filtered_row_and_up_at_top() {
        let mut f = FilterList::new(); // all four visible
        assert!(f.down(&hs()));
        assert!(f.down(&hs()));
        assert!(f.down(&hs()));
        assert!(!f.down(&hs()), "clamped at the last row");
        assert_eq!(f.cursor, 3);
        assert!(f.up());
        assert_eq!(f.cursor, 2);
        f.cursor = 0;
        assert!(!f.up(), "clamped at the top");
    }

    #[test]
    fn backspace_clamps_cursor_to_new_filtered_len() {
        let mut f = FilterList::new();
        f.filter = "split".into();
        f.cursor = 1; // valid: filtered [0,1]
        // Backspace to "spli" still matches [0,1]; cursor stays valid.
        assert!(f.backspace(&hs()));
        assert_eq!(f.filter, "spli");
        assert_eq!(f.cursor, 1);
    }

    #[test]
    fn clear_empties_filter_and_resets_cursor() {
        let mut f = FilterList::new();
        f.filter = "zoo".into();
        f.cursor = 0;
        assert!(f.clear());
        assert_eq!(f.filter, "");
        assert_eq!(f.cursor, 0);
        assert!(!f.clear(), "clearing an empty filter is a no-op");
    }

    #[test]
    fn selected_none_on_empty_filtered_view() {
        let mut f = FilterList::new();
        f.filter = "nomatch".into();
        assert_eq!(f.selected(&hs()), None);
    }

    #[test]
    fn classify_nav_keys() {
        assert_eq!(
            classify(&ev(Modifiers::empty(), Key::Escape)),
            FinderKey::Cancel
        );
        assert_eq!(
            classify(&ev(Modifiers::empty(), Key::Enter)),
            FinderKey::Accept
        );
        assert_eq!(
            classify(&ev(Modifiers::CTRL, Key::Char('k'))),
            FinderKey::Up
        );
        assert_eq!(
            classify(&ev(Modifiers::CTRL, Key::Char('j'))),
            FinderKey::Down
        );
        assert_eq!(
            classify(&ev(Modifiers::CTRL, Key::Char('u'))),
            FinderKey::Clear
        );
        assert_eq!(
            classify(&ev(Modifiers::empty(), Key::Char('a'))),
            FinderKey::Char('a')
        );
        assert_eq!(
            classify(&ev(Modifiers::SHIFT, Key::Char('A'))),
            FinderKey::Char('A')
        );
        // Ctrl-p / Ctrl-n are NO LONGER nav keys (they Pass).
        assert_eq!(
            classify(&ev(Modifiers::CTRL, Key::Char('p'))),
            FinderKey::Pass
        );
        assert_eq!(
            classify(&ev(Modifiers::CTRL, Key::Char('n'))),
            FinderKey::Pass
        );
    }
}
