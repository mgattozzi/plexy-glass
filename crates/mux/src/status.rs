//! Minimal status bar: window list + prefix indicator on the right.

use crate::pane_id::WindowId;
use plexy_glass_emulator::{Attrs, Cell, Color};
use smol_str::SmolStr;

#[derive(Debug, Clone)]
pub struct StatusLine {
    pub windows: Vec<WindowEntry>,
    pub prefix_active: bool,
    pub session_name: String,
    pub attached_clients: u8,
}

#[derive(Debug, Clone)]
pub struct WindowEntry {
    pub id: WindowId,
    pub name: String,
    pub active: bool,
}

/// Render a row of cells representing the status bar.
pub fn build(line: &StatusLine, cols: u16) -> Vec<Cell> {
    let mut text = String::new();
    for (idx, w) in line.windows.iter().enumerate() {
        if idx > 0 {
            text.push(' ');
        }
        let suffix = if w.active { "*" } else { "" };
        let id = w.id.raw();
        let name = &w.name;
        text.push_str(&format!("{id}:{name}{suffix}"));
    }
    let prefix_indicator = if line.prefix_active { "[prefix]" } else { "" };

    // Right-side: session name + optional client count, then prefix indicator.
    let session_chunk = if line.attached_clients >= 2 {
        format!("session: {} *{}", line.session_name, line.attached_clients)
    } else {
        format!("session: {}", line.session_name)
    };
    // Right side = "| {session_chunk}" then prefix indicator to the far right.
    // Layout (right-to-left): [prefix_indicator][session_sep_chunk]
    // We place prefix_indicator at the very right edge, then session_chunk
    // immediately to its left separated by " | ".
    let session_sep = format!(" | {session_chunk}");

    let mut row: Vec<Cell> = Vec::with_capacity(cols as usize);
    let text_chars: Vec<char> = text.chars().collect();
    let prefix_chars: Vec<char> = prefix_indicator.chars().collect();
    let session_sep_chars: Vec<char> = session_sep.chars().collect();

    // Total right-side width = session separator + prefix indicator.
    let right_width = session_sep_chars.len() + prefix_chars.len();
    let usable_text = (cols as usize).saturating_sub(right_width);

    for i in 0..(cols as usize) {
        let ch = if i < text_chars.len().min(usable_text) {
            text_chars[i]
        } else if i >= (cols as usize).saturating_sub(right_width)
            && i < (cols as usize).saturating_sub(prefix_chars.len())
        {
            // Session separator + chunk region.
            let sidx = i - (cols as usize).saturating_sub(right_width);
            if sidx < session_sep_chars.len() {
                session_sep_chars[sidx]
            } else {
                ' '
            }
        } else if i >= (cols as usize).saturating_sub(prefix_chars.len())
            && !prefix_chars.is_empty()
        {
            let pidx = i - ((cols as usize) - prefix_chars.len());
            prefix_chars[pidx]
        } else {
            ' '
        };

        let cell = Cell {
            grapheme: SmolStr::new(ch.to_string()),
            fg: Color::Default,
            bg: Color::Default,
            attrs: Attrs::REVERSE,
            ..Cell::default()
        };
        row.push(cell);
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_text(row: &[Cell]) -> String {
        row.iter().map(|c| c.grapheme.as_str()).collect()
    }

    #[test]
    fn single_window_appears_in_status() {
        let s = StatusLine {
            windows: vec![WindowEntry { id: WindowId(0), name: "zsh".into(), active: true }],
            prefix_active: false,
            session_name: "main".into(),
            attached_clients: 1,
        };
        let row = build(&s, 40);
        let txt = row_text(&row);
        assert!(txt.starts_with("0:zsh*"));
    }

    #[test]
    fn prefix_indicator_right_aligned() {
        let s = StatusLine {
            windows: vec![WindowEntry { id: WindowId(0), name: "a".into(), active: true }],
            prefix_active: true,
            session_name: "main".into(),
            attached_clients: 1,
        };
        let row = build(&s, 60);
        let txt = row_text(&row);
        assert!(txt.ends_with("[prefix]"));
    }

    #[test]
    fn reverse_attr_on_every_cell() {
        let s = StatusLine {
            windows: vec![WindowEntry { id: WindowId(0), name: "a".into(), active: true }],
            prefix_active: false,
            session_name: "main".into(),
            attached_clients: 1,
        };
        let row = build(&s, 30);
        assert!(row.iter().all(|c| c.attrs.contains(Attrs::REVERSE)));
    }

    #[test]
    fn session_name_appears_in_status() {
        let s = StatusLine {
            windows: vec![WindowEntry { id: WindowId(0), name: "zsh".into(), active: true }],
            prefix_active: false,
            session_name: "main".into(),
            attached_clients: 1,
        };
        let row = build(&s, 40);
        let txt = row_text(&row);
        assert!(txt.contains("main"), "expected session name in status bar: {txt}");
    }

    #[test]
    fn client_count_shown_when_multiple() {
        let s = StatusLine {
            windows: vec![WindowEntry { id: WindowId(0), name: "zsh".into(), active: true }],
            prefix_active: false,
            session_name: "main".into(),
            attached_clients: 3,
        };
        let row = build(&s, 40);
        let txt = row_text(&row);
        assert!(txt.contains("*3"), "expected *3 in status bar: {txt}");
    }
}
