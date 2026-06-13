//! Cell-level diff renderer: compares the current `VirtualScreen` against the
//! previous one and emits minimal ANSI to bring the host TTY up to date.

use crate::virtual_screen::VirtualScreen;
use plexy_glass_emulator::{Attrs, Cell, Color, UnderlineStyle};
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
                    let w = plexy_glass_emulator::grapheme_advance(cell.grapheme.as_str());
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
                        let w = plexy_glass_emulator::grapheme_advance(cell.grapheme.as_str());
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CellAttrs {
    fg: Color,
    bg: Color,
    underline_color: Color,
    underline_style: UnderlineStyle,
    attrs: Attrs,
}

impl CellAttrs {
    fn from_cell(c: &Cell) -> Self {
        Self {
            fg: c.fg,
            bg: c.bg,
            underline_color: c.underline_color,
            underline_style: c.underline_style,
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
    // Underline: re-emit the styled form (`4:N`) so undercurl/dotted/dashed
    // survive to the outer terminal instead of flattening to a plain underline.
    // `Single` uses bare `4m` for back-compat with terminals that don't grok the
    // colon sub-parameter. `None` emits nothing, the `\x1b[0m` prefix above
    // already reset the underline. If `UNDERLINE` is set but the style is `None`
    // (shouldn't normally happen), fall back to a plain underline.
    if new.attrs.contains(Attrs::UNDERLINE) {
        match new.underline_style {
            UnderlineStyle::None | UnderlineStyle::Single => out.push_str("\x1b[4m"),
            UnderlineStyle::Double => out.push_str("\x1b[4:2m"),
            UnderlineStyle::Curly => out.push_str("\x1b[4:3m"),
            UnderlineStyle::Dotted => out.push_str("\x1b[4:4m"),
            UnderlineStyle::Dashed => out.push_str("\x1b[4:5m"),
        }
    }
    if new.attrs.contains(Attrs::REVERSE) {
        out.push_str("\x1b[7m");
    }
    if new.attrs.contains(Attrs::HIGHLIGHT) {
        // Bright-yellow background (16-colour) distinguishes search matches
        // from REVERSE-based copy-mode selection.
        out.push_str("\x1b[103m");
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
    // Underline color (SGR 58). The `\x1b[0m` prefix earlier in this function
    // already reset it, so only a non-default value needs emitting. Use the colon
    // form (58:5:n / 58:2:r:g:b) for widest support; the outer terminal ignores it
    // when it draws no underline.
    match new.underline_color {
        Color::Default => {}
        Color::Indexed(n) => {
            let _ = write!(out, "\x1b[58:5:{n}m");
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(out, "\x1b[58:2:{r}:{g}:{b}m");
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

    #[test]
    fn wide_grapheme_full_repaint_skips_spacer() {
        let mut d = DiffRenderer::new();
        let mut v = VirtualScreen::blank(1, 4);
        v.put(0, 0, Cell { grapheme: SmolStr::new("世"), ..Cell::default() });
        v.put(0, 1, Cell::wide_spacer());
        v.put(0, 2, Cell { grapheme: SmolStr::new("X"), ..Cell::default() });
        let bytes = d.render(&v);
        let s = String::from_utf8_lossy(&bytes);
        assert_eq!(s.matches('世').count(), 1, "wide grapheme emitted once: {s:?}");
        assert!(s.contains("世X"), "spacer skipped; X immediately follows 世: {s:?}");
        assert!(!s.contains("世 X"), "no stray space painted for the spacer: {s:?}");
    }

    #[test]
    fn wide_grapheme_incremental_diff_targets_only_changed_cell() {
        let mut d = DiffRenderer::new();
        let mut v1 = VirtualScreen::blank(1, 4);
        v1.put(0, 0, Cell { grapheme: SmolStr::new("世"), ..Cell::default() });
        v1.put(0, 1, Cell::wide_spacer());
        v1.put(0, 2, Cell { grapheme: SmolStr::new("a"), ..Cell::default() });
        let _ = d.render(&v1);
        let mut v2 = VirtualScreen::blank(1, 4);
        v2.put(0, 0, Cell { grapheme: SmolStr::new("世"), ..Cell::default() });
        v2.put(0, 1, Cell::wide_spacer());
        v2.put(0, 2, Cell { grapheme: SmolStr::new("b"), ..Cell::default() });
        let bytes = d.render(&v2);
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains('b'), "changed cell emitted: {s:?}");
        assert!(!s.contains('世'), "unchanged wide grapheme not re-emitted: {s:?}");
        assert!(s.contains("\x1b[1;3H"), "CUP targets the changed cell at col 3: {s:?}");
    }

    #[test]
    fn underline_color_rgb_emits_58_2() {
        let mut d = DiffRenderer::new();
        let mut v = VirtualScreen::blank(1, 2);
        v.put(0, 0, Cell { grapheme: SmolStr::new("U"), underline_color: Color::Rgb(10, 20, 30), ..Cell::default() });
        let bytes = d.render(&v);
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("\x1b[58:2:10:20:30m"), "expected RGB underline-color SGR: {s:?}");
    }

    #[test]
    fn underline_color_indexed_emits_58_5() {
        let mut d = DiffRenderer::new();
        let mut v = VirtualScreen::blank(1, 2);
        v.put(0, 0, Cell { grapheme: SmolStr::new("U"), underline_color: Color::Indexed(9), ..Cell::default() });
        let bytes = d.render(&v);
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("\x1b[58:5:9m"), "expected indexed underline-color SGR: {s:?}");
    }

    #[test]
    fn default_underline_color_emits_no_58() {
        let mut d = DiffRenderer::new();
        let v = lettered(&[(0, 0, "A")], 1, 2);
        let bytes = d.render(&v);
        let s = String::from_utf8_lossy(&bytes);
        assert!(!s.contains("\x1b[58"), "default underline color must emit no 58: {s:?}");
    }

    #[test]
    fn underline_style_curly_emits_4_3() {
        let mut d = DiffRenderer::new();
        let mut v = VirtualScreen::blank(1, 2);
        v.put(0, 0, Cell {
            grapheme: SmolStr::new("U"),
            attrs: Attrs::UNDERLINE,
            underline_style: UnderlineStyle::Curly,
            ..Cell::default()
        });
        let bytes = d.render(&v);
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("\x1b[4:3m"), "expected curly underline SGR: {s:?}");
    }

    #[test]
    fn underline_style_single_emits_plain_4() {
        let mut d = DiffRenderer::new();
        let mut v = VirtualScreen::blank(1, 2);
        v.put(0, 0, Cell {
            grapheme: SmolStr::new("U"),
            attrs: Attrs::UNDERLINE,
            underline_style: UnderlineStyle::Single,
            ..Cell::default()
        });
        let bytes = d.render(&v);
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("\x1b[4m"), "expected plain underline SGR: {s:?}");
        assert!(!s.contains("\x1b[4:"), "single must not emit a colon form: {s:?}");
    }

    #[test]
    fn no_underline_emits_no_4() {
        let mut d = DiffRenderer::new();
        let v = lettered(&[(0, 0, "A")], 1, 2);
        let bytes = d.render(&v);
        let s = String::from_utf8_lossy(&bytes);
        assert!(!s.contains("\x1b[4m"), "no-underline cell must emit no 4: {s:?}");
        assert!(!s.contains("\x1b[4:"), "no-underline cell must emit no 4:N: {s:?}");
    }

    #[test]
    fn underline_style_change_in_diff_emits_4_3() {
        // Exercise the incremental diff path: a cell that gains a curly underline
        // on a later render must emit 4:3, proving `CellAttrs` tracks
        // `underline_style`.
        let mut d = DiffRenderer::new();
        let v1 = VirtualScreen::blank(1, 2);
        let _ = d.render(&v1);
        let mut v2 = VirtualScreen::blank(1, 2);
        v2.put(0, 0, Cell {
            grapheme: SmolStr::new("U"),
            attrs: Attrs::UNDERLINE,
            underline_style: UnderlineStyle::Curly,
            ..Cell::default()
        });
        let bytes = d.render(&v2);
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("\x1b[4:3m"), "diff path must emit 4:3: {s:?}");
    }

    #[test]
    fn underline_color_change_in_diff_emits_58() {
        // Exercise the incremental diff path (not just full repaint): a cell that
        // gains an underline color on a later render must emit SGR 58, proving
        // that `CellAttrs` `PartialEq` + `from_cell` track `underline_color`.
        let mut d = DiffRenderer::new();
        let v1 = VirtualScreen::blank(1, 2);
        let _ = d.render(&v1);
        let mut v2 = VirtualScreen::blank(1, 2);
        v2.put(
            0,
            0,
            Cell {
                grapheme: SmolStr::new("U"),
                underline_color: Color::Rgb(1, 2, 3),
                ..Cell::default()
            },
        );
        let bytes = d.render(&v2);
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("\x1b[58:2:1:2:3m"), "diff path must emit 58: {s:?}");
    }
}
