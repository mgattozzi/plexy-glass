//! Combine multiple pane screens into a single VirtualScreen, with borders
//! and an optional status-bar row.

use crate::{
    borders,
    pane_id::PaneId,
    rect::Rect,
    status::StatusLine,
    virtual_screen::VirtualScreen,
};
use plexy_glass_emulator::Screen;

pub struct PaneView<'a> {
    pub id: PaneId,
    pub rect: Rect,
    pub screen: &'a Screen,
    pub is_active: bool,
    /// 0 = follow the live screen. N > 0 = show N rows of scrollback above
    /// the active grid; the bottom rows of the active grid are clipped.
    pub scroll_offset: u32,
    /// When Some, the pane is in copy-mode; the compositor uses the copy-mode
    /// viewport instead of `scroll_offset` and renders overlays.
    pub copy_mode: Option<&'a crate::CopyMode>,
}

/// Where the status bar sits relative to the pane area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusPlacement {
    Top,
    Bottom,
}

/// A render-ready view of the active interactive overlay, built by the daemon
/// each frame. Painted on top of the pane band (and borders).
pub enum OverlayView<'a> {
    /// A single-line rename prompt. `label` is e.g. "rename window".
    RenamePrompt { label: &'a str, buf: &'a str },
    /// A scrollable help page: `(keys, description)` rows plus the top line
    /// index. The compositor clamps `scroll` to the content height.
    Help { lines: &'a [(String, String)], scroll: u16 },
}

pub struct Compositor;

impl Compositor {
    pub fn compose(
        panes: &[PaneView<'_>],
        host_size: (u16, u16),
        status: Option<&StatusLine>,
        placement: StatusPlacement,
        selection: Option<&crate::selection::Selection>,
        overlay: Option<&OverlayView<'_>>,
    ) -> VirtualScreen {
        let (host_rows, host_cols) = host_size;
        let host_rows = host_rows.max(1);
        let host_cols = host_cols.max(1);
        let mut screen = VirtualScreen::blank(host_rows, host_cols);

        let pane_area_rows = if status.is_some() {
            host_rows.saturating_sub(1).max(1)
        } else {
            host_rows
        };
        // Panes are laid out in a LOGICAL band `0..pane_area_rows` (all the
        // clips below operate on that logical row). When the status bar is on
        // top, the physical screen rows are shifted down by one and the status
        // is painted at row 0; on the bottom, no shift and status at the last
        // row. `pane_row_offset` is added at each physical write site only.
        let (pane_row_offset, status_row): (u16, u16) = match (status.is_some(), placement) {
            (true, StatusPlacement::Top) => (1, 0),
            (true, StatusPlacement::Bottom) => (0, host_rows.saturating_sub(1)),
            (false, _) => (0, 0),
        };

        // Copy each pane's emulator cells into its rect, mixing in scrollback
        // when scroll_offset > 0 (or when copy-mode overrides the viewport).
        for view in panes {
            let effective_scroll = match view.copy_mode {
                Some(cm) => {
                    let total_lines = view.screen.scrollback.len() as u32
                        + view.screen.active.num_rows() as u32;
                    total_lines
                        .saturating_sub(cm.viewport_top)
                        .saturating_sub(u32::from(view.rect.rows))
                }
                None => view.scroll_offset,
            };
            let max_r = view.rect.rows;
            let max_c = view.rect.cols.min(view.screen.active.num_cols());
            for r in 0..max_r {
                if view.rect.row.saturating_add(r) >= pane_area_rows {
                    continue;
                }
                let cells_src: Option<&[plexy_glass_emulator::Cell]> =
                    if effective_scroll > 0 {
                        let scroll_len = view.screen.scrollback.len() as u32;
                        let want_from_scrollback = effective_scroll.min(scroll_len);
                        if (r as u32) < want_from_scrollback {
                            // This row comes from scrollback.
                            let sb_idx =
                                (scroll_len - want_from_scrollback + r as u32) as usize;
                            view.screen
                                .scrollback
                                .iter()
                                .nth(sb_idx)
                                .map(|row| row.cells.as_slice())
                        } else {
                            // This row comes from the active grid (offset by the
                            // number of scrollback rows shown above).
                            let active_r = r - want_from_scrollback as u16;
                            view.screen
                                .active
                                .rows
                                .get(active_r as usize)
                                .map(|row| row.cells.as_slice())
                        }
                    } else {
                        view.screen
                            .active
                            .rows
                            .get(r as usize)
                            .map(|row| row.cells.as_slice())
                    };
                let Some(cells) = cells_src else { continue };
                for c in 0..max_c {
                    if view.rect.col.saturating_add(c) >= host_cols {
                        continue;
                    }
                    if let Some(cell) = cells.get(c as usize) {
                        screen.put(
                            pane_row_offset + view.rect.row.saturating_add(r),
                            view.rect.col.saturating_add(c),
                            cell.clone(),
                        );
                    }
                }
            }
        }

        // Selection overlay: OR REVERSE onto selected cells.
        if let Some(sel) = selection
            && let Some(view) = panes.iter().find(|v| v.id == sel.source_pane)
        {
            let cols = view.screen.active.num_cols();
            for (row, col) in sel.cells(cols) {
                let logical_r = view.rect.row.saturating_add(row);
                let host_c = view.rect.col.saturating_add(col);
                if logical_r >= pane_area_rows || host_c >= host_cols {
                    continue;
                }
                let host_r = pane_row_offset + logical_r;
                if let Some(cell) = screen.cell_mut(host_r, host_c) {
                    cell.attrs |= plexy_glass_emulator::Attrs::REVERSE;
                }
            }
        }

        // Copy-mode selection overlay (per pane).
        for view in panes {
            let Some(cm) = view.copy_mode else { continue };
            let Some(anchor) = cm.anchor else { continue };
            let (start, end) = if anchor <= cm.cursor {
                (anchor, cm.cursor)
            } else {
                (cm.cursor, anchor)
            };
            let viewport_lo = cm.viewport_top;
            let viewport_hi = cm.viewport_top + u32::from(view.rect.rows);
            for line in start.0..=end.0 {
                if line < viewport_lo || line >= viewport_hi {
                    continue;
                }
                let local_row = (line - viewport_lo) as u16;
                let host_r = pane_row_offset + view.rect.row + local_row;
                let row_start = if line == start.0 { start.1 } else { 0 };
                let row_end = if line == end.0 {
                    end.1
                } else {
                    view.rect.cols.saturating_sub(1)
                };
                for c in row_start..=row_end {
                    let host_c = view.rect.col + c;
                    if let Some(cell) = screen.cell_mut(host_r, host_c) {
                        cell.attrs |= plexy_glass_emulator::Attrs::REVERSE;
                    }
                }
            }
        }

        // Copy-mode search match highlights.
        for view in panes {
            let Some(cm) = view.copy_mode else { continue };
            if cm.search.matches.is_empty() {
                continue;
            }
            let viewport_lo = cm.viewport_top;
            let viewport_hi = cm.viewport_top + u32::from(view.rect.rows);
            for m in &cm.search.matches {
                if m.line_idx < viewport_lo || m.line_idx >= viewport_hi {
                    continue;
                }
                let local_row = (m.line_idx - viewport_lo) as u16;
                let host_r = pane_row_offset + view.rect.row + local_row;
                for c in m.col_start..=m.col_end {
                    let host_c = view.rect.col + c;
                    if let Some(cell) = screen.cell_mut(host_r, host_c) {
                        cell.attrs |= plexy_glass_emulator::Attrs::HIGHLIGHT;
                    }
                }
            }
        }

        // Borders. Offset rects by `pane_row_offset` so separators land on the
        // physical pane band (matters for top status placement).
        let rects: Vec<(Rect, bool)> = panes
            .iter()
            .map(|v| {
                let mut r = v.rect;
                r.row = r.row.saturating_add(pane_row_offset);
                (r, v.is_active)
            })
            .collect();
        borders::draw(&rects, &mut screen);

        // Status bar.
        if let Some(s) = status {
            paint_status_row(&mut screen, s, host_cols, status_row);
        }

        // Cursor from the active pane, overridden by the copy-mode cursor when present.
        if let Some(active) = panes.iter().find(|v| v.is_active) {
            let cursor_pos = match active.copy_mode {
                Some(cm) => {
                    if cm.cursor.0 >= cm.viewport_top
                        && cm.cursor.0 < cm.viewport_top + u32::from(active.rect.rows)
                    {
                        let local_row = (cm.cursor.0 - cm.viewport_top) as u16;
                        let host_r = active.rect.row.saturating_add(local_row);
                        let host_c = active.rect.col.saturating_add(cm.cursor.1);
                        Some((host_r, host_c))
                    } else {
                        None
                    }
                }
                None => {
                    let cur = &active.screen.cursor;
                    let r = active.rect.row.saturating_add(cur.row);
                    let c = active.rect.col.saturating_add(cur.col);
                    if r < pane_area_rows && c < host_cols {
                        Some((r, c))
                    } else {
                        None
                    }
                }
            };
            if let Some((r, c)) = cursor_pos
                && r < pane_area_rows && c < host_cols
            {
                screen.cursor = Some((pane_row_offset + r, c));
            }
            screen.cursor_visible = match active.copy_mode {
                Some(_) => true,
                None => active
                    .screen
                    .modes
                    .contains(plexy_glass_emulator::Modes::CURSOR_VISIBLE),
            };
        }

        // Copy-mode search prompt overlay on the active pane.
        if let Some(active) = panes.iter().find(|v| v.is_active)
            && let Some(cm) = active.copy_mode
            && cm.search.prompt_active
        {
            let prompt_row = pane_row_offset + active.rect.row + active.rect.rows.saturating_sub(1);
            let mut text = String::from("/");
            text.push_str(&cm.search.prompt_buf);
            let prompt_attrs = plexy_glass_emulator::Attrs::REVERSE;
            for (i, ch) in text.chars().enumerate() {
                let host_c = active.rect.col + i as u16;
                if host_c >= host_cols {
                    break;
                }
                let mut buf = [0u8; 4];
                let s = ch.encode_utf8(&mut buf);
                let cell = plexy_glass_emulator::Cell {
                    grapheme: smol_str::SmolStr::new(s),
                    attrs: prompt_attrs,
                    ..plexy_glass_emulator::Cell::default()
                };
                screen.put(prompt_row, host_c, cell);
            }
        }

        // Interactive overlay (rename prompt / help), painted last so it sits
        // on top of panes, borders, and the cursor logic above.
        if let Some(ov) = overlay {
            paint_overlay(&mut screen, ov, pane_row_offset, pane_area_rows, host_cols);
        }

        screen
    }
}

/// Paint the active overlay over the pane band.
fn paint_overlay(
    screen: &mut VirtualScreen,
    overlay: &OverlayView<'_>,
    pane_row_offset: u16,
    pane_area_rows: u16,
    cols: u16,
) {
    match overlay {
        OverlayView::RenamePrompt { label, buf } => {
            // A full-width REVERSE bar on the bottom row of the pane band.
            let row = pane_row_offset + pane_area_rows.saturating_sub(1);
            let text = format!(" {label} \u{25b8} {buf}");
            let attrs = plexy_glass_emulator::Attrs::REVERSE;
            // Fill the row with REVERSE blanks first, then the text.
            for c in 0..cols {
                put_char(screen, row, c, ' ', attrs);
            }
            let mut end_col = 0u16;
            for (i, ch) in text.chars().enumerate() {
                let col = i as u16;
                if col >= cols {
                    break;
                }
                put_char(screen, row, col, ch, attrs);
                end_col = col + 1;
            }
            // Block cursor just past the buffer.
            if end_col < cols {
                put_char(screen, row, end_col, '\u{2588}', attrs);
            }
            screen.cursor = Some((row, end_col.min(cols.saturating_sub(1))));
            screen.cursor_visible = false; // the block glyph is the cursor
        }
        OverlayView::Help { lines, scroll } => {
            paint_help_box(screen, lines, *scroll, pane_row_offset, pane_area_rows, cols);
        }
    }
}

/// Draw a centered bordered help box listing `(keys, description)` rows.
fn paint_help_box(
    screen: &mut VirtualScreen,
    lines: &[(String, String)],
    scroll: u16,
    pane_row_offset: u16,
    pane_area_rows: u16,
    cols: u16,
) {
    let title = " Keybindings ";
    let footer = " j/k scroll \u{b7} esc close ";
    // Key column width = widest key string (cap to keep the box reasonable).
    let key_w = lines.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0).min(20);
    let content_w = lines
        .iter()
        .map(|(k, d)| key_w.max(k.chars().count()) + 2 + d.chars().count())
        .max()
        .unwrap_or(0)
        .max(title.chars().count())
        .max(footer.chars().count());
    // Box width includes 1 cell of padding each side + 2 borders.
    let inner_w = (content_w + 2).min(cols.saturating_sub(2) as usize);
    let box_w = (inner_w + 2) as u16;
    // Height: top border + visible rows + footer + bottom border.
    let max_visible = pane_area_rows.saturating_sub(3) as usize; // borders + footer
    let visible = lines.len().min(max_visible.max(1));
    let box_h = (visible as u16) + 3;
    if box_w < 3 || box_h < 4 || box_w > cols {
        return; // viewport too small to draw a meaningful box
    }
    let max_scroll = lines.len().saturating_sub(visible);
    let top = (scroll as usize).min(max_scroll);

    let row0 = pane_row_offset + (pane_area_rows.saturating_sub(box_h)) / 2;
    let col0 = (cols.saturating_sub(box_w)) / 2;
    let attrs = plexy_glass_emulator::Attrs::empty();

    // Clear interior + draw border frame.
    for r in 0..box_h {
        for c in 0..box_w {
            let ch = border_glyph(r, c, box_h, box_w);
            put_char(screen, row0 + r, col0 + c, ch, attrs);
        }
    }
    // Title centered on the top border.
    let tcol = col0 + 1 + ((box_w.saturating_sub(2) as usize).saturating_sub(title.chars().count()) / 2) as u16;
    put_str(screen, row0, tcol, title, attrs, col0 + box_w - 1);
    // Footer centered on the bottom border.
    let fcol = col0 + 1 + ((box_w.saturating_sub(2) as usize).saturating_sub(footer.chars().count()) / 2) as u16;
    put_str(screen, row0 + box_h - 1, fcol, footer, attrs, col0 + box_w - 1);

    // Content rows.
    for (i, (keys, desc)) in lines.iter().skip(top).take(visible).enumerate() {
        let r = row0 + 1 + i as u16;
        let line = format!("{keys:<key_w$}  {desc}", key_w = key_w);
        put_str(screen, r, col0 + 1, &line, attrs, col0 + box_w - 1);
    }
}

/// Box-drawing glyph for cell (r, c) within a `h`x`w` frame; space inside.
fn border_glyph(r: u16, c: u16, h: u16, w: u16) -> char {
    let last_r = h - 1;
    let last_c = w - 1;
    match (r, c) {
        (0, 0) => '\u{250c}',
        (0, cc) if cc == last_c => '\u{2510}',
        (rr, 0) if rr == last_r => '\u{2514}',
        (rr, cc) if rr == last_r && cc == last_c => '\u{2518}',
        (0, _) | (_, 0) => {
            if r == 0 || r == last_r {
                '\u{2500}'
            } else {
                '\u{2502}'
            }
        }
        (rr, _) if rr == last_r => '\u{2500}',
        (_, cc) if cc == last_c => '\u{2502}',
        _ => ' ',
    }
}

/// Put a single char with attrs at (row, col), clipped to the screen.
fn put_char(screen: &mut VirtualScreen, row: u16, col: u16, ch: char, attrs: plexy_glass_emulator::Attrs) {
    if row >= screen.rows || col >= screen.cols {
        return;
    }
    let mut buf = [0u8; 4];
    let s = ch.encode_utf8(&mut buf);
    let cell = plexy_glass_emulator::Cell {
        grapheme: smol_str::SmolStr::new(s),
        attrs,
        ..plexy_glass_emulator::Cell::default()
    };
    screen.put(row, col, cell);
}

/// Put a string starting at (row, col), stopping at `max_col` (exclusive) or
/// the screen edge.
fn put_str(
    screen: &mut VirtualScreen,
    row: u16,
    col: u16,
    text: &str,
    attrs: plexy_glass_emulator::Attrs,
    max_col: u16,
) {
    for (i, ch) in text.chars().enumerate() {
        let c = col.saturating_add(i as u16);
        if c >= max_col || c >= screen.cols {
            break;
        }
        put_char(screen, row, c, ch, attrs);
    }
}

fn paint_status_row(
    screen: &mut VirtualScreen,
    status: &StatusLine,
    cols: u16,
    row: u16,
) {
    let cols_us = cols as usize;

    let mut left_cells = collect_cells(&status.left);
    let middle_cells = collect_cells(&status.middle);
    let right_cells = collect_cells(&status.right);

    // Truncate left if it overflows.
    if left_cells.len() > cols_us {
        left_cells.truncate(cols_us);
    }
    let left_w = left_cells.len();

    // Reserve the right side, and truncate if it would overflow.
    let mut right_w = right_cells.len();
    if left_w + right_w > cols_us {
        right_w = cols_us.saturating_sub(left_w);
    }
    let right_cells: Vec<_> = right_cells.into_iter().take(right_w).collect();

    // Middle fills the gap; ellipsize if needed.
    let middle_budget = cols_us.saturating_sub(left_w + right_w);
    let middle_cells = if middle_cells.len() <= middle_budget {
        middle_cells
    } else if middle_budget == 0 {
        Vec::new()
    } else {
        let mut truncated: Vec<_> = middle_cells.into_iter().take(middle_budget - 1).collect();
        truncated.push((smol_str::SmolStr::new("…"), plexy_glass_status::ResolvedStyle::default()));
        truncated
    };

    // Paint left starting at col 0.
    for (i, (g, style)) in left_cells.iter().enumerate() {
        screen.put(row, i as u16, cell_for(g, style));
    }
    // Paint middle starting after left.
    for (i, (g, style)) in middle_cells.iter().enumerate() {
        screen.put(row, (left_w + i) as u16, cell_for(g, style));
    }
    // Paint right pinned to the right edge.
    let right_start = cols_us.saturating_sub(right_w);
    for (i, (g, style)) in right_cells.iter().enumerate() {
        screen.put(row, (right_start + i) as u16, cell_for(g, style));
    }
}

fn collect_cells(
    segments: &[plexy_glass_status::Segment],
) -> Vec<(smol_str::SmolStr, plexy_glass_status::ResolvedStyle)> {
    let mut out = Vec::new();
    for seg in segments {
        for ch in seg.text.chars() {
            let mut buf = [0u8; 4];
            let s = ch.encode_utf8(&mut buf);
            out.push((smol_str::SmolStr::new(s), seg.style));
        }
    }
    out
}

fn cell_for(
    g: &smol_str::SmolStr,
    style: &plexy_glass_status::ResolvedStyle,
) -> plexy_glass_emulator::Cell {
    // Build with struct-update syntax so any extra fields on `Cell` pick up
    // their defaults, and so we dodge `clippy::field_reassign_with_default`.
    let mut cell = plexy_glass_emulator::Cell {
        grapheme: g.clone(),
        attrs: style.attrs,
        ..plexy_glass_emulator::Cell::default()
    };
    if let Some(fg) = style.fg {
        cell.fg = rgb_to_color(fg);
    }
    if let Some(bg) = style.bg {
        cell.bg = rgb_to_color(bg);
    }
    cell
}

fn rgb_to_color(rgb: plexy_glass_status::Rgb) -> plexy_glass_emulator::Color {
    // `Color::Rgb(u8, u8, u8)`, confirmed in `crates/emulator/src/color.rs`.
    plexy_glass_emulator::Color::Rgb(rgb.r, rgb.g, rgb.b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexy_glass_emulator::Emulator;

    fn pane(emu: &mut Emulator, bytes: &[u8]) {
        emu.advance(bytes);
    }

    #[test]
    fn single_pane_full_viewport() {
        let mut e = Emulator::new(4, 6);
        // Trailing space forces the parser to flush the preceding cluster
        // ("i") so it lands in the active grid. The parser leaves the trailing
        // space pending, so the cursor sits at (0, 2), one past "i".
        pane(&mut e, b"hi ");
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 4, 6),
            screen: e.screen(),
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
        };
        let vs = Compositor::compose(&[view], (4, 6), None, StatusPlacement::Bottom, None, None);
        assert_eq!(vs.cell(0, 0).unwrap().grapheme.as_str(), "h");
        assert_eq!(vs.cursor, Some((0, 2)));
    }

    #[test]
    fn selection_overlay_sets_reverse_attr() {
        use crate::selection::{Selection, SelectionKind};
        use plexy_glass_emulator::Attrs;
        let mut e = Emulator::new(4, 6);
        pane(&mut e, b"hello ");
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 4, 6),
            screen: e.screen(),
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
        };
        let mut sel = Selection::start(PaneId(0), 0, 0, SelectionKind::Char);
        sel.extend(0, 4, Rect::new(0, 0, 4, 6));
        let vs = Compositor::compose(&[view], (4, 6), None, StatusPlacement::Bottom, Some(&sel), None);
        for c in 0..=4 {
            let cell = vs.cell(0, c).unwrap();
            assert!(
                cell.attrs.contains(Attrs::REVERSE),
                "expected REVERSE on col {c}, got {:?}",
                cell.attrs
            );
        }
        let unsel = vs.cell(0, 5).unwrap();
        assert!(!unsel.attrs.contains(Attrs::REVERSE));
    }

    #[test]
    fn two_panes_vertical_split() {
        let mut left = Emulator::new(4, 3);
        let mut right = Emulator::new(4, 3);
        // Trailing space forces the parser to flush the preceding cluster.
        pane(&mut left, b"L ");
        pane(&mut right, b"R ");
        let lv = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 4, 3),
            screen: left.screen(),
            is_active: false,
            scroll_offset: 0,
            copy_mode: None,
        };
        let rv = PaneView {
            id: PaneId(1),
            rect: Rect::new(0, 4, 4, 3),
            screen: right.screen(),
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
        };
        let vs = Compositor::compose(&[lv, rv], (4, 7), None, StatusPlacement::Bottom, None, None);
        assert_eq!(vs.cell(0, 0).unwrap().grapheme.as_str(), "L");
        assert_eq!(vs.cell(0, 4).unwrap().grapheme.as_str(), "R");
        // Border column.
        assert_eq!(vs.cell(0, 3).unwrap().grapheme.as_str(), "│");
    }

    #[test]
    fn scroll_offset_pulls_rows_from_scrollback() {
        // Use \r\n so the cursor returns to column 0 on each line, producing
        // clean full-width rows in scrollback rather than partial overwrites.
        let mut e = Emulator::new(2, 4);
        e.advance(b"AAAA\r\nBBBB\r\nCCCC\r\nDDDD");
        // Flush any pending grapheme.
        e.advance(b"\x1b[m");
        // After "AAAA\r\nBBBB\r\nCCCC\r\nDDDD" on a 2-row screen:
        //   scrollback = [AAAA, BBBB], active = [CCCC, DDDD]
        // scroll_offset=1 shows the last scrollback row (BBBB) at row 0
        // and the first active row (CCCC) at row 1.
        let s = e.screen().clone();
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 2, 4),
            screen: &s,
            is_active: true,
            scroll_offset: 1,
            copy_mode: None,
        };
        let vs = Compositor::compose(&[view], (2, 4), None, StatusPlacement::Bottom, None, None);
        // Row 0 should be the last scrollback row (BBBB), not CCCC.
        let r0: String = (0..4)
            .map(|c| vs.cell(0, c).unwrap().grapheme.as_str().to_string())
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(r0, "BBBB", "expected BBBB at top; got {r0}");
    }

    #[test]
    fn copy_mode_overrides_cursor() {
        use plexy_glass_emulator::Emulator;
        let mut e = Emulator::new(5, 20);
        e.advance(b"hello");
        let screen = e.screen().clone();
        let cm = crate::CopyMode {
            cursor: (3, 7),
            anchor: None,
            search: crate::SearchState::default(),
            viewport_top: 0,
            pane_rows: 5,
            total_lines: 5,
        };
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 4, 20),
            screen: &screen,
            is_active: true,
            scroll_offset: 0,
            copy_mode: Some(&cm),
        };
        let vs = Compositor::compose(&[view], (5, 20), None, StatusPlacement::Bottom, None, None);
        assert_eq!(vs.cursor, Some((3, 7)));
    }

    #[test]
    fn copy_mode_selection_sets_reverse() {
        use plexy_glass_emulator::Emulator;
        let mut e = Emulator::new(5, 20);
        e.advance(b"hello world");
        let screen = e.screen().clone();
        let cm = crate::CopyMode {
            cursor: (0, 4),
            anchor: Some((0, 0)),
            search: crate::SearchState::default(),
            viewport_top: 0,
            pane_rows: 5,
            total_lines: 5,
        };
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 4, 20),
            screen: &screen,
            is_active: true,
            scroll_offset: 0,
            copy_mode: Some(&cm),
        };
        let vs = Compositor::compose(&[view], (5, 20), None, StatusPlacement::Bottom, None, None);
        for c in 0..=4 {
            let cell = vs.cell(0, c).unwrap();
            assert!(
                cell.attrs.contains(plexy_glass_emulator::Attrs::REVERSE),
                "expected REVERSE at col {c}"
            );
        }
    }

    fn status_with_left(text: &str) -> StatusLine {
        StatusLine {
            left: vec![plexy_glass_status::Segment {
                text: text.into(),
                style: plexy_glass_status::ResolvedStyle::default(),
                click_action: None,
            }],
            middle: vec![],
            right: vec![],
        }
    }

    #[test]
    fn status_top_paints_row_zero_and_panes_below() {
        // Host 3 rows; pane area = 2 rows. Top placement → status at row 0,
        // pane shifted to rows 1..3.
        let mut e = Emulator::new(2, 4);
        pane(&mut e, b"X ");
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 2, 4),
            screen: e.screen(),
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
        };
        let status = status_with_left("AB");
        let vs = Compositor::compose(&[view], (3, 4), Some(&status), StatusPlacement::Top, None, None);
        assert_eq!(vs.cell(0, 0).unwrap().grapheme.as_str(), "A", "status at row 0");
        assert_eq!(vs.cell(0, 1).unwrap().grapheme.as_str(), "B");
        assert_eq!(vs.cell(1, 0).unwrap().grapheme.as_str(), "X", "pane shifted to row 1");
    }

    #[test]
    fn status_bottom_paints_last_row_and_panes_above() {
        // Regression guard for the offset-0 path: status at row N-1, pane at row 0.
        let mut e = Emulator::new(2, 4);
        pane(&mut e, b"X ");
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 2, 4),
            screen: e.screen(),
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
        };
        let status = status_with_left("AB");
        let vs = Compositor::compose(&[view], (3, 4), Some(&status), StatusPlacement::Bottom, None, None);
        assert_eq!(vs.cell(0, 0).unwrap().grapheme.as_str(), "X", "pane stays at row 0");
        assert_eq!(vs.cell(2, 0).unwrap().grapheme.as_str(), "A", "status at last row");
    }

    #[test]
    fn overlay_rename_prompt_paints_reverse_bottom_row() {
        use plexy_glass_emulator::Attrs;
        let mut e = Emulator::new(4, 20);
        pane(&mut e, b"x ");
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 4, 20),
            screen: e.screen(),
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
        };
        let ov = OverlayView::RenamePrompt { label: "rename window", buf: "hi" };
        let vs = Compositor::compose(&[view], (4, 20), None, StatusPlacement::Bottom, None, Some(&ov));
        // Bottom row (3) is a REVERSE prompt bar.
        assert!(vs.cell(3, 0).unwrap().attrs.contains(Attrs::REVERSE), "prompt bar is REVERSE");
        // Text " rename window \u{25b8} hi", with 'r' at col 1.
        assert_eq!(vs.cell(3, 1).unwrap().grapheme.as_str(), "r");
    }

    #[test]
    fn overlay_help_box_renders_border_and_rows() {
        let mut e = Emulator::new(10, 40);
        pane(&mut e, b"x ");
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 10, 40),
            screen: e.screen(),
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
        };
        let lines = vec![("Ctrl+a c".to_string(), "New window".to_string())];
        let ov = OverlayView::Help { lines: &lines, scroll: 0 };
        let vs = Compositor::compose(&[view], (10, 40), None, StatusPlacement::Bottom, None, Some(&ov));
        let mut found_corner = false;
        let mut found_text = false;
        for r in 0..10 {
            let mut row = String::new();
            for c in 0..40 {
                row.push_str(vs.cell(r, c).unwrap().grapheme.as_str());
            }
            if row.contains('\u{250c}') {
                found_corner = true;
            }
            if row.contains("New window") {
                found_text = true;
            }
        }
        assert!(found_corner, "help box top-left corner drawn");
        assert!(found_text, "help row text drawn");
    }
}
