//! Full pane frames painted into a VirtualScreen: an outer frame around the
//! whole pane band, single-line separators between adjacent panes, box-drawing
//! joints where lines meet, and an optional title on each pane's top edge.
//!
//! The model is connectivity-based: a *border cell* is any cell inside the
//! band that is not inside a pane's content rect (the frame perimeter plus the
//! inter-pane gaps). Each border cell's glyph is chosen from which of its four
//! neighbours are also border cells, which yields correct corners, tees, and
//! crosses uniformly for the frame and the separators.

use crate::{rect::Rect, virtual_screen::VirtualScreen};
use plexy_glass_emulator::{Attrs, Cell, Color, graphemes_with_width};
use smol_str::SmolStr;

/// One pane to frame: its content `rect` (in the same physical coordinate
/// space as `band`), whether it is the active pane, and an optional title to
/// paint on its top edge.
pub struct PaneFrame<'a> {
    pub rect: Rect,
    pub active: bool,
    pub title: Option<&'a str>,
}

/// Paint the frame, separators, and titles for `frames` within `band` (the
/// physical rectangle enclosing every pane). Border cells adjacent to the
/// active pane get a brighter attribute.
pub fn draw(frames: &[PaneFrame<'_>], band: Rect, screen: &mut VirtualScreen) {
    let rects: Vec<Rect> = frames.iter().map(|f| f.rect).collect();
    let active_rect = frames.iter().find(|f| f.active).map(|f| f.rect);

    for r in band.row..=band.bottom_edge_row() {
        for c in band.col..=band.right_edge_col() {
            if !is_border(r, c, band, &rects) {
                continue;
            }
            let n = r > band.row && is_border(r - 1, c, band, &rects);
            let s = r < band.bottom_edge_row() && is_border(r + 1, c, band, &rects);
            let w = c > band.col && is_border(r, c - 1, band, &rects);
            let e = c < band.right_edge_col() && is_border(r, c + 1, band, &rects);
            let glyph = box_glyph(n, s, e, w);
            let mut cell = Cell { grapheme: SmolStr::new(glyph), ..Cell::default() };
            if let Some(ar) = active_rect
                && touches(r, c, ar)
            {
                cell.attrs = Attrs::BOLD;
                cell.fg = Color::Indexed(12); // bright blue
            }
            screen.put(r, c, cell);
        }
    }

    // Titles, painted over the border on each pane's top edge.
    for f in frames {
        let Some(title) = f.title.filter(|t| !t.is_empty()) else {
            continue;
        };
        if f.rect.row == 0 {
            continue; // no border row above (shouldn't happen with an inset band)
        }
        let title_row = f.rect.row - 1;
        // Start two cells in; clip to the pane width.
        let start = f.rect.col.saturating_add(1);
        let max_col = f.rect.right_edge_col();
        let active = active_rect == Some(f.rect);
        paint_title(screen, title_row, start, max_col, title, active);
    }
}

/// A cell is a border cell when it is inside the band but not inside any pane.
fn is_border(r: u16, c: u16, band: Rect, rects: &[Rect]) -> bool {
    if !band.contains(r, c) {
        return false;
    }
    !rects.iter().any(|rect| rect.contains(r, c))
}

/// Whether `(r, c)` lies on `rect`'s one-cell border ring, the cells of the
/// frame box immediately surrounding the pane, corners included. A border cell
/// in this ring belongs to the pane's frame and gets the active highlight.
fn touches(r: u16, c: u16, rect: Rect) -> bool {
    let top = rect.row.saturating_sub(1);
    let bottom = rect.bottom_edge_row().saturating_add(1);
    let left = rect.col.saturating_sub(1);
    let right = rect.right_edge_col().saturating_add(1);
    r >= top && r <= bottom && c >= left && c <= right
}

fn box_glyph(n: bool, s: bool, e: bool, w: bool) -> &'static str {
    match (n, s, e, w) {
        (true, true, true, true) => "\u{253c}",   // ┼
        (true, true, true, false) => "\u{251c}",  // ├
        (true, true, false, true) => "\u{2524}",  // ┤
        (true, false, true, true) => "\u{2534}",  // ┴
        (false, true, true, true) => "\u{252c}",  // ┬
        (false, true, true, false) => "\u{250c}", // ┌
        (false, true, false, true) => "\u{2510}", // ┐
        (true, false, true, false) => "\u{2514}", // └
        (true, false, false, true) => "\u{2518}", // ┘
        (true, true, false, false) => "\u{2502}", // │ (vertical, incl. dangling)
        (true, false, false, false) => "\u{2502}",
        (false, true, false, false) => "\u{2502}",
        (false, false, true, true) => "\u{2500}", // ─ (horizontal, incl. dangling)
        (false, false, true, false) => "\u{2500}",
        (false, false, false, true) => "\u{2500}",
        (false, false, false, false) => " ",
    }
}

fn paint_title(
    screen: &mut VirtualScreen,
    row: u16,
    start: u16,
    max_col: u16,
    title: &str,
    active: bool,
) {
    // " name " reads cleanly against the border line.
    let text = format!(" {title} ");
    let mut c = start;
    for (g, w) in graphemes_with_width(&text) {
        // `max_col` is inclusive here (the last border column the title may use).
        if c > max_col || c >= screen.cols {
            break;
        }
        if w == 2 && (c + 1 > max_col || c + 1 >= screen.cols) {
            break; // don't split a wide grapheme across the edge
        }
        let mut cell = Cell { grapheme: SmolStr::new(g), ..Cell::default() };
        if active {
            cell.attrs = Attrs::BOLD;
            cell.fg = Color::Indexed(12);
        }
        screen.put(row, c, cell);
        if w == 2 {
            screen.put(row, c + 1, Cell::wide_spacer());
        }
        c += w;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(rect: Rect, active: bool, title: Option<&str>) -> PaneFrame<'_> {
        PaneFrame { rect, active, title }
    }

    #[test]
    fn single_pane_gets_a_full_frame() {
        // Band 5x7; one pane inset to (1,1) sized 3x5.
        let band = Rect::new(0, 0, 5, 7);
        let pane = Rect::new(1, 1, 3, 5);
        let mut screen = VirtualScreen::blank(5, 7);
        draw(&[frame(pane, false, None)], band, &mut screen);
        assert_eq!(screen.cell(0, 0).unwrap().grapheme.as_str(), "\u{250c}"); // ┌
        assert_eq!(screen.cell(0, 6).unwrap().grapheme.as_str(), "\u{2510}"); // ┐
        assert_eq!(screen.cell(4, 0).unwrap().grapheme.as_str(), "\u{2514}"); // └
        assert_eq!(screen.cell(4, 6).unwrap().grapheme.as_str(), "\u{2518}"); // ┘
        assert_eq!(screen.cell(0, 3).unwrap().grapheme.as_str(), "\u{2500}"); // ─ top edge
        assert_eq!(screen.cell(2, 0).unwrap().grapheme.as_str(), "\u{2502}"); // │ left edge
    }

    #[test]
    fn side_by_side_panes_share_a_tee_jointed_separator() {
        // Band 5x9; two panes inset, separated by a vertical line at col 4.
        let band = Rect::new(0, 0, 5, 9);
        let left = Rect::new(1, 1, 3, 3); // cols 1..=3
        let right = Rect::new(1, 5, 3, 3); // cols 5..=7, gap at col 4
        let mut screen = VirtualScreen::blank(5, 9);
        draw(&[frame(left, false, None), frame(right, false, None)], band, &mut screen);
        // Top of the separator meets the top frame as a ┬.
        assert_eq!(screen.cell(0, 4).unwrap().grapheme.as_str(), "\u{252c}");
        // Middle of the separator is a vertical line.
        assert_eq!(screen.cell(2, 4).unwrap().grapheme.as_str(), "\u{2502}");
        // Bottom meets the bottom frame as a ┴.
        assert_eq!(screen.cell(4, 4).unwrap().grapheme.as_str(), "\u{2534}");
    }

    #[test]
    fn title_paints_on_top_edge() {
        let band = Rect::new(0, 0, 5, 12);
        let pane = Rect::new(1, 1, 3, 10);
        let mut screen = VirtualScreen::blank(5, 12);
        draw(&[frame(pane, false, Some("ed"))], band, &mut screen);
        // Title " ed " starts two cells in (col 2): space, e, d, space.
        assert_eq!(screen.cell(0, 3).unwrap().grapheme.as_str(), "e");
        assert_eq!(screen.cell(0, 4).unwrap().grapheme.as_str(), "d");
    }

    #[test]
    fn active_pane_frame_highlights_corners() {
        use plexy_glass_emulator::Attrs;
        let band = Rect::new(0, 0, 5, 7);
        let pane = Rect::new(1, 1, 3, 5);
        let mut screen = VirtualScreen::blank(5, 7);
        draw(&[frame(pane, true, None)], band, &mut screen);
        // The active pane's frame is bold all the way around, corners included.
        for (r, c) in [(0u16, 0u16), (0, 6), (4, 0), (4, 6)] {
            assert!(
                screen.cell(r, c).unwrap().attrs.contains(Attrs::BOLD),
                "active frame corner ({r},{c}) should be bold"
            );
        }
    }

    #[test]
    fn untitled_pane_keeps_a_plain_top_border() {
        let band = Rect::new(0, 0, 5, 7);
        let pane = Rect::new(1, 1, 3, 5);
        let mut screen = VirtualScreen::blank(5, 7);
        draw(&[frame(pane, false, None)], band, &mut screen);
        assert_eq!(screen.cell(0, 3).unwrap().grapheme.as_str(), "\u{2500}");
    }
}
