//! Single-line pane separators painted into a VirtualScreen.

use crate::{rect::Rect, virtual_screen::VirtualScreen};
use plexy_glass_emulator::{Attrs, Cell, Color};
use smol_str::SmolStr;

/// Paint borders around every rect in `rects`. Adjacent rects that touch at an
/// edge get a single-cell-wide separator between them; the outer edges are not
/// painted. `active_rect` (if any) gets a brighter border attribute.
pub fn draw(rects: &[(Rect, bool)], screen: &mut VirtualScreen) {
    for r in 0..screen.rows {
        for c in 0..screen.cols {
            if let Some((kind, active)) = border_kind_at(rects, r, c) {
                let ch = match kind {
                    BorderKind::Vertical => "│",
                    BorderKind::Horizontal => "─",
                    BorderKind::Cross => "┼",
                };
                let mut cell = Cell {
                    grapheme: SmolStr::new(ch),
                    ..Cell::default()
                };
                if active {
                    cell.attrs = Attrs::BOLD;
                    cell.fg = Color::Indexed(12); // bright blue
                }
                screen.put(r, c, cell);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BorderKind {
    Vertical,
    Horizontal,
    Cross,
}

fn border_kind_at(rects: &[(Rect, bool)], r: u16, c: u16) -> Option<(BorderKind, bool)> {
    let mut is_vertical = false;
    let mut is_horizontal = false;
    let mut active = false;

    for (rect, _is_active) in rects {
        if rect.contains(r, c) {
            return None;
        }
    }

    for (rect, is_active) in rects {
        if r >= rect.row
            && r <= rect.bottom_edge_row()
            && (c + 1 == rect.col || c == rect.right_edge_col().saturating_add(1))
        {
            is_vertical = true;
            if *is_active {
                active = true;
            }
        }
        if c >= rect.col
            && c <= rect.right_edge_col()
            && (r + 1 == rect.row || r == rect.bottom_edge_row().saturating_add(1))
        {
            is_horizontal = true;
            if *is_active {
                active = true;
            }
        }
    }

    match (is_vertical, is_horizontal) {
        (true, true) => Some((BorderKind::Cross, active)),
        (true, false) => Some((BorderKind::Vertical, active)),
        (false, true) => Some((BorderKind::Horizontal, active)),
        (false, false) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertical_separator_between_side_by_side_rects() {
        let left = Rect::new(0, 0, 4, 3);
        let right = Rect::new(0, 4, 4, 3);
        let mut screen = VirtualScreen::blank(4, 7);
        draw(&[(left, false), (right, false)], &mut screen);
        for r in 0..4 {
            assert_eq!(screen.cell(r, 3).unwrap().grapheme.as_str(), "│");
        }
        for r in 0..4 {
            assert!(screen.cell(r, 0).unwrap().is_blank());
            assert!(screen.cell(r, 6).unwrap().is_blank());
        }
    }

    #[test]
    fn horizontal_separator_between_stacked_rects() {
        let top = Rect::new(0, 0, 2, 5);
        let bot = Rect::new(3, 0, 2, 5);
        let mut screen = VirtualScreen::blank(5, 5);
        draw(&[(top, false), (bot, false)], &mut screen);
        for c in 0..5 {
            assert_eq!(screen.cell(2, c).unwrap().grapheme.as_str(), "─");
        }
    }

    #[test]
    fn active_border_uses_bold_attr() {
        let left = Rect::new(0, 0, 4, 3);
        let right = Rect::new(0, 4, 4, 3);
        let mut screen = VirtualScreen::blank(4, 7);
        draw(&[(left, true), (right, false)], &mut screen);
        let sep = screen.cell(2, 3).unwrap();
        assert_eq!(sep.grapheme.as_str(), "│");
        assert!(sep.attrs.contains(Attrs::BOLD));
    }
}
