//! Cell-level diff renderer: compares the current `VirtualScreen` against the
//! previous one and emits minimal ANSI to bring the host TTY up to date.

use crate::virtual_screen::VirtualScreen;
use plexy_glass_emulator::{Attrs, Cell, Color};
use std::fmt::Write as _;

pub struct DiffRenderer {
    previous: Option<VirtualScreen>,
}

impl DiffRenderer {
    pub fn new() -> Self {
        Self { previous: None }
    }

    /// Forcibly invalidate the cached frame so the next render is a full repaint.
    pub fn invalidate(&mut self) {
        self.previous = None;
    }

    pub fn render(&mut self, current: &VirtualScreen) -> Vec<u8> {
        let mut out = String::new();

        let full_repaint = match &self.previous {
            None => true,
            Some(p) => p.rows != current.rows || p.cols != current.cols,
        };

        if full_repaint {
            // Clear + home, then walk every row, every cell.
            out.push_str("\x1b[2J\x1b[H");
            let mut current_attrs = CellAttrs::default();
            for r in 0..current.rows {
                let _ = write!(out, "\x1b[{};1H", r + 1);
                let mut c = 0u16;
                while c < current.cols {
                    let Some(cell) = current.cell(r, c) else { break };
                    if cell.is_wide_spacer() {
                        c += 1;
                        continue;
                    }
                    apply_sgr_delta(&mut out, &current_attrs, cell);
                    current_attrs = CellAttrs::from_cell(cell);
                    out.push_str(cell.grapheme.as_str());
                    let w = grapheme_advance(cell.grapheme.as_str());
                    c += w;
                }
            }
        } else {
            // Diff per row.
            // invariant: full_repaint == false implies self.previous is Some.
            let prev = self
                .previous
                .as_ref()
                .expect("non-full-repaint => previous is Some");
            let mut current_attrs = CellAttrs::default();
            for r in 0..current.rows {
                let mut c = 0u16;
                while c < current.cols {
                    let pc = prev.cell(r, c);
                    let cc = current.cell(r, c);
                    if pc == cc {
                        c += 1;
                        continue;
                    }
                    // Run start.
                    let _ = write!(out, "\x1b[{};{}H", r + 1, c + 1);
                    while c < current.cols {
                        let Some(cell) = current.cell(r, c) else { break };
                        if Some(cell) == prev.cell(r, c) {
                            break;
                        }
                        if cell.is_wide_spacer() {
                            c += 1;
                            continue;
                        }
                        apply_sgr_delta(&mut out, &current_attrs, cell);
                        current_attrs = CellAttrs::from_cell(cell);
                        out.push_str(cell.grapheme.as_str());
                        let w = grapheme_advance(cell.grapheme.as_str());
                        c += w;
                    }
                }
            }
        }

        // Cursor.
        if current.cursor_visible {
            if let Some((r, c)) = current.cursor {
                let _ = write!(out, "\x1b[{};{}H\x1b[?25h", r + 1, c + 1);
            } else {
                out.push_str("\x1b[?25l");
            }
        } else {
            out.push_str("\x1b[?25l");
        }

        // Reset SGR at the very end so we don't leave attrs leaking into the host.
        out.push_str("\x1b[0m");

        self.previous = Some(current.clone());
        out.into_bytes()
    }
}

impl Default for DiffRenderer {
    fn default() -> Self {
        Self::new()
    }
}

/// Column advance for a grapheme: at least 1, clamped to u16 range.
fn grapheme_advance(s: &str) -> u16 {
    let w = unicode_width::UnicodeWidthStr::width(s);
    // invariant: terminal cell widths are 0..=2 for any one grapheme, so this
    // never overflows u16. We still clamp defensively.
    u16::try_from(w).unwrap_or(1).max(1)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CellAttrs {
    fg: Color,
    bg: Color,
    attrs: Attrs,
}

impl CellAttrs {
    fn from_cell(c: &Cell) -> Self {
        Self {
            fg: c.fg,
            bg: c.bg,
            attrs: c.attrs,
        }
    }
}

fn apply_sgr_delta(out: &mut String, prev: &CellAttrs, cell: &Cell) {
    let new = CellAttrs::from_cell(cell);
    if &new == prev {
        return;
    }
    // For simplicity, emit a full reset + reset every attribute. Cell-diffing
    // gives most of the bandwidth win, and tighter SGR diffing is a later
    // optimization.
    out.push_str("\x1b[0m");
    if new.attrs.contains(Attrs::BOLD) {
        out.push_str("\x1b[1m");
    }
    if new.attrs.contains(Attrs::DIM) {
        out.push_str("\x1b[2m");
    }
    if new.attrs.contains(Attrs::ITALIC) {
        out.push_str("\x1b[3m");
    }
    if new.attrs.contains(Attrs::UNDERLINE) {
        out.push_str("\x1b[4m");
    }
    if new.attrs.contains(Attrs::REVERSE) {
        out.push_str("\x1b[7m");
    }
    if new.attrs.contains(Attrs::STRIKETHROUGH) {
        out.push_str("\x1b[9m");
    }
    match new.fg {
        Color::Default => {}
        Color::Indexed(n @ 0..=7) => {
            let _ = write!(out, "\x1b[{}m", 30 + n);
        }
        Color::Indexed(n @ 8..=15) => {
            let _ = write!(out, "\x1b[{}m", 90 + (n - 8));
        }
        Color::Indexed(n) => {
            let _ = write!(out, "\x1b[38;5;{n}m");
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(out, "\x1b[38;2;{r};{g};{b}m");
        }
    }
    match new.bg {
        Color::Default => {}
        Color::Indexed(n @ 0..=7) => {
            let _ = write!(out, "\x1b[{}m", 40 + n);
        }
        Color::Indexed(n @ 8..=15) => {
            let _ = write!(out, "\x1b[{}m", 100 + (n - 8));
        }
        Color::Indexed(n) => {
            let _ = write!(out, "\x1b[48;5;{n}m");
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(out, "\x1b[48;2;{r};{g};{b}m");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smol_str::SmolStr;

    fn lettered(cells: &[(u16, u16, &str)], rows: u16, cols: u16) -> VirtualScreen {
        let mut v = VirtualScreen::blank(rows, cols);
        for (r, c, s) in cells {
            let cell = Cell {
                grapheme: SmolStr::new(*s),
                ..Cell::default()
            };
            v.put(*r, *c, cell);
        }
        v
    }

    #[test]
    fn first_render_full_repaint() {
        let mut d = DiffRenderer::new();
        let v = lettered(&[(0, 0, "A")], 2, 2);
        let bytes = d.render(&v);
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.starts_with("\x1b[2J\x1b[H"), "expected initial clear: {s:?}");
        assert!(s.contains("A"));
    }

    #[test]
    fn second_render_no_change_emits_only_cursor_reset() {
        let mut d = DiffRenderer::new();
        let v = lettered(&[(0, 0, "A")], 2, 2);
        let _ = d.render(&v);
        let bytes = d.render(&v);
        let s = String::from_utf8_lossy(&bytes);
        assert!(
            !s.contains("A"),
            "second render should not re-emit unchanged cells: {s:?}"
        );
    }

    #[test]
    fn changed_cell_emits_cup_for_that_cell() {
        let mut d = DiffRenderer::new();
        let v1 = lettered(&[(0, 0, "A")], 2, 2);
        let v2 = lettered(&[(0, 0, "A"), (1, 1, "B")], 2, 2);
        let _ = d.render(&v1);
        let bytes = d.render(&v2);
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("B"));
        assert!(s.contains("\x1b[2;2H"), "expected CUP to row 2 col 2: {s:?}");
    }

    #[test]
    fn size_change_forces_full_repaint() {
        let mut d = DiffRenderer::new();
        let v1 = lettered(&[(0, 0, "A")], 2, 2);
        let v2 = lettered(&[(0, 0, "A")], 4, 4);
        let _ = d.render(&v1);
        let bytes = d.render(&v2);
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.starts_with("\x1b[2J\x1b[H"));
    }
}
