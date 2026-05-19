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
}

pub struct Compositor;

impl Compositor {
    pub fn compose(
        panes: &[PaneView<'_>],
        host_size: (u16, u16),
        status: Option<&StatusLine>,
        selection: Option<&crate::selection::Selection>,
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

        // Copy each pane's emulator cells into its rect, mixing in scrollback
        // when scroll_offset > 0.
        for view in panes {
            let max_r = view.rect.rows;
            let max_c = view.rect.cols.min(view.screen.active.num_cols());
            for r in 0..max_r {
                if view.rect.row.saturating_add(r) >= pane_area_rows {
                    continue;
                }
                let cells_src: Option<&[plexy_glass_emulator::Cell]> =
                    if view.scroll_offset > 0 {
                        let scroll_len = view.screen.scrollback.len() as u32;
                        let want_from_scrollback = view.scroll_offset.min(scroll_len);
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
                            view.rect.row.saturating_add(r),
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
                let host_r = view.rect.row.saturating_add(row);
                let host_c = view.rect.col.saturating_add(col);
                if host_r >= pane_area_rows || host_c >= host_cols {
                    continue;
                }
                if let Some(cell) = screen.cell_mut(host_r, host_c) {
                    cell.attrs |= plexy_glass_emulator::Attrs::REVERSE;
                }
            }
        }

        // Borders.
        let rects: Vec<(Rect, bool)> = panes.iter().map(|v| (v.rect, v.is_active)).collect();
        borders::draw(&rects, &mut screen);

        // Status bar.
        if let Some(s) = status {
            let row_cells = crate::status::build(s, host_cols);
            for (c, cell) in row_cells.into_iter().enumerate() {
                if (c as u16) >= host_cols {
                    break;
                }
                screen.put(host_rows.saturating_sub(1), c as u16, cell);
            }
        }

        // Cursor from the active pane.
        if let Some(active) = panes.iter().find(|v| v.is_active) {
            let cur = &active.screen.cursor;
            let r = active.rect.row.saturating_add(cur.row);
            let c = active.rect.col.saturating_add(cur.col);
            if r < pane_area_rows && c < host_cols {
                screen.cursor = Some((r, c));
            }
            screen.cursor_visible = active
                .screen
                .modes
                .contains(plexy_glass_emulator::Modes::CURSOR_VISIBLE);
        }

        screen
    }
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
        };
        let vs = Compositor::compose(&[view], (4, 6), None, None);
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
        };
        let mut sel = Selection::start(PaneId(0), 0, 0, SelectionKind::Char);
        sel.extend(0, 4, Rect::new(0, 0, 4, 6));
        let vs = Compositor::compose(&[view], (4, 6), None, Some(&sel));
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
        };
        let rv = PaneView {
            id: PaneId(1),
            rect: Rect::new(0, 4, 4, 3),
            screen: right.screen(),
            is_active: true,
            scroll_offset: 0,
        };
        let vs = Compositor::compose(&[lv, rv], (4, 7), None, None);
        assert_eq!(vs.cell(0, 0).unwrap().grapheme.as_str(), "L");
        assert_eq!(vs.cell(0, 4).unwrap().grapheme.as_str(), "R");
        // Border column.
        assert_eq!(vs.cell(0, 3).unwrap().grapheme.as_str(), "│");
    }

    #[test]
    fn status_bar_renders_session_name() {
        let e = Emulator::new(4, 40);
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 3, 40),
            screen: e.screen(),
            is_active: true,
            scroll_offset: 0,
        };
        let status = StatusLine {
            windows: vec![crate::status::WindowEntry {
                id: crate::pane_id::WindowId(0),
                name: "shell0".into(),
                active: true,
            }],
            prefix_active: false,
            session_name: "main".into(),
            attached_clients: 1,
        };
        let vs = Compositor::compose(&[view], (4, 40), Some(&status), None);
        let row3: String = (0..40)
            .filter_map(|c| vs.cell(3, c).map(|cell| cell.grapheme.as_str().to_string()))
            .collect::<Vec<_>>()
            .join("");
        assert!(row3.contains("main"), "expected session name in status bar: {row3}");
    }

    #[test]
    fn status_bar_shows_client_count_when_multiple() {
        let e = Emulator::new(4, 40);
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 3, 40),
            screen: e.screen(),
            is_active: true,
            scroll_offset: 0,
        };
        let status = StatusLine {
            windows: vec![crate::status::WindowEntry {
                id: crate::pane_id::WindowId(0),
                name: "shell0".into(),
                active: true,
            }],
            prefix_active: false,
            session_name: "main".into(),
            attached_clients: 3,
        };
        let vs = Compositor::compose(&[view], (4, 40), Some(&status), None);
        let row3: String = (0..40)
            .filter_map(|c| vs.cell(3, c).map(|cell| cell.grapheme.as_str().to_string()))
            .collect::<Vec<_>>()
            .join("");
        assert!(row3.contains("*3"), "expected client count indicator: {row3}");
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
        };
        let vs = Compositor::compose(&[view], (2, 4), None, None);
        // Row 0 should be the last scrollback row (BBBB), not CCCC.
        let r0: String = (0..4)
            .map(|c| vs.cell(0, c).unwrap().grapheme.as_str().to_string())
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(r0, "BBBB", "expected BBBB at top; got {r0}");
    }
}
