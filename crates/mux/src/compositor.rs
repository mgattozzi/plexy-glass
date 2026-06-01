//! Combine multiple pane screens into a single VirtualScreen, with borders
//! and an optional status-bar row.

use crate::{
    borders,
    pane_id::PaneId,
    rect::Rect,
    status::StatusLine,
    virtual_screen::VirtualScreen,
};
use plexy_glass_emulator::{Screen, display_width};

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
    /// User-assigned pane name, painted on the pane's top border. `None` hides
    /// the title (plain border).
    pub title: Option<&'a str>,
    /// Whether this pane is the session's marked pane (drawn with a distinct
    /// border color).
    pub marked: bool,
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
    /// A single-line command prompt. Rendered like `RenamePrompt` but with a
    /// leading `:` instead of a label.
    Command { buf: &'a str },
    /// An fzf-style session picker: a centered box with a filter line and the
    /// filtered session rows, the selected one highlighted.
    SessionPicker {
        entries: &'a [crate::overlay::PickerEntry],
        filter: &'a str,
        selected: usize,
    },
    /// A fully-expanded session → window → pane tree (`choose-tree`): a centered
    /// box with depth-indented rows, the current-path nodes marked, the selected
    /// row highlighted, and a mode-dependent footer (navigate / confirm-kill /
    /// rename).
    Tree { state: &'a crate::tree::TreeState },
    /// The choose-buffer overlay: a centered box listing paste buffers
    /// (`name: preview`), the selected one highlighted.
    Buffer { state: &'a crate::buffer::BufferPickerState },
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
        message: Option<&str>,
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

        // Full pane frames. Offset each pane rect by `pane_row_offset` so the
        // frame lands on the physical pane band (matters for top status). The
        // band is the whole physical pane area; the layout already inset pane
        // rects by one cell on every side to leave room for the frame.
        let band = Rect::new(pane_row_offset, 0, pane_area_rows, host_cols);
        let frames: Vec<borders::PaneFrame<'_>> = panes
            .iter()
            .map(|v| {
                let mut r = v.rect;
                r.row = r.row.saturating_add(pane_row_offset);
                borders::PaneFrame { rect: r, active: v.is_active, marked: v.marked, title: v.title }
            })
            .collect();
        borders::draw(&frames, band, &mut screen);

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
            put_str(&mut screen, prompt_row, active.rect.col, &text, prompt_attrs, host_cols);
        }

        // Transient status-line message: a full-width REVERSE bar on the bottom
        // content row, shown only when no interactive overlay is open (the
        // overlay owns that row when present).
        if let Some(msg) = message
            && overlay.is_none()
        {
            let row = pane_row_offset + pane_area_rows.saturating_sub(1);
            let attrs = plexy_glass_emulator::Attrs::REVERSE;
            for c in 0..host_cols {
                put_char(&mut screen, row, c, ' ', attrs);
            }
            put_str(&mut screen, row, 0, &format!(" {msg}"), attrs, host_cols);
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
            // Block cursor just past the text (at its true display end).
            let end_col = put_str(screen, row, 0, &text, attrs, cols);
            if end_col < cols {
                put_char(screen, row, end_col, '\u{2588}', attrs);
            }
            screen.cursor = Some((row, end_col.min(cols.saturating_sub(1))));
            screen.cursor_visible = false; // the block glyph is the cursor
        }
        OverlayView::Help { lines, scroll } => {
            paint_help_box(screen, lines, *scroll, pane_row_offset, pane_area_rows, cols);
            // Suppress the underlying pane cursor while the box is up, matching
            // the rename/command overlays (otherwise it shows behind the box).
            screen.cursor_visible = false;
        }
        OverlayView::SessionPicker { entries, filter, selected } => {
            paint_session_picker(
                screen, entries, filter, *selected, pane_row_offset, pane_area_rows, cols,
            );
            screen.cursor_visible = false;
        }
        OverlayView::Tree { state } => {
            paint_tree(screen, state, pane_row_offset, pane_area_rows, cols);
            screen.cursor_visible = false;
        }
        OverlayView::Buffer { state } => {
            paint_buffers(screen, state, pane_row_offset, pane_area_rows, cols);
            screen.cursor_visible = false;
        }
        OverlayView::Command { buf } => {
            // A full-width REVERSE bar on the bottom row of the pane band,
            // ":<buf>" with a block cursor just past the text.
            let row = pane_row_offset + pane_area_rows.saturating_sub(1);
            let text = format!(" :{buf}");
            let attrs = plexy_glass_emulator::Attrs::REVERSE;
            for c in 0..cols {
                put_char(screen, row, c, ' ', attrs);
            }
            let end_col = put_str(screen, row, 0, &text, attrs, cols);
            if end_col < cols {
                put_char(screen, row, end_col, '\u{2588}', attrs);
            }
            screen.cursor = Some((row, end_col.min(cols.saturating_sub(1))));
            screen.cursor_visible = false;
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
    // Key column width = widest key string in display columns (cap to keep the
    // box reasonable).
    let dw = |s: &str| display_width(s) as usize;
    let key_w = lines.iter().map(|(k, _)| dw(k)).max().unwrap_or(0).min(20);
    let content_w = lines
        .iter()
        .map(|(k, d)| key_w.max(dw(k)) + 2 + dw(d))
        .max()
        .unwrap_or(0)
        .max(dw(title))
        .max(dw(footer));
    // Box width includes 1 cell of padding each side + 2 borders.
    let inner_w = (content_w + 2).min(cols.saturating_sub(2) as usize);
    let box_w = (inner_w + 2) as u16;
    // Height: top border + visible rows + footer + bottom border.
    let max_visible = pane_area_rows.saturating_sub(3) as usize; // borders + footer
    let visible = lines.len().min(max_visible.max(1));
    let box_h = (visible as u16) + 3;
    if box_w < 3 || box_h < 4 || box_w > cols || box_h > pane_area_rows {
        // Viewport too small to draw a meaningful box without overflowing the
        // pane band (and painting over the status row).
        return;
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
    let tcol = col0 + 1 + ((box_w.saturating_sub(2) as usize).saturating_sub(dw(title)) / 2) as u16;
    put_str(screen, row0, tcol, title, attrs, col0 + box_w - 1);
    // Footer centered on the bottom border.
    let fcol = col0 + 1 + ((box_w.saturating_sub(2) as usize).saturating_sub(dw(footer)) / 2) as u16;
    put_str(screen, row0 + box_h - 1, fcol, footer, attrs, col0 + box_w - 1);

    // Content rows. Pad the key column to `key_w` *display* columns (keys are
    // ASCII today, but pad by width so a wide glyph would still align).
    for (i, (keys, desc)) in lines.iter().skip(top).take(visible).enumerate() {
        let r = row0 + 1 + i as u16;
        let pad = " ".repeat(key_w.saturating_sub(dw(keys)));
        let line = format!("{keys}{pad}  {desc}");
        put_str(screen, r, col0 + 1, &line, attrs, col0 + box_w - 1);
    }
}

/// Draw the centered session-picker box: a filter line plus the filtered
/// session rows (current session marked `*`, selected row REVERSE), scrolled to
/// keep the selection visible.
fn paint_session_picker(
    screen: &mut VirtualScreen,
    entries: &[crate::overlay::PickerEntry],
    filter: &str,
    selected: usize,
    pane_row_offset: u16,
    pane_area_rows: u16,
    cols: u16,
) {
    let title = " Sessions ";
    let footer = " \u{2191}/\u{2193} select \u{b7} enter switch \u{b7} esc cancel ";
    let empty_msg = "(no matching sessions)";
    let filtered = crate::overlay::picker_filtered_indices(entries, filter);
    let rows: Vec<String> = filtered
        .iter()
        .map(|&i| {
            let e = &entries[i];
            let marker = if e.is_current { '*' } else { ' ' };
            format!("{marker} {}", e.label)
        })
        .collect();
    let filter_line = format!("filter: {filter}");

    // Rows fit between the top border + filter line and the bottom border.
    let max_visible = (pane_area_rows.saturating_sub(3)).max(1) as usize;
    let row_count = rows.len().max(1); // empty list still needs 1 message row
    let visible = row_count.min(max_visible);

    let dw = |s: &str| display_width(s) as usize;
    let content_w = rows
        .iter()
        .map(|s| dw(s))
        .chain([
            dw(&filter_line),
            dw(title),
            dw(footer),
            if rows.is_empty() { dw(empty_msg) } else { 0 },
        ])
        .max()
        .unwrap_or(0);
    let inner_w = (content_w + 2).min(cols.saturating_sub(2) as usize);
    let box_w = (inner_w + 2) as u16;
    let box_h = (visible as u16) + 3; // top border + filter line + rows + bottom
    if box_w < 3 || box_h < 4 || box_w > cols || box_h > pane_area_rows {
        return;
    }

    let sel = selected.min(filtered.len().saturating_sub(1));
    let top = if sel >= visible { sel - visible + 1 } else { 0 };

    let row0 = pane_row_offset + (pane_area_rows.saturating_sub(box_h)) / 2;
    let col0 = (cols.saturating_sub(box_w)) / 2;
    let plain = plexy_glass_emulator::Attrs::empty();
    let rev = plexy_glass_emulator::Attrs::REVERSE;

    // Border frame.
    for r in 0..box_h {
        for c in 0..box_w {
            put_char(screen, row0 + r, col0 + c, border_glyph(r, c, box_h, box_w), plain);
        }
    }
    // Title + footer centered on the borders.
    let tcol = col0 + 1 + ((box_w.saturating_sub(2) as usize).saturating_sub(dw(title)) / 2) as u16;
    put_str(screen, row0, tcol, title, plain, col0 + box_w - 1);
    let fcol = col0 + 1 + ((box_w.saturating_sub(2) as usize).saturating_sub(dw(footer)) / 2) as u16;
    put_str(screen, row0 + box_h - 1, fcol, footer, plain, col0 + box_w - 1);

    let inner_left = col0 + 1;
    let inner_right = col0 + box_w - 1; // exclusive max_col for put_str

    // Filter line with a block cursor.
    put_str(screen, row0 + 1, inner_left, &format!("{filter_line}\u{2588}"), plain, inner_right);

    // Session rows (or the empty-state message).
    if rows.is_empty() {
        put_str(screen, row0 + 2, inner_left, empty_msg, plain, inner_right);
    } else {
        let end = (top + visible).min(rows.len());
        for (vis_i, row_idx) in (top..end).enumerate() {
            let r = row0 + 2 + vis_i as u16;
            let row_attrs = if row_idx == sel { rev } else { plain };
            if row_idx == sel {
                for c in inner_left..inner_right {
                    put_char(screen, r, c, ' ', row_attrs);
                }
            }
            put_str(screen, r, inner_left, &rows[row_idx], row_attrs, inner_right);
        }
    }
}

/// Draw the centered choose-tree box: depth-indented rows (session → window →
/// pane), the current-path nodes marked `*`, the selected row REVERSE, scrolled
/// to keep the selection visible, with a mode-dependent footer.
fn paint_tree(
    screen: &mut VirtualScreen,
    state: &crate::tree::TreeState,
    pane_row_offset: u16,
    pane_area_rows: u16,
    cols: u16,
) {
    use crate::tree::{TreeKind, TreeMode};
    let title = " Tree ";
    let footer: String = match &state.mode {
        TreeMode::Navigate => {
            let session_selected = state
                .nodes
                .get(state.selected)
                .map(|n| n.kind() == TreeKind::Session)
                .unwrap_or(false);
            if session_selected {
                " \u{2191}/\u{2193} move \u{b7} enter switch \u{b7} x kill \u{b7} esc close ".into()
            } else {
                " \u{2191}/\u{2193} move \u{b7} enter switch \u{b7} x kill \u{b7} r rename \u{b7} esc close ".into()
            }
        }
        TreeMode::ConfirmKill => match state.nodes.get(state.selected) {
            Some(n) => {
                let kind = match n.kind() {
                    TreeKind::Session => "session",
                    TreeKind::Window => "window",
                    TreeKind::Pane => "pane",
                };
                format!(" Kill {kind} '{}'?  y / n ", n.name)
            }
            None => " Kill?  y / n ".into(),
        },
        TreeMode::Rename { buf } => format!(" rename: {buf}\u{2588}  enter ok \u{b7} esc cancel "),
    };

    let rows: Vec<String> = state
        .nodes
        .iter()
        .map(|n| {
            let indent = " ".repeat((n.depth as usize) * 2);
            let marker = if n.is_current { '*' } else { ' ' };
            format!("{indent}{marker} {}", n.label)
        })
        .collect();

    let dw = |s: &str| display_width(s) as usize;
    let max_visible = (pane_area_rows.saturating_sub(2)).max(1) as usize;
    let row_count = rows.len().max(1);
    let visible = row_count.min(max_visible);

    let content_w = rows
        .iter()
        .map(|s| dw(s))
        .chain([dw(title), dw(&footer)])
        .max()
        .unwrap_or(0);
    let inner_w = (content_w + 2).min(cols.saturating_sub(2) as usize);
    let box_w = (inner_w + 2) as u16;
    let box_h = (visible as u16) + 2; // top border + rows + bottom border (footer)
    if box_w < 3 || box_h < 3 || box_w > cols || box_h > pane_area_rows {
        return;
    }

    let sel = state.selected.min(rows.len().saturating_sub(1));
    let top = if sel >= visible { sel - visible + 1 } else { 0 };

    let row0 = pane_row_offset + (pane_area_rows.saturating_sub(box_h)) / 2;
    let col0 = (cols.saturating_sub(box_w)) / 2;
    let plain = plexy_glass_emulator::Attrs::empty();
    let rev = plexy_glass_emulator::Attrs::REVERSE;

    for r in 0..box_h {
        for c in 0..box_w {
            put_char(screen, row0 + r, col0 + c, border_glyph(r, c, box_h, box_w), plain);
        }
    }
    let tcol = col0 + 1 + ((box_w.saturating_sub(2) as usize).saturating_sub(dw(title)) / 2) as u16;
    put_str(screen, row0, tcol, title, plain, col0 + box_w - 1);
    let fcol = col0 + 1 + ((box_w.saturating_sub(2) as usize).saturating_sub(dw(&footer)) / 2) as u16;
    put_str(screen, row0 + box_h - 1, fcol, &footer, plain, col0 + box_w - 1);

    let inner_left = col0 + 1;
    let inner_right = col0 + box_w - 1; // exclusive max_col for put_str

    // Empty tree (last session just killed): blank interior for the one frame
    // before the client tears down.
    if rows.is_empty() {
        return;
    }
    let end = (top + visible).min(rows.len());
    for (vis_i, row_idx) in (top..end).enumerate() {
        let r = row0 + 1 + vis_i as u16;
        let row_attrs = if row_idx == sel { rev } else { plain };
        if row_idx == sel {
            for c in inner_left..inner_right {
                put_char(screen, r, c, ' ', row_attrs);
            }
        }
        put_str(screen, r, inner_left, &rows[row_idx], row_attrs, inner_right);
    }
}

/// Draw the centered choose-buffer box: one `name: preview` row per buffer, the
/// selected row REVERSE, scrolled to keep the selection visible.
fn paint_buffers(
    screen: &mut VirtualScreen,
    state: &crate::buffer::BufferPickerState,
    pane_row_offset: u16,
    pane_area_rows: u16,
    cols: u16,
) {
    let title = " Paste buffers ";
    let footer = " \u{2191}/\u{2193} move \u{b7} enter paste \u{b7} d delete \u{b7} esc close ";
    let empty_msg = "(no paste buffers)";
    let rows: Vec<String> = state
        .entries
        .iter()
        .map(|e| format!("{}: {}", e.name, e.preview))
        .collect();

    let dw = |s: &str| display_width(s) as usize;
    let max_visible = (pane_area_rows.saturating_sub(2)).max(1) as usize;
    let row_count = rows.len().max(1);
    let visible = row_count.min(max_visible);

    let content_w = rows
        .iter()
        .map(|s| dw(s))
        .chain([dw(title), dw(footer), if rows.is_empty() { dw(empty_msg) } else { 0 }])
        .max()
        .unwrap_or(0);
    let inner_w = (content_w + 2).min(cols.saturating_sub(2) as usize);
    let box_w = (inner_w + 2) as u16;
    let box_h = (visible as u16) + 2;
    if box_w < 3 || box_h < 3 || box_w > cols || box_h > pane_area_rows {
        return;
    }

    let sel = state.selected.min(rows.len().saturating_sub(1));
    let top = if sel >= visible { sel - visible + 1 } else { 0 };

    let row0 = pane_row_offset + (pane_area_rows.saturating_sub(box_h)) / 2;
    let col0 = (cols.saturating_sub(box_w)) / 2;
    let plain = plexy_glass_emulator::Attrs::empty();
    let rev = plexy_glass_emulator::Attrs::REVERSE;

    for r in 0..box_h {
        for c in 0..box_w {
            put_char(screen, row0 + r, col0 + c, border_glyph(r, c, box_h, box_w), plain);
        }
    }
    let tcol = col0 + 1 + ((box_w.saturating_sub(2) as usize).saturating_sub(dw(title)) / 2) as u16;
    put_str(screen, row0, tcol, title, plain, col0 + box_w - 1);
    let fcol = col0 + 1 + ((box_w.saturating_sub(2) as usize).saturating_sub(dw(footer)) / 2) as u16;
    put_str(screen, row0 + box_h - 1, fcol, footer, plain, col0 + box_w - 1);

    let inner_left = col0 + 1;
    let inner_right = col0 + box_w - 1;

    if rows.is_empty() {
        put_str(screen, row0 + 1, inner_left, empty_msg, plain, inner_right);
        return;
    }
    let end = (top + visible).min(rows.len());
    for (vis_i, row_idx) in (top..end).enumerate() {
        let r = row0 + 1 + vis_i as u16;
        let row_attrs = if row_idx == sel { rev } else { plain };
        if row_idx == sel {
            for c in inner_left..inner_right {
                put_char(screen, r, c, ' ', row_attrs);
            }
        }
        put_str(screen, r, inner_left, &rows[row_idx], row_attrs, inner_right);
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

/// Put a single char with attrs at (row, col), clipped to the screen. A
/// double-width char also writes a wide spacer in the next column. Returns the
/// display columns consumed (0 if clipped, else 1 or 2).
fn put_char(
    screen: &mut VirtualScreen,
    row: u16,
    col: u16,
    ch: char,
    attrs: plexy_glass_emulator::Attrs,
) -> u16 {
    if row >= screen.rows || col >= screen.cols {
        return 0;
    }
    // A placed glyph occupies at least one cell, even a lone combining mark.
    let w = plexy_glass_emulator::char_width(ch).max(1);
    if w == 2 && col + 1 >= screen.cols {
        return 0; // a wide glyph would straddle the edge; don't split it
    }
    let mut buf = [0u8; 4];
    let s = ch.encode_utf8(&mut buf);
    let cell = plexy_glass_emulator::Cell {
        grapheme: smol_str::SmolStr::new(s),
        attrs,
        ..plexy_glass_emulator::Cell::default()
    };
    screen.put(row, col, cell);
    if w == 2 {
        screen.put(row, col + 1, plexy_glass_emulator::Cell::wide_spacer());
    }
    w
}

/// Put a string starting at (row, col), advancing by each grapheme's display
/// width (a wide grapheme writes a spacer in its second column). Stops at
/// `max_col` (exclusive) or the screen edge, never splitting a wide grapheme.
/// Returns the display column just past the last grapheme written, so callers
/// can place a trailing cursor at the true end of the text.
fn put_str(
    screen: &mut VirtualScreen,
    row: u16,
    col: u16,
    text: &str,
    attrs: plexy_glass_emulator::Attrs,
    max_col: u16,
) -> u16 {
    let mut c = col;
    for (g, w) in plexy_glass_emulator::graphemes_with_width(text) {
        if c >= max_col || c >= screen.cols {
            break;
        }
        // A wide grapheme needs both its columns inside the bounds.
        if w == 2 && (c + 1 >= max_col || c + 1 >= screen.cols) {
            break;
        }
        let cell = plexy_glass_emulator::Cell {
            grapheme: smol_str::SmolStr::new(g),
            attrs,
            ..plexy_glass_emulator::Cell::default()
        };
        screen.put(row, c, cell);
        if w == 2 {
            screen.put(row, c + 1, plexy_glass_emulator::Cell::wide_spacer());
        }
        c += w;
    }
    c
}

/// One painted status-bar cell: a grapheme cluster, its display-column advance
/// (1 or 2), and its style.
type StatusCell = (smol_str::SmolStr, u16, plexy_glass_status::ResolvedStyle);

fn paint_status_row(
    screen: &mut VirtualScreen,
    status: &StatusLine,
    cols: u16,
    row: u16,
) {
    let cols_us = cols as usize;

    // Left takes priority and is clipped to the bar width.
    let left_cells = truncate_cells(collect_cells(&status.left), cols_us);
    let left_w = cells_width(&left_cells);

    // Right is pinned to the edge; clip it to whatever width remains.
    let right_cells = truncate_cells(collect_cells(&status.right), cols_us.saturating_sub(left_w));
    let right_w = cells_width(&right_cells);

    // Middle fills the gap; ellipsize (with a 1-column "…") if it overflows.
    let middle_budget = cols_us.saturating_sub(left_w + right_w);
    let middle_all = collect_cells(&status.middle);
    let middle_cells = if cells_width(&middle_all) <= middle_budget {
        middle_all
    } else if middle_budget == 0 {
        Vec::new()
    } else {
        let mut truncated = truncate_cells(middle_all, middle_budget - 1);
        truncated.push((
            smol_str::SmolStr::new("…"),
            1,
            plexy_glass_status::ResolvedStyle::default(),
        ));
        truncated
    };

    paint_cells(screen, row, 0, &left_cells);
    paint_cells(screen, row, left_w as u16, &middle_cells);
    paint_cells(screen, row, cols_us.saturating_sub(right_w) as u16, &right_cells);
}

/// Total display width of a run of status cells.
fn cells_width(cells: &[StatusCell]) -> usize {
    cells.iter().map(|(_, w, _)| *w as usize).sum()
}

/// Longest leading run of `cells` whose total display width is `<= max_w`,
/// never splitting a wide grapheme.
fn truncate_cells(cells: Vec<StatusCell>, max_w: usize) -> Vec<StatusCell> {
    let mut used = 0usize;
    let mut out = Vec::with_capacity(cells.len());
    for (g, w, style) in cells {
        if used + w as usize > max_w {
            break;
        }
        used += w as usize;
        out.push((g, w, style));
    }
    out
}

/// Paint status cells left-to-right from `start`, advancing by each grapheme's
/// display width and writing a wide spacer for 2-column graphemes.
fn paint_cells(screen: &mut VirtualScreen, row: u16, start: u16, cells: &[StatusCell]) {
    let mut c = start;
    for (g, w, style) in cells {
        if c >= screen.cols {
            break;
        }
        // A wide grapheme needs both of its columns inside the screen, so refuse
        // to place one that can't fit its spacer (matches `put_char`/`put_str` and
        // keeps the cell-grid invariant: a width-2 cell is always followed by a
        // wide spacer).
        if *w == 2 && c + 1 >= screen.cols {
            break;
        }
        screen.put(row, c, cell_for(g, style));
        if *w == 2 {
            screen.put(row, c + 1, plexy_glass_emulator::Cell::wide_spacer());
        }
        c = c.saturating_add(*w);
    }
}

fn collect_cells(segments: &[plexy_glass_status::Segment]) -> Vec<StatusCell> {
    let mut out = Vec::new();
    for seg in segments {
        for (g, w) in plexy_glass_emulator::graphemes_with_width(&seg.text) {
            out.push((smol_str::SmolStr::new(g), w, seg.style));
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
            title: None,
            marked: false,
        };
        let vs = Compositor::compose(&[view], (4, 6), None, StatusPlacement::Bottom, None, None, None);
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
            title: None,
            marked: false,
        };
        let mut sel = Selection::start(PaneId(0), 0, 0, SelectionKind::Char);
        sel.extend(0, 4, Rect::new(0, 0, 4, 6));
        let vs = Compositor::compose(&[view], (4, 6), None, StatusPlacement::Bottom, Some(&sel), None, None);
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
            title: None,
            marked: false,
        };
        let rv = PaneView {
            id: PaneId(1),
            rect: Rect::new(0, 4, 4, 3),
            screen: right.screen(),
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
            title: None,
            marked: false,
        };
        let vs = Compositor::compose(&[lv, rv], (4, 7), None, StatusPlacement::Bottom, None, None, None);
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
            title: None,
            marked: false,
        };
        let vs = Compositor::compose(&[view], (2, 4), None, StatusPlacement::Bottom, None, None, None);
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
            title: None,
            marked: false,
        };
        let vs = Compositor::compose(&[view], (5, 20), None, StatusPlacement::Bottom, None, None, None);
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
            title: None,
            marked: false,
        };
        let vs = Compositor::compose(&[view], (5, 20), None, StatusPlacement::Bottom, None, None, None);
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
    fn paint_cells_drops_wide_grapheme_that_cannot_fit_its_spacer() {
        use plexy_glass_status::ResolvedStyle;
        // 2-column screen: "a" fits at col 0, but "中" needs cols 1-2 and only
        // col 1 remains, so it must be dropped (never a width-2 glyph in the last
        // column with a non-spacer neighbour).
        let mut vs = VirtualScreen::blank(1, 2);
        let cells: Vec<StatusCell> = vec![
            (smol_str::SmolStr::new("a"), 1, ResolvedStyle::default()),
            (smol_str::SmolStr::new("中"), 2, ResolvedStyle::default()),
        ];
        paint_cells(&mut vs, 0, 0, &cells);
        assert_eq!(vs.cell(0, 0).unwrap().grapheme.as_str(), "a");
        assert_ne!(vs.cell(0, 1).unwrap().grapheme.as_str(), "中", "wide glyph must not straddle the edge");
    }

    #[test]
    fn status_left_places_wide_grapheme_with_spacer() {
        // "中B": 中 occupies cols 0-1 (cell + spacer), B lands at col 2.
        let mut e = Emulator::new(2, 8);
        pane(&mut e, b"X ");
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 2, 8),
            screen: e.screen(),
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
            title: None,
            marked: false,
        };
        let status = status_with_left("中B");
        let vs = Compositor::compose(&[view], (3, 8), Some(&status), StatusPlacement::Bottom, None, None, None);
        assert_eq!(vs.cell(2, 0).unwrap().grapheme.as_str(), "中");
        assert!(vs.cell(2, 1).unwrap().grapheme.is_empty(), "wide spacer after 中");
        assert_eq!(vs.cell(2, 2).unwrap().grapheme.as_str(), "B");
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
            title: None,
            marked: false,
        };
        let status = status_with_left("AB");
        let vs = Compositor::compose(&[view], (3, 4), Some(&status), StatusPlacement::Top, None, None, None);
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
            title: None,
            marked: false,
        };
        let status = status_with_left("AB");
        let vs = Compositor::compose(&[view], (3, 4), Some(&status), StatusPlacement::Bottom, None, None, None);
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
            title: None,
            marked: false,
        };
        let ov = OverlayView::RenamePrompt { label: "rename window", buf: "hi" };
        let vs = Compositor::compose(&[view], (4, 20), None, StatusPlacement::Bottom, None, Some(&ov), None);
        // Bottom row (3) is a REVERSE prompt bar.
        assert!(vs.cell(3, 0).unwrap().attrs.contains(Attrs::REVERSE), "prompt bar is REVERSE");
        // Text " rename window \u{25b8} hi", with 'r' at col 1.
        assert_eq!(vs.cell(3, 1).unwrap().grapheme.as_str(), "r");
    }

    #[test]
    fn overlay_command_prompt_paints_colon_bar() {
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
            title: None,
            marked: false,
        };
        let ov = OverlayView::Command { buf: "spl" };
        let vs = Compositor::compose(&[view], (4, 20), None, StatusPlacement::Bottom, None, Some(&ov), None);
        assert!(vs.cell(3, 0).unwrap().attrs.contains(Attrs::REVERSE), "command bar is REVERSE");
        // Text " :spl": ':' at col 1, 's' at col 2.
        assert_eq!(vs.cell(3, 1).unwrap().grapheme.as_str(), ":");
        assert_eq!(vs.cell(3, 2).unwrap().grapheme.as_str(), "s");
    }

    #[test]
    fn status_message_paints_reverse_bottom_row() {
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
            title: None,
            marked: false,
        };
        let vs = Compositor::compose(
            &[view],
            (4, 20),
            None,
            StatusPlacement::Bottom,
            None,
            None,
            Some("no session: foo"),
        );
        // Bottom row (3) is a REVERSE message bar; text rendered after a space.
        assert!(vs.cell(3, 0).unwrap().attrs.contains(Attrs::REVERSE), "message bar is REVERSE");
        assert_eq!(vs.cell(3, 1).unwrap().grapheme.as_str(), "n");
        assert_eq!(vs.cell(3, 2).unwrap().grapheme.as_str(), "o");
    }

    #[test]
    fn open_overlay_suppresses_status_message() {
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
            title: None,
            marked: false,
        };
        let ov = OverlayView::RenamePrompt { label: "rename window", buf: "hi" };
        let vs = Compositor::compose(
            &[view],
            (4, 20),
            None,
            StatusPlacement::Bottom,
            None,
            Some(&ov),
            Some("this message must not show"),
        );
        // The overlay owns the bottom row: 'r' of "rename window" is at col 1,
        // proving the message did not overwrite it.
        assert!(vs.cell(3, 0).unwrap().attrs.contains(Attrs::REVERSE));
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
            title: None,
            marked: false,
        };
        let lines = vec![("Ctrl+a c".to_string(), "New window".to_string())];
        let ov = OverlayView::Help { lines: &lines, scroll: 0 };
        let vs = Compositor::compose(&[view], (10, 40), None, StatusPlacement::Bottom, None, Some(&ov), None);
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
        // The help overlay must suppress the underlying pane cursor (the pane
        // is live with a visible cursor), matching rename/command overlays.
        assert!(!vs.cursor_visible, "help overlay hides the pane cursor");
    }

    fn picker_view(name: &str, label: &str, current: bool) -> crate::overlay::PickerEntry {
        crate::overlay::PickerEntry { name: name.into(), label: label.into(), is_current: current }
    }

    #[test]
    fn session_picker_renders_box_marker_and_selection() {
        use plexy_glass_emulator::Attrs;
        let mut e = Emulator::new(10, 50);
        pane(&mut e, b"x ");
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 10, 50),
            screen: e.screen(),
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
            title: None,
            marked: false,
        };
        let entries = vec![
            picker_view("main", "main - 1 win", true),
            picker_view("work", "work - 2 win", false),
        ];
        let ov = OverlayView::SessionPicker { entries: &entries, filter: "", selected: 1 };
        let vs = Compositor::compose(&[view], (10, 50), None, StatusPlacement::Bottom, None, Some(&ov), None);

        let mut found_corner = false;
        let mut found_marker = false;
        let mut selected_reverse = false;
        for r in 0..10 {
            for c in 0..50 {
                let cell = vs.cell(r, c).unwrap();
                match cell.grapheme.as_str() {
                    "\u{250c}" => found_corner = true,
                    "*" => found_marker = true,
                    "w" if cell.attrs.contains(Attrs::REVERSE) => selected_reverse = true,
                    _ => {}
                }
            }
        }
        assert!(found_corner, "picker box border drawn");
        assert!(found_marker, "current session marked with *");
        assert!(selected_reverse, "selected row painted REVERSE");
        assert!(!vs.cursor_visible, "picker hides the pane cursor");
    }

    #[test]
    fn session_picker_places_wide_grapheme_with_spacer() {
        let mut e = Emulator::new(10, 50);
        pane(&mut e, b"x ");
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 10, 50),
            screen: e.screen(),
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
            title: None,
            marked: false,
        };
        // A CJK session name must be sized and placed as one cell + a spacer.
        let entries = vec![picker_view("中文", "中文", false)];
        let ov = OverlayView::SessionPicker { entries: &entries, filter: "", selected: 0 };
        let vs = Compositor::compose(&[view], (10, 50), None, StatusPlacement::Bottom, None, Some(&ov), None);
        let mut found = false;
        for r in 0..10 {
            for c in 0..49 {
                if vs.cell(r, c).unwrap().grapheme.as_str() == "中" {
                    // The wide grapheme's second column is a wide spacer.
                    assert!(
                        vs.cell(r, c + 1).unwrap().grapheme.is_empty(),
                        "wide grapheme must be followed by a wide spacer"
                    );
                    found = true;
                }
            }
        }
        assert!(found, "wide grapheme rendered in the picker");
    }

    #[test]
    fn session_picker_shows_no_match_message() {
        let mut e = Emulator::new(10, 50);
        pane(&mut e, b"x ");
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 10, 50),
            screen: e.screen(),
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
            title: None,
            marked: false,
        };
        let entries = vec![picker_view("main", "main", true)];
        let ov = OverlayView::SessionPicker { entries: &entries, filter: "zzz", selected: 0 };
        let vs = Compositor::compose(&[view], (10, 50), None, StatusPlacement::Bottom, None, Some(&ov), None);
        let mut text = String::new();
        for r in 0..10 {
            for c in 0..50 {
                text.push_str(vs.cell(r, c).unwrap().grapheme.as_str());
            }
        }
        assert!(text.contains("no matching sessions"), "empty-state message shown");
    }

    #[test]
    fn help_box_suppressed_when_pane_area_too_small() {
        // Host 4 rows with a status bar → pane band is 3 rows; the smallest
        // help box is 4 rows, so it must be suppressed rather than overflow
        // onto the status row.
        let mut e = Emulator::new(3, 40);
        pane(&mut e, b"x ");
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 3, 40),
            screen: e.screen(),
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
            title: None,
            marked: false,
        };
        let lines = vec![("Ctrl+a c".to_string(), "New window".to_string())];
        let ov = OverlayView::Help { lines: &lines, scroll: 0 };
        let status = status_with_left("S");
        let vs =
            Compositor::compose(&[view], (4, 40), Some(&status), StatusPlacement::Bottom, None, Some(&ov), None);
        let mut found_corner = false;
        for r in 0..4 {
            for c in 0..40 {
                if vs.cell(r, c).unwrap().grapheme.as_str() == "\u{250c}" {
                    found_corner = true;
                }
            }
        }
        assert!(!found_corner, "help box must be suppressed when it would overflow the band");
    }

    fn tree_node(
        session: &str,
        window: Option<u32>,
        pane: Option<u32>,
        depth: u8,
        label: &str,
        is_current: bool,
    ) -> crate::tree::TreeNode {
        crate::tree::TreeNode {
            session: session.into(),
            window: window.map(crate::WindowId),
            pane: pane.map(crate::PaneId),
            depth,
            label: label.into(),
            name: label.into(),
            index: 1,
            is_current,
        }
    }

    #[test]
    fn tree_overlay_renders_box_marker_indent_and_selection() {
        use plexy_glass_emulator::Attrs;
        let mut e = Emulator::new(12, 50);
        pane(&mut e, b"x ");
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 12, 50),
            screen: e.screen(),
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
            title: None,
            marked: false,
        };
        let state = crate::tree::TreeState {
            nodes: vec![
                tree_node("main", None, None, 0, "main — 1 win, 2 panes", true),
                tree_node("main", Some(0), None, 1, "1: shell", true),
                tree_node("main", Some(0), Some(0), 2, "pane 1", false),
                tree_node("main", Some(0), Some(1), 2, "pane 2", false),
            ],
            selected: 1,
            mode: crate::tree::TreeMode::Navigate,
        };
        let ov = OverlayView::Tree { state: &state };
        let vs = Compositor::compose(&[view], (12, 50), None, StatusPlacement::Bottom, None, Some(&ov), None);

        let mut found_corner = false;
        let mut found_marker = false;
        let mut selected_reverse = false;
        // Reconstruct each row's text to check depth indentation.
        let mut session_row: Option<String> = None;
        let mut pane_row: Option<String> = None;
        for r in 0..12 {
            let mut line = String::new();
            for c in 0..50 {
                let cell = vs.cell(r, c).unwrap();
                line.push_str(cell.grapheme.as_str());
                match cell.grapheme.as_str() {
                    "\u{250c}" => found_corner = true,
                    "*" => found_marker = true,
                    // selected row is "1: shell" (REVERSE); 's' of shell qualifies.
                    "s" if cell.attrs.contains(Attrs::REVERSE) => selected_reverse = true,
                    _ => {}
                }
            }
            if line.contains("1 win") {
                session_row = Some(line.clone());
            }
            if line.contains("pane 2") {
                pane_row = Some(line.clone());
            }
        }
        assert!(found_corner, "tree box border drawn");
        assert!(found_marker, "current-path node marked *");
        assert!(selected_reverse, "selected row painted REVERSE");
        assert!(!vs.cursor_visible, "tree hides the pane cursor");
        // Depth-2 pane row is indented further than the depth-0 session row.
        // Both rows share the same box-border prefix, so the in-line offset of
        // the content reflects the interior indent.
        let sr = session_row.expect("session row rendered");
        let pr = pane_row.expect("pane row rendered");
        assert!(
            pr.find("pane 2").unwrap() > sr.find("main").unwrap(),
            "deeper node is indented more"
        );
    }

    #[test]
    fn buffer_overlay_renders_box_rows_and_selection() {
        use plexy_glass_emulator::Attrs;
        let mut e = Emulator::new(10, 50);
        pane(&mut e, b"x ");
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 10, 50),
            screen: e.screen(),
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
            title: None,
            marked: false,
        };
        let state = crate::buffer::BufferPickerState {
            entries: vec![
                crate::buffer::BufferEntry { name: "buffer1".into(), preview: "hello".into() },
                crate::buffer::BufferEntry { name: "buffer0".into(), preview: "world".into() },
            ],
            selected: 1,
        };
        let ov = OverlayView::Buffer { state: &state };
        let vs = Compositor::compose(&[view], (10, 50), None, StatusPlacement::Bottom, None, Some(&ov), None);
        let mut text = String::new();
        let mut selected_reverse = false;
        for r in 0..10 {
            for c in 0..50 {
                let cell = vs.cell(r, c).unwrap();
                text.push_str(cell.grapheme.as_str());
                if cell.grapheme.as_str() == "w" && cell.attrs.contains(Attrs::REVERSE) {
                    selected_reverse = true; // 'w' of the selected "world" row
                }
            }
        }
        assert!(text.contains("buffer1: hello"));
        assert!(text.contains("buffer0: world"));
        assert!(selected_reverse, "selected row painted REVERSE");
        assert!(!vs.cursor_visible);
    }

    #[test]
    fn buffer_overlay_empty_shows_message() {
        let mut e = Emulator::new(10, 50);
        pane(&mut e, b"x ");
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 10, 50),
            screen: e.screen(),
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
            title: None,
            marked: false,
        };
        let state = crate::buffer::BufferPickerState { entries: vec![], selected: 0 };
        let ov = OverlayView::Buffer { state: &state };
        let vs = Compositor::compose(&[view], (10, 50), None, StatusPlacement::Bottom, None, Some(&ov), None);
        let mut text = String::new();
        for r in 0..10 {
            for c in 0..50 {
                text.push_str(vs.cell(r, c).unwrap().grapheme.as_str());
            }
        }
        assert!(text.contains("no paste buffers"));
    }

    #[test]
    fn marked_paneview_renders_magenta_border() {
        use plexy_glass_emulator::Color;
        let mut e = Emulator::new(1, 6);
        pane(&mut e, b"x ");
        // Inset the pane so a border ring exists around it within the band.
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(1, 1, 1, 6),
            screen: e.screen(),
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
            title: None,
            marked: true,
        };
        let vs = Compositor::compose(&[view], (3, 8), None, StatusPlacement::Bottom, None, None, None);
        let mut magenta = false;
        for r in 0..3 {
            for c in 0..8 {
                if vs.cell(r, c).unwrap().fg == Color::Indexed(13) {
                    magenta = true;
                }
            }
        }
        assert!(magenta, "marked PaneView renders a magenta border via compose");
    }

    #[test]
    fn tree_overlay_confirm_kill_footer() {
        let mut e = Emulator::new(12, 60);
        pane(&mut e, b"x ");
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 12, 60),
            screen: e.screen(),
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
            title: None,
            marked: false,
        };
        let state = crate::tree::TreeState {
            nodes: vec![tree_node("main", Some(0), None, 1, "1: shell", false)],
            selected: 0,
            mode: crate::tree::TreeMode::ConfirmKill,
        };
        let ov = OverlayView::Tree { state: &state };
        let vs = Compositor::compose(&[view], (12, 60), None, StatusPlacement::Bottom, None, Some(&ov), None);
        let mut text = String::new();
        for r in 0..12 {
            for c in 0..60 {
                text.push_str(vs.cell(r, c).unwrap().grapheme.as_str());
            }
        }
        assert!(text.contains("Kill window"), "confirm-kill footer shown: {text}");
    }
}
