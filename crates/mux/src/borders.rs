//! Full pane frames painted into a VirtualScreen: an outer frame around the
//! whole pane band, single-line separators between adjacent panes, box-drawing
//! joints where lines meet, and an optional title on each pane's top edge.
//!
//! The model is connectivity-based: a *border cell* is any cell inside the
//! band that is not inside a pane's content rect (the frame perimeter plus the
//! inter-pane gaps). Each border cell's glyph is chosen from which of its four
//! neighbours are also border cells, which yields correct corners, tees, and
//! crosses uniformly for the frame and the separators.

use std::time::Duration;

use plexy_glass_emulator::{Attrs, Cell, Color, graphemes_with_width};
use smol_str::SmolStr;

use crate::blocks::BlockLineStatus;
use crate::compositor::PaneDragRole;
use crate::rect::{Point, Rect};
use crate::virtual_screen::VirtualScreen;

/// Border color of the pane being dragged (source) during a pane-swap drag.
pub(crate) const SOURCE_DRAG_COLOR: u8 = 14; // bright cyan
/// Border color of the pane under the cursor (target) during a pane-swap drag.
pub(crate) const TARGET_DRAG_COLOR: u8 = 10; // bright green

/// Palette-resolved colors for the pane border rings (active focus, marked pane,
/// and the two pane-swap drag roles). Resolved from config by the coordinator so
/// the rings match the theme instead of clashing fixed ANSI indices.
#[derive(Clone, Copy)]
pub struct RingColors {
    pub active: Color,
    pub marked: Color,
    pub drag_source: Color,
    pub drag_target: Color,
}

impl RingColors {
    /// The historical fixed ANSI ring colors (bright blue / magenta / cyan /
    /// green). Fallback + test default; production drives these from the palette.
    pub const fn ansi_default() -> Self {
        Self {
            active: Color::Indexed(12),
            marked: Color::Indexed(13),
            drag_source: Color::Indexed(SOURCE_DRAG_COLOR),
            drag_target: Color::Indexed(TARGET_DRAG_COLOR),
        }
    }
}

/// Colors used to paint block exit-status segments on a pane's left border, plus
/// the per-frame policy for the other block annotations (duration + sticky
/// header). Carried together because all three are gated by `blocks.enabled` and
/// reuse the same `ok`/`fail` colors; `None`/`false` here means the feature is off.
pub struct BlockBorderColors {
    /// Foreground color for a successfully completed block row (exit code 0).
    pub ok: Color,
    /// Foreground color for a failed block row (nonzero exit code). Also
    /// triggers a `│` → `▐` glyph replacement on plain vertical segments.
    pub fail: Color,
    /// Minimum duration to show the inline/header duration annotation; `None`
    /// disables the duration feature.
    pub duration_threshold: Option<Duration>,
    /// Pin the command line at the pane top when its block's output has scrolled
    /// above the viewport (live view only).
    pub sticky_header: bool,
}

/// A selected command block to outline with a capped bracket on the pane's
/// left border column (block mode). `rows` are viewport-relative row indices
/// (inclusive), same basis as [`PaneFrame::block_rows`]. Caps are drawn only
/// when the real block boundary is in view, so a scrolled block reads as
/// continuing past the edge.
pub struct SelectedBlock {
    pub rows: (u16, u16),
    pub cap_top: bool,
    pub cap_bottom: bool,
    pub color: Color,
}

/// One pane to frame: its content `rect` (in the same physical coordinate
/// space as `band`), whether it is the active pane, and an optional title to
/// paint on its top edge.
pub struct PaneFrame<'a> {
    pub rect: Rect,
    pub active: bool,
    /// The session's marked pane (join/swap target). Its border ring is colored
    /// distinctly (bright magenta), independent of `active` and of whether the
    /// pane has a `title`, so an unnamed marked pane is still clearly indicated.
    pub marked: bool,
    /// Whether this pane is the source or target of an in-progress pane-swap
    /// drag. `PaneDragRole::None` for panes not involved in a drag.
    pub drag_role: PaneDragRole,
    pub title: Option<&'a str>,
    /// Per-viewport-row block exit status, indexed by `r` in `0..rect.rows`.
    /// An empty vec means no block painting for this pane (feature disabled or
    /// not yet computed). Length must equal `rect.rows` when non-empty.
    pub block_rows: Vec<Option<BlockLineStatus>>,
    /// When `Some`, paint a capped selection bracket on this pane's left
    /// border for the given viewport rows (block mode). Highest precedence on
    /// those cells.
    pub selected_block: Option<SelectedBlock>,
}

/// Paint the frame, separators, and titles for `frames` within `band` (the
/// physical rectangle enclosing every pane). Border cells adjacent to the
/// active pane get a brighter attribute.
///
/// `blocks` enables block exit-status coloring on each pane's **left segment**
/// (`c == rect.col - 1`, `rect.row <= r < rect.row + rect.rows`).
/// Passing `None` skips all block work for this frame (zero cost; feature
/// disabled or `enabled #false`).
///
/// Precedence per cell (highest wins):
///   0. Selected-block bracket (block mode: color + `┏`/`┃`/`┗` glyph)
///   1. Marked ring (color + glyph; no `▐` ever on a marked ring)
///   2. Block status (ok/fail fg; fail replaces `│` → `▐` when plain vertical)
///   3. Active ring (bright blue)
pub fn draw(
    frames: &[PaneFrame<'_>],
    band: Rect,
    screen: &mut VirtualScreen,
    blocks: Option<&BlockBorderColors>,
    rings: RingColors,
) {
    let rects: Vec<Rect> = frames.iter().map(|f| f.rect).collect();
    let active_rect = frames.iter().find(|f| f.active).map(|f| f.rect);
    let marked_rect = frames.iter().find(|f| f.marked).map(|f| f.rect);
    let source_rect = frames
        .iter()
        .find(|f| matches!(f.drag_role, PaneDragRole::Source))
        .map(|f| f.rect);
    let target_rect = frames
        .iter()
        .find(|f| matches!(f.drag_role, PaneDragRole::Target))
        .map(|f| f.rect);

    for r in band.row()..=band.bottom_edge_row() {
        for c in band.col()..=band.right_edge_col() {
            if !is_border(r, c, band, &rects) {
                continue;
            }
            let n = r > band.row() && is_border(r - 1, c, band, &rects);
            // Equivalent note (`s`: `< → <=`): `is_border(r+1, ...)` calls
            // `band.contains(r+1, c)` which is false when r = bottom_edge_row,
            // so `r < B` and `r <= B` both produce `s = false` at the boundary.
            let s = r < band.bottom_edge_row() && is_border(r + 1, c, band, &rects);
            let w = c > band.col() && is_border(r, c - 1, band, &rects);
            // Equivalent note (`e`: `< → <=`): same argument for columns, when
            // c = right_edge_col, `is_border(r, c+1, ...)` returns false because
            // c+1 is outside the band, so `<` and `<=` give the same `e` value.
            let e = c < band.right_edge_col() && is_border(r, c + 1, band, &rects);
            let glyph = box_glyph(n, s, e, w);
            let mut cell = Cell {
                grapheme: SmolStr::new(glyph),
                ..Cell::default()
            };

            // Precedence: selected bracket > drag source/target > marked > block status > active.
            // Selected-block bracket (block mode): color + ┏/┃/┗ glyph.
            if let Some((color, glyph)) = selected_bracket(r, c, frames) {
                cell.attrs = Attrs::BOLD;
                cell.fg = color;
                cell.grapheme = SmolStr::new_static(glyph);
            } else if let Some(sr) = source_rect
                && touches(r, c, sr)
            {
                cell.attrs = Attrs::BOLD;
                cell.fg = rings.drag_source;
            } else if let Some(tr) = target_rect
                && touches(r, c, tr)
            {
                cell.attrs = Attrs::BOLD;
                cell.fg = rings.drag_target;
            } else if let Some(mr) = marked_rect
                && touches(r, c, mr)
            {
                cell.attrs = Attrs::BOLD;
                cell.fg = rings.marked;
            } else if let Some(colors) = blocks
                && let Some(status) = left_segment_status(r, c, frames)
            {
                // Block status takes precedence over the active ring.
                match status {
                    BlockLineStatus::Ok => {
                        cell.fg = colors.ok;
                        // Parity with Failed: a plain vertical `│` becomes the
                        // half-block `▐` so a passing block reads as a solid
                        // bar, not a faint line. Color carries pass/fail.
                        if glyph == "\u{2502}" {
                            // │ → ▐
                            cell.grapheme = SmolStr::new_static("\u{2590}");
                        }
                    }
                    BlockLineStatus::Failed => {
                        cell.fg = colors.fail;
                        // Replace a plain vertical `│` with the half-block `▐`.
                        // A `┤` keeps its glyph (it's the only other glyph possible on the
                        // left segment, since its east neighbour is always pane content).
                        if glyph == "\u{2502}" {
                            // │ → ▐
                            cell.grapheme = SmolStr::new_static("\u{2590}");
                        }
                    }
                }
            } else if let Some(ar) = active_rect
                && touches(r, c, ar)
            {
                cell.attrs = Attrs::BOLD;
                cell.fg = rings.active;
            }
            screen.put(r, c, cell);
        }
    }

    // Titles, painted over the border on each pane's top edge.
    for f in frames {
        let Some(title) = f.title.filter(|t| !t.is_empty()) else {
            continue;
        };
        if f.rect.row() == 0 {
            continue; // no border row above (shouldn't happen with an inset band)
        }
        let title_row = f.rect.row() - 1;
        // Start two cells in; clip to the pane width.
        let start = f.rect.col().saturating_add(1);
        let max_col = f.rect.right_edge_col();
        let active_color = (active_rect == Some(f.rect)).then_some(rings.active);
        paint_title(screen, title_row, start, max_col, title, active_color);
    }
}

/// Returns the `BlockLineStatus` for cell `(r, c)` if it lies in exactly one
/// pane's **left segment** and that pane's `block_rows` has a status for the
/// corresponding row.
///
/// The left segment of pane P is the column `P.rect.col - 1` for rows
/// `P.rect.row .. P.rect.row + P.rect.rows`. At most one pane can claim any
/// cell (rects don't overlap).
fn left_segment_status(r: u16, c: u16, frames: &[PaneFrame<'_>]) -> Option<BlockLineStatus> {
    for f in frames {
        if f.block_rows.is_empty() {
            continue;
        }
        // Left segment column is rect.col - 1; skip this frame if the pane is
        // flush against the left edge (col=0 means no border column to the left).
        let Some(left_col) = f.rect.col().checked_sub(1) else {
            continue;
        };
        if c != left_col {
            continue;
        }
        if r < f.rect.row() || r >= f.rect.row().saturating_add(f.rect.rows()) {
            continue;
        }
        let row_idx = (r - f.rect.row()) as usize;
        return f.block_rows.get(row_idx).and_then(|s| *s);
    }
    None
}

/// The selection-bracket color + glyph for cell `(r, c)` if it lies on the
/// left border column of a pane whose `selected_block` covers row `r`. Glyphs:
/// `┏` at the top row (when `cap_top`), `┗` at the bottom (when `cap_bottom`),
/// `┃` elsewhere.
fn selected_bracket(r: u16, c: u16, frames: &[PaneFrame<'_>]) -> Option<(Color, &'static str)> {
    for f in frames {
        let Some(sel) = &f.selected_block else {
            continue;
        };
        let Some(left_col) = f.rect.col().checked_sub(1) else {
            continue;
        };
        if c != left_col {
            continue;
        }
        let top_abs = f.rect.row().saturating_add(sel.rows.0);
        let bot_abs = f.rect.row().saturating_add(sel.rows.1);
        if r < top_abs || r > bot_abs {
            continue;
        }
        let glyph = if r == top_abs && sel.cap_top {
            "\u{250f}" // ┏
        } else if r == bot_abs && sel.cap_bottom {
            "\u{2517}" // ┗
        } else {
            "\u{2503}" // ┃
        };
        return Some((sel.color, glyph));
    }
    None
}

/// A cell is a border cell when it is inside the band but not inside any pane.
fn is_border(r: u16, c: u16, band: Rect, rects: &[Rect]) -> bool {
    if !band.contains(Point::new(r, c)) {
        return false;
    }
    !rects.iter().any(|rect| rect.contains(Point::new(r, c)))
}

/// Whether `(r, c)` lies on `rect`'s one-cell border ring, the cells of the
/// frame box immediately surrounding the pane, corners included. A border cell
/// in this ring belongs to the pane's frame and gets the active highlight.
const fn touches(r: u16, c: u16, rect: Rect) -> bool {
    let top = rect.row().saturating_sub(1);
    let bottom = rect.bottom_edge_row().saturating_add(1);
    let left = rect.col().saturating_sub(1);
    let right = rect.right_edge_col().saturating_add(1);
    r >= top && r <= bottom && c >= left && c <= right
}

const fn box_glyph(n: bool, s: bool, e: bool, w: bool) -> &'static str {
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
        // │ (vertical, incl. dangling)
        (_, true, false, false) | (true, false, false, false) => "\u{2502}",
        // ─ (horizontal, incl. dangling)
        (false, false, _, true) | (false, false, true, false) => "\u{2500}",
        (false, false, false, false) => " ",
    }
}

fn paint_title(
    screen: &mut VirtualScreen,
    row: u16,
    start: u16,
    max_col: u16,
    title: &str,
    active_color: Option<Color>,
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
        let mut cell = Cell {
            grapheme: SmolStr::new(g),
            ..Cell::default()
        };
        if let Some(color) = active_color {
            cell.attrs = Attrs::BOLD;
            cell.fg = color;
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
    use crate::blocks::BlockLineStatus;
    use crate::rect::Size;

    fn frame(rect: Rect, active: bool, title: Option<&str>) -> PaneFrame<'_> {
        PaneFrame {
            rect,
            active,
            marked: false,
            drag_role: PaneDragRole::None,
            title,
            block_rows: vec![],
            selected_block: None,
        }
    }

    fn marked_frame(rect: Rect, active: bool, title: Option<&str>) -> PaneFrame<'_> {
        PaneFrame {
            rect,
            active,
            marked: true,
            drag_role: PaneDragRole::None,
            title,
            block_rows: vec![],
            selected_block: None,
        }
    }

    fn frame_with_blocks(
        rect: Rect,
        active: bool,
        marked: bool,
        block_rows: Vec<Option<BlockLineStatus>>,
    ) -> PaneFrame<'static> {
        PaneFrame {
            rect,
            active,
            marked,
            drag_role: PaneDragRole::None,
            title: None,
            block_rows,
            selected_block: None,
        }
    }

    fn frame_with_selected(rect: Rect, sel: Option<SelectedBlock>) -> PaneFrame<'static> {
        PaneFrame {
            rect,
            active: false,
            marked: false,
            drag_role: PaneDragRole::None,
            title: None,
            block_rows: vec![],
            selected_block: sel,
        }
    }

    fn sel_color() -> Color {
        Color::Rgb(0xdc, 0xa5, 0x61)
    }

    /// A fully-visible block: ┏ cap at top row, ┃ middle, ┗ cap at bottom.
    #[test]
    fn selected_block_draws_capped_bracket() {
        // Band 6x7; pane inset at (1,1) sized 4x5. Left segment = col 0, rows 1..=4.
        let band = Rect::new(Point::new(0, 0), Size::new(6, 7));
        let pane = Rect::new(Point::new(1, 1), Size::new(4, 5));
        let sel = SelectedBlock {
            rows: (0, 3),
            cap_top: true,
            cap_bottom: true,
            color: sel_color(),
        };
        let mut screen = VirtualScreen::blank(6, 7);
        draw(
            &[frame_with_selected(pane, Some(sel))],
            band,
            &mut screen,
            None,
            RingColors::ansi_default(),
        );
        assert_eq!(
            screen.cell(1, 0).unwrap().grapheme.as_str(),
            "\u{250f}",
            "top cap ┏"
        );
        assert_eq!(
            screen.cell(2, 0).unwrap().grapheme.as_str(),
            "\u{2503}",
            "middle ┃"
        );
        assert_eq!(
            screen.cell(4, 0).unwrap().grapheme.as_str(),
            "\u{2517}",
            "bottom cap ┗"
        );
        assert_eq!(
            screen.cell(2, 0).unwrap().fg,
            sel_color(),
            "bracket fg = selection color"
        );
    }

    /// Off-screen top: no ┏ cap, ┃ continues to the visible top row.
    #[test]
    fn selected_block_no_top_cap_when_scrolled() {
        let band = Rect::new(Point::new(0, 0), Size::new(6, 7));
        let pane = Rect::new(Point::new(1, 1), Size::new(4, 5));
        let sel = SelectedBlock {
            rows: (0, 3),
            cap_top: false,
            cap_bottom: true,
            color: sel_color(),
        };
        let mut screen = VirtualScreen::blank(6, 7);
        draw(
            &[frame_with_selected(pane, Some(sel))],
            band,
            &mut screen,
            None,
            RingColors::ansi_default(),
        );
        assert_eq!(
            screen.cell(1, 0).unwrap().grapheme.as_str(),
            "\u{2503}",
            "no cap → ┃"
        );
        assert_eq!(
            screen.cell(4, 0).unwrap().grapheme.as_str(),
            "\u{2517}",
            "bottom cap ┗"
        );
    }

    /// The bracket beats block-status coloring on its rows.
    #[test]
    fn selected_block_beats_block_status() {
        let band = Rect::new(Point::new(0, 0), Size::new(5, 7));
        let pane = Rect::new(Point::new(1, 1), Size::new(3, 5));
        let colors = test_colors();
        let sel = SelectedBlock {
            rows: (0, 2),
            cap_top: true,
            cap_bottom: true,
            color: sel_color(),
        };
        let f = PaneFrame {
            rect: pane,
            active: false,
            marked: false,
            drag_role: PaneDragRole::None,
            title: None,
            block_rows: vec![Some(BlockLineStatus::Failed); 3],
            selected_block: Some(sel),
        };
        let mut screen = VirtualScreen::blank(5, 7);
        draw(
            &[f],
            band,
            &mut screen,
            Some(&colors),
            RingColors::ansi_default(),
        );
        // Mid row: selection color + ┃, NOT the fail color / ▐.
        let cell = screen.cell(2, 0).unwrap();
        assert_eq!(cell.fg, sel_color(), "selection beats fail color");
        assert_eq!(cell.grapheme.as_str(), "\u{2503}", "selection glyph, not ▐");
    }

    fn test_colors() -> BlockBorderColors {
        BlockBorderColors {
            ok: Color::Rgb(135, 169, 135),   // #87a987
            fail: Color::Rgb(196, 116, 110), // #c4746e
            duration_threshold: None,
            sticky_header: false,
        }
    }

    #[test]
    fn single_pane_gets_a_full_frame() {
        // Band 5x7; one pane inset to (1,1) sized 3x5.
        let band = Rect::new(Point::new(0, 0), Size::new(5, 7));
        let pane = Rect::new(Point::new(1, 1), Size::new(3, 5));
        let mut screen = VirtualScreen::blank(5, 7);
        draw(
            &[frame(pane, false, None)],
            band,
            &mut screen,
            None,
            RingColors::ansi_default(),
        );
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
        let band = Rect::new(Point::new(0, 0), Size::new(5, 9));
        let left = Rect::new(Point::new(1, 1), Size::new(3, 3)); // cols 1..=3
        let right = Rect::new(Point::new(1, 5), Size::new(3, 3)); // cols 5..=7, gap at col 4
        let mut screen = VirtualScreen::blank(5, 9);
        draw(
            &[frame(left, false, None), frame(right, false, None)],
            band,
            &mut screen,
            None,
            RingColors::ansi_default(),
        );
        // Top of the separator meets the top frame as a ┬.
        assert_eq!(screen.cell(0, 4).unwrap().grapheme.as_str(), "\u{252c}");
        // Middle of the separator is a vertical line.
        assert_eq!(screen.cell(2, 4).unwrap().grapheme.as_str(), "\u{2502}");
        // Bottom meets the bottom frame as a ┴.
        assert_eq!(screen.cell(4, 4).unwrap().grapheme.as_str(), "\u{2534}");
    }

    #[test]
    fn title_paints_on_top_edge() {
        let band = Rect::new(Point::new(0, 0), Size::new(5, 12));
        let pane = Rect::new(Point::new(1, 1), Size::new(3, 10));
        let mut screen = VirtualScreen::blank(5, 12);
        draw(
            &[frame(pane, false, Some("ed"))],
            band,
            &mut screen,
            None,
            RingColors::ansi_default(),
        );
        // Title " ed " starts two cells in (col 2): space, e, d, space.
        assert_eq!(screen.cell(0, 3).unwrap().grapheme.as_str(), "e");
        assert_eq!(screen.cell(0, 4).unwrap().grapheme.as_str(), "d");
    }

    #[test]
    fn title_paints_wide_grapheme_with_spacer() {
        let band = Rect::new(Point::new(0, 0), Size::new(5, 12));
        let pane = Rect::new(Point::new(1, 1), Size::new(3, 10));
        let mut screen = VirtualScreen::blank(5, 12);
        draw(
            &[frame(pane, false, Some("好"))],
            band,
            &mut screen,
            None,
            RingColors::ansi_default(),
        );
        // Title " 好 ": the wide grapheme occupies its cell plus a wide spacer.
        assert_eq!(screen.cell(0, 3).unwrap().grapheme.as_str(), "好");
        assert!(screen.cell(0, 4).unwrap().is_wide_spacer());
    }

    #[test]
    fn long_title_does_not_overrun_the_right_corner() {
        let band = Rect::new(Point::new(0, 0), Size::new(5, 7));
        let pane = Rect::new(Point::new(1, 1), Size::new(3, 5));
        let mut screen = VirtualScreen::blank(5, 7);
        draw(
            &[frame(pane, false, Some("a very long title"))],
            band,
            &mut screen,
            None,
            RingColors::ansi_default(),
        );
        // The title clips before the right border, so the corner glyph survives.
        assert_eq!(screen.cell(0, 6).unwrap().grapheme.as_str(), "\u{2510}"); // ┐
    }

    #[test]
    fn active_pane_frame_highlights_corners() {
        use plexy_glass_emulator::Attrs;
        let band = Rect::new(Point::new(0, 0), Size::new(5, 7));
        let pane = Rect::new(Point::new(1, 1), Size::new(3, 5));
        let mut screen = VirtualScreen::blank(5, 7);
        draw(
            &[frame(pane, true, None)],
            band,
            &mut screen,
            None,
            RingColors::ansi_default(),
        );
        // The active pane's frame is bold all the way around, corners included.
        for (r, c) in [(0u16, 0u16), (0, 6), (4, 0), (4, 6)] {
            assert!(
                screen.cell(r, c).unwrap().attrs.contains(Attrs::BOLD),
                "active frame corner ({r},{c}) should be bold"
            );
        }
    }

    #[test]
    fn marked_unnamed_pane_gets_a_magenta_ring_with_intact_glyphs() {
        use plexy_glass_emulator::{Attrs, Color};
        // No title, so the marked indicator must be the border color, not a glyph.
        let band = Rect::new(Point::new(0, 0), Size::new(5, 7));
        let pane = Rect::new(Point::new(1, 1), Size::new(3, 5));
        let mut screen = VirtualScreen::blank(5, 7);
        draw(
            &[marked_frame(pane, false, None)],
            band,
            &mut screen,
            None,
            RingColors::ansi_default(),
        );
        // Corners are still correct box glyphs...
        assert_eq!(screen.cell(0, 0).unwrap().grapheme.as_str(), "\u{250c}");
        assert_eq!(screen.cell(4, 6).unwrap().grapheme.as_str(), "\u{2518}");
        // ...and the ring is bright magenta + bold.
        for (r, c) in [(0u16, 0u16), (0, 6), (4, 0), (4, 6), (2, 0)] {
            let cell = screen.cell(r, c).unwrap();
            assert_eq!(
                cell.fg,
                Color::Indexed(13),
                "marked ring at ({r},{c}) is magenta"
            );
            assert!(cell.attrs.contains(Attrs::BOLD));
        }
    }

    #[test]
    fn ring_colors_come_from_the_supplied_palette_not_hardcoded_ansi() {
        use plexy_glass_emulator::Color;
        let band = Rect::new(Point::new(0, 0), Size::new(5, 7));
        let pane = Rect::new(Point::new(1, 1), Size::new(3, 5));
        let mut screen = VirtualScreen::blank(5, 7);
        // A bespoke (non-ANSI) ring palette, what the coordinator passes after
        // resolving `highlight`/`warn`/etc. from config.
        let rings = RingColors {
            active: Color::Rgb(1, 2, 3),
            marked: Color::Rgb(4, 5, 6),
            drag_source: Color::Rgb(7, 8, 9),
            drag_target: Color::Rgb(10, 11, 12),
        };
        draw(&[frame(pane, true, None)], band, &mut screen, None, rings);
        // The active ring uses the supplied RGB, not the old Color::Indexed(12).
        assert_eq!(screen.cell(0, 0).unwrap().fg, Color::Rgb(1, 2, 3));
        assert_ne!(screen.cell(0, 0).unwrap().fg, Color::Indexed(12));
    }

    #[test]
    fn untitled_pane_keeps_a_plain_top_border() {
        let band = Rect::new(Point::new(0, 0), Size::new(5, 7));
        let pane = Rect::new(Point::new(1, 1), Size::new(3, 5));
        let mut screen = VirtualScreen::blank(5, 7);
        draw(
            &[frame(pane, false, None)],
            band,
            &mut screen,
            None,
            RingColors::ansi_default(),
        );
        assert_eq!(screen.cell(0, 3).unwrap().grapheme.as_str(), "\u{2500}");
    }

    #[test]
    fn bottom_interior_edge_is_horizontal_not_tee() {
        // The cells directly below a pane's interior (row=4, cols 1-5 in a 5x7 band
        // with pane at (1,1) 3x5) should be ─ (horizontal).  With `replace - with /`
        // in the north-neighbour check (line 136), `is_border(r, c)` is used instead
        // of `is_border(r-1, c)`, making n=true for those cells and turning ─ into ┴.
        let band = Rect::new(Point::new(0, 0), Size::new(5, 7));
        let pane = Rect::new(Point::new(1, 1), Size::new(3, 5));
        let mut screen = VirtualScreen::blank(5, 7);
        draw(
            &[frame(pane, false, None)],
            band,
            &mut screen,
            None,
            RingColors::ansi_default(),
        );
        // Bottom edge cells at the pane's bottom interior boundary (row 4, cols 1-5).
        // Their north neighbour is inside the pane (row 3 is the last pane row),
        // so `n` must be false → glyph = ─, not ┴.
        assert_eq!(
            screen.cell(4, 1).unwrap().grapheme.as_str(),
            "\u{2500}",
            "bottom-edge col 1"
        );
        assert_eq!(
            screen.cell(4, 3).unwrap().grapheme.as_str(),
            "\u{2500}",
            "bottom-edge col 3"
        );
        assert_eq!(
            screen.cell(4, 5).unwrap().grapheme.as_str(),
            "\u{2500}",
            "bottom-edge col 5"
        );
    }

    #[test]
    fn active_pane_title_gets_active_color() {
        // `active_color = (active_rect == Some(f.rect)).then_some(...)`.  The
        // `== → !=` mutation would apply the active color to the NON-active pane.
        use plexy_glass_emulator::Color;
        let band = Rect::new(Point::new(0, 0), Size::new(5, 12));
        let pane = Rect::new(Point::new(1, 1), Size::new(3, 10));
        let rings = RingColors {
            active: Color::Rgb(255, 0, 0), // red
            marked: Color::Rgb(0, 255, 0),
            drag_source: Color::Rgb(0, 0, 255),
            drag_target: Color::Rgb(128, 128, 128),
        };
        // Active pane with title: title cells should be the active (red) color.
        let mut screen = VirtualScreen::blank(5, 12);
        draw(
            &[frame(pane, true, Some("hi"))],
            band,
            &mut screen,
            None,
            rings,
        );
        // " hi " starts at col 2 (1 for the frame-start + 1 offset), so 'h' is at col 3.
        let title_cell = screen.cell(0, 3).unwrap();
        assert_eq!(title_cell.grapheme.as_str(), "h");
        assert_eq!(
            title_cell.fg,
            Color::Rgb(255, 0, 0),
            "active pane title fg must be active color"
        );

        // Inactive pane with the same title must NOT get the active color.
        let mut screen2 = VirtualScreen::blank(5, 12);
        draw(
            &[frame(pane, false, Some("hi"))],
            band,
            &mut screen2,
            None,
            rings,
        );
        let title_cell2 = screen2.cell(0, 3).unwrap();
        assert_ne!(
            title_cell2.fg,
            Color::Rgb(255, 0, 0),
            "inactive pane title must not be active color"
        );
    }

    #[test]
    fn selected_bracket_does_not_color_cells_outside_block_rows() {
        // The `|| → &&` mutation at line 263 makes `r < top || r > bot` → `&&`
        // which is always false, coloring ALL left-border cells as bracket.
        // Verify cells above and below the block range keep their normal styling.
        let band = Rect::new(Point::new(0, 0), Size::new(8, 7));
        let pane = Rect::new(Point::new(1, 1), Size::new(6, 5)); // rows 1-6, left border at col 0
        // Block covers ONLY rows 1-3 (top at 0+1=1, bot at 2+1=3,
        // cap_top = true, cap_bottom = false).
        let sel = SelectedBlock {
            rows: (0, 2),
            cap_top: true,
            cap_bottom: false,
            color: sel_color(),
        };
        let mut screen = VirtualScreen::blank(8, 7);
        draw(
            &[frame_with_selected(pane, Some(sel))],
            band,
            &mut screen,
            None,
            RingColors::ansi_default(),
        );
        // Row 1 is inside the block range → must be bracket color.
        assert_eq!(
            screen.cell(1, 0).unwrap().fg,
            sel_color(),
            "row 1 in block → bracket color"
        );
        // Row 5 is outside the block range → must NOT be bracket color.
        assert_ne!(
            screen.cell(5, 0).unwrap().fg,
            sel_color(),
            "row 5 outside block must not get bracket"
        );

        // Equivalent notes:
        // - Line 137 (`s`: `< → <=`): `is_border(r+1, ...)` returns false when
        //   r = bottom_edge_row, so `<=` and `<` give the same `s` value.
        // - Line 139 (`e`: `< → <=`): same argument for columns.
        // Both are equivalent because `is_border` bounds-checks the band.
    }

    #[test]
    fn paint_title_clips_wide_grapheme_at_edge() {
        // A wide grapheme (width=2) that straddles the max_col boundary must not
        // be painted; the guard on line 334 must use the right threshold.
        // With `+ → -` or `+ → *` in `c + 1 > max_col`, the check is wrong and
        // the wide grapheme would overrun the border.
        let band = Rect::new(Point::new(0, 0), Size::new(5, 7));
        // Pane at (1,1) 3x3: right border at col 4, pane right edge col is col 3.
        let pane = Rect::new(Point::new(1, 1), Size::new(3, 3)); // cols 1-3, right_edge_col=4
        let mut screen = VirtualScreen::blank(5, 7);
        // "好" (wide, width=2) followed by "x" (narrow): the wide glyph needs cols
        // 2 and 3. start = pane.col + 1 = 2. max_col = pane.right_edge_col() = 4.
        // " 好 " → col 2 = space, col 3+4 = 好+spacer, col 5 = space.
        // max_col = 4. At col 3: c + 1 = 4 = max_col → guard fires, wide NOT painted.
        draw(
            &[frame(pane, false, Some("好"))],
            band,
            &mut screen,
            None,
            RingColors::ansi_default(),
        );
        // The wide grapheme "好" must not overrun max_col=4 (right border at col 4).
        // col 4 must remain the border glyph (┐), not the wide spacer.
        let border_cell = screen.cell(0, 4).unwrap();
        assert!(
            !border_cell.is_wide_spacer(),
            "col 4 must not be wide spacer"
        );
    }

    // ── Block exit-status border segment tests ────────────────────────────────────

    /// Ok status row: left-segment cell gets ok fg AND the heavy `▐` (parity with fail).
    #[test]
    fn block_ok_segment_fg_and_glyph() {
        // Band 5x7; pane inset at (1,1) sized 3x5.
        // Left segment is col 0, rows 1..=3.
        let band = Rect::new(Point::new(0, 0), Size::new(5, 7));
        let pane = Rect::new(Point::new(1, 1), Size::new(3, 5));
        let block_rows = vec![Some(BlockLineStatus::Ok); 3];
        let colors = test_colors();
        let mut screen = VirtualScreen::blank(5, 7);
        let f = frame_with_blocks(pane, false, false, block_rows);
        draw(
            &[f],
            band,
            &mut screen,
            Some(&colors),
            RingColors::ansi_default(),
        );
        // Row 2 (mid-pane), col 0: ok color + heavy bar ▐.
        let cell = screen.cell(2, 0).unwrap();
        assert_eq!(cell.fg, colors.ok, "ok segment: fg = ok color");
        assert_eq!(
            cell.grapheme.as_str(),
            "\u{2590}",
            "ok segment: heavy bar (▐)"
        );
    }

    /// Failed status row: left-segment `│` becomes `▐` with fail fg.
    #[test]
    fn block_failed_segment_replaces_pipe_with_half_block() {
        let band = Rect::new(Point::new(0, 0), Size::new(5, 7));
        let pane = Rect::new(Point::new(1, 1), Size::new(3, 5));
        let block_rows = vec![Some(BlockLineStatus::Failed); 3];
        let colors = test_colors();
        let mut screen = VirtualScreen::blank(5, 7);
        let f = frame_with_blocks(pane, false, false, block_rows);
        draw(
            &[f],
            band,
            &mut screen,
            Some(&colors),
            RingColors::ansi_default(),
        );
        // Mid-pane left-segment cell: fail fg + ▐ glyph.
        let cell = screen.cell(2, 0).unwrap();
        assert_eq!(cell.fg, colors.fail, "failed segment: fg = fail color");
        assert_eq!(cell.grapheme.as_str(), "\u{2590}", "failed segment: │ → ▐");
    }

    /// A `┤` at the exact left-segment position: glyph kept, fail color applied.
    ///
    /// Layout: band 5x10; left pane (1,1,3,2) with no blocks; right pane
    /// (1,5,3,4) with all-Failed rows. The right pane's left segment is col 4.
    /// A gap (col 3) between the two panes' content areas gives col 4 a western
    /// border neighbour, so the mid-height cell at (2,4) receives connectivity
    /// (N=T, S=T, W=T, E=F) → ┤ rather than │. The `│` → `▐` substitution must
    /// not fire on ┤.
    #[test]
    fn block_failed_tee_at_left_segment_col_keeps_glyph() {
        let band = Rect::new(Point::new(0, 0), Size::new(5, 10));
        let left_pane = Rect::new(Point::new(1, 1), Size::new(3, 2)); // cols 1..=2
        let right_pane = Rect::new(Point::new(1, 5), Size::new(3, 4)); // cols 5..=8; left segment = col 4
        let colors = test_colors();
        let f_left = frame_with_blocks(left_pane, false, false, vec![]);
        let f_right = frame_with_blocks(
            right_pane,
            false,
            false,
            vec![
                Some(BlockLineStatus::Failed),
                Some(BlockLineStatus::Failed),
                Some(BlockLineStatus::Failed),
            ],
        );
        let mut screen = VirtualScreen::blank(5, 10);
        draw(
            &[f_left, f_right],
            band,
            &mut screen,
            Some(&colors),
            RingColors::ansi_default(),
        );
        // Check the ┤ at mid-height of the right pane's left segment.
        let cell = screen.cell(2, 4).unwrap();
        assert_eq!(
            cell.grapheme.as_str(),
            "\u{2524}",
            "expected ┤ at (2,4); got {}",
            cell.grapheme.as_str()
        );
        // ┤ takes the fail color but keeps its glyph (no ▐ replacement).
        assert_eq!(cell.fg, colors.fail, "┤ cell: fail fg");
        assert_ne!(cell.grapheme.as_str(), "\u{2590}", "┤ must not become ▐");
    }

    /// None rows: frame drawn with `Some(colors)` and all-None block_rows is
    /// byte-identical to a frame drawn with `None`.
    #[test]
    fn none_rows_identical_to_blocks_disabled() {
        let band = Rect::new(Point::new(0, 0), Size::new(5, 7));
        let pane = Rect::new(Point::new(1, 1), Size::new(3, 5));
        let colors = test_colors();
        // Frame with Some(colors) but all-None block_rows.
        let block_rows = vec![None; 3];
        let f1 = frame_with_blocks(pane, false, false, block_rows);
        let mut s1 = VirtualScreen::blank(5, 7);
        draw(
            &[f1],
            band,
            &mut s1,
            Some(&colors),
            RingColors::ansi_default(),
        );
        // Frame with None (feature disabled).
        let f2 = frame(pane, false, None);
        let mut s2 = VirtualScreen::blank(5, 7);
        draw(&[f2], band, &mut s2, None, RingColors::ansi_default());
        // Both screens must be cell-identical (full Cell equality, not just grapheme+fg).
        for r in 0..5u16 {
            for c in 0..7u16 {
                let c1 = s1.cell(r, c).unwrap().clone();
                let c2 = s2.cell(r, c).unwrap().clone();
                assert_eq!(c1, c2, "cell mismatch at ({r},{c}): Some(all-None) vs None");
            }
        }
    }

    /// Marked pane: ring color + glyph win over Failed (no ▐ on a marked ring).
    #[test]
    fn marked_pane_beats_failed_block_status() {
        let band = Rect::new(Point::new(0, 0), Size::new(5, 7));
        let pane = Rect::new(Point::new(1, 1), Size::new(3, 5));
        let colors = test_colors();
        let block_rows = vec![Some(BlockLineStatus::Failed); 3];
        let f = frame_with_blocks(pane, false, true, block_rows);
        let mut screen = VirtualScreen::blank(5, 7);
        draw(
            &[f],
            band,
            &mut screen,
            Some(&colors),
            RingColors::ansi_default(),
        );
        // Left-segment cells (col 0, rows 1..=3) must be magenta (marked), not fail color.
        for r in 1..=3u16 {
            let cell = screen.cell(r, 0).unwrap();
            assert_eq!(
                cell.fg,
                Color::Indexed(13),
                "marked ring at ({r},0) must beat fail status (got {:?})",
                cell.fg
            );
            // No ▐ on a marked ring.
            assert_ne!(
                cell.grapheme.as_str(),
                "\u{2590}",
                "marked ring at ({r},0) must not have ▐"
            );
        }
    }

    /// Active pane: Failed beats active blue on the status row; None rows keep blue.
    #[test]
    fn failed_beats_active_ring_on_status_rows() {
        // Pane is active. Row 0 of block_rows = Failed, rows 1..2 = None.
        let band = Rect::new(Point::new(0, 0), Size::new(5, 7));
        let pane = Rect::new(Point::new(1, 1), Size::new(3, 5));
        let colors = test_colors();
        let block_rows = vec![Some(BlockLineStatus::Failed), None, None];
        let f = frame_with_blocks(pane, true, false, block_rows);
        let mut screen = VirtualScreen::blank(5, 7);
        draw(
            &[f],
            band,
            &mut screen,
            Some(&colors),
            RingColors::ansi_default(),
        );
        // Row 1 (block_rows[0] = Failed): fail color, not blue.
        let failed_cell = screen.cell(1, 0).unwrap();
        assert_eq!(failed_cell.fg, colors.fail, "failed row beats active blue");
        assert_eq!(
            failed_cell.grapheme.as_str(),
            "\u{2590}",
            "failed row: │ → ▐"
        );
        // Rows 2..=3 (block_rows[1..2] = None): active blue.
        for r in 2..=3u16 {
            let cell = screen.cell(r, 0).unwrap();
            assert_eq!(
                cell.fg,
                Color::Indexed(12),
                "none row at ({r},0) keeps active blue"
            );
        }
    }

    /// Shared separator: two side-by-side panes, LEFT active, RIGHT has a failed
    /// block row → that shared cell shows fail color/▐; left pane's other ring
    /// cells stay blue.
    #[test]
    fn shared_separator_right_status_beats_left_active_ring() {
        // Band 5x9; left (1,1,3,3) active, right (1,5,3,3) inactive with Failed.
        // Separator column = col 4 (right pane's left segment, rows 1..=3).
        let band = Rect::new(Point::new(0, 0), Size::new(5, 9));
        let left_rect = Rect::new(Point::new(1, 1), Size::new(3, 3));
        let right_rect = Rect::new(Point::new(1, 5), Size::new(3, 3));
        let colors = test_colors();
        let f_left = frame_with_blocks(left_rect, true, false, vec![]);
        let f_right = frame_with_blocks(
            right_rect,
            false,
            false,
            vec![Some(BlockLineStatus::Failed); 3],
        );
        let mut screen = VirtualScreen::blank(5, 9);
        draw(
            &[f_left, f_right],
            band,
            &mut screen,
            Some(&colors),
            RingColors::ansi_default(),
        );
        // The separator (col 4, rows 1..=3) is in right pane's left segment.
        // It should show fail color / ▐, not left pane's active blue.
        for r in 1..=3u16 {
            let cell = screen.cell(r, 4).unwrap();
            assert_eq!(
                cell.fg, colors.fail,
                "shared separator at ({r},4): right fail beats left active"
            );
            assert_eq!(
                cell.grapheme.as_str(),
                "\u{2590}",
                "shared separator at ({r},4): │ → ▐"
            );
        }
        // Left pane's other ring cells (top, bottom, right side) stay blue.
        // Top-left corner (0,0) touches both panes' rings, marked? No.
        // The left pane's own non-separator border cells: e.g. col 0 rows 1..=3
        // (the true left outer edge of the left pane, but that's the outer border,
        // not the pane's own separator side). The LEFT pane's ring includes col 0
        // (outer left border), so those cells are only touched by the left pane's
        // active ring (no block status from the right pane applies there).
        for r in 1..=3u16 {
            let outer_left = screen.cell(r, 0).unwrap();
            assert_eq!(
                outer_left.fg,
                Color::Indexed(12),
                "left pane outer-left ring at ({r},0) stays active blue"
            );
        }
    }

    /// Marked LEFT pane + Failed RIGHT pane: the shared separator (right pane's
    /// left segment) is also on the marked ring of the LEFT pane. Marked takes
    /// precedence over block status → cell keeps magenta (Indexed 13) and the
    /// `│` glyph (no `▐`).
    #[test]
    fn marked_left_pane_beats_failed_right_on_shared_separator() {
        // Band 5x9; left (1,1,3,3) marked, right (1,5,3,3) inactive with Failed.
        // Separator column = col 4 (right pane's left segment AND left pane's ring).
        let band = Rect::new(Point::new(0, 0), Size::new(5, 9));
        let left_rect = Rect::new(Point::new(1, 1), Size::new(3, 3));
        let right_rect = Rect::new(Point::new(1, 5), Size::new(3, 3));
        let colors = test_colors();
        let f_left = frame_with_blocks(left_rect, false, true, vec![]);
        let f_right = frame_with_blocks(
            right_rect,
            false,
            false,
            vec![Some(BlockLineStatus::Failed); 3],
        );
        let mut screen = VirtualScreen::blank(5, 9);
        draw(
            &[f_left, f_right],
            band,
            &mut screen,
            Some(&colors),
            RingColors::ansi_default(),
        );
        // The separator (col 4, rows 1..=3) is the marked ring: magenta + │.
        for r in 1..=3u16 {
            let cell = screen.cell(r, 4).unwrap();
            assert_eq!(
                cell.fg,
                Color::Indexed(13),
                "shared separator at ({r},4): marked magenta beats right fail"
            );
            assert_eq!(
                cell.grapheme.as_str(),
                "\u{2502}",
                "shared separator at ({r},4): marked ring must keep │, not ▐"
            );
        }
    }

    /// Segment confinement: the pane's right border and other panes' borders
    /// are untouched by its block status.
    #[test]
    fn segment_confined_to_left_border_col() {
        // Single pane. All-Failed block rows.
        // The right border (col = rect.right_edge_col() + 1 = col 6) should NOT
        // be colored; only col 0 (the left segment) should be.
        let band = Rect::new(Point::new(0, 0), Size::new(5, 7));
        let pane = Rect::new(Point::new(1, 1), Size::new(3, 5));
        let colors = test_colors();
        let block_rows = vec![Some(BlockLineStatus::Failed); 3];
        let f = frame_with_blocks(pane, false, false, block_rows);
        let mut screen = VirtualScreen::blank(5, 7);
        draw(
            &[f],
            band,
            &mut screen,
            Some(&colors),
            RingColors::ansi_default(),
        );
        // Right border (col 6): NOT fail colored.
        for r in 1..=3u16 {
            let cell = screen.cell(r, 6).unwrap();
            assert_ne!(
                cell.fg, colors.fail,
                "right border col 6 row {r} must not have fail color"
            );
            assert_ne!(
                cell.grapheme.as_str(),
                "\u{2590}",
                "right border col 6 row {r} must not have ▐"
            );
        }
        // Left segment (col 0) IS fail colored.
        for r in 1..=3u16 {
            let cell = screen.cell(r, 0).unwrap();
            assert_eq!(cell.fg, colors.fail, "left segment col 0 row {r} = fail");
        }
    }

    #[test]
    fn drag_source_and_target_get_distinct_ring_colors() {
        // Two side-by-side panes inset in a band; one is the drag source, the other the target.
        // Band 5x12; src pane (1,1,3,4); tgt pane (1,7,3,4).
        let band = Rect::new(Point::new(0, 0), Size::new(5, 12));
        let src_rect = Rect::new(Point::new(1, 1), Size::new(3, 4));
        let tgt_rect = Rect::new(Point::new(1, 7), Size::new(3, 4));
        let frames = vec![
            PaneFrame {
                rect: src_rect,
                active: false,
                marked: false,
                drag_role: PaneDragRole::Source,
                title: None,
                block_rows: vec![],
                selected_block: None,
            },
            PaneFrame {
                rect: tgt_rect,
                active: false,
                marked: false,
                drag_role: PaneDragRole::Target,
                title: None,
                block_rows: vec![],
                selected_block: None,
            },
        ];
        let mut screen = VirtualScreen::blank(5, 12);
        draw(&frames, band, &mut screen, None, RingColors::ansi_default());
        // A border cell of the source pane uses the source color; the target the target color.
        let src_cell = screen.cell(0, 0).expect("src border cell");
        let tgt_cell = screen.cell(0, 11).expect("tgt border cell");
        assert_eq!(
            src_cell.fg,
            plexy_glass_emulator::Color::Indexed(SOURCE_DRAG_COLOR)
        );
        assert_eq!(
            tgt_cell.fg,
            plexy_glass_emulator::Color::Indexed(TARGET_DRAG_COLOR)
        );
    }

    #[test]
    fn border_cell_below_pane_content_is_horizontal_not_tee() {
        // Kills: 136:49 `- → /`: `is_border(r/1, c)` equals `is_border(r, c)` which
        // is always true, making n=true for cells whose northern neighbor is pane
        // content. This turns the bottom-border `─` into `┴` (T-junction).
        let band = Rect::new(Point::new(0, 0), Size::new(5, 7));
        let pane = Rect::new(Point::new(1, 1), Size::new(3, 5)); // content rows 1-3; bottom border at row 4
        let mut screen = VirtualScreen::blank(5, 7);
        draw(
            &[frame(pane, false, None)],
            band,
            &mut screen,
            None,
            RingColors::ansi_default(),
        );
        // (4, 1): cell is a border (in band, not in pane). The cell above (3,1) IS
        // in the pane → n must be false → glyph is ─ (not ┴).
        assert_eq!(
            screen.cell(4, 1).unwrap().grapheme.as_str(),
            "\u{2500}", // ─
            "bottom border below pane content must be horizontal (─), not T-junction (┴)"
        );
    }

    #[test]
    fn paint_title_narrow_char_at_max_col_is_painted() {
        // Kills: 337:14 `> → ==` / `> → >=`: break one col too early, missing the
        // character that lands exactly on max_col.
        // Also kills: 340:14 `== → !=`: with !=, the wide guard fires for narrow
        // chars (w=1) and breaks before 'a' reaches the screen.
        let band = Rect::new(Point::new(0, 0), Size::new(5, 7));
        // pane col=1, cols=3 → max_col=3, start=2.  title "ab" → text=" ab ".
        // 'a' lands at col 3 = max_col (must be painted).
        let pane = Rect::new(Point::new(1, 1), Size::new(3, 3));
        let mut screen = VirtualScreen::blank(5, 7);
        draw(
            &[frame(pane, false, Some("ab"))],
            band,
            &mut screen,
            None,
            RingColors::ansi_default(),
        );
        assert_eq!(
            screen.cell(0, 3).unwrap().grapheme.as_str(),
            "a",
            "character at exactly max_col must be painted (max_col is inclusive)"
        );
    }

    #[test]
    fn paint_title_wide_char_fits_when_ends_at_max_col() {
        // Kills: 340:29 `> → >=`: `c + 1 >= max_col` rejects a wide char whose
        // right half lands exactly at max_col, even though it fits.
        let band = Rect::new(Point::new(0, 0), Size::new(5, 10));
        // pane col=1, cols=4 → max_col=4, start=2.  "好" lands at c=3, ends at col 4.
        // c+1=4: original `4 > 4 = false` → painted. Mutation `4 >= 4 = true` → rejected.
        let pane = Rect::new(Point::new(1, 1), Size::new(3, 4));
        let mut screen = VirtualScreen::blank(5, 10);
        draw(
            &[frame(pane, false, Some("好"))],
            band,
            &mut screen,
            None,
            RingColors::ansi_default(),
        );
        assert_eq!(
            screen.cell(0, 3).unwrap().grapheme.as_str(),
            "好",
            "wide char whose right half ends at max_col must be painted"
        );
    }

    /// Status-row safety: with the pane rect hitting the band bottom, the
    /// band-bounded loop must not paint past `band.bottom_edge_row()`.
    #[test]
    fn status_row_safety_band_bounded() {
        // Simulate a tight layout: band 4 rows, pane uses 3 content rows (1..=3),
        // block_rows length = 3. Band bottom = row 3. The pane's left segment
        // spans rows 1..=3 which is within the band, so no overflow.
        let band = Rect::new(Point::new(0, 0), Size::new(4, 7));
        let pane = Rect::new(Point::new(1, 1), Size::new(2, 5)); // content rows 1..=2 only
        let colors = test_colors();
        let block_rows = vec![Some(BlockLineStatus::Failed); 2];
        let f = frame_with_blocks(pane, false, false, block_rows);
        // Screen has 5 rows; band is 4; band.bottom_edge_row() = 3.
        let mut screen = VirtualScreen::blank(5, 7);
        draw(
            &[f],
            band,
            &mut screen,
            Some(&colors),
            RingColors::ansi_default(),
        );
        // Row 4 (outside the band) must be untouched (blank default).
        for c in 0..7u16 {
            let cell = screen.cell(4, c).unwrap();
            assert_eq!(
                cell.grapheme.as_str(),
                " ",
                "row 4 (outside band) col {c} must be blank"
            );
        }
    }
}
