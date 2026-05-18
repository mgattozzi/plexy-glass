//! Minimal status bar: window list + prefix indicator on the right.

use crate::pane_id::WindowId;
use plexy_glass_emulator::{Attrs, Cell, Color};
use smol_str::SmolStr;

#[derive(Debug, Clone)]
pub struct StatusLine {
    pub windows: Vec<WindowEntry>,
    pub prefix_active: bool,
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

    let mut row: Vec<Cell> = Vec::with_capacity(cols as usize);
    let text_chars: Vec<char> = text.chars().collect();
    let prefix_chars: Vec<char> = prefix_indicator.chars().collect();

    let usable_text = (cols as usize).saturating_sub(prefix_chars.len());
    for i in 0..(cols as usize) {
        let ch = if i < text_chars.len().min(usable_text) {
            text_chars[i]
        } else if i >= (cols as usize) - prefix_chars.len() && !prefix_chars.is_empty() {
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
        };
        let row = build(&s, 20);
        let txt = row_text(&row);
        assert!(txt.starts_with("0:zsh*"));
    }

    #[test]
    fn prefix_indicator_right_aligned() {
        let s = StatusLine {
            windows: vec![WindowEntry { id: WindowId(0), name: "a".into(), active: true }],
            prefix_active: true,
        };
        let row = build(&s, 20);
        let txt = row_text(&row);
        assert!(txt.ends_with("[prefix]"));
    }

    #[test]
    fn reverse_attr_on_every_cell() {
        let s = StatusLine {
            windows: vec![WindowEntry { id: WindowId(0), name: "a".into(), active: true }],
            prefix_active: false,
        };
        let row = build(&s, 10);
        assert!(row.iter().all(|c| c.attrs.contains(Attrs::REVERSE)));
    }
}
