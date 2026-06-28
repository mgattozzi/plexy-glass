use super::{Severity, WindowManager};
use crate::error::DaemonError;
use plexy_glass_mux::{
    BorderHit, BorderSide, Command, MouseButton, MouseEncoding, MouseEvent, MouseKind, PaneId,
    Rect, Selection, WindowId, encode_for_child, extract_text, prev_prompt_line,
};
use std::time::Instant;

/// Active border drag-resize. Cleared on Release. While `Some`, all mouse
/// events go to `handle_resize_drag_event`.
pub(super) struct ResizeDrag {
    adjacent_pane: PaneId,
    side: BorderSide,
    last_pos: (u16, u16),
}

/// Active tab reorder drag. While `Some`, every mouse event routes to
/// `handle_tab_drag_event`; cleared on Release. `source` is the dragged
/// window's id (re-resolved to its current index on drop).
pub(super) struct TabDrag {
    pub(super) source: WindowId,
}

/// Active pane-swap drag. While `Some`, every mouse event routes to
/// `handle_pane_drag_event`; cleared on Release. `source` is the dragged
/// pane; `target` is the pane under the cursor (updated on Move, for the
/// highlight). Both re-resolved at drop via `swap_panes`.
pub(super) struct PaneDrag {
    pub(super) source: PaneId,
    pub(super) target: Option<PaneId>,
}

/// Last left-press metadata for multi-click classification (double-click =
/// Word, triple-click = Line). Resets when the click target changes or the
/// 400ms window expires.
pub(super) struct ClickHistory {
    pane: PaneId,
    row: u16,
    col: u16,
    button: MouseButton,
    at: Instant,
    count: u8,
}

impl WindowManager {
    /// Dispatch one decoded mouse event through the precedence ladder
    /// (Rule 0: modal popup, see docs/superpowers/specs/2026-06-09-popup-panes-design.md;
    /// then docs/superpowers/specs/2026-05-22-full-mouse-design.md §6).
    pub async fn handle_mouse(&mut self, event: MouseEvent) -> Result<(), DaemonError> {
        // Rule 0: a floating popup owns the mouse entirely while open. A click
        // in the box interior is forwarded to the child (translated to interior
        // coordinates) when it enabled mouse reporting; everything else (border,
        // outside, status bar) is swallowed. Modal by design.
        if let Some(popup) = self.popup.as_ref() {
            let event = self.to_pane_coords(event);
            let rect = plexy_glass_mux::popup_rect(self.viewport());
            let interior = rect.rows >= 3
                && rect.cols >= 3
                && event.row > rect.row
                && event.row < rect.row + rect.rows - 1
                && event.col > rect.col
                && event.col < rect.col + rect.cols - 1;
            if !interior {
                return Ok(());
            }
            if !popup.pane.with_screen(|s| s.modes.any_mouse_mode_active()) {
                return Ok(());
            }
            let mut local = event;
            local.row = event.row - rect.row - 1;
            local.col = event.col - rect.col - 1;
            let encoding = popup.pane.with_screen(|s| mouse_encoding_for(s.modes));
            let bytes = encode_for_child(local, encoding);
            let pane = popup.pane.clone();
            let _ = pane.send_input(bytes::Bytes::from(bytes)).await;
            return Ok(());
        }
        // Rule 1a: tab-drag modal (physical coords, since the pointer may leave
        // the status row mid-drag). Takes precedence over the status-bar hit so a
        // drag that wanders off the tab strip still routes to the drag handler.
        if self.tab_drag.is_some() {
            return self.handle_tab_drag_event(event).await;
        }
        // Rule 2 (first): status-bar row hit. The bar lives outside the pane
        // band, so test it against the *physical* row before translating. A
        // drag in progress still consumes everything, including moves that
        // stray onto the status row.
        if self.resize_drag.is_none() && self.is_status_bar_row(event.row) {
            return self.handle_status_bar_event(event).await;
        }
        // Everything below addresses panes/borders, which live in the layout's
        // logical coordinate space. Translate away the status-bar offset (1 row
        // when the bar is on top; 0 otherwise, leaving bottom placement byte
        // for byte unchanged).
        let event = self.to_pane_coords(event);
        // Pane-swap drag in progress consumes everything (pane-logical coords).
        if self.pane_drag.is_some() {
            return self.handle_pane_drag_event(event).await;
        }
        // Rule 1: resize-drag in progress consumes everything until release.
        if self.resize_drag.is_some() {
            return self.handle_resize_drag_event(event).await;
        }
        // Rule 3: border hit on left press.
        if matches!(event.kind, MouseKind::Press)
            && event.button == MouseButton::Left
            && let Some(hit) = self.layout_border_at(event.row, event.col)
        {
            self.resize_drag = Some(ResizeDrag {
                adjacent_pane: hit.adjacent_pane,
                side: hit.side,
                last_pos: (event.row, event.col),
            });
            return Ok(());
        }
        // Rule 4: copy-mode pane.
        let viewport = self.viewport();
        let Some(pane_id) = self
            .active_window()
            .layout()
            .pane_at_coord(viewport, event.row, event.col)
        else {
            return Ok(());
        };
        // Drag-modifier + left-press starts a pane-swap drag. Placed ahead of
        // copy-mode, focus-only, and child-mouse forwarding so a pane running an
        // interactive (mouse-mode) program can still be swapped.
        if matches!(event.kind, MouseKind::Press) && event.button == MouseButton::Left {
            let drag_held = match self.config.mouse.drag_modifier {
                plexy_glass_config::DragModifier::Alt => event.modifiers.alt,
                plexy_glass_config::DragModifier::Ctrl => event.modifiers.ctrl,
            };
            if drag_held {
                self.pane_drag = Some(PaneDrag { source: pane_id, target: None });
                self.notify.notify_one();
                return Ok(());
            }
        }
        if self.pane_is_in_copy_mode(pane_id) {
            return self.handle_copy_mode_mouse(pane_id, event).await;
        }
        // Rule 4.5: a left-press on a *non-active* pane focuses it and is
        // consumed, even when the pane's app has mouse mode on. Without this,
        // panes running mouse-reporting apps (less, hx, TUIs) would forward the
        // click via Rule 5 and never become focusable. Mirrors the focus-only
        // behavior `handle_left_press` gives plain panes.
        if matches!(event.kind, MouseKind::Press)
            && event.button == MouseButton::Left
            && pane_id != self.active_window().active()
        {
            self.active_window_mut().focus(pane_id);
            self.notify.notify_one();
            return Ok(());
        }
        // Rule 5: pane has child-app mouse-mode on → passthrough.
        if self.pane_has_any_mouse_mode(pane_id) {
            return self.forward_mouse_to_pane(pane_id, event).await;
        }
        // Rule 6: default daemon handlers.
        self.handle_default_mouse(pane_id, event, viewport).await
    }

    // ----- Precedence-ladder helpers -----

    fn is_status_bar_row(&self, row: u16) -> bool {
        self.status_bar_row == Some(row)
    }

    /// Translate a physical mouse event into the layout's logical pane
    /// coordinates by removing the status-bar offset. A no-op when the bar is
    /// at the bottom (offset 0).
    fn to_pane_coords(&self, mut event: MouseEvent) -> MouseEvent {
        event.row = event.row.saturating_sub(self.pane_row_offset);
        event
    }

    async fn handle_status_bar_event(
        &mut self,
        event: MouseEvent,
    ) -> Result<(), DaemonError> {
        if !matches!(event.kind, MouseKind::Press) || event.button != MouseButton::Left {
            return Ok(());
        }
        let Some(hit) = self
            .status_hits
            .iter()
            .find(|h| h.col_range.contains(&event.col))
            .cloned()
        else {
            return Ok(());
        };
        use plexy_glass_status::ClickAction;
        match hit.action {
            ClickAction::SelectWindow(idx) => {
                let reorder_held = match self.config.mouse.drag_modifier {
                    plexy_glass_config::DragModifier::Alt => event.modifiers.alt,
                    plexy_glass_config::DragModifier::Ctrl => event.modifiers.ctrl,
                };
                if reorder_held {
                    // Begin a tab-drag; the modal rule above takes the rest.
                    let source = self.windows.get(idx).map(|w| w.id);
                    if let Some(source) = source {
                        self.tab_drag = Some(TabDrag { source });
                        self.notify.notify_one();
                    }
                } else {
                    // SelectWindow takes u8; clamp on overflow (unlikely with
                    // realistic window counts).
                    let n = u8::try_from(idx).unwrap_or(u8::MAX);
                    self.handle_command(Command::SelectWindow(n))?;
                }
            }
            ClickAction::ToggleSyncPanes => {
                self.handle_command(Command::ToggleSyncPanes)?;
            }
            ClickAction::ExitCopyMode => {
                if let Some(pane) = self.active_window().active_pane().cloned() {
                    pane.exit_copy_mode();
                }
                self.notify.notify_one();
            }
            ClickAction::Detach => {
                self.detach_requested = true;
                self.notify.notify_one();
            }
            ClickAction::NoOp => {}
        }
        Ok(())
    }

    fn layout_border_at(&self, row: u16, col: u16) -> Option<BorderHit> {
        self.active_window()
            .layout()
            .border_at(self.viewport(), row, col)
    }

    async fn handle_resize_drag_event(
        &mut self,
        event: MouseEvent,
    ) -> Result<(), DaemonError> {
        let Some(drag) = self.resize_drag.as_mut() else {
            return Ok(());
        };
        match event.kind {
            MouseKind::Move => {
                let delta = match drag.side {
                    BorderSide::Right => event.col as i16 - drag.last_pos.1 as i16,
                    BorderSide::Bottom => event.row as i16 - drag.last_pos.0 as i16,
                };
                if delta == 0 {
                    return Ok(());
                }
                let pane = drag.adjacent_pane;
                let side = drag.side;
                let viewport = self.viewport();
                let applied = self
                    .active_window_mut()
                    .layout_mut()
                    .adjust_split(pane, side, delta, viewport);
                if applied != 0 {
                    // Step last_pos by the actually-applied delta so we don't
                    // accumulate slip when the drag bottoms out at min-size.
                    let drag = self.resize_drag.as_mut().expect("just held above");
                    match side {
                        BorderSide::Right => {
                            drag.last_pos.1 = (drag.last_pos.1 as i16 + applied) as u16;
                        }
                        BorderSide::Bottom => {
                            drag.last_pos.0 = (drag.last_pos.0 as i16 + applied) as u16;
                        }
                    }
                    self.active_window_mut().resize(viewport)?;
                    self.notify.notify_one();
                }
                Ok(())
            }
            MouseKind::Release => {
                self.resize_drag = None;
                let viewport = self.viewport();
                self.active_window_mut().resize(viewport)?;
                self.notify.notify_one();
                Ok(())
            }
            _ => Ok(()),
        }
    }

    async fn handle_pane_drag_event(&mut self, event: MouseEvent) -> Result<(), DaemonError> {
        match event.kind {
            MouseKind::Move => {
                let target = self.drop_target_pane(event.row, event.col);
                if let Some(drag) = self.pane_drag.as_mut() {
                    drag.target = target;
                }
                self.notify.notify_one();
                Ok(())
            }
            MouseKind::Release => {
                let Some(drag) = self.pane_drag.take() else {
                    return Ok(());
                };
                if let Some(target) = self.drop_target_pane(event.row, event.col)
                    && target != drag.source
                {
                    let viewport = self.viewport();
                    let w = self.active_window_mut();
                    if w.layout_mut().swap_panes(drag.source, target) {
                        w.focus(drag.source);
                        w.resize(viewport)?;
                    }
                }
                self.notify.notify_one();
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// The pane under physical-translated `(row, col)`, or `None` if off any
    /// pane. Used for the drag's live target and the drop.
    fn drop_target_pane(&self, row: u16, col: u16) -> Option<PaneId> {
        self.active_window()
            .layout()
            .pane_at_coord(self.viewport(), row, col)
    }

    async fn handle_tab_drag_event(&mut self, event: MouseEvent) -> Result<(), DaemonError> {
        if !matches!(event.kind, MouseKind::Release) {
            // Move / stray press mid-drag: nothing live to update (the
            // highlight is static for drop-to-position).
            return Ok(());
        }
        let Some(drag) = self.tab_drag.take() else {
            return Ok(());
        };
        if let Some(to) = self.drop_target_at(event.row, event.col) {
            self.move_window_by_id(drag.source, to);
        }
        self.notify.notify_one();
        Ok(())
    }

    /// Resolve the drop slot for a tab-drag release at physical `(row, col)`:
    /// the tab under the cursor, the end if released right of all tabs, or
    /// `None` (abort) if off the status row or on a non-tab segment.
    fn drop_target_at(&self, row: u16, col: u16) -> Option<usize> {
        use plexy_glass_status::ClickAction;
        if !self.is_status_bar_row(row) {
            return None;
        }
        if let Some(hit) = self.status_hits.iter().find(|h| h.col_range.contains(&col)) {
            return match hit.action {
                ClickAction::SelectWindow(t) => Some(t),
                _ => None,
            };
        }
        // No hit under the cursor: if right of every tab, drop at the end.
        let max_tab_end = self
            .status_hits
            .iter()
            .filter_map(|h| match h.action {
                ClickAction::SelectWindow(_) => Some(h.col_range.end),
                _ => None,
            })
            .max();
        match max_tab_end {
            Some(end) if col >= end => Some(self.windows.len()),
            _ => None,
        }
    }

    fn pane_is_in_copy_mode(&self, pane: PaneId) -> bool {
        self.active_window()
            .pane(pane)
            .map(|p| p.is_in_copy_mode())
            .unwrap_or(false)
    }

    async fn handle_copy_mode_mouse(
        &mut self,
        pane_id: PaneId,
        event: MouseEvent,
    ) -> Result<(), DaemonError> {
        let click_count = self.classify_click_count(pane_id, &event);
        // CopyMode::handle_mouse treats the event as pane-local 0-based, so the
        // viewport-space event must be translated to the pane rect origin first,
        // same class as forward_mouse_to_pane / handle_left_press. Without it,
        // a copy-mode click in any pane (origin >= (1,1), more when split) lands
        // off by the rect origin.
        let viewport = self.viewport();
        let rect = self
            .active_window()
            .layout()
            .rect_of(pane_id, viewport)
            .unwrap_or(viewport);
        let mut local = event;
        local.row = event.row.saturating_sub(rect.row);
        local.col = event.col.saturating_sub(rect.col);
        let Some(pane) = self.active_window().pane(pane_id).cloned() else {
            return Ok(());
        };
        // The handler mutates copy-mode state; we need both with_screen + with_copy_mode_mut.
        let action: Option<plexy_glass_mux::CopyModeAction> = pane.with_screen(|screen| {
            pane.with_copy_mode_mut(|cm| cm.handle_mouse(&local, click_count, screen))
        });
        if let Some(action) = action {
            use plexy_glass_mux::CopyModeAction;
            match action {
                CopyModeAction::Render => self.notify.notify_one(),
                CopyModeAction::Exit => {
                    pane.exit_copy_mode();
                    self.notify.notify_one();
                }
                CopyModeAction::Yank(text) => {
                    let msg = crate::osc_actions::copied_message(&text);
                    tokio::spawn(async move {
                        let _ = crate::osc_actions::write_clipboard(text.as_bytes()).await;
                    });
                    pane.exit_copy_mode();
                    self.set_status_message(msg, Severity::Success);
                    self.notify.notify_one();
                }
            }
        }
        Ok(())
    }

    /// Classify the current left-press as count=1/2/3 based on time + target
    /// match against `click_history`. Updates `click_history` and returns
    /// the new count. Non-left-press events return 1 without updating.
    pub(super) fn classify_click_count(&mut self, pane: PaneId, event: &MouseEvent) -> u8 {
        if !matches!(event.kind, MouseKind::Press) || event.button != MouseButton::Left {
            return 1;
        }
        let now = Instant::now();
        let same_target = match &self.click_history {
            Some(h) => {
                h.pane == pane
                    && h.row == event.row
                    && h.col == event.col
                    && h.button == MouseButton::Left
                    && now.saturating_duration_since(h.at)
                        < std::time::Duration::from_millis(400)
            }
            None => false,
        };
        let count = if same_target {
            self.click_history
                .as_ref()
                .map(|h| h.count.saturating_add(1).min(3))
                .unwrap_or(1)
        } else {
            1
        };
        self.click_history = Some(ClickHistory {
            pane,
            row: event.row,
            col: event.col,
            button: MouseButton::Left,
            at: now,
            count,
        });
        count
    }

    pub(super) fn pane_has_any_mouse_mode(&self, pane_id: PaneId) -> bool {
        self.active_window()
            .pane(pane_id)
            .map(|p| p.with_screen(|s| s.modes.any_mouse_mode_active()))
            .unwrap_or(false)
    }

    async fn forward_mouse_to_pane(
        &mut self,
        pane_id: PaneId,
        event: MouseEvent,
    ) -> Result<(), DaemonError> {
        // The event is in viewport (layout) coordinates, but the child's grid is
        // its own, starting at the pane rect origin, not the viewport's.
        // Translate before encoding (the popup path and `handle_left_press` do
        // the same) so a click in a split or frame-inset pane reports the right
        // cell to the child: without this, a mouse-reporting TUI (editor, pager,
        // Claude Code's click-to-move-cursor, …) targets a column/row offset by
        // the pane's position and the click misses.
        let viewport = self.viewport();
        let rect = self
            .active_window()
            .layout()
            .rect_of(pane_id, viewport)
            .unwrap_or(viewport);
        if let Some(pane) = self.active_window().pane(pane_id).cloned() {
            let mut local = event;
            local.row = event.row.saturating_sub(rect.row);
            local.col = event.col.saturating_sub(rect.col);
            let encoding = pane.with_screen(|s| mouse_encoding_for(s.modes));
            let bytes = encode_for_child(local, encoding);
            let _ = pane.send_input(bytes::Bytes::from(bytes)).await;
        }
        Ok(())
    }

    async fn handle_default_mouse(
        &mut self,
        pane_id: PaneId,
        event: MouseEvent,
        viewport: Rect,
    ) -> Result<(), DaemonError> {
        match event.kind {
            MouseKind::Press if event.button == MouseButton::Left => {
                self.handle_left_press(pane_id, event, viewport).await?;
            }
            MouseKind::Release if event.button == MouseButton::Left => {
                self.handle_left_release().await?;
            }
            MouseKind::Move if event.button == MouseButton::Left => {
                self.handle_left_drag(event, viewport);
            }
            MouseKind::Press if event.button == MouseButton::Middle => {
                self.handle_middle_press(pane_id).await?;
            }
            // Only a vertical wheel scrolls scrollback; a horizontal wheel on a
            // non-mouse-mode pane is ignored (mouse-mode panes get it forwarded
            // verbatim via Rule 5 / encode_for_child).
            MouseKind::Wheel { delta, horizontal: false } => {
                self.handle_wheel(pane_id, delta);
            }
            _ => {}
        }
        self.notify.notify_one();
        Ok(())
    }

    /// Middle-click pastes from the system clipboard. Bracketed-paste-aware:
    /// if the active pane's emulator has `Modes::BRACKETED_PASTE` on, the
    /// pasted bytes are wrapped with `\x1b[200~ ... \x1b[201~` so inner apps
    /// can distinguish paste from typed input.
    async fn handle_middle_press(&mut self, pane_id: PaneId) -> Result<(), DaemonError> {
        let bytes = crate::osc_actions::read_clipboard().await;
        if bytes.is_empty() {
            return Ok(());
        }
        let bracketed = self
            .active_window()
            .pane(pane_id)
            .map(|p| {
                p.with_screen(|s| {
                    s.modes
                        .contains(plexy_glass_emulator::Modes::BRACKETED_PASTE)
                })
            })
            .unwrap_or(false);
        let to_send = if bracketed {
            let mut v = Vec::with_capacity(bytes.len() + 12);
            v.extend_from_slice(b"\x1b[200~");
            v.extend_from_slice(&bytes);
            v.extend_from_slice(b"\x1b[201~");
            v
        } else {
            bytes
        };
        if let Some(pane) = self.active_window().pane(pane_id).cloned() {
            let _ = pane.send_input(bytes::Bytes::from(to_send)).await;
        }
        Ok(())
    }

    async fn handle_left_press(
        &mut self,
        pane_id: PaneId,
        event: MouseEvent,
        viewport: Rect,
    ) -> Result<(), DaemonError> {
        // Click in a non-active pane → focus only.
        if pane_id != self.active_window().active() {
            self.active_window_mut().focus(pane_id);
            return Ok(());
        }

        let pane_rect = self
            .active_window()
            .layout()
            .rect_of(pane_id, viewport)
            .unwrap_or(viewport);
        let local_row = event.row.saturating_sub(pane_rect.row);
        let local_col = event.col.saturating_sub(pane_rect.col);

        // Shift+left-click EXTENDS the existing selection in this pane
        // instead of starting a new one.
        if event.modifiers.shift
            && self
                .selection
                .as_ref()
                .map(|s| s.source_pane == pane_id)
                .unwrap_or(false)
        {
            if let Some(sel) = self.selection.as_mut() {
                sel.extend(local_row, local_col, pane_rect);
            }
            return Ok(());
        }

        // Block-aware prompt jump: plain (unmodified) left press on a scrolled
        // viewport, clicking a row that maps to a PROMPT_START absolute line →
        // snap that prompt to the viewport top. Inserted AFTER shift+click
        // selection-extend (extends across prompt rows correctly) and BEFORE
        // the hyperlink lookup and click_to_position (both are scroll-unaware
        // and read the live grid regardless of the displayed scrollback).
        if event.modifiers == plexy_glass_mux::MouseModifiers::default()
            && let Some(pane) = self.active_window().pane(pane_id)
        {
            let scroll_offset = pane.scroll_offset();
            if scroll_offset > 0 {
                let jumped = pane.with_screen(|s| {
                    // Fold-aware: map the clicked display row to its unified line
                    // through the visible-space projection.
                    let rows = s.active.num_rows();
                    let abs_line =
                        plexy_glass_mux::blocks::scroll_line_at(s, rows, scroll_offset, local_row);
                    // is_prompt is private; use the public prev_prompt_line:
                    // prev_prompt_line(s, abs_line + 1) == Some(abs_line) iff
                    // abs_line itself carries PROMPT_START.
                    let is_prompt =
                        prev_prompt_line(s, abs_line.saturating_add(1)) == Some(abs_line);
                    if is_prompt {
                        // Put the prompt at the viewport top (visible-space offset;
                        // a line already in the live view saturates to 0).
                        let new_offset =
                            plexy_glass_mux::blocks::scroll_offset_for_top(s, rows, abs_line);
                        let max = plexy_glass_mux::blocks::max_scroll_offset(s, rows);
                        Some((new_offset, max))
                    } else {
                        None
                    }
                });
                if let Some((new_offset, max)) = jumped {
                    pane.set_scroll_offset(new_offset, max);
                    self.notify.notify_one();
                    return Ok(());
                }
            }
        }

        // OSC 8 hyperlink under the cell? Open in the OS browser; suppress
        // selection start. Read through the pane's scroll position so a click
        // made while scrolled back opens the link shown, not the live grid's.
        let url = self.active_window().pane(pane_id).and_then(|p| {
            let off = p.scroll_offset();
            p.with_screen(|s| {
                plexy_glass_mux::viewport_content_row(s, s.active.num_rows(), off, local_row)
                    .and_then(|row| {
                        // A click on a wide grapheme's spacer half (the glyph's
                        // right cell) must read the hyperlink off the owning
                        // grapheme cell, which holds the id.
                        let mut c = local_col as usize;
                        if c > 0 && row.cells.get(c).is_some_and(|cell| cell.is_wide_spacer()) {
                            c -= 1;
                        }
                        row.cells.get(c)
                    })
                    .and_then(|cell| cell.hyperlink_id)
                    .and_then(|id| s.hyperlinks.get(id).map(str::to_owned))
            })
        });
        if let Some(url) = url {
            tokio::spawn(async move {
                let _ = crate::osc_actions::open_url(&url).await;
            });
            return Ok(());
        }

        // NOTE: cursor-click-to-move (click_to_position) is handled on RELEASE,
        // not here (see `handle_left_release`). Firing on the press would
        // consume the event before a drag could turn it into a selection, so
        // you could never drag-select the prompt line. A plain click (press +
        // release with no intervening drag) leaves an empty selection, which
        // release detects and repositions the cursor for.

        // Multi-click classification: double = Word, triple = Line. Word/line
        // boundaries are read through the pane's scroll position so a click made
        // while scrolled back targets the scrollback, not the live grid.
        let count = self.classify_click_count(pane_id, &event);
        let new_sel = self.active_window().pane(pane_id).and_then(|p| {
            let off = p.scroll_offset();
            if count >= 3 {
                p.with_screen(|s| {
                    plexy_glass_mux::line_at(pane_id, s, s.active.num_rows(), off, local_row)
                })
            } else if count == 2 {
                p.with_screen(|s| {
                    plexy_glass_mux::word_at(
                        pane_id,
                        s,
                        s.active.num_rows(),
                        off,
                        local_row,
                        local_col,
                    )
                })
            } else {
                None
            }
        });
        // A word/line selection (double/triple-click) is an explicit selection,
        // never a reposition, so exempt it from the click dead-zone at release.
        self.selection_word_line = new_sel.is_some();
        self.selection = new_sel.or_else(|| Some(Selection::start(pane_id, local_row, local_col)));
        // Remember whether the press landed on the live (unscrolled) view, so a
        // later release can tell a click-to-reposition (anchor in live-grid
        // space) from a click on scrollback that was wheeled to the bottom.
        self.selection_press_scroll = self
            .active_window()
            .pane(pane_id)
            .map(|p| p.scroll_offset())
            .unwrap_or(0);
        Ok(())
    }

    fn handle_left_drag(&mut self, event: MouseEvent, viewport: Rect) {
        let Some(source_pane) = self.selection.as_ref().map(|s| s.source_pane) else {
            return;
        };
        let Some(pane_rect) = self.active_window().layout().rect_of(source_pane, viewport) else {
            return;
        };
        let local_row = event.row.saturating_sub(pane_rect.row);
        let local_col = event.col.saturating_sub(pane_rect.col);
        if let Some(sel) = self.selection.as_mut() {
            sel.extend(local_row, local_col, pane_rect);
        }
    }

    async fn handle_left_release(&mut self) -> Result<(), DaemonError> {
        let Some(sel) = self.selection.take() else {
            return Ok(());
        };
        if !self.selection_word_line && sel.is_click() {
            // A click (no drag beyond the one-cell dead-zone, and not an
            // explicit word/line double/triple-click): on the live,
            // unscrolled view, treat a click on the cursor's OWN row as a cursor
            // reposition (Ghostty-style cursor-click-to-move). Both the press
            // (`selection_press_scroll`) and the current view must be unscrolled,
            // so the press anchor is in the same live-grid space as the cursor.
            //
            // Deliberate minor limitations (kept simple, the move is instant,
            // not deferred behind a multi-click timer): the first click of a
            // double/triple-click ON THE INPUT LINE repositions before the
            // word/line is selected (the selection still copies; rare, since
            // double-clicking output rows leaves the cursor's row untouched), and
            // a click on a non-shell input row (a REPL, or a raw password prompt)
            // still injects arrows, exactly as a bare terminal would.
            if self.selection_press_scroll == 0
                && let Some(pane) = self.active_window().pane(sel.source_pane).cloned()
                && pane.scroll_offset() == 0
            {
                let (row, col) = sel.anchor;
                if crate::osc_actions::click_to_position(&pane, row, col).await? {
                    self.notify.notify_one();
                }
            }
            return Ok(());
        }
        let mut copied: Option<String> = None;
        if let Some(pane) = self.active_window().pane(sel.source_pane) {
            // Map the viewport-relative selection through the pane's current
            // scroll position so a selection made while scrolled back copies the
            // highlighted scrollback, not the live grid underneath it.
            let scroll_offset = pane.scroll_offset();
            let text =
                pane.with_screen(|s| extract_text(&sel, s, s.active.num_rows(), scroll_offset));
            if !text.is_empty() {
                copied = Some(crate::osc_actions::copied_message(&text));
                tokio::spawn(async move {
                    let _ = crate::osc_actions::write_clipboard(text.as_bytes()).await;
                });
            }
        }
        if let Some(msg) = copied {
            self.set_status_message(msg, Severity::Success);
            self.notify.notify_one();
        }
        Ok(())
    }

    fn handle_wheel(&mut self, pane_id: PaneId, delta: i16) {
        let Some(pane) = self.active_window().pane(pane_id) else {
            return;
        };
        // Visible-space max so each notch moves one visible line (folds skipped),
        // with no over-scroll dead zone.
        let max_offset =
            pane.with_screen(|s| plexy_glass_mux::blocks::max_scroll_offset(s, s.active.num_rows()));
        // Wheel-up = positive delta = scroll INTO older history.
        pane.scroll_by(delta.into(), max_offset);
    }
}

/// Derive the wire encoding from a pane's mouse-related modes. `?1006` (SGR)
/// takes precedence; otherwise the most-specific legacy mode is used.
fn mouse_encoding_for(modes: plexy_glass_emulator::Modes) -> MouseEncoding {
    use plexy_glass_emulator::Modes;
    if modes.contains(Modes::MOUSE_SGR) {
        MouseEncoding::Sgr
    } else if modes.contains(Modes::MOUSE_ANY) {
        MouseEncoding::AnyEvent
    } else if modes.contains(Modes::MOUSE_BTN) || modes.contains(Modes::MOUSE_BTN_EVENT) {
        // ?1000 and ?1002 share the legacy button-event wire encoding.
        MouseEncoding::ButtonEvent
    } else {
        // ?9 (X10) or no explicit mode: X10 click-only form.
        MouseEncoding::X10
    }
}
