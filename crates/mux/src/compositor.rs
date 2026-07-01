//! Combine multiple pane screens into a single VirtualScreen, with borders
//! and an optional status-bar row.

use crate::{
    blocks::{self, FoldProjection, block_status_at, viewport_block_status},
    borders::{self, BlockBorderColors},
    pane_id::PaneId,
    rect::Rect,
    status::StatusLine,
    virtual_screen::VirtualScreen,
};
use plexy_glass_emulator::{Attrs, Screen, display_width};
use std::collections::HashMap;

/// Resolved render colors for hint mode (built from `cfg.hints`).
#[derive(Debug, Clone, Copy)]
pub struct HintColors {
    pub label_fg: plexy_glass_emulator::Color,
    pub label_bg: plexy_glass_emulator::Color,
    pub match_fg: plexy_glass_emulator::Color,
}

/// Per-pane fold rendering context, built once per frame: the fold projection
/// (identity for copy/block-mode panes, where folds don't apply) and the top
/// **visible** line index for the current scroll position. Display row `r` of a
/// pane shows unified line `proj.to_unified(top_visible + r)`.
struct FoldCtx {
    proj: FoldProjection,
    top_visible: u32,
}

impl FoldCtx {
    /// Build for a pane. The live view and **block mode** honour folds; copy mode
    /// renders expanded (raw text for selection).
    fn for_view(view: &PaneView<'_>) -> Self {
        let rows = u32::from(view.rect.rows);
        // Copy mode: identity projection, the prior viewport_top behavior.
        if view.copy_mode.is_some() {
            let proj = FoldProjection::identity(blocks::total_lines(view.screen));
            let top_visible = proj
                .visible_total()
                .saturating_sub(rows)
                .saturating_sub(effective_scroll_for(view));
            return Self { proj, top_visible };
        }
        let proj = FoldProjection::build(view.screen);
        let top_visible = if let Some(bm) = view.block_mode {
            // Block mode renders folds: show the selected block (recenter pins
            // `viewport_top` to it) at the top in visible space, clamped so we never
            // scroll past the bottom screenful.
            proj.from_unified(bm.viewport_top)
                .unwrap_or(0)
                .min(proj.visible_total().saturating_sub(rows))
        } else {
            // Live + wheel: `scroll_offset` is VISIBLE-line space (the daemon's
            // wheel / prompt-jump produce visible offsets), so a plain bottom
            // anchor works: at offset 0 the prompt sits at the bottom, and
            // scrolling moves by visible lines (folds skipped).
            proj.visible_total()
                .saturating_sub(rows)
                .saturating_sub(view.scroll_offset)
        };
        Self { proj, top_visible }
    }

    /// Unified line shown at display row `r`, or `None` when `r` is past the
    /// pane's visible content (paint blank).
    fn line_at(&self, r: u16) -> Option<u32> {
        let v = self.top_visible + u32::from(r);
        (v < self.proj.visible_total()).then(|| self.proj.to_unified(v))
    }

    /// Display row of unified `line`, or `None` when it's folded away or outside
    /// the visible window.
    fn display_row(&self, line: u32, rows: u16) -> Option<u16> {
        let v = self.proj.from_unified(line)?;
        let r = v.checked_sub(self.top_visible)?;
        (r < u32::from(rows)).then_some(r as u16)
    }
}

/// Which side of an in-progress pane-swap drag a pane is, for the border
/// highlight. `None` for panes not involved in a drag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaneDragRole {
    #[default]
    None,
    Source,
    Target,
}

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
    /// When Some, the pane is in block-mode; the compositor uses the block-mode
    /// viewport and paints the selected-block bracket.
    pub block_mode: Option<&'a crate::BlockMode>,
    /// User-assigned pane name, painted on the pane's top border. `None` hides
    /// the title (plain border).
    pub title: Option<&'a str>,
    /// Whether this pane is the session's marked pane (drawn with a distinct
    /// border color).
    pub marked: bool,
    /// Whether this pane is the source or target of an in-progress pane-swap
    /// drag, for the border highlight.
    pub drag_role: PaneDragRole,
}

/// Minimum popup box (border included): 3-row × 10-col interior.
const MIN_POPUP_ROWS: u16 = 5;
const MIN_POPUP_COLS: u16 = 12;

/// The floating popup's outer box (border included): 80% × 80% of `pane_area`
/// (the logical layout band), centered, clamped to a minimum interior of
/// 3 rows × 10 cols, and never larger than the band itself. The single source
/// of truth for popup geometry: the daemon computes it once per use (spawn,
/// resize, mouse, render) over `WindowManager::viewport()`.
pub fn popup_rect(pane_area: Rect) -> Rect {
    // invariant: rows/cols are u16, so (u32 * 8) / 10 < u16::MAX and the
    // narrowing back to u16 cannot truncate.
    let want_rows = u16::try_from((u32::from(pane_area.rows) * 8) / 10).unwrap_or(u16::MAX);
    let want_cols = u16::try_from((u32::from(pane_area.cols) * 8) / 10).unwrap_or(u16::MAX);
    let rows = want_rows.max(MIN_POPUP_ROWS).min(pane_area.rows);
    let cols = want_cols.max(MIN_POPUP_COLS).min(pane_area.cols);
    let row = pane_area.row + (pane_area.rows - rows) / 2;
    let col = pane_area.col + (pane_area.cols - cols) / 2;
    Rect::new(row, col, rows, cols)
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
    /// The structured history palette: a centered box with a filter line and one
    /// row per matching command block (status glyph, duration, `session/window`
    /// provenance, command), the selected row highlighted.
    History { state: &'a crate::history::HistoryState },
    /// Hint mode: labels painted over the dimmed pane, one per still-matching
    /// target. No box, the labels float directly on the pane content.
    Hint {
        state: &'a crate::hint::HintState,
        colors: HintColors,
    },
    /// The one-time welcome modal: a centered box of pre-built lines (greeting,
    /// essential keys, how to get help/detach, how to disable). Any key dismisses.
    Welcome { lines: &'a [String] },
}

/// A render-ready view of the floating popup pane: a live PTY-backed grid in
/// a bordered, titled box. `rect` is the OUTER box in logical pane-band
/// coordinates (same space as `PaneView.rect`), computed by the daemon via
/// `popup_rect` so render and hit-testing cannot drift.
pub struct PopupView<'a> {
    pub rect: Rect,
    pub screen: &'a Screen,
    pub title: &'a str,
}

/// A resolved transient status-line message, ready to paint. The coordinator
/// resolves the severity to a concrete glyph + colors (mirroring how
/// [`BlockBorderColors`] are pre-resolved from config), so the compositor stays
/// config-/palette-free.
pub struct MessageView<'a> {
    pub text: &'a str,
    /// Leading glyph, already selected for the active glyph tier. The non-color
    /// severity channel (legible without color).
    pub glyph: &'a str,
    pub fg: plexy_glass_emulator::Color,
    pub bg: plexy_glass_emulator::Color,
}

/// Palette-resolved chrome colors (pane border rings and overlay-box styling),
/// pre-resolved by the coordinator (like [`BlockBorderColors`]) so the
/// compositor stays palette-free. The overlay-box selection row stays `REVERSE`
/// (a deliberate, theme-agnostic selection cue).
#[derive(Clone, Copy)]
pub struct ChromeColors {
    pub rings: crate::borders::RingColors,
    pub overlay_border: plexy_glass_emulator::Color,
    pub overlay_title: plexy_glass_emulator::Color,
    pub overlay_footer: plexy_glass_emulator::Color,
    pub overlay_bg: plexy_glass_emulator::Color,
}

impl ChromeColors {
    /// Terminal-default chrome: the historical ANSI border rings and an
    /// uncolored overlay box. Fallback + test default (so tests render exactly as
    /// before); production drives every color from the palette.
    pub const fn ansi_default() -> Self {
        use plexy_glass_emulator::Color;
        Self {
            rings: crate::borders::RingColors::ansi_default(),
            overlay_border: Color::Default,
            overlay_title: Color::Default,
            overlay_footer: Color::Default,
            overlay_bg: Color::Default,
        }
    }
}

// One optional layer per frame element (status/selection/overlay/message/
// popup/blocks); a params struct would just rename the same positions.
#[allow(clippy::too_many_arguments)] // optional frame layers; a params struct would just rename them
pub fn compose(
    panes: &[PaneView<'_>],
    host_size: (u16, u16),
    status: Option<&StatusLine>,
    placement: StatusPlacement,
    selection: Option<&crate::selection::Selection>,
    overlay: Option<&OverlayView<'_>>,
    message: Option<MessageView<'_>>,
    popup: Option<&PopupView<'_>>,
    // blocks: None = feature disabled (no block work per frame).
    blocks: Option<&BlockBorderColors>,
    // Color of the block-mode selection bracket (always available, the
    // bracket works even when `blocks` coloring is disabled).
    block_select_color: plexy_glass_emulator::Color,
    // Palette-resolved pane-border-ring + overlay-box colors.
    chrome: ChromeColors,
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

    // Per-pane fold context (projection + visible top), built once and reused by
    // the content copy, block-status border, inline images, and the cursor so a
    // folded block occupies zero display rows consistently everywhere.
    let fold_ctx: HashMap<PaneId, FoldCtx> =
        panes.iter().map(|v| (v.id, FoldCtx::for_view(v))).collect();

    // Copy each pane's emulator cells into its rect. Display row `r` maps to a
    // unified line through the pane's fold context (which collapses folded blocks
    // in the live view and block mode; copy mode is identity). A folded block's
    // command rows are dimmed so a fold reads as folded, not as a no-output run.
    for view in panes {
        let ctx = &fold_ctx[&view.id];
        let max_r = view.rect.rows;
        let max_c = view.rect.cols.min(view.screen.active.num_cols());
        for r in 0..max_r {
            if view.rect.row.saturating_add(r) >= pane_area_rows {
                continue;
            }
            let Some(line) = ctx.line_at(r) else { continue };
            let Some(row) = crate::blocks::row_at(view.screen, line) else { continue };
            let dim = !ctx.proj.is_identity() && crate::blocks::is_folded_command_line(view.screen, line);
            let cells = row.cells.as_slice();
            for c in 0..max_c {
                if view.rect.col.saturating_add(c) >= host_cols {
                    continue;
                }
                if let Some(cell) = cells.get(c as usize) {
                    let mut cell = cell.clone();
                    // Dim only the actual command glyphs, and keep trailing blanks
                    // truly blank so the fold summary's overlap check still works.
                    if dim && !cell.is_blank() {
                        cell.attrs |= Attrs::DIM;
                    }
                    screen.put(
                        pane_row_offset + view.rect.row.saturating_add(r),
                        view.rect.col.saturating_add(c),
                        cell,
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
            // Clamp to the pane's own columns: `cm.cursor`/`anchor` cols are captured
            // against the grid width that was live when the user navigated, so a
            // column-shrinking resize while copy mode stays open can leave a col
            // past the pane rect. `cell_mut` is only bounds-safe against the whole
            // host screen, so without this the REVERSE would bleed onto the pane
            // border / a neighbour (mirrors the search-highlight + content paths).
            let last = view.rect.cols.saturating_sub(1);
            let row_start = if line == start.0 { start.1 } else { 0 }.min(last);
            let row_end = if line == end.0 { end.1 } else { last }.min(last);
            for c in row_start..=row_end {
                let host_c = view.rect.col + c;
                if host_c >= host_cols {
                    break;
                }
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
            // Clamp to the pane's own columns: a match's `col_end` is captured
            // at search time against the grid width, so a column-shrinking
            // resize while copy mode stays open can leave `col_end` past the
            // pane rect. `cell_mut` is only bounds-safe against the whole host
            // screen, so without this the HIGHLIGHT would bleed onto the
            // pane border / a neighbouring pane (mirrors the content path).
            let last_col = m.col_end.min(view.rect.cols.saturating_sub(1));
            for c in m.col_start..=last_col {
                let host_c = view.rect.col + c;
                if host_c >= host_cols {
                    break;
                }
                if let Some(cell) = screen.cell_mut(host_r, host_c) {
                    cell.attrs |= plexy_glass_emulator::Attrs::HIGHLIGHT;
                }
            }
        }
    }

    // Block-mode filter: dim non-matching blocks, highlight the query in matches.
    // Mirrors the copy-mode selection/search passes; gated on an active filter
    // with a non-empty query, and suppressed on the alt screen (the block marks
    // live on the main grid, like every other block-mode render path).
    for view in panes {
        let Some(bm) = view.block_mode else { continue };
        let Some(filter) = &bm.filter else { continue };
        if filter.query.is_empty() || view.screen.alt.is_some() {
            continue;
        }
        // Map each DISPLAY row to its unified line through the SAME fold
        // projection the content copy used. Block mode renders folds, so the old
        // `viewport_top + r` (a unified base plus a visible-row offset) landed on
        // the wrong rows whenever a block above the viewport was folded.
        let ctx = &fold_ctx[&view.id];

        // Dim: any display row whose governing block is not a match (including
        // rows with no governing prompt) gets DIM on its content cells.
        for r in 0..view.rect.rows {
            let Some(line) = ctx.line_at(r) else { continue };
            let is_match = crate::blocks::prompt_at_or_above(view.screen, line)
                .is_some_and(|p| filter.matches.contains(&p));
            if is_match {
                continue;
            }
            let host_r = pane_row_offset + view.rect.row + r;
            for c in 0..view.rect.cols {
                let host_c = view.rect.col + c;
                if let Some(cell) = screen.cell_mut(host_r, host_c) {
                    cell.attrs |= plexy_glass_emulator::Attrs::DIM;
                }
            }
        }

        // Highlight: query occurrences on each visible display row's unified line.
        for r in 0..view.rect.rows {
            let Some(line) = ctx.line_at(r) else { continue };
            let host_r = pane_row_offset + view.rect.row + r;
            for (_, col_start, col_end) in
                filter_match_spans(view.screen, &filter.query, line, line + 1)
            {
                let last_col = col_end.min(view.rect.cols.saturating_sub(1));
                for c in col_start..=last_col {
                    let host_c = view.rect.col + c;
                    if host_c >= host_cols {
                        break;
                    }
                    if let Some(cell) = screen.cell_mut(host_r, host_c) {
                        cell.attrs |= plexy_glass_emulator::Attrs::HIGHLIGHT;
                    }
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
            // Same fold context as the content copy, so block status and the
            // selection bracket agree with what's painted. `top` is the VISIBLE
            // top line; block extents map into visible space through `ctx.proj`.
            let ctx = &fold_ctx[&v.id];
            let top = ctx.top_visible;
            let block_rows: Vec<Option<crate::blocks::BlockLineStatus>> = if blocks.is_none() {
                vec![]
            } else if ctx.proj.is_identity() {
                // No folds: the single-pass scan (display row r == unified top+r).
                viewport_block_status(v.screen, top, v.rect.rows)
            } else {
                // Folded: status per display row through the projection.
                (0..v.rect.rows)
                    .map(|r| ctx.line_at(r).and_then(|line| block_status_at(v.screen, line)))
                    .collect()
            };
            // Selected-block bracket (independent of the blocks toggle, but
            // suppressed on the alt screen: a full-screen app entered while
            // block mode was open must not get a bracket painted over it; the
            // marks belong to the main grid. Mirrors `viewport_block_status`'s
            // alt guard. The 0-row guard keeps the `vp_end - 1` math total.)
            let selected_block = v.block_mode.and_then(|bm| {
                if v.rect.rows == 0 || v.screen.alt.is_some() {
                    return None;
                }
                let (u_start, u_end_full) = crate::blocks::block_extent(v.screen, bm.selected);
                // A folded selected block's bracket spans only its visible command
                // rows (the output is collapsed).
                let folded = crate::blocks::row_at(v.screen, bm.selected)
                    .is_some_and(|r| r.mark.is_folded());
                let u_end = if folded {
                    crate::blocks::foldable_output(v.screen, bm.selected)
                        .map_or(u_end_full, |(out_start, _)| out_start.saturating_sub(1))
                } else {
                    u_end_full
                };
                // Map to visible display rows (prompt + command rows are never folded).
                let vs = ctx.proj.from_unified(u_start)?;
                let ve = ctx.proj.from_unified(u_end)?;
                let vp_end = top + u32::from(v.rect.rows); // exclusive, visible
                if ve < top || vs >= vp_end {
                    return None; // block entirely off-screen
                }
                let clip_start = vs.max(top);
                let clip_end = ve.min(vp_end - 1);
                Some(borders::SelectedBlock {
                    rows: ((clip_start - top) as u16, (clip_end - top) as u16),
                    cap_top: vs >= top,
                    cap_bottom: ve < vp_end,
                    color: block_select_color,
                })
            });
            borders::PaneFrame {
                rect: r,
                active: v.is_active,
                marked: v.marked,
                drag_role: v.drag_role,
                title: v.title,
                block_rows,
                selected_block,
            }
        })
        .collect();
    borders::draw(&frames, band, &mut screen, blocks, chrome.rings);

    // Command-row annotations: a dim, right-aligned note on each visible command
    // row, composing the fold summary ("▸ N lines ✓/✗", folded blocks only) with
    // the block's duration ("2.3s", when ≥ threshold). The annotation anchors on
    // the COMMAND row (the `133;B` prompt-end row, or `133;A` when there's no B),
    // not the prompt-start row, so it lands on the typed command even under a
    // multi-line prompt (starship etc.). Block data is keyed off the block's
    // prompt line. Shown in the collapsed/live view and block mode; duration
    // suppressed in copy mode; omitted when the command text leaves no room.
    for v in panes {
        if v.screen.alt.is_some() {
            continue;
        }
        let ctx = &fold_ctx[&v.id];
        let threshold = blocks.and_then(|b| b.duration_threshold_ms);
        let duration_on = threshold.is_some() && v.copy_mode.is_none();
        for r in 0..v.rect.rows {
            if v.rect.row.saturating_add(r) >= pane_area_rows {
                continue;
            }
            let Some(line) = ctx.line_at(r) else { continue };
            if !crate::blocks::is_command_row(v.screen, line) {
                continue;
            }
            // The block this command row belongs to (its prompt-start line).
            let prompt_line = crate::blocks::prompt_at_or_above(v.screen, line).unwrap_or(line);
            let folded = crate::blocks::row_at(v.screen, prompt_line)
                .is_some_and(|row| row.mark.is_folded());
            let status = block_status_at(v.screen, prompt_line);
            // Fold summary part: folded blocks only (unchanged logic).
            let summary = if folded {
                crate::blocks::foldable_output(v.screen, prompt_line).map(|(start, end)| {
                    let n = end - start + 1;
                    let glyph = match status {
                        Some(crate::blocks::BlockLineStatus::Ok) => " ✓",
                        Some(crate::blocks::BlockLineStatus::Failed) => " ✗",
                        None => "",
                    };
                    format!("▸ {n} lines{glyph}")
                })
            } else {
                None
            };
            // Duration part: gated by the threshold, suppressed in copy mode.
            let dur = duration_on
                .then(|| crate::blocks::closing_duration(v.screen, prompt_line))
                .flatten()
                .filter(|&ms| ms >= threshold.unwrap_or(u32::MAX))
                .map(crate::blocks::format_duration);
            let annotation = match (summary, dur) {
                (Some(s), Some(d)) => format!("{s} · {d}"),
                (Some(s), None) => s,
                (None, Some(d)) => d,
                (None, None) => continue,
            };
            let sw = display_width(&annotation);
            let host_row = pane_row_offset + v.rect.row + r;
            let pane_right = (v.rect.col + v.rect.cols).min(host_cols);
            if pane_right <= v.rect.col + sw {
                continue; // pane too narrow for the annotation
            }
            let start_col = pane_right - sw;
            // Don't overwrite the command text: scan the WHOLE pane row (including
            // the annotation columns) for the command's right edge; omit the
            // annotation when the command reaches into (or within one cell of) it.
            let cmd_end = (v.rect.col..pane_right)
                .rev()
                .find(|&c| screen.cell(host_row, c).is_some_and(|cell| !cell.is_blank()))
                .map_or(v.rect.col, |c| c + 1);
            if cmd_end >= start_col {
                continue; // command fills the row up to the annotation → omit
            }
            put_str(&mut screen, host_row, start_col, &annotation, Attrs::DIM, pane_right);
            // Color the annotation with the block's ok/fail color when known.
            if let (Some(bc), Some(st)) = (blocks, status) {
                let color = match st {
                    crate::blocks::BlockLineStatus::Ok => bc.ok,
                    crate::blocks::BlockLineStatus::Failed => bc.fail,
                };
                for c in start_col..pane_right {
                    if let Some(cell) = screen.cell_mut(host_row, c) {
                        cell.fg = color;
                    }
                }
            }
        }
    }

    // Sticky command header: while a live pane is SCROLLED BACK and its top row
    // sits inside a block whose command line is above the viewport, pin that
    // command (dimmed, so it blends with the theme rather than shouting) on the
    // pane's top row, so you know what produced the history you're scrolled into.
    // Only during scrollback: at the live bottom you're watching fresh output and
    // don't need it. Block mode shows command lines already; copy mode owns
    // selection. Folds compose for free: a folded block has no visible output
    // rows, so the top line can never land inside one.
    if blocks.is_some_and(|b| b.sticky_header) {
        for v in panes {
            if v.screen.alt.is_some()
                || v.copy_mode.is_some()
                || v.block_mode.is_some()
                || v.scroll_offset == 0
            {
                continue;
            }
            if v.rect.row >= pane_area_rows {
                continue;
            }
            let ctx = &fold_ctx[&v.id];
            let Some(top_line) = ctx.line_at(0) else { continue };
            let Some(prompt) = crate::blocks::prompt_at_or_above(v.screen, top_line) else {
                continue;
            };
            if prompt >= top_line {
                continue; // the command row itself is visible at the top
            }
            let Some(cmd) = crate::blocks::block_command_line(v.screen, prompt) else {
                continue;
            };
            let host_row = pane_row_offset + v.rect.row;
            let pane_right = (v.rect.col + v.rect.cols).min(host_cols);
            // Replace the row's content with the dimmed command line, no bright
            // bar: the cleared cells keep the theme background.
            for c in v.rect.col..pane_right {
                if let Some(cell) = screen.cell_mut(host_row, c) {
                    *cell = plexy_glass_emulator::Cell::default();
                }
            }
            let cmd_end = put_str(&mut screen, host_row, v.rect.col, &cmd, Attrs::DIM, pane_right);
            // Right-aligned duration on the header (same overlap guard as inline).
            if let Some(ms) = blocks
                .and_then(|b| b.duration_threshold_ms)
                .and_then(|t| crate::blocks::closing_duration(v.screen, prompt).filter(|&ms| ms >= t))
            {
                let d = crate::blocks::format_duration(ms);
                let dw = display_width(&d);
                if pane_right > v.rect.col + dw {
                    let start_col = pane_right - dw;
                    if cmd_end < start_col {
                        put_str(&mut screen, host_row, start_col, &d, Attrs::DIM, pane_right);
                    }
                }
            }
        }
    }

    // Inline-image placements: resolve each pane's placements to host cells,
    // clipped to the visible sub-rectangle (viewport rows ∩ pane columns ∩ the
    // logical pane band) with a matching source-pixel crop. Copy/block mode is a
    // per-pane viewport (honoured via effective_scroll_for), so images follow it.
    // A modal overlay or popup owns the screen, so we suppress all images then,
    // since a Kitty source rect can't crop around an arbitrary floating box. The
    // seq is folded with the pane id so per-Screen counters can't collide.
    if overlay.is_none() && popup.is_none() {
        let mut placements: Vec<crate::virtual_screen::VisiblePlacement> = Vec::new();
        for v in panes {
            if v.screen.alt.is_some() || v.screen.placements.is_empty() {
                continue;
            }
            // Map through the pane's fold context: a folded block's output (and
            // any image in it) collapses, and the window is in visible space.
            let ctx = &fold_ctx[&v.id];
            let top = ctx.top_visible;
            // Pane bottom in visible viewport rows, also bounded by the logical
            // pane band (so a tall image can't paint over the status bar).
            let band_rows = u32::from(pane_area_rows.saturating_sub(v.rect.row));
            let vis_bottom_local = u32::from(v.rect.rows).min(band_rows);
            for p in &v.screen.placements {
                let Some(img) = v.screen.images.get(p.image_id) else {
                    continue; // image evicted, skip
                };
                // The image's top in visible space; `None` when it sits inside a
                // folded block → hidden. An unfolded block's output is contiguous
                // and fully visible, so its rows are [img_top, img_top + rows).
                let Some(img_top) = ctx.proj.from_unified(p.anchor_line) else {
                    continue;
                };
                let img_bot = img_top + u32::from(p.rows); // exclusive
                // Vertical visible span, in visible viewport rows.
                let vis_top = img_top.max(top);
                let vis_bot = img_bot.min(top + vis_bottom_local);
                if vis_bot <= vis_top {
                    continue; // fully scrolled off / below the band
                }
                let rows_off = (vis_top - img_top) as u16; // image rows hidden above
                let vis_rows = (vis_bot - vis_top) as u16;
                // Horizontal: clip the cell box to the pane's columns.
                let avail_cols = v.rect.cols.saturating_sub(p.col);
                let vis_cols = p.cols.min(avail_cols);
                if vis_cols == 0 {
                    continue;
                }
                let local_row = (vis_top - top) as u16;
                let host_row = pane_row_offset + v.rect.row + local_row;
                let host_col = v.rect.col + p.col;
                // Source pixel crop, proportional to the cell clip.
                let (src_y, src_h) = crop_axis(img.pixel_h, p.rows, rows_off, vis_rows);
                let (src_x, src_w) = crop_axis(img.pixel_w, p.cols, 0, vis_cols);
                let key = (u64::from(v.id.0) << 40) | (p.seq & ((1u64 << 40) - 1));
                placements.push(crate::virtual_screen::VisiblePlacement {
                    key,
                    image_id: host_image_id(v.id.0, p.image_id),
                    placement_id: p.placement_id,
                    protocol: img.protocol,
                    iterm_args: img.iterm_args.clone(),
                    generation: img.generation,
                    format: img.format,
                    pixel_w: img.pixel_w,
                    pixel_h: img.pixel_h,
                    src_x,
                    src_y,
                    src_w,
                    src_h,
                    data_b64: img.data_b64.clone(),
                    host_row,
                    host_col,
                    rows: vis_rows,
                    cols: vis_cols,
                });
            }
        }
        screen.placements = placements;

        // Unicode-placeholder (virtual) placements: the terminal composites the
        // image onto the app's placeholder cells (which flow through the cell
        // diff), so we only surface the image data to transmit once + the box to
        // emit. Raw id is kept (the placeholder cells reference it). No viewport
        // clipping; the placeholder cells are clipped by the cell copy.
        let mut virtual_placements: Vec<crate::virtual_screen::VisibleVirtualPlacement> = Vec::new();
        for v in panes {
            if v.screen.alt.is_some() || v.screen.virtual_placements.is_empty() {
                continue;
            }
            for vp in &v.screen.virtual_placements {
                let Some(img) = v.screen.images.get(vp.image_id) else {
                    continue;
                };
                let key = (u64::from(v.id.0) << 40) | (vp.seq & ((1u64 << 40) - 1));
                virtual_placements.push(crate::virtual_screen::VisibleVirtualPlacement {
                    key,
                    image_id: vp.image_id,
                    placement_id: vp.placement_id,
                    generation: img.generation,
                    format: img.format,
                    pixel_w: img.pixel_w,
                    pixel_h: img.pixel_h,
                    data_b64: img.data_b64.clone(),
                    rows: vp.rows,
                    cols: vp.cols,
                });
            }
        }
        screen.virtual_placements = virtual_placements;
    }

    // Status bar.
    if let Some(s) = status {
        paint_status_row(&mut screen, s, host_cols, status_row);
    }

    // Cursor from the active pane, overridden by the copy-mode cursor when present.
    if let Some(active) = panes.iter().find(|v| v.is_active) {
        let cursor_pos = if let Some(cm) = active.copy_mode {
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
        } else {
            let cur = &active.screen.cursor;
            let c = active.rect.col.saturating_add(cur.col);
            // Live panes map the cursor's unified line through the fold
            // context (folds above it shift it up; a folded/off-screen cursor
            // hides). Copy/block panes keep the prior active-grid placement.
            let local_row = if active.copy_mode.is_none() && active.block_mode.is_none() {
                let sb_len = active.screen.scrollback.len() as u32;
                fold_ctx[&active.id].display_row(sb_len + u32::from(cur.row), active.rect.rows)
            } else {
                Some(cur.row)
            };
            local_row.and_then(|lr| {
                let r = active.rect.row.saturating_add(lr);
                (r < pane_area_rows && c < host_cols).then_some((r, c))
            })
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

    // Block-mode filter prompt overlay on the active pane.
    if let Some(active) = panes.iter().find(|v| v.is_active)
        && let Some(bm) = active.block_mode
        && let Some(filter) = &bm.filter
        && filter.prompt_active
    {
        let total = crate::blocks::all_prompt_lines(active.screen).len();
        let prompt_row = pane_row_offset + active.rect.row + active.rect.rows.saturating_sub(1);
        let text = format!("filter: {} ({}/{})", filter.query, filter.matches.len(), total);
        put_str(
            &mut screen,
            prompt_row,
            active.rect.col,
            &text,
            plexy_glass_emulator::Attrs::REVERSE,
            host_cols,
        );
    }

    // Transient status-line message: a themed bar on the bottom content row,
    // shown only when no interactive overlay is open (the overlay owns that row
    // when present). The leading glyph (a `✓`/`✗`/… severity cue) is the
    // color-independent channel; the severity color is the secondary one.
    if let Some(msg) = message
        && overlay.is_none()
    {
        let row = pane_row_offset + pane_area_rows.saturating_sub(1);
        let plain = plexy_glass_emulator::Attrs::empty();
        // Fill the whole row with the message background.
        let blank = " ".repeat(host_cols as usize);
        put_colored(&mut screen, row, 0, &blank, msg.fg, msg.bg, plain, host_cols);
        // " <glyph> <text>": one-space pad, bold glyph, then the text.
        let after_glyph = put_colored(
            &mut screen,
            row,
            1,
            msg.glyph,
            msg.fg,
            msg.bg,
            plexy_glass_emulator::Attrs::BOLD,
            host_cols,
        );
        put_colored(&mut screen, row, after_glyph + 1, msg.text, msg.fg, msg.bg, plain, host_cols);
    }

    // Floating popup pane: above panes/borders/status/cursor, below any
    // static overlay (mutually exclusive with overlays in practice).
    if let Some(p) = popup {
        paint_popup(&mut screen, p, pane_row_offset, pane_area_rows, host_cols, blocks);
    }

    // Interactive overlay (rename prompt / help), painted last so it sits
    // on top of panes, borders, and the cursor logic above. The active pane's
    // rect lets pane-scoped overlays (hint mode) paint at the focused pane's
    // origin; full-screen modal overlays ignore it.
    if let Some(ov) = overlay {
        let active_rect = panes.iter().find(|v| v.is_active).map(|v| v.rect);
        paint_overlay(&mut screen, ov, pane_row_offset, pane_area_rows, host_cols, active_rect, chrome);
    }

    screen
}

/// Fold a pane id and a per-pane (raw) image id into a host-global wire id, so
/// two panes that each use the same raw Kitty image id don't collide in the
/// client's single terminal. Multiplicative hash 64→32; non-zero (Kitty treats
/// i=0 as "no id"). ponytail: collision ~ N²/2³³ over distinct (pane,image)
/// pairs, negligible at real pane/image counts; swap for a per-client id map if
/// it ever bites.
fn host_image_id(pane_id: u32, raw_image_id: u32) -> u32 {
    let mixed = ((u64::from(pane_id) << 32) | u64::from(raw_image_id))
        .wrapping_mul(0x9E37_79B9_7F4A_7C15);
    ((mixed >> 32) as u32).max(1)
}

/// Map a cell-space clip to a source-pixel rectangle along one axis. Given the
/// full pixel extent, the total cell count the image is displayed in, the number
/// of leading cells hidden (`off`), and the number of visible cells (`vis`),
/// returns `(src_offset_px, src_extent_px)`. Uses cumulative endpoints so
/// rounding never leaves a sub-pixel gap; the uncropped case is exact.
fn crop_axis(pixels: u32, cells: u16, off: u16, vis: u16) -> (u32, u32) {
    let cells = u64::from(cells.max(1));
    if off == 0 && u64::from(vis) >= cells {
        return (0, pixels); // full extent, no rounding drift
    }
    let px = u64::from(pixels);
    let start = px * u64::from(off) / cells;
    let end = (px * (u64::from(off) + u64::from(vis)) / cells).min(px);
    (start as u32, end.saturating_sub(start) as u32)
}

/// Compute the effective scroll offset for a pane view. This is the single
/// source of truth used by both the content copy and the block-status scan so
/// both always show the same viewport.
fn effective_scroll_for(view: &PaneView<'_>) -> u32 {
    // copy and block mode are mutually exclusive; both pin the viewport via an
    // absolute viewport_top, so derive the scroll offset the same way.
    let viewport_top = match (view.copy_mode, view.block_mode) {
        (Some(cm), _) => Some(cm.viewport_top),
        (None, Some(bm)) => Some(bm.viewport_top),
        (None, None) => None,
    };
    match viewport_top {
        Some(vt) => {
            let total_lines = view.screen.scrollback.len() as u32
                + u32::from(view.screen.active.num_rows());
            total_lines
                .saturating_sub(vt)
                .saturating_sub(u32::from(view.rect.rows))
        }
        None => view.scroll_offset,
    }
}

/// Case-insensitive query occurrences within unified lines `[lo, hi)`, returned
/// as `(line, col_start, col_end)` grid spans. Mirrors `copy_mode::find_matches`:
/// a cell's grid column is its index (wide graphemes occupy a cell + a spacer).
fn filter_match_spans(
    screen: &Screen,
    query: &str,
    lo: u32,
    hi: u32,
) -> Vec<(u32, u16, u16)> {
    let mut out = Vec::new();
    if query.is_empty() {
        return out;
    }
    let q = query.to_lowercase();
    let cols = screen.active.num_cols();
    let total = screen.scrollback.rows().len() as u32 + u32::from(screen.active.num_rows());
    let span = display_width(&q).max(1);
    for line in lo..hi.min(total) {
        let Some(row) = crate::blocks::row_at(screen, line) else { continue };
        // Build the line's lowercased text + a column map (byte offset → grid
        // column), keyed on the SAME lowercased text the byte offsets index into.
        let mut line_text = String::new();
        let mut starts: Vec<(usize, u16)> = Vec::new();
        let mut grid_col = 0u16;
        for cell in &row.cells {
            if cell.is_wide_spacer() {
                grid_col += 1;
                continue;
            }
            starts.push((line_text.len(), grid_col));
            line_text.push_str(&cell.grapheme.as_str().to_lowercase());
            grid_col += 1;
        }
        let mut start = 0usize;
        while let Some(idx) = line_text[start..].find(&q) {
            let byte_off = start + idx;
            let col_start = starts
                .iter()
                .rev()
                .find(|(b, _)| *b <= byte_off)
                .map_or(0, |(_, gc)| *gc);
            let col_end = col_start
                .saturating_add(span.saturating_sub(1))
                .min(cols.saturating_sub(1));
            out.push((line, col_start, col_end));
            start = byte_off + q.len();
        }
    }
    out
}

/// Paint the active overlay over the pane band.
fn paint_overlay(
    screen: &mut VirtualScreen,
    overlay: &OverlayView<'_>,
    pane_row_offset: u16,
    pane_area_rows: u16,
    cols: u16,
    active_rect: Option<Rect>,
    chrome: ChromeColors,
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
            paint_help_box(screen, lines, *scroll, pane_row_offset, pane_area_rows, cols, chrome);
            // Suppress the underlying pane cursor while the box is up, matching
            // the rename/command overlays (otherwise it shows behind the box).
            screen.cursor_visible = false;
        }
        OverlayView::SessionPicker { entries, filter, selected } => {
            paint_session_picker(
                screen, entries, filter, *selected, pane_row_offset, pane_area_rows, cols, chrome,
            );
            screen.cursor_visible = false;
        }
        OverlayView::Tree { state } => {
            paint_tree(screen, state, pane_row_offset, pane_area_rows, cols, chrome);
            screen.cursor_visible = false;
        }
        OverlayView::Buffer { state } => {
            paint_buffers(screen, state, pane_row_offset, pane_area_rows, cols, chrome);
            screen.cursor_visible = false;
        }
        OverlayView::History { state } => {
            paint_history(screen, state, pane_row_offset, pane_area_rows, cols, chrome);
            screen.cursor_visible = false;
        }
        OverlayView::Hint { state, colors } => {
            // Hint mode is pane-scoped: dim and label the FOCUSED pane only, at
            // its rect origin. Fall back to the full pane band if (impossibly)
            // there's no active pane.
            let rect = active_rect.unwrap_or_else(|| Rect::new(0, 0, pane_area_rows, cols));
            paint_hint(screen, state, *colors, pane_row_offset, rect, cols);
            screen.cursor_visible = false;
        }
        OverlayView::Welcome { lines } => {
            paint_welcome(screen, lines, pane_row_offset, pane_area_rows, cols, chrome);
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

/// Draw a centered box's scaffolding: the border frame (cleared interior) plus
/// a centered `title` on the top border and `footer` on the bottom border. All
/// four overlay boxes share this exact framing; only their interior content
/// differs. Painted with empty attrs (no reverse), matching every caller.
// ponytail: box geometry (rect) + title/footer + theme; a struct would just
// rename the same transient call-site args.
#[allow(clippy::too_many_arguments)]
fn draw_box(
    screen: &mut VirtualScreen,
    row0: u16,
    col0: u16,
    box_h: u16,
    box_w: u16,
    title: &str,
    footer: &str,
    chrome: ChromeColors,
) {
    let plain = plexy_glass_emulator::Attrs::empty();
    let bold = plexy_glass_emulator::Attrs::BOLD;
    let bg = chrome.overlay_bg;
    let dw = |s: &str| display_width(s) as usize;
    let mut buf = [0u8; 4];
    for r in 0..box_h {
        for c in 0..box_w {
            let g = border_glyph(r, c, box_h, box_w);
            // Border perimeter in the accent color; interior fills with the box
            // background (both `Default` under `ansi_default`, so tests are
            // unchanged).
            let fg = if g == ' ' { bg } else { chrome.overlay_border };
            put_colored(screen, row0 + r, col0 + c, g.encode_utf8(&mut buf), fg, bg, plain, col0 + box_w);
        }
    }
    // Title centered on the top border (bold/highlight), footer on the bottom
    // border (dim/muted).
    let tcol = col0 + 1 + ((box_w.saturating_sub(2) as usize).saturating_sub(dw(title)) / 2) as u16;
    put_colored(screen, row0, tcol, title, chrome.overlay_title, bg, bold, col0 + box_w - 1);
    let fcol = col0 + 1 + ((box_w.saturating_sub(2) as usize).saturating_sub(dw(footer)) / 2) as u16;
    put_colored(screen, row0 + box_h - 1, fcol, footer, chrome.overlay_footer, bg, plain, col0 + box_w - 1);
}

/// Paint a scrollable list of `rows` into a box interior, the `sel` row drawn
/// REVERSE (its background filled to the box edges). `first_row` is the physical
/// row of the first visible entry; `top`/`visible` are the scroll window. Shared
/// by the session-picker, tree, and buffer overlays, since their selectable
/// bodies are otherwise identical.
// ponytail: 8 grid-geometry args (box interior rect + scroll window + selection);
// a wrapper struct would be pure transient call-site noise (cf. `compose`'s allow).
#[allow(clippy::too_many_arguments)]
fn paint_selectable_rows(
    screen: &mut VirtualScreen,
    rows: &[String],
    first_row: u16,
    inner_left: u16,
    inner_right: u16,
    top: usize,
    visible: usize,
    sel: usize,
) {
    let plain = plexy_glass_emulator::Attrs::empty();
    let rev = plexy_glass_emulator::Attrs::REVERSE;
    let end = (top + visible).min(rows.len());
    for (vis_i, row_idx) in (top..end).enumerate() {
        let r = first_row + vis_i as u16;
        let row_attrs = if row_idx == sel { rev } else { plain };
        if row_idx == sel {
            for c in inner_left..inner_right {
                put_char(screen, r, c, ' ', row_attrs);
            }
        }
        put_str(screen, r, inner_left, &rows[row_idx], row_attrs, inner_right);
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
    chrome: ChromeColors,
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

    draw_box(screen, row0, col0, box_h, box_w, title, footer, chrome);

    // Content rows. Pad the key column to `key_w` *display* columns (keys are
    // ASCII today, but pad by width so a wide glyph would still align).
    for (i, (keys, desc)) in lines.iter().skip(top).take(visible).enumerate() {
        let r = row0 + 1 + i as u16;
        let pad = " ".repeat(key_w.saturating_sub(dw(keys)));
        let line = format!("{keys}{pad}  {desc}");
        put_str(screen, r, col0 + 1, &line, attrs, col0 + box_w - 1);
    }
}

/// Draw the one-time welcome modal: a centered themed box of pre-built lines
/// (greeting, essential keys, how to get help/detach, how to disable). The lines
/// are built by the coordinator from config (resolved prefix + config path).
fn paint_welcome(
    screen: &mut VirtualScreen,
    lines: &[String],
    pane_row_offset: u16,
    pane_area_rows: u16,
    cols: u16,
    chrome: ChromeColors,
) {
    let title = " Welcome to plexy-glass ";
    let footer = " press any key to continue ";
    let dw = |s: &str| display_width(s) as usize;
    let content_w = lines
        .iter()
        .map(|l| dw(l))
        .max()
        .unwrap_or(0)
        .max(dw(title))
        .max(dw(footer));
    let inner_w = (content_w + 2).min(cols.saturating_sub(2) as usize);
    let box_w = (inner_w + 2) as u16;
    // Top border + lines + a blank gap + bottom border (footer on it), mirroring
    // the help box's spacing.
    let box_h = (lines.len() as u16) + 3;
    if box_w < 3 || box_h < 4 || box_w > cols || box_h > pane_area_rows {
        return; // viewport too small to draw without overflowing the pane band
    }
    let row0 = pane_row_offset + (pane_area_rows.saturating_sub(box_h)) / 2;
    let col0 = (cols.saturating_sub(box_w)) / 2;
    draw_box(screen, row0, col0, box_h, box_w, title, footer, chrome);
    let plain = plexy_glass_emulator::Attrs::empty();
    for (i, line) in lines.iter().enumerate() {
        let r = row0 + 1 + i as u16;
        put_colored(
            screen,
            r,
            col0 + 1,
            line,
            plexy_glass_emulator::Color::Default,
            chrome.overlay_bg,
            plain,
            col0 + box_w - 1,
        );
    }
}

/// Draw the centered session-picker box: a filter line plus the filtered
/// session rows (current session marked `*`, selected row REVERSE), scrolled to
/// keep the selection visible.
// ponytail: filter/selection state + box geometry + theme; a struct would just
// rename the same transient call-site args.
#[allow(clippy::too_many_arguments)]
fn paint_session_picker(
    screen: &mut VirtualScreen,
    entries: &[crate::overlay::PickerEntry],
    filter: &str,
    selected: usize,
    pane_row_offset: u16,
    pane_area_rows: u16,
    cols: u16,
    chrome: ChromeColors,
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

    draw_box(screen, row0, col0, box_h, box_w, title, footer, chrome);

    let inner_left = col0 + 1;
    let inner_right = col0 + box_w - 1; // exclusive max_col for put_str

    // Filter line with a block cursor.
    put_str(screen, row0 + 1, inner_left, &format!("{filter_line}\u{2588}"), plain, inner_right);

    // Session rows (or the empty-state message). Rows start below the filter
    // line, at row0 + 2.
    if rows.is_empty() {
        put_str(screen, row0 + 2, inner_left, empty_msg, plain, inner_right);
    } else {
        paint_selectable_rows(screen, &rows, row0 + 2, inner_left, inner_right, top, visible, sel);
    }
}

/// Draw the centered history-palette box: a filter line plus one row per
/// matching block (`glyph dur session/window  command`), the selected row
/// REVERSE, scrolled to keep the selection visible. Mirrors the session picker.
fn paint_history(
    screen: &mut VirtualScreen,
    state: &crate::history::HistoryState,
    pane_row_offset: u16,
    pane_area_rows: u16,
    cols: u16,
    chrome: ChromeColors,
) {
    let title = " History ";
    let footer = " \u{2191}/\u{2193} select \u{b7} enter jump \u{b7} esc cancel ";
    let empty_msg = "(no matching blocks)";
    let visible = state.visible_indices();
    let rows: Vec<String> = visible
        .iter()
        .map(|&i| {
            let e = &state.entries[i];
            let glyph = match e.exit {
                Some(0) => '\u{2713}', // ✓
                Some(_) => '\u{2717}', // ✗
                None => ' ',
            };
            let dur = e.duration.map(crate::blocks::format_duration).unwrap_or_default();
            format!("{glyph} {dur:<6} {}/{}  {}", e.session, e.window_idx, e.command)
        })
        .collect();
    let filter_line = format!("filter: {}", state.filter);

    let max_visible = (pane_area_rows.saturating_sub(3)).max(1) as usize;
    let row_count = rows.len().max(1);
    let visible_rows = row_count.min(max_visible);

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
    let box_h = (visible_rows as u16) + 3;
    if box_w < 3 || box_h < 4 || box_w > cols || box_h > pane_area_rows {
        return;
    }

    // Position of the selected entry within the visible list.
    let sel = visible.iter().position(|&i| i == state.selected).unwrap_or(0);
    let top = if sel >= visible_rows { sel - visible_rows + 1 } else { 0 };

    let row0 = pane_row_offset + (pane_area_rows.saturating_sub(box_h)) / 2;
    let col0 = (cols.saturating_sub(box_w)) / 2;
    let plain = plexy_glass_emulator::Attrs::empty();

    draw_box(screen, row0, col0, box_h, box_w, title, footer, chrome);
    let inner_left = col0 + 1;
    let inner_right = col0 + box_w - 1;

    put_str(screen, row0 + 1, inner_left, &format!("{filter_line}\u{2588}"), plain, inner_right);
    // Right-aligned live count (visible / total).
    let count = format!("{}/{}", visible.len(), state.entries.len());
    let cw = display_width(&count);
    if inner_right > inner_left + cw {
        put_str(screen, row0 + 1, inner_right - cw, &count, plain, inner_right);
    }
    if rows.is_empty() {
        put_str(screen, row0 + 2, inner_left, empty_msg, plain, inner_right);
    } else {
        paint_selectable_rows(screen, &rows, row0 + 2, inner_left, inner_right, top, visible_rows, sel);
    }
}

/// Dim the pane band and paint each still-matching target's label at its start
/// column. The typed prefix is dim (`match_fg`); the remaining suffix is bold
/// (`label_fg` on `label_bg`).
fn paint_hint(
    screen: &mut VirtualScreen,
    state: &crate::hint::HintState,
    colors: HintColors,
    pane_row_offset: u16,
    rect: Rect,
    host_cols: u16,
) {
    // Hint targets are scanned in the focused pane's LOCAL grid coords (row 0 /
    // col 0 = the pane's top-left), so every paint gets translated by the pane's
    // rect origin. Without that the labels land at the screen origin, i.e. in
    // whatever pane sits at column 0. Both the dim wash and the labels stay
    // inside the focused pane's rect.
    for r in 0..rect.rows {
        for c in 0..rect.cols {
            let col = rect.col + c;
            if col >= host_cols {
                break;
            }
            if let Some(cell) = screen.cell_mut(pane_row_offset + rect.row + r, col) {
                cell.attrs |= Attrs::DIM;
            }
        }
    }
    let clip = (rect.col + rect.cols).min(host_cols);
    let typed_len = state.typed.len();
    for (label, target) in state.visible() {
        let (trow, tcol) = target.start;
        if trow >= rect.rows || tcol >= rect.cols {
            continue;
        }
        let r = pane_row_offset + rect.row + trow;
        let base_col = rect.col + tcol;
        let pre = &label[..typed_len.min(label.len())];
        let suf = &label[typed_len.min(label.len())..];
        let after = put_colored(
            screen,
            r,
            base_col,
            pre,
            colors.match_fg,
            plexy_glass_emulator::Color::Default,
            Attrs::DIM,
            clip,
        );
        put_colored(
            screen,
            r,
            after,
            suf,
            colors.label_fg,
            colors.label_bg,
            Attrs::BOLD,
            clip,
        );
    }
}

/// Like `put_str`, but writes explicit fg/bg (the overlay `put_str` only takes
/// attrs). Returns the column just past the last grapheme.
// ponytail: 8 cell-paint args (grid coord + text + fg/bg/attrs + clip); a
// struct would be transient call-site ceremony (cf. `compose`'s allow).
#[allow(clippy::too_many_arguments)] // 8 cell-paint args: grid coord, text, fg, bg, attrs, clip; no grouping
fn put_colored(
    screen: &mut VirtualScreen,
    row: u16,
    mut col: u16,
    text: &str,
    fg: plexy_glass_emulator::Color,
    bg: plexy_glass_emulator::Color,
    attrs: plexy_glass_emulator::Attrs,
    max_col: u16,
) -> u16 {
    for (g, w) in plexy_glass_emulator::graphemes_with_width(text) {
        if col >= max_col || col >= screen.cols {
            break;
        }
        if w == 2 && (col + 1 >= max_col || col + 1 >= screen.cols) {
            break;
        }
        let cell = plexy_glass_emulator::Cell {
            grapheme: g.into(),
            fg,
            bg,
            attrs,
            ..plexy_glass_emulator::Cell::default()
        };
        screen.put(row, col, cell);
        if w == 2 {
            screen.put(row, col + 1, plexy_glass_emulator::Cell::wide_spacer());
        }
        col += w.max(1);
    }
    col
}

/// Draw the centered choose-tree box: depth-indented VISIBLE rows (collapsed
/// subtrees and filtered-out rows are skipped), the current-path nodes marked
/// `*`, the selected row REVERSE, scrolled by the selection's visible index,
/// with a mode-dependent footer (`/{filter}` while filtering, a `(filtered)`
/// hint in navigate mode when a filter is active).
fn paint_tree(
    screen: &mut VirtualScreen,
    state: &crate::tree::TreeState,
    pane_row_offset: u16,
    pane_area_rows: u16,
    cols: u16,
    chrome: ChromeColors,
) {
    use crate::tree::{TreeKind, TreeMode};
    let title = " Tree ";
    let footer: String = match &state.mode {
        TreeMode::Navigate => {
            let base =
                " \u{2191}/\u{2193} move \u{b7} enter switch \u{b7} x kill \u{b7} r rename \u{b7} esc close ";
            if state.filter.is_empty() {
                base.into()
            } else {
                format!("{base}(filtered) ")
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
        TreeMode::Filter => {
            format!(" /{}\u{2588}  enter keep \u{b7} esc clear ", state.filter)
        }
    };

    let vis = state.visible_indices();
    let rows: Vec<String> = vis
        .iter()
        .map(|&i| {
            let n = &state.nodes[i];
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

    // Scroll math runs over the selection's VISIBLE position, not its raw index.
    let sel = vis.iter().position(|&i| i == state.selected).unwrap_or(0);
    let top = if sel >= visible { sel - visible + 1 } else { 0 };

    let row0 = pane_row_offset + (pane_area_rows.saturating_sub(box_h)) / 2;
    let col0 = (cols.saturating_sub(box_w)) / 2;

    draw_box(screen, row0, col0, box_h, box_w, title, &footer, chrome);

    let inner_left = col0 + 1;
    let inner_right = col0 + box_w - 1; // exclusive max_col for put_str

    // No visible rows (last session just killed, or a filter matching
    // nothing): blank interior.
    if rows.is_empty() {
        return;
    }
    paint_selectable_rows(screen, &rows, row0 + 1, inner_left, inner_right, top, visible, sel);
}

/// Draw the centered choose-buffer box: one `name: preview` row per buffer, the
/// selected row REVERSE, scrolled to keep the selection visible.
fn paint_buffers(
    screen: &mut VirtualScreen,
    state: &crate::buffer::BufferPickerState,
    pane_row_offset: u16,
    pane_area_rows: u16,
    cols: u16,
    chrome: ChromeColors,
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

    draw_box(screen, row0, col0, box_h, box_w, title, footer, chrome);

    let inner_left = col0 + 1;
    let inner_right = col0 + box_w - 1;

    if rows.is_empty() {
        put_str(screen, row0 + 1, inner_left, empty_msg, plain, inner_right);
        return;
    }
    paint_selectable_rows(screen, &rows, row0 + 1, inner_left, inner_right, top, visible, sel);
}

/// Paint the floating popup: a bordered, titled box at `popup.rect` whose
/// interior shows the popup pane's live grid. The popup is focused, so its
/// child's cursor replaces whatever the active layout pane set above.
///
/// When `blocks` is `Some(colors)`, the left border cells for interior rows
/// are colored per block exit-status (same rules as regular panes, but without
/// active/marked rings, so precedence is just status-or-plain). Popups always
/// render the live grid, so `top = scrollback.len()` (no scrollback offset).
/// Alt screen → `viewport_block_status` returns all-None already; no separate
/// guard is needed here.
fn paint_popup(
    screen: &mut VirtualScreen,
    popup: &PopupView<'_>,
    pane_row_offset: u16,
    pane_area_rows: u16,
    cols: u16,
    blocks: Option<&crate::borders::BlockBorderColors>,
) {
    let rect = popup.rect;
    if rect.rows < 3 || rect.cols < 3 {
        return;
    }
    if rect.row.saturating_add(rect.rows) > pane_area_rows
        || rect.col.saturating_add(rect.cols) > cols
    {
        // Stale geometry mid-resize; skip this frame rather than overflow.
        return;
    }
    let attrs = plexy_glass_emulator::Attrs::empty();
    // Border frame + cleared interior.
    for r in 0..rect.rows {
        for c in 0..rect.cols {
            let ch = border_glyph(r, c, rect.rows, rect.cols);
            put_char(screen, pane_row_offset + rect.row + r, rect.col + c, ch, attrs);
        }
    }
    // Title centered on the top border.
    let title = format!(" {} ", popup.title);
    let inner_w = rect.cols - 2;
    let tw = display_width(&title);
    let tcol = rect.col + 1 + inner_w.saturating_sub(tw) / 2;
    put_str(screen, pane_row_offset + rect.row, tcol, &title, attrs, rect.col + rect.cols - 1);
    // Interior: the popup pane's grid.
    let max_r = (rect.rows - 2).min(popup.screen.active.num_rows());
    let max_c = (rect.cols - 2).min(popup.screen.active.num_cols());
    for r in 0..max_r {
        let Some(row) = popup.screen.active.rows.get(r as usize) else { continue };
        for c in 0..max_c {
            if let Some(cell) = row.cells.get(c as usize) {
                screen.put(
                    pane_row_offset + rect.row + 1 + r,
                    rect.col + 1 + c,
                    cell.clone(),
                );
            }
        }
    }
    // Block exit-status coloring on the left border cells (interior rows only).
    // Popups always show the live grid: top = scrollback length (no offset).
    // Alt screen → `viewport_block_status` already returns all-None, so no extra
    // guard is needed here.
    if let Some(colors) = blocks {
        let interior_rows = rect.rows - 2;
        let top = popup.screen.scrollback.len() as u32;
        let statuses = viewport_block_status(popup.screen, top, interior_rows);
        for (r, status) in statuses.into_iter().enumerate() {
            let Some(status) = status else { continue };
            let border_row = pane_row_offset + rect.row + 1 + r as u16;
            let border_col = rect.col;
            // Clip: don't paint outside the box or the screen.
            if border_row >= pane_area_rows.saturating_add(pane_row_offset)
                || border_col >= cols
            {
                continue;
            }
            let Some(cell) = screen.cell_mut(border_row, border_col) else { continue };
            match status {
                crate::blocks::BlockLineStatus::Ok => {
                    cell.fg = colors.ok;
                    // Glyph stays │ (or whatever border_glyph placed there).
                }
                crate::blocks::BlockLineStatus::Failed => {
                    cell.fg = colors.fail;
                    // Replace a plain vertical │ with the half-block ▐.
                    if cell.grapheme.as_str() == "\u{2502}" {
                        cell.grapheme = smol_str::SmolStr::new_static("\u{2590}");
                    }
                }
            }
        }
    }
    // Focused popup: its child cursor wins (translated to the interior).
    let cur = &popup.screen.cursor;
    if cur.row < rect.rows - 2 && cur.col < rect.cols - 2 {
        screen.cursor =
            Some((pane_row_offset + rect.row + 1 + cur.row, rect.col + 1 + cur.col));
    } else {
        // Resize race: the popup grid momentarily exceeds the interior. No
        // cursor beats a stale layout-pane cursor floating over the box.
        screen.cursor = None;
    }
    screen.cursor_visible = popup
        .screen
        .modes
        .contains(plexy_glass_emulator::Modes::CURSOR_VISIBLE);
}

/// Box-drawing glyph for cell (r, c) within a `h`x`w` frame; space inside.
const fn border_glyph(r: u16, c: u16, h: u16, w: u16) -> char {
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

const fn rgb_to_color(rgb: plexy_glass_status::Rgb) -> plexy_glass_emulator::Color {
    // `Color::Rgb(u8, u8, u8)`, confirmed in `crates/emulator/src/color.rs`.
    plexy_glass_emulator::Color::Rgb(rgb.r, rgb.g, rgb.b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexy_glass_emulator::{Emulator, RowMark};

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
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let vs = compose(&[view], (4, 6), None, StatusPlacement::Bottom, None, None, None, None, None, plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
        assert_eq!(vs.cell(0, 0).unwrap().grapheme.as_str(), "h");
        assert_eq!(vs.cursor, Some((0, 2)));
    }

    #[test]
    fn popup_rect_is_80pct_centered() {
        // Band like a 24x80 host: `host_viewport` → (1,1,21,78).
        let band = Rect::new(1, 1, 21, 78);
        let r = popup_rect(band);
        assert_eq!((r.rows, r.cols), (16, 62)); // floor(21*0.8), floor(78*0.8)
        assert_eq!(r.row, 1 + (21 - 16) / 2);
        assert_eq!(r.col, 1 + (78 - 62) / 2);
    }

    #[test]
    fn popup_rect_clamps_to_min_and_band() {
        // Tiny band: the min box is 5x12 but the band caps it.
        let band = Rect::new(0, 0, 4, 10);
        let r = popup_rect(band);
        assert_eq!((r.rows, r.cols), (4, 10));
        // Small-but-roomy band: the 5x12 minimum wins over 80%.
        let band = Rect::new(0, 0, 6, 14);
        let r = popup_rect(band);
        assert_eq!((r.rows, r.cols), (5, 12));
    }

    #[test]
    fn compose_paints_popup_box_title_grid_and_cursor() {
        // Layout pane with text positioned *under* the popup interior (CUP is
        // 1-indexed: row 4, col 12 = grid (3, 11), inside interior rows 3..=8
        // cols 11..=30), so it must be covered by the popup.
        let mut e = Emulator::new(10, 40);
        pane(&mut e, b"\x1b[4;12HUNDERNEATH ");
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 10, 40),
            screen: e.screen(),
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        // Popup pane: 6x20 grid showing "hi".
        let mut pe = Emulator::new(6, 20);
        pane(&mut pe, b"hi ");
        let rect = Rect::new(2, 10, 8, 22); // interior 6x20
        let pv = PopupView { rect, screen: pe.screen(), title: "cat" };
        let vs = compose(
            &[view],
            (10, 40),
            None,
            StatusPlacement::Bottom,
            None,
            None,
            None,
            Some(&pv),
            None,
        plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
        // Corners of the border frame.
        assert_eq!(vs.cell(2, 10).unwrap().grapheme.as_str(), "┌");
        assert_eq!(vs.cell(9, 31).unwrap().grapheme.as_str(), "┘");
        // Title " cat " appears on the top border row.
        let top_row: String = (10..32)
            .map(|c| vs.cell(2, c).unwrap().grapheme.as_str().to_string())
            .collect();
        assert!(top_row.contains(" cat "), "top border missing title: {top_row}");
        // Interior shows the popup grid at (rect.row+1, rect.col+1), so the
        // pane's "UN" at (3, 11)-(3, 12) is covered by the popup's "hi".
        assert_eq!(vs.cell(3, 11).unwrap().grapheme.as_str(), "h");
        assert_eq!(vs.cell(3, 12).unwrap().grapheme.as_str(), "i");
        // Past the popup text, the pane's 'D' at (3, 13) is covered by the
        // popup grid's blank cell, not shown through.
        assert_ne!(vs.cell(3, 13).unwrap().grapheme.as_str(), "D");
        // Cursor follows the popup child (pe cursor at (0,2) after "hi ").
        assert_eq!(vs.cursor, Some((3, 13)));
    }

    #[test]
    fn popup_paints_shifted_down_when_status_bar_on_top() {
        let mut e = Emulator::new(9, 40);
        pane(&mut e, b"x ");
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 9, 40),
            screen: e.screen(),
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let mut pe = Emulator::new(3, 10);
        pane(&mut pe, b"p ");
        let rect = Rect::new(2, 10, 5, 12); // interior 3x10
        let pv = PopupView { rect, screen: pe.screen(), title: "t" };
        let status = status_with_left("AB");
        let vs = compose(
            &[view],
            (10, 40),
            Some(&status),
            StatusPlacement::Top,
            None,
            None,
            None,
            Some(&pv),
            None,
        plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
        // Top status row occupies physical row 0; the popup's logical row 2
        // paints at physical row 3 (pane_row_offset = 1).
        assert_eq!(vs.cell(3, 10).unwrap().grapheme.as_str(), "┌");
        // Interior glyph and cursor are shifted by the same offset: the
        // popup's 'p' at grid (0,0) lands at (3+1, 10+1); its cursor at
        // grid (0,1) after "p " (the trailing space stays buffered) lands
        // at (3+1, 10+1+1).
        assert_eq!(vs.cell(4, 11).unwrap().grapheme.as_str(), "p");
        assert_eq!(vs.cursor, Some((4, 12)));
    }

    #[test]
    fn selection_overlay_sets_reverse_attr() {
        use crate::selection::Selection;
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
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let mut sel = Selection::start(PaneId(0), 0, 0);
        sel.extend(0, 4, Rect::new(0, 0, 4, 6));
        let vs = compose(&[view], (4, 6), None, StatusPlacement::Bottom, Some(&sel), None, None, None, None, plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
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
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let rv = PaneView {
            id: PaneId(1),
            rect: Rect::new(0, 4, 4, 3),
            screen: right.screen(),
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let vs = compose(&[lv, rv], (4, 7), None, StatusPlacement::Bottom, None, None, None, None, None, plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
        assert_eq!(vs.cell(0, 0).unwrap().grapheme.as_str(), "L");
        assert_eq!(vs.cell(0, 4).unwrap().grapheme.as_str(), "R");
        // Border column.
        assert_eq!(vs.cell(0, 3).unwrap().grapheme.as_str(), "│");
    }

    #[test]
    fn hint_labels_paint_in_the_focused_pane_not_at_screen_origin() {
        use crate::hint::{HintKind, HintState, HintTarget};
        // Two side-by-side 4x3 panes; the RIGHT one (rect.col = 4) is focused.
        let mut left = Emulator::new(4, 3);
        let mut right = Emulator::new(4, 3);
        pane(&mut left, b"L ");
        pane(&mut right, b"R ");
        let lv = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 4, 3),
            screen: left.screen(),
            is_active: false,
            scroll_offset: 0,
            copy_mode: None,
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let rv = PaneView {
            id: PaneId(1),
            rect: Rect::new(0, 4, 4, 3),
            screen: right.screen(),
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        // One hint target at the focused pane's LOCAL (0,0). It must paint at the
        // focused pane's screen origin (row 0, col 4), not the screen's (0,0),
        // which is the unfocused left pane.
        let target =
            HintTarget { start: (0, 0), text: "https://x".into(), kind: HintKind::Url };
        let state = HintState::new(vec![target], "asdf");
        let first = state.visible().next().unwrap().0.chars().next().unwrap().to_string();
        let colors = HintColors {
            label_fg: plexy_glass_emulator::Color::Default,
            label_bg: plexy_glass_emulator::Color::Default,
            match_fg: plexy_glass_emulator::Color::Default,
        };
        let ov = OverlayView::Hint { state: &state, colors };
        let vs = compose(&[lv, rv], (4, 7), None, StatusPlacement::Bottom, None, Some(&ov), None, None, None, plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
        assert_eq!(
            vs.cell(0, 4).unwrap().grapheme.as_str(),
            first,
            "hint label belongs at the focused pane's origin (col 4)"
        );
        assert_ne!(
            vs.cell(0, 0).unwrap().grapheme.as_str(),
            first,
            "hint label must NOT paint in the unfocused left pane (col 0)"
        );
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
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let vs = compose(&[view], (2, 4), None, StatusPlacement::Bottom, None, None, None, None, None, plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
        // Row 0 should be the last scrollback row (BBBB), not CCCC.
        let r0: String = (0..4)
            .map(|c| vs.cell(0, c).unwrap().grapheme.as_str().to_string())
            .collect::<String>();
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
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let vs = compose(&[view], (5, 20), None, StatusPlacement::Bottom, None, None, None, None, None, plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
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
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let vs = compose(&[view], (5, 20), None, StatusPlacement::Bottom, None, None, None, None, None, plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
        for c in 0..=4 {
            let cell = vs.cell(0, c).unwrap();
            assert!(
                cell.attrs.contains(plexy_glass_emulator::Attrs::REVERSE),
                "expected REVERSE at col {c}"
            );
        }
    }

    #[test]
    fn copy_mode_search_match_sets_highlight() {
        use plexy_glass_emulator::{Attrs, Emulator};
        let mut e = Emulator::new(5, 20);
        e.advance(b"hello world");
        let screen = e.screen().clone();
        let cm = crate::CopyMode {
            cursor: (0, 0),
            anchor: None,
            search: crate::SearchState {
                query: "ell".into(),
                matches: vec![crate::MatchSpan { line_idx: 0, col_start: 1, col_end: 3 }],
                current: 0,
                prompt_active: false,
                prompt_buf: String::new(),
            },
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
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let vs = compose(&[view], (5, 20), None, StatusPlacement::Bottom, None, None, None, None, None, plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
        for c in 1..=3 {
            assert!(vs.cell(0, c).unwrap().attrs.contains(Attrs::HIGHLIGHT), "col {c} highlighted");
        }
        assert!(!vs.cell(0, 0).unwrap().attrs.contains(Attrs::HIGHLIGHT), "col 0 not highlighted");
        assert!(!vs.cell(0, 4).unwrap().attrs.contains(Attrs::HIGHLIGHT), "col 4 not highlighted");
    }

    #[test]
    fn copy_mode_search_match_out_of_viewport_not_painted() {
        use plexy_glass_emulator::{Attrs, Emulator};
        let mut e = Emulator::new(5, 20);
        e.advance(b"hello");
        let screen = e.screen().clone();
        // viewport covers lines 2..=5; a match on line 0 is above it.
        let cm = crate::CopyMode {
            cursor: (3, 0),
            anchor: None,
            search: crate::SearchState {
                query: "h".into(),
                matches: vec![crate::MatchSpan { line_idx: 0, col_start: 0, col_end: 0 }],
                current: 0,
                prompt_active: false,
                prompt_buf: String::new(),
            },
            viewport_top: 2,
            pane_rows: 4,
            total_lines: 6,
        };
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 4, 20),
            screen: &screen,
            is_active: true,
            scroll_offset: 0,
            copy_mode: Some(&cm),
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let vs = compose(&[view], (5, 20), None, StatusPlacement::Bottom, None, None, None, None, None, plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
        // The out-of-viewport match must not paint any HIGHLIGHT in the band.
        for r in 0..4 {
            for c in 0..20 {
                assert!(
                    !vs.cell(r, c).unwrap().attrs.contains(Attrs::HIGHLIGHT),
                    "no highlight expected at ({r},{c})"
                );
            }
        }
    }

    #[test]
    fn copy_mode_search_prompt_paints_reverse_bar() {
        use plexy_glass_emulator::{Attrs, Emulator};
        let mut e = Emulator::new(5, 20);
        e.advance(b"hello");
        let screen = e.screen().clone();
        let cm = crate::CopyMode {
            cursor: (0, 0),
            anchor: None,
            search: crate::SearchState {
                query: String::new(),
                matches: vec![],
                current: 0,
                prompt_active: true,
                prompt_buf: "foo".into(),
            },
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
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let vs = compose(&[view], (5, 20), None, StatusPlacement::Bottom, None, None, None, None, None, plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
        // Prompt bar on the pane's bottom row (rect.row + rect.rows - 1 = 3).
        let row: Vec<String> = (0..4)
            .map(|c| vs.cell(3, c).unwrap().grapheme.to_string())
            .collect();
        assert_eq!(row.join(""), "/foo");
        for c in 0..4 {
            assert!(
                vs.cell(3, c).unwrap().attrs.contains(Attrs::REVERSE),
                "prompt cell {c} is REVERSE"
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
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let status = status_with_left("中B");
        let vs = compose(&[view], (3, 8), Some(&status), StatusPlacement::Bottom, None, None, None, None, None, plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
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
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let status = status_with_left("AB");
        let vs = compose(&[view], (3, 4), Some(&status), StatusPlacement::Top, None, None, None, None, None, plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
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
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let status = status_with_left("AB");
        let vs = compose(&[view], (3, 4), Some(&status), StatusPlacement::Bottom, None, None, None, None, None, plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
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
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let ov = OverlayView::RenamePrompt { label: "rename window", buf: "hi" };
        let vs = compose(&[view], (4, 20), None, StatusPlacement::Bottom, None, Some(&ov), None, None, None, plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
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
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let ov = OverlayView::Command { buf: "spl" };
        let vs = compose(&[view], (4, 20), None, StatusPlacement::Bottom, None, Some(&ov), None, None, None, plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
        assert!(vs.cell(3, 0).unwrap().attrs.contains(Attrs::REVERSE), "command bar is REVERSE");
        // Text " :spl": ':' at col 1, 's' at col 2.
        assert_eq!(vs.cell(3, 1).unwrap().grapheme.as_str(), ":");
        assert_eq!(vs.cell(3, 2).unwrap().grapheme.as_str(), "s");
    }

    #[test]
    fn status_message_paints_themed_bottom_row() {
        use plexy_glass_emulator::{Attrs, Color};
        let mut e = Emulator::new(4, 20);
        pane(&mut e, b"x ");
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 4, 20),
            screen: e.screen(),
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let fg = Color::Rgb(0xc4, 0x74, 0x6e); // alert
        let bg = Color::Rgb(0x28, 0x27, 0x27); // bg_bar
        let vs = compose(
            &[view],
            (4, 20),
            None,
            StatusPlacement::Bottom,
            None,
            None,
            Some(MessageView { text: "no session: foo", glyph: "✗", fg, bg }),
            None,
            None,
        plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
        // Bottom row (3): " ✗ <text>", themed, NOT the old REVERSE bar.
        assert!(!vs.cell(3, 0).unwrap().attrs.contains(Attrs::REVERSE), "no longer REVERSE");
        let glyph_cell = vs.cell(3, 1).unwrap();
        assert_eq!(glyph_cell.grapheme.as_str(), "✗");
        assert_eq!(glyph_cell.fg, fg);
        assert_eq!(glyph_cell.bg, bg);
        assert!(glyph_cell.attrs.contains(Attrs::BOLD), "glyph is bold");
        // Glyph at col 1 (width 1), a pad at col 2, then the text from col 3.
        assert_eq!(vs.cell(3, 3).unwrap().grapheme.as_str(), "n");
        assert_eq!(vs.cell(3, 3).unwrap().fg, fg);
        assert_eq!(vs.cell(3, 4).unwrap().grapheme.as_str(), "o");
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
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let ov = OverlayView::RenamePrompt { label: "rename window", buf: "hi" };
        let vs = compose(
            &[view],
            (4, 20),
            None,
            StatusPlacement::Bottom,
            None,
            Some(&ov),
            Some(MessageView {
                text: "this message must not show",
                glyph: "ℹ",
                fg: plexy_glass_emulator::Color::Default,
                bg: plexy_glass_emulator::Color::Default,
            }),
            None,
            None,
        plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
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
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let lines = vec![("Ctrl+a c".to_string(), "New window".to_string())];
        let ov = OverlayView::Help { lines: &lines, scroll: 0 };
        let vs = compose(&[view], (10, 40), None, StatusPlacement::Bottom, None, Some(&ov), None, None, None, plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
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

    #[test]
    fn overlay_box_border_uses_the_chrome_palette() {
        use plexy_glass_emulator::Color;
        let mut e = Emulator::new(10, 40);
        pane(&mut e, b"x ");
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 10, 40),
            screen: e.screen(),
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let lines = vec![("Ctrl+a c".to_string(), "New window".to_string())];
        let ov = OverlayView::Help { lines: &lines, scroll: 0 };
        let border = Color::Rgb(0x12, 0x34, 0x56);
        let chrome = ChromeColors { overlay_border: border, ..ChromeColors::ansi_default() };
        let vs = compose(
            &[view], (10, 40), None, StatusPlacement::Bottom, None, Some(&ov), None, None, None,
            plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), chrome,
        );
        // Find a box-drawing border cell and confirm it carries the chrome color.
        let mut themed_border = false;
        for r in 0..10u16 {
            for c in 0..40u16 {
                let cell = vs.cell(r, c).unwrap();
                if cell.grapheme.as_str() == "\u{250c}" {
                    assert_eq!(cell.fg, border, "box corner uses the chrome border color");
                    themed_border = true;
                }
            }
        }
        assert!(themed_border, "overlay box border was painted");
    }

    #[test]
    fn overlay_welcome_renders_box_and_content() {
        let mut e = Emulator::new(16, 60);
        pane(&mut e, b"x ");
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 16, 60),
            screen: e.screen(),
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let lines = vec![
            "The prefix is Ctrl+a — press it, then a key:".to_string(),
            "  c   new window".to_string(),
        ];
        let ov = OverlayView::Welcome { lines: &lines };
        let vs = compose(
            &[view], (16, 60), None, StatusPlacement::Bottom, None, Some(&ov), None, None, None,
            plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default(),
        );
        let mut found_corner = false;
        let mut found_text = false;
        for r in 0..16 {
            let mut row = String::new();
            for c in 0..60 {
                row.push_str(vs.cell(r, c).unwrap().grapheme.as_str());
            }
            if row.contains('\u{250c}') {
                found_corner = true;
            }
            if row.contains("new window") {
                found_text = true;
            }
        }
        assert!(found_corner, "welcome box top-left corner drawn");
        assert!(found_text, "welcome content drawn");
        assert!(!vs.cursor_visible, "welcome overlay hides the pane cursor");
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
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let entries = vec![
            picker_view("main", "main - 1 win", true),
            picker_view("work", "work - 2 win", false),
        ];
        let ov = OverlayView::SessionPicker { entries: &entries, filter: "", selected: 1 };
        let vs = compose(&[view], (10, 50), None, StatusPlacement::Bottom, None, Some(&ov), None, None, None, plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());

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
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        // A CJK session name must be sized and placed as one cell + a spacer.
        let entries = vec![picker_view("中文", "中文", false)];
        let ov = OverlayView::SessionPicker { entries: &entries, filter: "", selected: 0 };
        let vs = compose(&[view], (10, 50), None, StatusPlacement::Bottom, None, Some(&ov), None, None, None, plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
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
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let entries = vec![picker_view("main", "main", true)];
        let ov = OverlayView::SessionPicker { entries: &entries, filter: "zzz", selected: 0 };
        let vs = compose(&[view], (10, 50), None, StatusPlacement::Bottom, None, Some(&ov), None, None, None, plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
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
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let lines = vec![("Ctrl+a c".to_string(), "New window".to_string())];
        let ov = OverlayView::Help { lines: &lines, scroll: 0 };
        let status = status_with_left("S");
        let vs =
            compose(&[view], (4, 40), Some(&status), StatusPlacement::Bottom, None, Some(&ov), None, None, None, plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
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
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let state = crate::tree::TreeState {
            nodes: vec![
                tree_node("main", None, None, 0, "main — 1 win, 2 panes", true),
                tree_node("main", Some(0), None, 1, "1: shell", true),
                tree_node("main", Some(0), Some(0), 2, "pane 1", false),
                tree_node("main", Some(0), Some(1), 2, "pane 2", false),
            ],
            selected: 1,
            ..Default::default()
        };
        let ov = OverlayView::Tree { state: &state };
        let vs = compose(&[view], (12, 50), None, StatusPlacement::Bottom, None, Some(&ov), None, None, None, plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());

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
    fn history_overlay_renders_rows_and_selection() {
        use plexy_glass_emulator::Attrs;
        let mut e = Emulator::new(12, 60);
        pane(&mut e, b"hi");
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 12, 60),
            screen: e.screen(),
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let mk = |session: &str, cmd: &str, exit: Option<i32>, dur: Option<u32>, line: u32| {
            crate::history::HistoryEntry {
                session: session.into(),
                window: crate::WindowId(0),
                window_idx: 2,
                pane: PaneId(0),
                prompt_line: line,
                command: cmd.into(),
                exit,
                duration: dur,
                haystack: cmd.to_lowercase(),
            }
        };
        let state = crate::history::HistoryState {
            entries: vec![
                mk("api", "docker compose up", Some(0), Some(2300), 10),
                mk("web", "cargo test", Some(1), Some(45_000), 4),
            ],
            selected: 1,
            filter: String::new(),
        };
        let ov = OverlayView::History { state: &state };
        let vs = compose(&[view], (12, 60), None, StatusPlacement::Bottom, None, Some(&ov), None, None, None, plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());

        let mut found_corner = false;
        let mut selected_reverse = false;
        let mut rows: Vec<String> = Vec::new();
        for r in 0..12 {
            let mut line = String::new();
            for c in 0..60 {
                let cell = vs.cell(r, c).unwrap();
                line.push_str(cell.grapheme.as_str());
                if cell.grapheme.as_str() == "\u{250c}" {
                    found_corner = true;
                }
                // selected row is "cargo test" (REVERSE).
                if cell.grapheme.as_str() == "c" && cell.attrs.contains(Attrs::REVERSE) {
                    selected_reverse = true;
                }
            }
            rows.push(line);
        }
        assert!(found_corner, "history box border drawn");
        assert!(selected_reverse, "selected row painted REVERSE");
        assert!(!vs.cursor_visible, "history hides the pane cursor");
        let joined = rows.join("\n");
        assert!(joined.contains("docker compose up"), "command rendered: {joined:?}");
        assert!(joined.contains("api/2"), "provenance rendered: {joined:?}");
        assert!(joined.contains("2.3s"), "duration rendered: {joined:?}");
        assert!(joined.contains('\u{2717}'), "fail glyph for the nonzero-exit block");
        assert!(joined.contains("2/2"), "live visible/total count rendered: {joined:?}");
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
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let state = crate::buffer::BufferPickerState {
            entries: vec![
                crate::buffer::BufferEntry { name: "buffer1".into(), preview: "hello".into() },
                crate::buffer::BufferEntry { name: "buffer0".into(), preview: "world".into() },
            ],
            selected: 1,
        };
        let ov = OverlayView::Buffer { state: &state };
        let vs = compose(&[view], (10, 50), None, StatusPlacement::Bottom, None, Some(&ov), None, None, None, plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
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
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let state = crate::buffer::BufferPickerState { entries: vec![], selected: 0 };
        let ov = OverlayView::Buffer { state: &state };
        let vs = compose(&[view], (10, 50), None, StatusPlacement::Bottom, None, Some(&ov), None, None, None, plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
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
            block_mode: None,
            title: None,
            marked: true,
            drag_role: PaneDragRole::None,
        };
        let vs = compose(&[view], (3, 8), None, StatusPlacement::Bottom, None, None, None, None, None, plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
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
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let state = crate::tree::TreeState {
            nodes: vec![tree_node("main", Some(0), None, 1, "1: shell", false)],
            selected: 0,
            mode: crate::tree::TreeMode::ConfirmKill,
            ..Default::default()
        };
        let ov = OverlayView::Tree { state: &state };
        let vs = compose(&[view], (12, 60), None, StatusPlacement::Bottom, None, Some(&ov), None, None, None, plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
        let mut text = String::new();
        for r in 0..12 {
            for c in 0..60 {
                text.push_str(vs.cell(r, c).unwrap().grapheme.as_str());
            }
        }
        assert!(text.contains("Kill window"), "confirm-kill footer shown: {text}");
    }

    /// Compose the tree overlay over a blank pane and return the frame text.
    fn tree_frame(state: &crate::tree::TreeState, rows: u16, cols: u16) -> String {
        let mut e = Emulator::new(rows, cols);
        pane(&mut e, b"x ");
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, rows, cols),
            screen: e.screen(),
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let ov = OverlayView::Tree { state };
        let vs = compose(&[view], (rows, cols), None, StatusPlacement::Bottom, None, Some(&ov), None, None, None, plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
        let mut text = String::new();
        for r in 0..rows {
            for c in 0..cols {
                text.push_str(vs.cell(r, c).unwrap().grapheme.as_str());
            }
        }
        text
    }

    fn tree_v2_nodes() -> Vec<crate::tree::TreeNode> {
        vec![
            tree_node("main", None, None, 0, "main — 1 win, 2 panes", true),
            tree_node("main", Some(0), None, 1, "1: shell", true),
            tree_node("main", Some(0), Some(0), 2, "pane 1", false),
            tree_node("main", Some(0), Some(1), 2, "pane 2", false),
        ]
    }

    #[test]
    fn tree_overlay_hides_collapsed_rows() {
        let state = crate::tree::TreeState {
            nodes: tree_v2_nodes(),
            collapsed: std::iter::once(crate::tree::NodeKey::Session("main".into())).collect(),
            ..Default::default()
        };
        let text = tree_frame(&state, 12, 50);
        assert!(text.contains("1 win"), "session row still rendered: {text}");
        assert!(!text.contains("shell"), "collapsed window row hidden: {text}");
        assert!(!text.contains("pane 1"), "collapsed pane rows hidden: {text}");
    }

    #[test]
    fn tree_overlay_filter_mode_footer() {
        let state = crate::tree::TreeState {
            nodes: tree_v2_nodes(),
            selected: 1,
            mode: crate::tree::TreeMode::Filter,
            filter: "she".into(),
            ..Default::default()
        };
        let text = tree_frame(&state, 12, 60);
        assert!(text.contains("/she"), "filter footer shows the live pattern: {text}");
        assert!(!text.contains("pane 1"), "non-matching rows hidden while filtering: {text}");
    }

    #[test]
    fn tree_overlay_navigate_footer_flags_active_filter() {
        let state = crate::tree::TreeState {
            nodes: tree_v2_nodes(),
            selected: 1,
            filter: "shell".into(),
            ..Default::default()
        };
        let text = tree_frame(&state, 12, 90);
        assert!(text.contains("(filtered)"), "navigate footer flags the kept filter: {text}");
    }

    // ── Block exit-status compositor tests ───────────────────────────────────

    fn block_colors() -> BlockBorderColors {
        BlockBorderColors {
            ok: plexy_glass_emulator::Color::Rgb(135, 169, 135),
            fail: plexy_glass_emulator::Color::Rgb(196, 116, 110),
            duration_threshold_ms: None,
            sticky_header: false,
        }
    }

    /// `blocks: None` suppresses all block painting even when block marks exist.
    /// Build a screen WITH a completed failed block, compose with `blocks: None`
    /// → output must contain no `▐` and no fail-color cell.
    #[test]
    fn compose_blocks_none_no_block_rows() {
        use plexy_glass_emulator::Emulator as RawEmulator;
        // Feed a completed failed block so viewport_block_status would fire if enabled.
        let mut e = RawEmulator::new(3, 20);
        e.advance(
            b"\x1b]133;A\x07$ fail\r\n\
              \x1b]133;C\x07output\r\n\
              \x1b]133;D;1\x07done",
        );
        e.advance(b"\x1b[m");
        let screen = e.screen().clone();
        let colors = block_colors();
        let fail_color = colors.fail;
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(1, 1, 1, 18),
            screen: &screen,
            is_active: false,
            scroll_offset: 0,
            copy_mode: None,
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let vs = compose(
            &[view],
            (3, 20),
            None,
            StatusPlacement::Bottom,
            None, None, None, None,
            None, // blocks disabled
            plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61),
            ChromeColors::ansi_default(),
        );
        // No ▐ anywhere and no fail-color cell.
        for r in 0..3u16 {
            for c in 0..20u16 {
                let cell = vs.cell(r, c).unwrap();
                assert_ne!(
                    cell.grapheme.as_str(), "\u{2590}",
                    "no ▐ with blocks=None at ({r},{c})"
                );
                assert_ne!(
                    cell.fg, fail_color,
                    "no fail-color with blocks=None at ({r},{c})"
                );
            }
        }
    }

    /// No-marks regression: a pane with no block marks composed with
    /// `Some(colors)` is cell-identical to one composed with `None`.
    #[test]
    fn compose_markless_pane_identical_with_and_without_blocks() {
        let mut e = Emulator::new(4, 6);
        pane(&mut e, b"hi ");
        // Use a pane inset so a border exists.
        let view_fn = || PaneView {
            id: PaneId(0),
            rect: Rect::new(1, 1, 2, 4),
            screen: e.screen(),
            is_active: false,
            scroll_offset: 0,
            copy_mode: None,
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let colors = block_colors();
        let vs_some = compose(&[view_fn()], (4, 6), None, StatusPlacement::Bottom, None, None, None, None, Some(&colors), plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
        let vs_none = compose(&[view_fn()], (4, 6), None, StatusPlacement::Bottom, None, None, None, None, None, plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
        for r in 0..4u16 {
            for c in 0..6u16 {
                let c1 = vs_some.cell(r, c).unwrap().clone();
                let c2 = vs_none.cell(r, c).unwrap().clone();
                assert_eq!(c1, c2, "cell mismatch at ({r},{c})");
            }
        }
    }

    /// Scrolled viewport (scroll_offset > 0) shifts which rows get status.
    /// Build a screen with a failed block that spans the first 3 lines (all in
    /// scrollback after scrolling). With scroll_offset = 0, those rows are not
    /// visible; with scroll_offset = 3 they are visible and should be colored.
    #[test]
    fn compose_scrolled_viewport_shifts_block_rows() {
        use plexy_glass_emulator::Emulator;
        // 3-row pane; feed 6 lines so lines 0..2 go to scrollback.
        // Block: A at line 0, D;1 on line 3 (shared with A of block 2).
        let mut e = Emulator::new(3, 20);
        e.advance(
            b"\x1b]133;A\x07$ fail\r\n\
              \x1b]133;C\x07out1\r\n\
              out2\r\n\
              \x1b]133;D;1\x07\x1b]133;A\x07$ next\r\n\
              \x1b]133;C\x07x\r\n\
              y",
        );
        e.advance(b"\x1b[m");
        let screen = e.screen().clone();
        // scrollback should have 3 rows (lines 0..2), active grid = lines 3..5.
        assert_eq!(screen.scrollback.rows().len(), 3, "setup: 3 scrollback rows");

        let colors = block_colors();

        // scroll_offset = 3: viewport shows lines 0..2 (all in scrollback).
        // Block 1 (lines 0..2) is Failed → left-segment cells should be colored.
        let view_scrolled = PaneView {
            id: PaneId(0),
            rect: Rect::new(1, 1, 3, 18),
            screen: &screen,
            is_active: false,
            scroll_offset: 3,
            copy_mode: None,
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let vs_scrolled = compose(
            &[view_scrolled],
            (5, 20),
            None,
            StatusPlacement::Bottom,
            None,
            None,
            None,
            None,
            Some(&colors),
        plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
        // Left-segment column = pane.rect.col - 1 = 0. Pane rows 1..=3 map to
        // block rows 0..2. All should be Failed (fail color / ▐).
        for r in 1..=3u16 {
            let cell = vs_scrolled.cell(r, 0).unwrap();
            assert_eq!(
                cell.fg, colors.fail,
                "scrolled viewport row {r}: expected fail color on left segment"
            );
        }

        // scroll_offset = 0: viewport shows lines 3..5 (active grid, block 2
        // running) → left-segment should NOT be fail colored.
        let view_live = PaneView {
            id: PaneId(0),
            rect: Rect::new(1, 1, 3, 18),
            screen: &screen,
            is_active: false,
            scroll_offset: 0,
            copy_mode: None,
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let vs_live = compose(
            &[view_live],
            (5, 20),
            None,
            StatusPlacement::Bottom,
            None,
            None,
            None,
            None,
            Some(&colors),
        plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
        for r in 1..=3u16 {
            let cell = vs_live.cell(r, 0).unwrap();
            assert_ne!(
                cell.fg, colors.fail,
                "live viewport row {r}: should not have fail color (block 2 running)"
            );
        }
    }

    // ── block-mode selection bracket (render path) ───────────────────────────

    /// 8-row screen, two OSC-133 blocks; same bytes as `block_mode::tests`.
    fn two_block_screen() -> plexy_glass_emulator::Screen {
        use plexy_glass_emulator::Emulator;
        let mut e = Emulator::new(8, 20);
        e.advance(
            b"\x1b]133;A\x07$ \x1b]133;B\x07one\r\n\
              \x1b]133;C\x07out1\r\n\
              out2\r\n\
              \x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07two\r\n\
              \x1b]133;C\x07out3",
        );
        e.advance(b"\x1b[m");
        e.screen().clone()
    }

    const SEL: plexy_glass_emulator::Color = plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61);

    /// A fully-visible selected block gets ┏ at its top content row, ┃ between,
    /// and ┗ at its bottom, all on the pane's left border column.
    #[test]
    fn selected_block_maps_fully_visible_block_with_both_caps() {
        let screen = two_block_screen();
        // Select block 1 (prompt line 0, extent lines 0..=2), viewport at top.
        let bm = crate::BlockMode { selected: 0, viewport_top: 0, pane_rows: 8, total_lines: 8, filter: None };
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(1, 1, 8, 18),
            screen: &screen,
            is_active: false,
            scroll_offset: 0,
            copy_mode: None,
            block_mode: Some(&bm),
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let vs = compose(&[view], (10, 20), None, StatusPlacement::Bottom, None, None, None, None, None, SEL, ChromeColors::ansi_default());
        // Left segment col = rect.col - 1 = 0. Block rows 0..=2 → host rows 1..=3.
        assert_eq!(vs.cell(1, 0).unwrap().grapheme.as_str(), "\u{250f}", "top cap ┏");
        assert_eq!(vs.cell(2, 0).unwrap().grapheme.as_str(), "\u{2503}", "middle ┃");
        assert_eq!(vs.cell(3, 0).unwrap().grapheme.as_str(), "\u{2517}", "bottom cap ┗");
        assert_eq!(vs.cell(2, 0).unwrap().fg, SEL, "bracket uses the select color");
    }

    /// When the selected block's top is scrolled above the viewport, the
    /// topmost visible bracket cell is ┃ (no ┏ cap).
    #[test]
    fn selected_block_omits_top_cap_when_scrolled_above_viewport() {
        use plexy_glass_emulator::Emulator;
        // 3-row pane fed 6 lines → 3 scrollback rows; block 1 = lines 0..=2.
        let mut e = Emulator::new(3, 20);
        e.advance(
            b"\x1b]133;A\x07$ one\r\n\
              \x1b]133;C\x07out1\r\n\
              out2\r\n\
              \x1b]133;D;0\x07\x1b]133;A\x07$ two\r\n\
              \x1b]133;C\x07out3\r\n\
              y",
        );
        e.advance(b"\x1b[m");
        let screen = e.screen().clone();
        assert_eq!(screen.scrollback.rows().len(), 3, "setup: 3 scrollback rows");
        // Select block 1 (line 0); viewport_top = 1 so its top (line 0) is above
        // the viewport. effective_scroll = 6 - 1 - 3 = 2 → top = 3 - 2 = 1.
        let bm = crate::BlockMode { selected: 0, viewport_top: 1, pane_rows: 3, total_lines: 6, filter: None };
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(1, 1, 3, 18),
            screen: &screen,
            is_active: false,
            scroll_offset: 0,
            copy_mode: None,
            block_mode: Some(&bm),
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let vs = compose(&[view], (5, 20), None, StatusPlacement::Bottom, None, None, None, None, None, SEL, ChromeColors::ansi_default());
        // Topmost visible bracket cell (host row 1) is ┃, not ┏.
        assert_eq!(vs.cell(1, 0).unwrap().grapheme.as_str(), "\u{2503}", "no top cap → ┃");
        assert_ne!(vs.cell(1, 0).unwrap().grapheme.as_str(), "\u{250f}");
    }

    /// A pane in block mode whose child entered the alt screen must NOT get a
    /// bracket painted over the full-screen app. (Regression: the bracket
    /// closure now short-circuits on `screen.alt.is_some()`.)
    #[test]
    fn selected_block_suppressed_on_alt_screen() {
        use plexy_glass_emulator::Emulator;
        let mut e = Emulator::new(6, 20);
        // A completed block on the main screen, then enter the alt screen.
        e.advance(b"\x1b]133;A\x07$ one\r\n\x1b]133;C\x07out\r\n\x1b]133;D;0\x07");
        e.advance(b"\x1b[?1049h\x1b]133;A\x07$ alt");
        e.advance(b"\x1b[m");
        let screen = e.screen().clone();
        assert!(screen.alt.is_some(), "setup: alt screen active");
        let bm = crate::BlockMode { selected: 0, viewport_top: 0, pane_rows: 6, total_lines: 6, filter: None };
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(1, 1, 6, 18),
            screen: &screen,
            is_active: false,
            scroll_offset: 0,
            copy_mode: None,
            block_mode: Some(&bm),
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let vs = compose(&[view], (8, 20), None, StatusPlacement::Bottom, None, None, None, None, None, SEL, ChromeColors::ansi_default());
        // No bracket glyph anywhere on the pane's left border column.
        for r in 0..8u16 {
            let g = vs.cell(r, 0).unwrap().grapheme.as_str().to_string();
            assert!(
                g != "\u{250f}" && g != "\u{2503}" && g != "\u{2517}",
                "bracket glyph {g:?} leaked onto the alt screen at row {r}"
            );
        }
    }

    // ── block-mode filter (dim / highlight / prompt bar) ──────────────────────

    /// A two-block screen filtered to block 1 ("alpha"): block 2's content is
    /// dimmed; block 1's matched text is highlighted.
    #[test]
    fn filter_dims_non_matches_and_highlights_matches() {
        use plexy_glass_emulator::{Attrs, Emulator};
        let mut e = Emulator::new(8, 20);
        e.advance(
            b"\x1b]133;A\x07$ \x1b]133;B\x07alpha\r\n\
              \x1b]133;C\x07out\r\n\
              \x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07beta\r\n\
              \x1b]133;C\x07out",
        );
        e.advance(b"\x1b[m");
        let screen = e.screen().clone();
        // Block 1 prompt at line 0, block 2 prompt at line 2 (D+A share a row).
        let bm = crate::BlockMode {
            selected: 0,
            viewport_top: 0,
            pane_rows: 8,
            total_lines: 8,
            filter: Some(crate::Filter {
                query: "alpha".into(),
                prompt_active: false,
                matches: vec![0],
            }),
        };
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(1, 1, 8, 18),
            screen: &screen,
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
            block_mode: Some(&bm),
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let vs = compose(&[view], (10, 20), None, StatusPlacement::Bottom, None, None, None, None, None, SEL, ChromeColors::ansi_default());
        // Block 1 command row = host row 1; "alpha" begins after "$ " at content
        // col 2 → host col 1 + 2 = 3. It must be HIGHLIGHT, not DIM.
        assert!(vs.cell(1, 3).unwrap().attrs.contains(Attrs::HIGHLIGHT), "match highlighted");
        assert!(!vs.cell(1, 3).unwrap().attrs.contains(Attrs::DIM), "match row not dimmed");
        // Block 2 command row = line 2 → host row 3 → dimmed.
        assert!(vs.cell(3, 2).unwrap().attrs.contains(Attrs::DIM), "non-match dimmed");
    }

    /// No dim/highlight on the alt screen even with a filter present.
    #[test]
    fn filter_suppressed_on_alt_screen() {
        use plexy_glass_emulator::{Attrs, Emulator};
        let mut e = Emulator::new(6, 20);
        e.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07alpha\r\n\x1b]133;C\x07out\r\n\x1b]133;D;0\x07");
        e.advance(b"\x1b[?1049h$ alt");
        e.advance(b"\x1b[m");
        let screen = e.screen().clone();
        assert!(screen.alt.is_some());
        let bm = crate::BlockMode {
            selected: 0,
            viewport_top: 0,
            pane_rows: 6,
            total_lines: 6,
            filter: Some(crate::Filter {
                query: "alpha".into(),
                prompt_active: false,
                matches: vec![0],
            }),
        };
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(1, 1, 6, 18),
            screen: &screen,
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
            block_mode: Some(&bm),
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let vs = compose(&[view], (8, 20), None, StatusPlacement::Bottom, None, None, None, None, None, SEL, ChromeColors::ansi_default());
        for r in 0..8u16 {
            for c in 0..20u16 {
                let a = vs.cell(r, c).unwrap().attrs;
                assert!(
                    !a.contains(Attrs::DIM) && !a.contains(Attrs::HIGHLIGHT),
                    "no filter paint on alt at ({r},{c})"
                );
            }
        }
    }

    /// The filter prompt bar renders `filter: <query> (<n>/<total>)` on the
    /// pane's bottom row while typing.
    #[test]
    fn filter_prompt_bar_renders_query_and_count() {
        use plexy_glass_emulator::Emulator;
        let mut e = Emulator::new(8, 20);
        e.advance(
            b"\x1b]133;A\x07$ \x1b]133;B\x07alpha\r\n\
              \x1b]133;C\x07out\r\n\
              \x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07beta\r\n\
              \x1b]133;C\x07out",
        );
        e.advance(b"\x1b[m");
        let screen = e.screen().clone();
        let bm = crate::BlockMode {
            selected: 0,
            viewport_top: 0,
            pane_rows: 8,
            total_lines: 8,
            filter: Some(crate::Filter {
                query: "alpha".into(),
                prompt_active: true,
                matches: vec![0],
            }),
        };
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(1, 1, 8, 18),
            screen: &screen,
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
            block_mode: Some(&bm),
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let vs = compose(&[view], (10, 20), None, StatusPlacement::Bottom, None, None, None, None, None, SEL, ChromeColors::ansi_default());
        // Bottom content row of the pane = rect.row + rect.rows - 1 = 8. Read the
        // full host width, since the bar ("filter: alpha (1/2)", 19 cols) starts
        // at col 1 and runs past the pane's content columns.
        let row: String = (1..20)
            .filter_map(|c| vs.cell(8, c).map(|cell| cell.grapheme.as_str().to_string()))
            .collect();
        assert!(row.contains("filter: alpha (1/2)"), "prompt bar text: {row:?}");
    }

    /// Status bar on TOP shifts every pane down by one physical row
    /// (`pane_row_offset = 1`), and the block segment must shift WITH the pane:
    /// fail color at `offset + rect.row + r`, not one row off, and never on
    /// the status row.
    #[test]
    fn compose_status_top_offsets_block_segment_with_the_pane() {
        use plexy_glass_emulator::Emulator;
        // Same fixture as the scrolled test: block 1 (lines 0..2) Failed.
        let mut e = Emulator::new(3, 20);
        e.advance(
            b"\x1b]133;A\x07$ fail\r\n\
              \x1b]133;C\x07out1\r\n\
              out2\r\n\
              \x1b]133;D;1\x07\x1b]133;A\x07$ next\r\n\
              \x1b]133;C\x07x\r\n\
              y",
        );
        e.advance(b"\x1b[m");
        let screen = e.screen().clone();
        let colors = block_colors();
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(1, 1, 3, 18),
            screen: &screen,
            is_active: false,
            scroll_offset: 3, // viewport = lines 0..2, all Failed
            copy_mode: None,
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let status = status_with_left("AB");
        let vs = compose(
            &[view],
            (6, 20),
            Some(&status),
            StatusPlacement::Top,
            None,
            None,
            None,
            None,
            Some(&colors),
        plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
        // Logical pane rows 1..=3 paint physically at rows 2..=4 (offset 1).
        for r in 2..=4u16 {
            let cell = vs.cell(r, 0).unwrap();
            assert_eq!(
                cell.fg, colors.fail,
                "top placement: fail color at physical row {r} col 0"
            );
        }
        // The unshifted positions: row 1 is the band's top frame line and row 0
        // the status bar, so neither may carry the segment color.
        for r in 0..=1u16 {
            let cell = vs.cell(r, 0).unwrap();
            assert_ne!(
                cell.fg, colors.fail,
                "top placement: row {r} col 0 must not take the segment color"
            );
        }
    }

    /// Copy-mode viewport uses the copy-mode top for block status.
    #[test]
    fn compose_copy_mode_viewport_uses_copy_mode_top() {
        use plexy_glass_emulator::Emulator;
        // Same screen as above: block 1 (lines 0..2) Failed, block 2 (lines 3..5) running.
        let mut e = Emulator::new(3, 20);
        e.advance(
            b"\x1b]133;A\x07$ fail\r\n\
              \x1b]133;C\x07out1\r\n\
              out2\r\n\
              \x1b]133;D;1\x07\x1b]133;A\x07$ next\r\n\
              \x1b]133;C\x07x\r\n\
              y",
        );
        e.advance(b"\x1b[m");
        let screen = e.screen().clone();

        let colors = block_colors();

        // Copy-mode with viewport_top = 0: shows lines 0..2 (block 1, Failed).
        let cm = crate::CopyMode {
            cursor: (0, 0),
            anchor: None,
            search: crate::SearchState::default(),
            viewport_top: 0,
            pane_rows: 3,
            total_lines: 6,
        };
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(1, 1, 3, 18),
            screen: &screen,
            is_active: false,
            scroll_offset: 0, // copy-mode overrides this
            copy_mode: Some(&cm),
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let vs = compose(
            &[view],
            (5, 20),
            None,
            StatusPlacement::Bottom,
            None,
            None,
            None,
            None,
            Some(&colors),
        plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
        // Lines 0..2 visible → block 1 (Failed) → left-segment cells colored.
        for r in 1..=3u16 {
            let cell = vs.cell(r, 0).unwrap();
            assert_eq!(
                cell.fg, colors.fail,
                "copy-mode top=0, row {r}: expected fail color"
            );
        }
    }

    // ── Popup border block exit-status tests ─────────────────────────────────

    /// Build a popup emulator screen seeded with OSC 133 sequences.
    /// A trailing SGR-reset flushes the last pending grapheme into the grid.
    fn popup_screen_from(rows: u16, cols: u16, bytes: &[u8]) -> plexy_glass_emulator::Screen {
        let mut e = Emulator::new(rows, cols);
        e.advance(bytes);
        e.advance(b"\x1b[m");
        e.screen().clone()
    }

    /// Construct a minimal compose call with a popup and optional block colors.
    /// Layout: host 12x40, one background pane (full host), popup rect (2,10,8,22)
    /// so the popup box has 6 interior rows and 20 interior cols. The left border
    /// of the popup is at col 10, rows 3..=8 (pane_row_offset=0, rect.row=2, rows=8).
    fn compose_with_popup(
        popup_screen: &plexy_glass_emulator::Screen,
        blocks: Option<&crate::borders::BlockBorderColors>,
    ) -> VirtualScreen {
        let mut bg = Emulator::new(12, 40);
        bg.advance(b"x ");
        let view = PaneView {
            id: PaneId(0),
            rect: Rect::new(0, 0, 12, 40),
            screen: bg.screen(),
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        // Outer box 8 rows × 22 cols at (2, 10); interior = 6 rows × 20 cols.
        let rect = Rect::new(2, 10, 8, 22);
        let pv = PopupView { rect, screen: popup_screen, title: "test" };
        compose(
            &[view],
            (12, 40),
            None,
            StatusPlacement::Bottom,
            None,
            None,
            None,
            Some(&pv),
            blocks,
        plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default())
    }

    /// Popup with a failed block → left border rows colored fail + ▐ glyph.
    ///
    /// Popup screen: 6 rows × 20 cols; one completed failed block.
    ///   row 0: A "$ fail"
    ///   row 1: C output
    ///   row 2..5: D;1 + A on row 2
    ///
    /// The popup's live viewport top = scrollback.len() = 0 (popup is live).
    /// Interior rows 0..5 → absolute lines 0..5 in the popup screen.
    /// Block 1 (lines 0..1) is closed with D;1 on line 2 (D+A shared) → Failed.
    /// The left border cells at host (rect.row+1+r, rect.col) = (3+r, 10)
    /// for r in 0..1 should be fail-colored with ▐.
    #[test]
    fn popup_failed_block_left_border_colored() {
        let screen = popup_screen_from(
            6,
            20,
            b"\x1b]133;A\x07$ fail\r\n\
              \x1b]133;C\x07output\r\n\
              \x1b]133;D;1\x07\x1b]133;A\x07$ next",
        );
        let colors = block_colors();
        let vs = compose_with_popup(&screen, Some(&colors));
        // Popup left border col = rect.col = 10.
        // Interior rows 0 and 1 (host rows 3 and 4) map to block 1 (Failed).
        // Row 2 (host row 5) is the shared D+A row (block 2 prompt, running) → None.
        let fail_row_0 = vs.cell(3, 10).unwrap();
        assert_eq!(fail_row_0.fg, colors.fail, "popup left border row 3: fail color");
        assert_eq!(fail_row_0.grapheme.as_str(), "\u{2590}", "popup left border row 3: │ → ▐");
        let fail_row_1 = vs.cell(4, 10).unwrap();
        assert_eq!(fail_row_1.fg, colors.fail, "popup left border row 4: fail color");
        assert_eq!(fail_row_1.grapheme.as_str(), "\u{2590}", "popup left border row 4: │ → ▐");
        // Row 5 (shared D+A, block 2 running) → plain border (no fail color, no ▐).
        let plain_row_2 = vs.cell(5, 10).unwrap();
        assert_ne!(plain_row_2.fg, colors.fail, "popup left border row 5: not fail (running block)");
        assert_ne!(plain_row_2.grapheme.as_str(), "\u{2590}", "popup left border row 5: no ▐");
    }

    /// Popup with an ok block → left border rows colored ok, glyph stays │.
    ///
    /// Popup screen: 6 rows × 20 cols; block closed with D;0.
    ///   row 0: A "$ ok"
    ///   row 1: C output
    ///   row 2: D;0 + A (shared D+A)
    #[test]
    fn popup_ok_block_left_border_colored() {
        let screen = popup_screen_from(
            6,
            20,
            b"\x1b]133;A\x07$ ok\r\n\
              \x1b]133;C\x07output\r\n\
              \x1b]133;D;0\x07\x1b]133;A\x07$ next",
        );
        let colors = block_colors();
        let vs = compose_with_popup(&screen, Some(&colors));
        // Interior rows 0 and 1 (host rows 3 and 4) → block 1 (Ok).
        let ok_row_0 = vs.cell(3, 10).unwrap();
        assert_eq!(ok_row_0.fg, colors.ok, "popup left border row 3: ok color");
        assert_eq!(ok_row_0.grapheme.as_str(), "\u{2502}", "popup left border row 3: │ unchanged (ok)");
        let ok_row_1 = vs.cell(4, 10).unwrap();
        assert_eq!(ok_row_1.fg, colors.ok, "popup left border row 4: ok color");
        assert_eq!(ok_row_1.grapheme.as_str(), "\u{2502}", "popup left border row 4: │ unchanged (ok)");
        // Row 2 (shared D+A, next block running) → plain border.
        let plain_row_2 = vs.cell(5, 10).unwrap();
        assert_ne!(plain_row_2.fg, colors.ok, "popup left border row 5: not ok (running)");
    }

    /// Popup with no block marks: compose WITH Some(colors) → left border cells
    /// are cell-identical to a compose WITHOUT colors (plain border).
    #[test]
    fn popup_no_marks_identical_with_and_without_blocks() {
        let screen = popup_screen_from(6, 20, b"plain text ");
        let colors = block_colors();
        let vs_with = compose_with_popup(&screen, Some(&colors));
        let vs_without = compose_with_popup(&screen, None);
        // Left border column = 10, rows 3..=8 (interior + top/bottom borders).
        // All cells in the popup box's left column must be cell-identical.
        for r in 2..=9u16 {
            let c1 = vs_with.cell(r, 10).unwrap().clone();
            let c2 = vs_without.cell(r, 10).unwrap().clone();
            assert_eq!(
                c1, c2,
                "popup left col 10 row {r}: with/without blocks must be identical (no marks)"
            );
        }
    }

    /// blocks=None with a marked popup screen → left border plain (not colored).
    #[test]
    fn popup_blocks_none_screen_with_marks_is_plain() {
        // Screen has a completed failed block, but blocks=None.
        let screen = popup_screen_from(
            6,
            20,
            b"\x1b]133;A\x07$ fail\r\n\
              \x1b]133;C\x07output\r\n\
              \x1b]133;D;1\x07done",
        );
        let colors = block_colors();
        let vs = compose_with_popup(&screen, None);
        // No cell on the popup's left border should have fail color or ▐.
        for r in 2..=9u16 {
            let cell = vs.cell(r, 10).unwrap();
            assert_ne!(
                cell.fg, colors.fail,
                "blocks=None row {r}: no fail color on popup border"
            );
            assert_ne!(
                cell.grapheme.as_str(), "\u{2590}",
                "blocks=None row {r}: no ▐ on popup border"
            );
        }
    }

    /// Alt-screen popup: `viewport_block_status` returns all-None for alt screen;
    /// the left border stays plain even if block marks exist on the primary screen.
    #[test]
    fn popup_alt_screen_left_border_plain() {
        // Feed a completed failed block, then enter alt screen.
        let screen = popup_screen_from(
            6,
            20,
            b"\x1b]133;A\x07$ fail\r\n\
              \x1b]133;C\x07output\r\n\
              \x1b]133;D;1\x07done\r\n\
              \x1b[?1049h\
              \x1b]133;A\x07$ alt",
        );
        // Confirm alt screen is active.
        assert!(screen.alt.is_some(), "setup: alt screen must be active");
        let colors = block_colors();
        let vs = compose_with_popup(&screen, Some(&colors));
        // viewport_block_status returns all-None on alt screen → no coloring.
        for r in 3..=8u16 {
            let cell = vs.cell(r, 10).unwrap();
            assert_ne!(
                cell.fg, colors.fail,
                "alt-screen popup left border row {r}: no fail color"
            );
            assert_ne!(
                cell.grapheme.as_str(), "\u{2590}",
                "alt-screen popup left border row {r}: no ▐"
            );
        }
    }

    /// A Kitty image fed through a real `Emulator` becomes a placement, and
    /// `compose()` resolves it to a host-positioned `VisiblePlacement` carrying the
    /// image id + data.
    #[test]
    fn inline_image_resolves_to_visible_placement() {
        use plexy_glass_emulator::Emulator;
        let mut e = Emulator::new(8, 40);
        // Two lines of text, then an inline RGB image (10x20px → 1x1 cell at the
        // default 10x20 cell size) placed at row 2.
        e.advance(b"line1\r\nline2\r\n\x1b_Ga=T,i=5,f=24,s=10,v=20;QUJD\x1b\\");
        e.advance(b"\x1b[m");
        let screen = e.screen().clone();
        assert_eq!(screen.placements.len(), 1, "emulator captured the placement");

        let view = PaneView {
            id: PaneId(3),
            rect: Rect::new(1, 1, 8, 38),
            screen: &screen,
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        };
        let vs = compose(&[view], (10, 40), None, StatusPlacement::Bottom, None, None, None, None, None, plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61), ChromeColors::ansi_default());
        assert_eq!(vs.placements.len(), 1, "compose resolved one visible placement");
        let p = &vs.placements[0];
        assert_eq!(p.image_id, host_image_id(3, 5), "raw id 5 folded with pane 3");
        // anchor_line 2, top 0 → host row = pane_row_offset(0) + rect.row(1) + 2 = 3.
        assert_eq!(p.host_row, 3);
        assert_eq!(p.host_col, 1, "rect.col + 0");
        // Uncropped: source rect is the whole image (10×20 px, 1×1 cell).
        assert_eq!((p.src_x, p.src_y, p.src_w, p.src_h), (0, 0, 10, 20));
        assert_eq!((p.rows, p.cols), (1, 1));
        assert_eq!(p.data_b64.as_ref(), b"QUJD", "carries the image data");
        // key folds the pane id (3) into the high bits.
        assert_eq!(p.key, (3u64 << 40), "pane-3, seq 0");
    }

    #[test]
    fn same_raw_image_id_in_two_panes_gets_distinct_host_ids() {
        // Two panes both using Kitty image id 5 must not collide on the wire.
        let a = host_image_id(1, 5);
        let b = host_image_id(2, 5);
        assert_ne!(a, b, "pane id namespaces the host image id");
        assert_ne!(a, 0, "host id is non-zero (Kitty i=0 means no id)");
        assert_ne!(b, 0);
        // Deterministic across frames so transmit-once stays stable.
        assert_eq!(a, host_image_id(1, 5));
    }

    #[test]
    fn crop_axis_maps_cells_to_source_pixels() {
        // Uncropped: exact full extent.
        assert_eq!(crop_axis(80, 4, 0, 4), (0, 80));
        // Bottom crop: show top 2 of 4 cells → first 40 px.
        assert_eq!(crop_axis(80, 4, 0, 2), (0, 40));
        // Top crop: hide top 1 of 4, show next 3 → offset 20, height 60.
        assert_eq!(crop_axis(80, 4, 1, 3), (20, 60));
        // Middle slice: hide 1, show 2 → [20, 60).
        assert_eq!(crop_axis(80, 4, 1, 2), (20, 40));
    }

    const TEST_COLOR: plexy_glass_emulator::Color =
        plexy_glass_emulator::Color::Rgb(0xdc, 0xa5, 0x61);

    /// 30×80-px image (= 3 cols × 4 rows at the default 10×20 cell) anchored at
    /// line 0 of a fresh `rows`-row emulator.
    fn screen_with_tall_image(rows: u16) -> Screen {
        use plexy_glass_emulator::Emulator;
        let mut e = Emulator::new(rows, 40);
        e.advance(b"\x1b_Ga=T,i=5,f=24,s=30,v=80;QUJD\x1b\\");
        e.advance(b"\x1b[m");
        e.screen().clone()
    }

    fn plain_view(screen: &Screen, rect: Rect) -> PaneView<'_> {
        PaneView {
            id: PaneId(1),
            rect,
            screen,
            is_active: true,
            scroll_offset: 0,
            copy_mode: None,
            block_mode: None,
            title: None,
            marked: false,
            drag_role: PaneDragRole::None,
        }
    }

    fn scrolled_view(screen: &Screen, rect: Rect, scroll_offset: u32) -> PaneView<'_> {
        PaneView { scroll_offset, ..plain_view(screen, rect) }
    }

    #[test]
    fn tall_image_cropped_to_pane_bottom() {
        // Realistic: a 4-row image in a 2-row pane (the emulator is pane-sized,
        // so the image scrolls into scrollback). Scroll up to its top → the pane
        // shows the top 2 of 4 rows, the rest clipped at the pane bottom.
        let screen = screen_with_tall_image(2);
        assert_eq!(screen.placements[0].rows, 4, "4-row footprint captured");
        let view = scrolled_view(&screen, Rect::new(0, 0, 2, 40), 3);
        let vs = compose(&[view], (3, 40), None, StatusPlacement::Bottom, None, None, None, None, None, TEST_COLOR, ChromeColors::ansi_default());
        assert_eq!(vs.placements.len(), 1);
        let p = &vs.placements[0];
        assert_eq!((p.rows, p.cols), (2, 3), "clipped to pane bottom");
        assert_eq!((p.src_x, p.src_y, p.src_w, p.src_h), (0, 0, 30, 40), "top 2 of 4 rows → 40px");
    }

    #[test]
    fn wide_image_cropped_to_pane_right() {
        let screen = screen_with_tall_image(8);
        // Pane only 2 cols wide → image clipped to its left 2 cols.
        let view = plain_view(&screen, Rect::new(0, 0, 8, 2));
        let vs = compose(&[view], (8, 2), None, StatusPlacement::Bottom, None, None, None, None, None, TEST_COLOR, ChromeColors::ansi_default());
        assert_eq!(vs.placements.len(), 1);
        let p = &vs.placements[0];
        assert_eq!((p.rows, p.cols), (4, 2), "full height, clipped width");
        assert_eq!((p.src_x, p.src_w), (0, 20), "left 2 of 3 cols → 20px");
        assert_eq!(p.src_h, 80, "full height");
    }

    #[test]
    fn image_scrolled_off_top_is_dropped() {
        use plexy_glass_emulator::Emulator;
        let mut e = Emulator::new(3, 40);
        e.advance(b"\x1b_Ga=T,i=5,f=24,s=10,v=20;QUJD\x1b\\"); // 1×1 image, anchor 0
        for _ in 0..6 {
            e.advance(b"\r\n"); // push the image's row up into scrollback
        }
        let screen = e.screen().clone();
        // Viewing at the bottom (scroll_offset 0): the image's line is above the
        // viewport top → no visible placement.
        let view = plain_view(&screen, Rect::new(0, 0, 3, 40));
        let vs = compose(&[view], (4, 40), None, StatusPlacement::Bottom, None, None, None, None, None, TEST_COLOR, ChromeColors::ansi_default());
        assert!(vs.placements.is_empty(), "scrolled-off image is dropped");
    }

    #[test]
    fn overlay_suppresses_all_images() {
        let screen = screen_with_tall_image(8);
        let view = plain_view(&screen, Rect::new(0, 0, 8, 40));
        let ov = OverlayView::RenamePrompt { label: "rename", buf: "x" };
        let vs = compose(&[view], (9, 40), None, StatusPlacement::Bottom, None, Some(&ov), None, None, None, TEST_COLOR, ChromeColors::ansi_default());
        assert!(vs.placements.is_empty(), "modal overlay owns the screen — no images");
    }

    #[test]
    fn popup_suppresses_all_images() {
        use plexy_glass_emulator::Emulator;
        let screen = screen_with_tall_image(8);
        let view = plain_view(&screen, Rect::new(0, 0, 8, 40));
        let mut pe = Emulator::new(4, 20);
        pe.advance(b"popup");
        let pv = PopupView { rect: Rect::new(2, 2, 4, 20), screen: pe.screen(), title: "p" };
        let vs = compose(&[view], (9, 40), None, StatusPlacement::Bottom, None, None, None, Some(&pv), None, TEST_COLOR, ChromeColors::ansi_default());
        assert!(vs.placements.is_empty(), "popup suppresses underlying images");
    }

    #[test]
    fn image_visible_in_copy_mode() {
        let screen = screen_with_tall_image(4); // active rows == rect.rows → top = viewport_top
        let cm = crate::CopyMode {
            cursor: (0, 0),
            anchor: None,
            search: crate::SearchState::default(),
            viewport_top: 0,
            pane_rows: 4,
            total_lines: 4,
        };
        let view = PaneView { copy_mode: Some(&cm), ..plain_view(&screen, Rect::new(0, 0, 4, 40)) };
        let vs = compose(&[view], (5, 40), None, StatusPlacement::Bottom, None, None, None, None, None, TEST_COLOR, ChromeColors::ansi_default());
        assert_eq!(vs.placements.len(), 1, "image follows the copy-mode viewport");
    }

    #[test]
    fn image_visible_in_block_mode() {
        let screen = screen_with_tall_image(4);
        let bm = crate::BlockMode { selected: 0, viewport_top: 0, pane_rows: 4, total_lines: 4, filter: None };
        let view = PaneView { block_mode: Some(&bm), ..plain_view(&screen, Rect::new(0, 0, 4, 40)) };
        let vs = compose(&[view], (5, 40), None, StatusPlacement::Bottom, None, None, None, None, None, TEST_COLOR, ChromeColors::ansi_default());
        assert_eq!(vs.placements.len(), 1, "image follows the block-mode viewport");
    }

    #[test]
    fn alt_screen_hides_then_restores_image() {
        use plexy_glass_emulator::Emulator;
        let mut e = Emulator::new(8, 40);
        e.advance(b"\x1b_Ga=T,i=5,f=24,s=10,v=20;QUJD\x1b\\");
        e.advance(b"\x1b[m");
        // On the main grid: one visible placement.
        let s_main = e.screen().clone();
        let v1 = plain_view(&s_main, Rect::new(0, 0, 8, 40));
        let vs1 = compose(&[v1], (9, 40), None, StatusPlacement::Bottom, None, None, None, None, None, TEST_COLOR, ChromeColors::ansi_default());
        assert_eq!(vs1.placements.len(), 1);
        // Enter alt-screen: image suppressed.
        e.advance(b"\x1b[?1049h");
        let s_alt = e.screen().clone();
        let v2 = plain_view(&s_alt, Rect::new(0, 0, 8, 40));
        let vs2 = compose(&[v2], (9, 40), None, StatusPlacement::Bottom, None, None, None, None, None, TEST_COLOR, ChromeColors::ansi_default());
        assert!(vs2.placements.is_empty(), "no images while on alt-screen");
        // Leave alt-screen: the main-grid placement resolves again.
        e.advance(b"\x1b[?1049l");
        let s_back = e.screen().clone();
        let v3 = plain_view(&s_back, Rect::new(0, 0, 8, 40));
        let vs3 = compose(&[v3], (9, 40), None, StatusPlacement::Bottom, None, None, None, None, None, TEST_COLOR, ChromeColors::ansi_default());
        assert_eq!(vs3.placements.len(), 1, "image restored after leaving alt-screen");
    }

    /// A tall image anchored at line 0, scrolled so its top rows are above the
    /// viewport (real scrollback so `top > 0`).
    #[test]
    fn tall_image_cropped_at_top_when_scrolled() {
        // A 4-row image in a 4-row pane: its top row scrolled into scrollback, so
        // the live view shows the lower 3 rows, cropped at the top.
        let screen = screen_with_tall_image(4);
        let view = plain_view(&screen, Rect::new(0, 0, 4, 40));
        let vs = compose(&[view], (5, 40), None, StatusPlacement::Bottom, None, None, None, None, None, TEST_COLOR, ChromeColors::ansi_default());
        assert_eq!(vs.placements.len(), 1);
        let p = &vs.placements[0];
        assert_eq!(p.host_row, 0, "visible part starts at the pane top");
        assert!(p.rows < 4, "top rows clipped");
        assert!(p.src_y > 0, "source cropped from the top");
        // Cumulative crop is exact along the bottom edge: src spans the visible rows.
        assert_eq!(p.src_y, 20 * (4 - u32::from(p.rows)), "skipped rows × cell height");
        assert_eq!(p.src_h, 20 * u32::from(p.rows), "visible rows × cell height");
    }

    #[test]
    fn tall_image_cropped_both_ends_in_a_one_row_pane() {
        // A single-row pane scrolled to a middle row of the image: cropped above
        // AND below.
        let screen = screen_with_tall_image(1);
        let view = scrolled_view(&screen, Rect::new(0, 0, 1, 40), 2);
        let vs = compose(&[view], (2, 40), None, StatusPlacement::Bottom, None, None, None, None, None, TEST_COLOR, ChromeColors::ansi_default());
        assert_eq!(vs.placements.len(), 1);
        let p = &vs.placements[0];
        assert_eq!(p.rows, 1, "one visible row");
        assert!(p.src_y > 0, "cropped above");
        assert!(p.src_y + p.src_h < p.pixel_h, "cropped below");
    }

    #[test]
    fn image_host_row_accounts_for_top_status_bar() {
        // 4-row image in a 2-row pane, scrolled to its top, with a top status bar.
        let screen = screen_with_tall_image(2);
        let status = status_with_left("S");
        let view = scrolled_view(&screen, Rect::new(0, 0, 2, 40), 3);
        let vs = compose(&[view], (3, 40), Some(&status), StatusPlacement::Top, None, None, None, None, None, TEST_COLOR, ChromeColors::ansi_default());
        assert_eq!(vs.placements.len(), 1);
        let p = &vs.placements[0];
        assert_eq!(p.host_row, 1, "shifted down by the top status row");
        assert_eq!(p.rows, 2, "clipped to the 2-row pane band");
    }

    fn composed_row(vs: &VirtualScreen, r: u16) -> String {
        (0..vs.cols)
            .filter_map(|c| vs.cell(r, c))
            .map(|c| c.grapheme.as_str())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    /// Full-frame snapshot dump. Plain mode is one line per row of cell graphemes
    /// (wide-spacer cells have an empty grapheme, so a wide char renders once, same
    /// as `composed_row`/`screen_text`). With `attrs`, a second grid is appended
    /// marking per-cell character attributes: (R)everse, (H)ighlight, (D)im, (B)old,
    /// (U)nderline, `.` none, so attribute-only differences (copy-mode selection,
    /// search highlight, the dim sticky header) are visible in the snapshot, which a
    /// plain-text dump would render identically to its baseline. In `attrs` mode the
    /// text section keeps its full row height (so the two grids stay column-aligned),
    /// and a multi-attribute cell shows a single mark by priority R > H > D > B > U.
    /// Note that `dump_frame` has no color channel, so color-only distinctions (e.g.
    /// the block-status border's ok/fail colors) are not captured here.
    fn dump_frame(vs: &VirtualScreen, attrs: bool) -> String {
        let mut text: Vec<String> = (0..vs.rows).map(|r| composed_row(vs, r)).collect();
        if !attrs {
            while text.last().is_some_and(std::string::String::is_empty) {
                text.pop();
            }
            return text.join("\n");
        }
        let marks: Vec<String> = (0..vs.rows)
            .map(|r| {
                (0..vs.cols)
                    .map(|c| match vs.cell(r, c) {
                        Some(cell) if cell.attrs.contains(plexy_glass_emulator::Attrs::REVERSE) => 'R',
                        Some(cell) if cell.attrs.contains(plexy_glass_emulator::Attrs::HIGHLIGHT) => 'H',
                        Some(cell) if cell.attrs.contains(plexy_glass_emulator::Attrs::DIM) => 'D',
                        Some(cell) if cell.attrs.contains(plexy_glass_emulator::Attrs::BOLD) => 'B',
                        Some(cell) if cell.attrs.contains(plexy_glass_emulator::Attrs::UNDERLINE) => 'U',
                        _ => '.',
                    })
                    .collect::<String>()
                    .trim_end_matches('.')
                    .to_string()
            })
            .collect();
        format!(
            "{}\n--- attrs (R)everse (H)ighlight (D)im (B)old (U)nderline ---\n{}",
            text.join("\n"),
            marks.join("\n"),
        )
    }

    // ── Command duration + sticky header tests ───────────────────────────────

    fn block_colors_with(
        duration_threshold_ms: Option<u32>,
        sticky_header: bool,
    ) -> BlockBorderColors {
        BlockBorderColors { duration_threshold_ms, sticky_header, ..block_colors() }
    }

    /// 8×40 screen: one completed block ("$ <cmd>") with a stamped duration on
    /// its closing D row.
    fn duration_block_screen(cmd: &str, exit: u8, ms: u32) -> Screen {
        use plexy_glass_emulator::Emulator;
        let mut e = Emulator::new(8, 40);
        let bytes = format!("\x1b]133;A\x07{cmd}\r\n\x1b]133;C\x07out\r\n\x1b]133;D;{exit}\x07done");
        e.advance(bytes.as_bytes());
        e.advance(b"\x1b[m");
        let mut s = e.screen().clone();
        let d = s
            .active
            .rows
            .iter_mut()
            .find(|r| r.mark.contains(RowMark::BLOCK_END))
            .expect("a BLOCK_END row");
        d.mark.set_duration(Some(ms));
        s
    }

    fn frame_has(vs: &VirtualScreen, needle: &str) -> bool {
        (0..vs.rows).any(|r| composed_row(vs, r).contains(needle))
    }

    fn row_has_attr(vs: &VirtualScreen, r: u16, attr: plexy_glass_emulator::Attrs) -> bool {
        (0..vs.cols).any(|c| vs.cell(r, c).is_some_and(|cell| cell.attrs.contains(attr)))
    }

    #[test]
    fn inline_duration_painted_above_threshold() {
        let screen = duration_block_screen("$ slow", 0, 3000);
        let colors = block_colors_with(Some(2000), false);
        let view = plain_view(&screen, Rect::new(0, 0, 8, 40));
        let vs = compose(&[view], (8, 40), None, StatusPlacement::Bottom, None, None, None, None, Some(&colors), TEST_COLOR, ChromeColors::ansi_default());
        let row0 = composed_row(&vs, 0);
        assert!(row0.starts_with("$ slow"), "command kept: {row0:?}");
        assert!(row0.contains("3.0s"), "duration painted: {row0:?}");
    }

    #[test]
    fn duration_anchors_on_command_row_under_multiline_prompt() {
        use plexy_glass_emulator::Emulator;
        // Two-line prompt: 133;A on row 0, 133;B + the typed command on row 1.
        let mut e = Emulator::new(8, 40);
        e.advance(
            b"\x1b]133;A\x07prompt\r\n\x1b]133;B\x07sleep 3\r\n\x1b]133;C\x07out\r\n\x1b]133;D;0\x07x",
        );
        e.advance(b"\x1b[m");
        let mut screen = e.screen().clone();
        let d = screen
            .active
            .rows
            .iter_mut()
            .find(|r| r.mark.contains(RowMark::BLOCK_END))
            .expect("BLOCK_END row");
        d.mark.set_duration(Some(3000));
        let colors = block_colors_with(Some(2000), false);
        let view = plain_view(&screen, Rect::new(0, 0, 8, 40));
        let vs = compose(&[view], (8, 40), None, StatusPlacement::Bottom, None, None, None, None, Some(&colors), TEST_COLOR, ChromeColors::ansi_default());
        assert!(!composed_row(&vs, 0).contains("3.0s"), "not on the prompt-start row: {:?}", composed_row(&vs, 0));
        assert!(composed_row(&vs, 1).contains("3.0s"), "on the command (133;B) row: {:?}", composed_row(&vs, 1));
    }

    #[test]
    fn inline_duration_omitted_below_threshold() {
        let screen = duration_block_screen("$ quick", 0, 500);
        let colors = block_colors_with(Some(2000), false);
        let view = plain_view(&screen, Rect::new(0, 0, 8, 40));
        let vs = compose(&[view], (8, 40), None, StatusPlacement::Bottom, None, None, None, None, Some(&colors), TEST_COLOR, ChromeColors::ansi_default());
        assert_eq!(composed_row(&vs, 0), "$ quick", "below-threshold → no annotation");
    }

    #[test]
    fn inline_duration_threshold_zero_shows_all() {
        let screen = duration_block_screen("$ quick", 0, 500);
        let colors = block_colors_with(Some(0), false);
        let view = plain_view(&screen, Rect::new(0, 0, 8, 40));
        let vs = compose(&[view], (8, 40), None, StatusPlacement::Bottom, None, None, None, None, Some(&colors), TEST_COLOR, ChromeColors::ansi_default());
        assert!(composed_row(&vs, 0).contains("500ms"), "threshold 0 shows everything");
    }

    #[test]
    fn folded_summary_combines_with_duration() {
        let mut screen = two_block_fold_screen(); // block 0 folded, 2 hidden lines
        if let Some(d) = screen
            .active
            .rows
            .iter_mut()
            .find(|r| r.mark.contains(RowMark::BLOCK_END))
        {
            d.mark.set_duration(Some(3000));
        }
        let colors = block_colors_with(Some(2000), false);
        let view = plain_view(&screen, Rect::new(0, 0, 8, 40));
        let vs = compose(&[view], (8, 40), None, StatusPlacement::Bottom, None, None, None, None, Some(&colors), TEST_COLOR, ChromeColors::ansi_default());
        let row0 = composed_row(&vs, 0);
        assert!(row0.contains("2 lines"), "fold summary: {row0:?}");
        assert!(row0.contains("3.0s"), "duration appended: {row0:?}");
        assert!(row0.contains('·'), "summary · duration: {row0:?}");
    }

    #[test]
    fn duration_suppressed_in_copy_mode() {
        let screen = duration_block_screen("$ slow", 0, 3000);
        let cm = crate::CopyMode {
            cursor: (0, 0),
            anchor: None,
            search: crate::SearchState::default(),
            viewport_top: 0,
            pane_rows: 8,
            total_lines: 8,
        };
        let view = PaneView { copy_mode: Some(&cm), ..plain_view(&screen, Rect::new(0, 0, 8, 40)) };
        let colors = block_colors_with(Some(2000), false);
        let vs = compose(&[view], (8, 40), None, StatusPlacement::Bottom, None, None, None, None, Some(&colors), TEST_COLOR, ChromeColors::ansi_default());
        assert!(!frame_has(&vs, "3.0s"), "duration suppressed in copy mode");
    }

    /// 3-row pane fed a prompt + 5 output lines, so the "$ cargo build" command
    /// scrolls into scrollback (off the pane top). The block is left unclosed.
    fn tall_block_screen() -> Screen {
        use plexy_glass_emulator::Emulator;
        let mut e = Emulator::new(3, 40);
        e.advance(
            b"\x1b]133;A\x07$ \x1b]133;B\x07cargo build\r\n\
              \x1b]133;C\x07l1\r\nl2\r\nl3\r\nl4\r\nl5",
        );
        e.advance(b"\x1b[m");
        e.screen().clone()
    }

    #[test]
    fn sticky_header_pins_command_when_scrolled_back() {
        // Scrolled back one line → the command line is above the viewport top and
        // gets pinned as a dim (not reverse) header.
        let screen = tall_block_screen();
        let colors = block_colors_with(None, true);
        let view = scrolled_view(&screen, Rect::new(0, 0, 3, 40), 1);
        let vs = compose(&[view], (3, 40), None, StatusPlacement::Bottom, None, None, None, None, Some(&colors), TEST_COLOR, ChromeColors::ansi_default());
        let row0 = composed_row(&vs, 0);
        assert!(row0.contains("cargo build"), "header pins the command: {row0:?}");
        assert!(row_has_attr(&vs, 0, Attrs::DIM), "header is dimmed (blends with theme)");
        assert!(
            !row_has_attr(&vs, 0, Attrs::REVERSE),
            "no bright reverse-video bar"
        );
    }

    #[test]
    fn sticky_header_absent_at_live_bottom() {
        // At the live bottom (scroll_offset 0) the header must NOT appear, even
        // when the command has overflowed off the top of the pane.
        let screen = tall_block_screen();
        let colors = block_colors_with(None, true);
        let view = plain_view(&screen, Rect::new(0, 0, 3, 40)); // scroll_offset 0
        let vs = compose(&[view], (3, 40), None, StatusPlacement::Bottom, None, None, None, None, Some(&colors), TEST_COLOR, ChromeColors::ansi_default());
        let row0 = composed_row(&vs, 0);
        assert!(!row0.contains("cargo build"), "no header at the live bottom: {row0:?}");
        assert_eq!(row0, "l3", "real top-of-viewport content");
    }

    #[test]
    fn sticky_header_absent_when_command_visible() {
        // Scrolled all the way to the top → the command row is itself visible, so
        // the header is (correctly) skipped even though we're scrolled back.
        let screen = tall_block_screen();
        let colors = block_colors_with(None, true);
        let view = scrolled_view(&screen, Rect::new(0, 0, 3, 40), 3);
        let vs = compose(&[view], (3, 40), None, StatusPlacement::Bottom, None, None, None, None, Some(&colors), TEST_COLOR, ChromeColors::ansi_default());
        let row0 = composed_row(&vs, 0);
        assert!(row0.starts_with("$ cargo build"), "real command row: {row0:?}");
        assert!(!row_has_attr(&vs, 0, Attrs::DIM), "no pinned header");
    }

    #[test]
    fn sticky_header_disabled_paints_nothing() {
        let screen = tall_block_screen();
        let colors = block_colors_with(None, false);
        let view = scrolled_view(&screen, Rect::new(0, 0, 3, 40), 1);
        let vs = compose(&[view], (3, 40), None, StatusPlacement::Bottom, None, None, None, None, Some(&colors), TEST_COLOR, ChromeColors::ansi_default());
        let row0 = composed_row(&vs, 0);
        assert!(!row0.contains("cargo build"), "no header when disabled: {row0:?}");
        assert_eq!(row0, "l2", "real top-of-viewport content");
    }

    #[test]
    fn sticky_header_shows_duration() {
        let mut screen = tall_block_screen();
        // Close block 0 with a duration on its last output row so the header can
        // surface it.
        let last = screen.active.rows.len() - 1;
        screen.active.rows[last].mark.set(RowMark::BLOCK_END);
        screen.active.rows[last].mark.set_duration(Some(4000));
        let colors = block_colors_with(Some(2000), true);
        let view = scrolled_view(&screen, Rect::new(0, 0, 3, 40), 1);
        let vs = compose(&[view], (3, 40), None, StatusPlacement::Bottom, None, None, None, None, Some(&colors), TEST_COLOR, ChromeColors::ansi_default());
        let row0 = composed_row(&vs, 0);
        assert!(row0.contains("cargo build"), "header command: {row0:?}");
        assert!(row0.contains("4.0s"), "header carries duration: {row0:?}");
    }

    #[test]
    fn sticky_header_omits_duration_when_command_is_long() {
        use plexy_glass_emulator::Emulator;
        // Wide emulator (command doesn't wrap) rendered in a narrow pane, so the
        // pinned command reaches the duration columns → the duration is omitted
        // rather than clobbering the command's tail (the inline overlap guard).
        let mut e = Emulator::new(3, 60);
        e.advance(
            b"\x1b]133;A\x07$ \x1b]133;B\x07cargo build --workspace\r\n\
              \x1b]133;C\x07l1\r\nl2\r\nl3\r\nl4\r\nl5",
        );
        e.advance(b"\x1b[m");
        let mut screen = e.screen().clone();
        let last = screen.active.rows.len() - 1;
        screen.active.rows[last].mark.set(RowMark::BLOCK_END);
        screen.active.rows[last].mark.set_duration(Some(3000));
        let colors = block_colors_with(Some(2000), true);
        let view = scrolled_view(&screen, Rect::new(0, 0, 3, 24), 1);
        let vs = compose(&[view], (3, 60), None, StatusPlacement::Bottom, None, None, None, None, Some(&colors), TEST_COLOR, ChromeColors::ansi_default());
        let row0 = composed_row(&vs, 0);
        assert!(row0.contains("workspace"), "command tail preserved (not clobbered): {row0:?}");
        assert!(!row0.contains("3.0s"), "duration omitted when it would overlap: {row0:?}");
    }

    #[test]
    fn folded_block_hides_output_rows_in_live_view() {
        use plexy_glass_emulator::Emulator;
        let mut e = Emulator::new(8, 40);
        e.advance(
            b"\x1b]133;A\x07$ one\r\n\x1b]133;C\x07out1\r\nout2\r\n\
              \x1b]133;D;0\x07\x1b]133;A\x07$ two\r\n\x1b]133;C\x07out3",
        );
        e.advance(b"\x1b[m");
        let mut screen = e.screen().clone();
        crate::blocks::set_block_folded(&mut screen, 0, true); // fold "$ one"'s output
        let view = plain_view(&screen, Rect::new(0, 0, 8, 40));
        let vs = compose(&[view], (8, 40), None, StatusPlacement::Bottom, None, None, None, None, None, TEST_COLOR, ChromeColors::ansi_default());
        // Command row kept (with a right-aligned fold summary); out1/out2
        // collapsed → the next prompt follows directly.
        assert!(composed_row(&vs, 0).starts_with("$ one"), "command row stays");
        assert_eq!(composed_row(&vs, 1), "$ two", "output collapsed; next prompt follows");
        assert_eq!(composed_row(&vs, 2), "out3");
    }

    #[test]
    fn image_inside_a_folded_block_is_hidden() {
        use plexy_glass_emulator::Emulator;
        let mut e = Emulator::new(8, 40);
        // Block 0's output is an inline image; block 1 is the next prompt.
        e.advance(b"\x1b]133;A\x07$ img\r\n\x1b]133;C\x07\x1b_Ga=T,i=5,f=24,s=10,v=20;QUJD\x1b\\");
        e.advance(b"\x1b]133;A\x07$ next\r\nx");
        let mut screen = e.screen().clone();
        // Before folding: the image resolves to one placement.
        let v0 = plain_view(&screen, Rect::new(0, 0, 8, 40));
        let vs0 = compose(&[v0], (8, 40), None, StatusPlacement::Bottom, None, None, None, None, None, TEST_COLOR, ChromeColors::ansi_default());
        assert_eq!(vs0.placements.len(), 1, "image visible before folding");
        // Fold the image's block → the image hides.
        crate::blocks::set_block_folded(&mut screen, 0, true);
        let v1 = plain_view(&screen, Rect::new(0, 0, 8, 40));
        let vs1 = compose(&[v1], (8, 40), None, StatusPlacement::Bottom, None, None, None, None, None, TEST_COLOR, ChromeColors::ansi_default());
        assert!(vs1.placements.is_empty(), "image inside a folded block is hidden");
    }

    #[test]
    fn folded_block_paints_marker_and_summary() {
        use plexy_glass_emulator::Emulator;
        let mut e = Emulator::new(8, 40);
        e.advance(
            b"\x1b]133;A\x07$ one\r\n\x1b]133;C\x07out1\r\nout2\r\n\
              \x1b]133;D;0\x07\x1b]133;A\x07$ two\r\n\x1b]133;C\x07out3",
        );
        e.advance(b"\x1b[m");
        let mut screen = e.screen().clone();
        crate::blocks::set_block_folded(&mut screen, 0, true);
        let view = plain_view(&screen, Rect::new(0, 0, 8, 40));
        let vs = compose(&[view], (8, 40), None, StatusPlacement::Bottom, None, None, None, None, None, TEST_COLOR, ChromeColors::ansi_default());
        let row0 = composed_row(&vs, 0);
        assert!(row0.starts_with("$ one"), "command row kept: {row0:?}");
        assert!(row0.contains('▸'), "fold marker present: {row0:?}");
        assert!(row0.contains("2 lines"), "hidden line count: {row0:?}");
        assert!(row0.contains('✓'), "ok status glyph: {row0:?}");
    }

    fn two_block_fold_screen() -> Screen {
        use plexy_glass_emulator::Emulator;
        let mut e = Emulator::new(8, 40);
        e.advance(
            b"\x1b]133;A\x07$ one\r\n\x1b]133;C\x07out1\r\nout2\r\n\
              \x1b]133;D;0\x07\x1b]133;A\x07$ two\r\n\x1b]133;C\x07out3",
        );
        e.advance(b"\x1b[m");
        let mut s = e.screen().clone();
        crate::blocks::set_block_folded(&mut s, 0, true);
        s
    }

    #[test]
    fn block_mode_renders_folds_collapsed() {
        // Folding takes effect IN block mode (instant + persists on re-entry).
        let screen = two_block_fold_screen();
        let bm = crate::BlockMode { selected: 0, viewport_top: 0, pane_rows: 8, total_lines: 8, filter: None };
        let view = PaneView { block_mode: Some(&bm), ..plain_view(&screen, Rect::new(0, 0, 8, 40)) };
        let vs = compose(&[view], (8, 40), None, StatusPlacement::Bottom, None, None, None, None, None, TEST_COLOR, ChromeColors::ansi_default());
        assert!(composed_row(&vs, 0).starts_with("$ one"), "command row kept");
        assert_eq!(composed_row(&vs, 1), "$ two", "output collapsed in block mode too");
    }

    #[test]
    fn folded_command_row_is_dimmed() {
        let screen = two_block_fold_screen();
        let view = plain_view(&screen, Rect::new(0, 0, 8, 40));
        let vs = compose(&[view], (8, 40), None, StatusPlacement::Bottom, None, None, None, None, None, TEST_COLOR, ChromeColors::ansi_default());
        assert!(vs.cell(0, 0).unwrap().attrs.contains(Attrs::DIM), "folded command dimmed");
        assert!(!vs.cell(1, 0).unwrap().attrs.contains(Attrs::DIM), "unfolded next prompt not dimmed");
    }

    #[test]
    fn fold_summary_omitted_when_command_fills_the_row() {
        use plexy_glass_emulator::Emulator;
        let mut e = Emulator::new(8, 40);
        let long = "x".repeat(36); // command nearly fills the 40-col row
        let bytes = format!(
            "\x1b]133;A\x07$ {long}\r\n\x1b]133;C\x07out\r\n\x1b]133;A\x07$ next\r\ny"
        );
        e.advance(bytes.as_bytes());
        e.advance(b"\x1b[m");
        let mut screen = e.screen().clone();
        crate::blocks::set_block_folded(&mut screen, 0, true);
        let view = plain_view(&screen, Rect::new(0, 0, 8, 40));
        let vs = compose(&[view], (8, 40), None, StatusPlacement::Bottom, None, None, None, None, None, TEST_COLOR, ChromeColors::ansi_default());
        let row0 = composed_row(&vs, 0);
        assert!(!row0.contains('▸'), "summary omitted when the command fills the row: {row0:?}");
        assert!(row0.contains(&long), "command text not overwritten: {row0:?}");
    }

    #[test]
    fn live_cursor_shifts_up_under_a_fold() {
        use plexy_glass_emulator::Emulator;
        let mut e = Emulator::new(8, 40);
        // Block 0 with 2 output rows, then the active prompt (cursor on it).
        e.advance(b"\x1b]133;A\x07$ one\r\n\x1b]133;C\x07o1\r\no2\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ two ");
        let screen = e.screen().clone();
        let vs0 = compose(
            &[plain_view(&screen, Rect::new(0, 0, 8, 40))],
            (8, 40), None, StatusPlacement::Bottom, None, None, None, None, None, TEST_COLOR,
            ChromeColors::ansi_default(),
        );
        let mut folded = screen;
        crate::blocks::set_block_folded(&mut folded, 0, true);
        let vs1 = compose(
            &[plain_view(&folded, Rect::new(0, 0, 8, 40))],
            (8, 40), None, StatusPlacement::Bottom, None, None, None, None, None, TEST_COLOR,
            ChromeColors::ansi_default(),
        );
        let (r0, r1) = (vs0.cursor.unwrap().0, vs1.cursor.unwrap().0);
        assert_eq!(r1, r0 - 2, "folding 2 output rows above the cursor shifts it up by 2");
    }

    #[test]
    fn virtual_placement_surfaces_and_is_suppressed_under_overlay() {
        use plexy_glass_emulator::Emulator;
        let mut e = Emulator::new(8, 40);
        e.advance(b"\x1b_Ga=T,U=1,i=9,f=24,s=10,v=20,c=2,r=1;QUJD\x1b\\");
        let screen = e.screen().clone();
        assert_eq!(screen.virtual_placements.len(), 1, "emulator captured virtual placement");

        // Surfaces with no overlay.
        let view = plain_view(&screen, Rect::new(0, 0, 8, 40));
        let vs = compose(&[view], (9, 40), None, StatusPlacement::Bottom, None, None, None, None, None, TEST_COLOR, ChromeColors::ansi_default());
        assert_eq!(vs.virtual_placements.len(), 1);
        assert_eq!(vs.virtual_placements[0].image_id, 9, "raw id kept (no host fold)");

        // Suppressed under a modal overlay.
        let view2 = plain_view(&screen, Rect::new(0, 0, 8, 40));
        let ov = OverlayView::RenamePrompt { label: "r", buf: "x" };
        let vs2 = compose(&[view2], (9, 40), None, StatusPlacement::Bottom, None, Some(&ov), None, None, None, TEST_COLOR, ChromeColors::ansi_default());
        assert!(vs2.virtual_placements.is_empty(), "overlay suppresses virtual placements");

        // Suppressed under a popup.
        let mut pe = Emulator::new(4, 20);
        pe.advance(b"p");
        let pv = PopupView { rect: Rect::new(2, 2, 4, 20), screen: pe.screen(), title: "p" };
        let view3 = plain_view(&screen, Rect::new(0, 0, 8, 40));
        let vs3 = compose(&[view3], (9, 40), None, StatusPlacement::Bottom, None, None, None, Some(&pv), None, TEST_COLOR, ChromeColors::ansi_default());
        assert!(vs3.virtual_placements.is_empty(), "popup suppresses virtual placements");

        // Suppressed on alt-screen.
        e.advance(b"\x1b[?1049h");
        let alt = e.screen().clone();
        let view4 = plain_view(&alt, Rect::new(0, 0, 8, 40));
        let vs4 = compose(&[view4], (9, 40), None, StatusPlacement::Bottom, None, None, None, None, None, TEST_COLOR, ChromeColors::ansi_default());
        assert!(vs4.virtual_placements.is_empty(), "alt-screen suppresses virtual placements");
    }

    #[test]
    fn two_panes_each_with_an_image_resolve_independently() {
        let s0 = screen_with_tall_image(8);
        let s1 = screen_with_tall_image(8);
        let v0 = PaneView { id: PaneId(1), ..plain_view(&s0, Rect::new(0, 0, 8, 20)) };
        let v1 = PaneView { id: PaneId(2), ..plain_view(&s1, Rect::new(0, 21, 8, 19)) };
        let vs = compose(&[v0, v1], (9, 41), None, StatusPlacement::Bottom, None, None, None, None, None, TEST_COLOR, ChromeColors::ansi_default());
        assert_eq!(vs.placements.len(), 2, "one placement per pane");
        assert_ne!(vs.placements[0].key, vs.placements[1].key, "distinct keys");
        assert_ne!(vs.placements[0].image_id, vs.placements[1].image_id, "distinct host ids");
        assert_eq!(vs.placements[1].host_col, 21, "second pane's column offset");
    }

    // ── Foundational snapshot tests (insta) ──────────────────────────────────

    #[test]
    fn snapshot_single_pane_wide_char() {
        // 'hello 世 ', which exercises a wide grapheme + its spacer in the dump.
        let mut e = Emulator::new(3, 12);
        pane(&mut e, "hello 世 ".as_bytes());
        let screen = e.screen().clone();
        let view = plain_view(&screen, Rect::new(0, 0, 3, 12));
        let vs = compose(
            &[view], (3, 12), None, StatusPlacement::Bottom, None, None, None, None, None,
            TEST_COLOR, ChromeColors::ansi_default(),
        );
        insta::assert_snapshot!(dump_frame(&vs, false));
    }

    #[test]
    fn snapshot_two_pane_split_border() {
        let mut left = Emulator::new(3, 4);
        let mut right = Emulator::new(3, 4);
        pane(&mut left, "L世 ".as_bytes());
        pane(&mut right, b"R ");
        let lv = PaneView {
            id: PaneId(0),
            is_active: false,
            ..plain_view(left.screen(), Rect::new(0, 0, 3, 4))
        };
        let rv = PaneView {
            id: PaneId(1),
            ..plain_view(right.screen(), Rect::new(0, 5, 3, 4))
        };
        let vs = compose(
            &[lv, rv], (3, 9), None, StatusPlacement::Bottom, None, None, None, None, None,
            TEST_COLOR, ChromeColors::ansi_default(),
        );
        insta::assert_snapshot!(dump_frame(&vs, false));
    }

    #[test]
    fn snapshot_status_bar_bottom() {
        // Emulator is 3 rows to match the pane rect; the status bar takes the
        // fourth host row.  top_visible = 0 so "body" lands at host row 0.
        let mut e = Emulator::new(3, 16);
        pane(&mut e, b"body ");
        let screen = e.screen().clone();
        let status = status_with_left("plexy");
        let view = plain_view(&screen, Rect::new(0, 0, 3, 16));
        let vs = compose(
            &[view], (4, 16), Some(&status), StatusPlacement::Bottom, None, None, None, None, None,
            TEST_COLOR, ChromeColors::ansi_default(),
        );
        insta::assert_snapshot!(dump_frame(&vs, false));
    }

    #[test]
    fn snapshot_status_bar_top() {
        // Emulator is 3 rows to match the pane rect; pane rect starts at
        // logical row 1 so a border row appears between status and content.
        let mut e = Emulator::new(3, 16);
        pane(&mut e, b"body ");
        let screen = e.screen().clone();
        let status = status_with_left("plexy");
        let view = plain_view(&screen, Rect::new(1, 0, 3, 16));
        let vs = compose(
            &[view], (4, 16), Some(&status), StatusPlacement::Top, None, None, None, None, None,
            TEST_COLOR, ChromeColors::ansi_default(),
        );
        insta::assert_snapshot!(dump_frame(&vs, false));
    }

    #[test]
    fn snapshot_block_mode_folds_collapsed() {
        let screen = two_block_fold_screen();
        let bm = crate::BlockMode {
            selected: 0,
            viewport_top: 0,
            pane_rows: 8,
            total_lines: 8,
            filter: None,
        };
        let view = PaneView { block_mode: Some(&bm), ..plain_view(&screen, Rect::new(0, 0, 8, 40)) };
        let vs = compose(
            &[view], (8, 40), None, StatusPlacement::Bottom, None, None, None, None, None,
            TEST_COLOR, ChromeColors::ansi_default(),
        );
        // attrs=true captures the DIM on the folded command row + `▸ N lines`
        // summary. That dimming is fold-driven (not block-mode-specific; the
        // live-view fold dims identically); block mode's own distinction is the
        // selected-block bracket, a color `dump_frame` has no channel for. The
        // attrs grid still makes this golden non-identical to the plain live-view
        // fold and guards the fold-dim integration.
        insta::assert_snapshot!(dump_frame(&vs, true));
    }

    #[test]
    fn snapshot_live_view_folded_block() {
        let screen = two_block_fold_screen();
        let view = plain_view(&screen, Rect::new(0, 0, 8, 40));
        let vs = compose(
            &[view], (8, 40), None, StatusPlacement::Bottom, None, None, None, None, None,
            TEST_COLOR, ChromeColors::ansi_default(),
        );
        insta::assert_snapshot!(dump_frame(&vs, false));
    }

    #[test]
    fn snapshot_overlay_help_box() {
        let mut e = Emulator::new(10, 40);
        pane(&mut e, b"body ");
        let screen = e.screen().clone();
        let view = plain_view(&screen, Rect::new(0, 0, 10, 40));
        let lines = vec![
            ("Ctrl+a c".to_string(), "New window".to_string()),
            ("Ctrl+a |".to_string(), "Split right".to_string()),
        ];
        let ov = OverlayView::Help { lines: &lines, scroll: 0 };
        let vs = compose(
            &[view], (10, 40), None, StatusPlacement::Bottom, None, Some(&ov), None, None, None,
            TEST_COLOR, ChromeColors::ansi_default(),
        );
        insta::assert_snapshot!(dump_frame(&vs, false));
    }

    #[test]
    fn snapshot_overlay_command_prompt() {
        let mut e = Emulator::new(6, 30);
        pane(&mut e, b"body ");
        let screen = e.screen().clone();
        let view = plain_view(&screen, Rect::new(0, 0, 6, 30));
        let ov = OverlayView::Command { buf: "split-window" };
        let vs = compose(
            &[view], (6, 30), None, StatusPlacement::Bottom, None, Some(&ov), None, None, None,
            TEST_COLOR, ChromeColors::ansi_default(),
        );
        insta::assert_snapshot!(dump_frame(&vs, false));
    }

    #[test]
    fn snapshot_popup_box() {
        let mut bg = Emulator::new(10, 40);
        pane(&mut bg, b"background ");
        let bg_screen = bg.screen().clone();
        let view = plain_view(&bg_screen, Rect::new(0, 0, 10, 40));

        let mut pe = Emulator::new(6, 20);
        pane(&mut pe, b"hi ");
        let pv = PopupView { rect: Rect::new(2, 10, 8, 22), screen: pe.screen(), title: "cat" };

        let vs = compose(
            &[view], (10, 40), None, StatusPlacement::Bottom, None, None, None, Some(&pv), None,
            TEST_COLOR, ChromeColors::ansi_default(),
        );
        insta::assert_snapshot!(dump_frame(&vs, false));
    }

    // ── attribute-aware snapshots (dump_frame(.., true)) ────────────────────

    /// Copy-mode selection: the selected "hello" span (cols 0-4, row 0) should
    /// carry R (REVERSE) marks in the attribute grid.
    #[test]
    fn snapshot_copy_mode_selection() {
        let mut e = Emulator::new(5, 20);
        pane(&mut e, b"hello world ");
        let screen = e.screen().clone();
        let cm = crate::CopyMode {
            cursor: (0, 4),
            anchor: Some((0, 0)),
            search: crate::SearchState::default(),
            viewport_top: 0,
            pane_rows: 5,
            total_lines: 5,
        };
        let view = PaneView { copy_mode: Some(&cm), ..plain_view(&screen, Rect::new(0, 0, 5, 20)) };
        let vs = compose(
            &[view], (5, 20), None, StatusPlacement::Bottom, None, None, None, None, None,
            TEST_COLOR, ChromeColors::ansi_default(),
        );
        // attrs=true: the selected span 'hello' on row 0 shows R marks.
        insta::assert_snapshot!(dump_frame(&vs, true));
    }

    /// Copy-mode search highlight: the 'ell' match (cols 1-3, row 0) should
    /// carry H (HIGHLIGHT) marks in the attribute grid.
    #[test]
    fn snapshot_copy_mode_search_highlight() {
        let mut e = Emulator::new(5, 20);
        pane(&mut e, b"hello world ");
        let screen = e.screen().clone();
        let cm = crate::CopyMode {
            cursor: (0, 0),
            anchor: None,
            search: crate::SearchState {
                query: "ell".into(),
                matches: vec![crate::MatchSpan { line_idx: 0, col_start: 1, col_end: 3 }],
                current: 0,
                prompt_active: false,
                prompt_buf: String::new(),
            },
            viewport_top: 0,
            pane_rows: 5,
            total_lines: 5,
        };
        let view = PaneView { copy_mode: Some(&cm), ..plain_view(&screen, Rect::new(0, 0, 5, 20)) };
        let vs = compose(
            &[view], (5, 20), None, StatusPlacement::Bottom, None, None, None, None, None,
            TEST_COLOR, ChromeColors::ansi_default(),
        );
        // attrs=true: the 'ell' match (cols 1-3) on row 0 shows H marks.
        insta::assert_snapshot!(dump_frame(&vs, true));
    }

    /// Sticky-header: when scrolled back one line the command is pinned as a
    /// dim (not reverse) header on row 0. The attribute grid should show D marks.
    #[test]
    fn snapshot_sticky_header_dim() {
        let screen = tall_block_screen();
        let colors = block_colors_with(None, true);
        let view = scrolled_view(&screen, Rect::new(0, 0, 3, 40), 1);
        let vs = compose(
            &[view], (3, 40), None, StatusPlacement::Bottom, None, None, None, None, Some(&colors),
            TEST_COLOR, ChromeColors::ansi_default(),
        );
        // attrs=true: row 0 shows the pinned command text with D (dim) marks, not R.
        insta::assert_snapshot!(dump_frame(&vs, true));
    }

    /// Block-status border: the left border column (col 0) shows the half-block
    /// `▐` on every block row. Both ok and failed blocks use `▐`, and the ok/fail
    /// distinction is carried by COLOR (`colors.ok`/`colors.fail`), which a text
    /// dump cannot capture (that color split is unit-tested in `borders.rs`). This
    /// snapshot guards the integration-level "border band is drawn on block rows."
    /// The pane starts at col 1 so col 0 is the border band.
    #[test]
    fn snapshot_block_status_border() {
        use plexy_glass_emulator::Emulator as RawEmulator;
        // One ok block (exit 0) then one failed block (exit 1).
        let mut e = RawEmulator::new(6, 29);
        e.advance(
            b"\x1b]133;A\x07$ ok\r\n\x1b]133;C\x07done\r\n\x1b]133;D;0\x07\
              \x1b]133;A\x07$ bad\r\n\x1b]133;C\x07boom\r\n\x1b]133;D;1\x07",
        );
        e.advance(b"\x1b[m");
        let screen = e.screen().clone();
        let colors = block_colors();
        // Pane at col 1 so the left border occupies col 0.
        let view = plain_view(&screen, Rect::new(0, 1, 6, 29));
        let vs = compose(
            &[view], (6, 30), None, StatusPlacement::Bottom, None, None, None, None, Some(&colors),
            TEST_COLOR, ChromeColors::ansi_default(),
        );
        insta::assert_snapshot!(dump_frame(&vs, false));
    }

    // ── Pure helper unit tests ────────────────────────────────────────────────

    #[test]
    fn fold_ctx_display_row_excludes_row_at_viewport_boundary() {
        // `display_row` returns None when `r == rows` (one past the last valid row).
        // The `< → <=` mutation at line 78 would return Some(rows) instead of None,
        // causing an out-of-bounds paint in the compositor.
        let proj = crate::blocks::FoldProjection::identity(10);
        let ctx = FoldCtx { proj, top_visible: 2 };
        // Line at unified 4 → visible 4, r = 4 - 2 = 2.
        assert_eq!(ctx.display_row(4, 3), Some(2), "r=2 < rows=3 → in viewport");
        // r == rows: must return None (out of viewport).
        assert_eq!(ctx.display_row(4, 2), None, "r=2 == rows=2 → out of viewport");
    }

    #[test]
    fn crop_axis_no_offset_full_visible_is_exact() {
        // The fast path `off == 0 && vis >= cells` must return (0, pixels) exactly.
        // With `== → !=`, the fast path fires when off != 0 instead: a non-zero
        // offset would return (0, pixels) (wrong crop).
        assert_eq!(crop_axis(100, 10, 0, 10), (0, 100)); // full extent
        assert_eq!(crop_axis(100, 10, 0, 8), (0, 80));   // off=0 but not fully visible
        // non-zero offset must NOT take the fast path:
        let (start, extent) = crop_axis(100, 10, 2, 6);
        assert_eq!(start, 20, "off=2 → start at 20% of 100");
        assert_eq!(extent, 60, "6 of 10 cells → 60% of 100");
    }

    #[test]
    fn cells_width_sums_widths_correctly() {
        use smol_str::SmolStr;
        use plexy_glass_status::ResolvedStyle;
        let style = ResolvedStyle::default();
        // Kills `replace cells_width with 0` and `with 1` (both wrong return values).
        let cells: Vec<StatusCell> = vec![
            (SmolStr::new("a"), 1, style),
            (SmolStr::new("好"), 2, style),
            (SmolStr::new("b"), 1, style),
        ];
        assert_eq!(cells_width(&cells), 4);
        assert_eq!(cells_width(&[]), 0);
    }

    #[test]
    fn truncate_cells_cuts_at_max_width() {
        use smol_str::SmolStr;
        use plexy_glass_status::ResolvedStyle;
        let style = ResolvedStyle::default();
        let make = |g: &str, w: u16| -> StatusCell { (SmolStr::new(g), w, style) };
        // Three cells: widths 1, 2, 1 → total 4. Truncate to 3 keeps the first two.
        // `> → ==` mutation: keeps cells even when used > max (allows overflow).
        // `+ → *` mutation: `used * w > max` gives wildly wrong threshold.
        // `+= → *=` mutation: used *= w instead of accumulating.
        let cells = vec![make("a", 1), make("好", 2), make("b", 1)];
        let result = truncate_cells(cells.clone(), 3);
        assert_eq!(result.len(), 2, "max_w=3 should keep 'a'(1) and '好'(2), drop 'b'");
        assert_eq!(result[0].0.as_str(), "a");
        assert_eq!(result[1].0.as_str(), "好");
        // Exact fit: max_w = 4 keeps all three.
        assert_eq!(truncate_cells(cells, 4).len(), 3);
    }

    #[test]
    fn filter_match_spans_returns_correct_columns() {
        // Verifies that grid_col tracking is correct (kills `+= → *=` and `+= → -=`
        // mutations in filter_match_spans, and `+ → *` in the col_end calculation).
        let mut e = Emulator::new(3, 20);
        e.advance(b"hello world ");
        let screen = e.screen().clone();
        // "world" starts at col 6 in the grid (h-e-l-l-o-space = 6 cells).
        let spans = filter_match_spans(&screen, "world", 0, 3);
        assert!(!spans.is_empty(), "should find 'world' in the screen");
        let (_, col_start, _) = spans[0];
        assert_eq!(col_start, 6, "col_start for 'world' must be 6");
    }

    #[test]
    fn filter_match_spans_tracks_wide_char_spacers() {
        // A wide char (好, 2 cells) followed by ASCII. The wide spacer increments
        // grid_col; with `+= → -=` or `+= → *=` at line 1000, the spacer's column
        // increment is wrong and subsequent columns are off by 1.
        let mut e = Emulator::new(3, 20);
        // "好" is 3 bytes / 2 display cols. The parser needs a trailing byte to flush.
        e.advance("好ab ".as_bytes());
        let screen = e.screen().clone();
        // "ab" starts at col 2 (wide char takes cols 0-1).
        let spans = filter_match_spans(&screen, "ab", 0, 3);
        assert!(!spans.is_empty(), "should find 'ab' in the screen");
        let (_, col_start, _) = spans[0];
        assert_eq!(col_start, 2, "col_start for 'ab' must be 2 (after the 2-col wide char)");
    }

    // Equivalent note: `effective_scroll_for` line 965 `+ → *` (scrollback * active
    // instead of +): real gap, no test exercises the copy/block-mode branch with
    // a scrollback-containing screen, so the wrong total_lines value goes undetected.
    // Gap left as-is (would require setting up a full PaneView with scrollback).
    //
    // Equivalent note: `crop_axis` line 942 `== → !=`: EQUIVALENT. When off != 0,
    // vis = cells - off < cells, so `vis >= cells` is false and the fast path still
    // does not fire. When off == 0 the general path produces the same result as the
    // fast path (0, pixels). Both branches are observationally identical in all
    // reachable inputs.

    #[test]
    fn display_row_returns_none_at_viewport_boundary() {
        // Kills: 78:12 `< → <=`: with `<=`, display_row(rows, rows) incorrectly
        // returns Some(rows) instead of None for a row exactly at the viewport height.
        let proj = FoldProjection::identity(10);
        let ctx = FoldCtx { proj, top_visible: 0 };
        assert_eq!(ctx.display_row(4, 5), Some(4), "last valid row must be Some");
        assert_eq!(ctx.display_row(5, 5), None, "row == rows must be None");
    }

    #[test]
    fn put_char_preserves_attrs() {
        // Kills: 1837:9 `delete field attrs`. Without it, all put_char cells get
        // Attrs::empty(), losing bold/italic/dim styling.
        use plexy_glass_emulator::Attrs;
        let mut screen = VirtualScreen::blank(5, 20);
        put_char(&mut screen, 0, 0, 'X', Attrs::BOLD);
        let cell = screen.cell(0, 0).expect("cell must exist");
        assert!(cell.attrs.contains(Attrs::BOLD), "put_char must pass attrs through to the cell");
    }

    #[test]
    fn cell_for_preserves_attrs() {
        // Kills: 1986:9 `delete field attrs`. cell_for must copy style.attrs.
        use plexy_glass_emulator::Attrs;
        use plexy_glass_status::ResolvedStyle;
        let style = ResolvedStyle { attrs: Attrs::BOLD, fg: None, bg: None };
        let cell = cell_for(&smol_str::SmolStr::new("X"), &style);
        assert!(
            cell.attrs.contains(Attrs::BOLD),
            "cell_for must copy attrs from the resolved style"
        );
    }
}
