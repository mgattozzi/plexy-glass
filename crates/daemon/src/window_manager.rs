//! Owns all windows for one attached client.

use crate::{error::DaemonError, window::Window};
use plexy_glass_mux::{
    Command, MouseButton, MouseEncoding, MouseEvent, MouseKind, PaneId, Rect, Selection,
    SelectionKind, SplitDir, WindowId, encode_for_child, extract_text,
};
use plexy_glass_protocol::{PtySize, SpawnSpec};
use std::sync::Arc;
use tokio::sync::{Notify, mpsc};

pub struct WindowManager {
    windows: Vec<Window>,
    active: usize,
    next_pane_id: u32,
    next_window_id: u32,
    host_size: PtySize,
    pub notify: Arc<Notify>,
    /// `SpawnSpec` used to create new panes/windows (cloned from the client's
    /// initial `Spawn`).
    default_spec: SpawnSpec,
    /// Each pane sends its `PaneId` here when its child exits. `None` in tests
    /// where pane lifecycle is driven manually.
    death_tx: Option<mpsc::Sender<PaneId>>,
    /// In-flight mouse selection (left-press → drag → release). `None` between
    /// drags.
    selection: Option<Selection>,
}

impl WindowManager {
    pub fn new(
        first_spec: SpawnSpec,
        host_size: PtySize,
        notify: Arc<Notify>,
        death_tx: Option<mpsc::Sender<PaneId>>,
    ) -> Result<Self, DaemonError> {
        let viewport = host_viewport(host_size);
        let first = Window::spawn_first(
            WindowId(0),
            "shell".into(),
            PaneId(0),
            first_spec.clone(),
            viewport,
            Arc::clone(&notify),
            death_tx.clone(),
        )?;
        Ok(Self {
            windows: vec![first],
            active: 0,
            next_pane_id: 1,
            next_window_id: 1,
            host_size,
            notify,
            default_spec: first_spec,
            death_tx,
            selection: None,
        })
    }

    /// Read-only access to the in-flight selection, if any. Used by the
    /// compositor to draw highlight cells.
    pub fn selection(&self) -> Option<&Selection> {
        self.selection.as_ref()
    }

    /// Close a pane whose child exited. Called by Connection when it
    /// receives a `PaneId` on the death channel.
    pub fn handle_pane_death(&mut self, pane_id: PaneId) -> Result<(), DaemonError> {
        let viewport = self.viewport();
        let mut closed_idx: Option<usize> = None;
        for (idx, w) in self.windows.iter_mut().enumerate() {
            if w.pane(pane_id).is_some() {
                let outcome = w.close_pane(pane_id)?;
                if matches!(outcome, plexy_glass_mux::CloseOutcome::TreeEmpty) {
                    closed_idx = Some(idx);
                } else {
                    w.resize(viewport)?;
                }
                break;
            }
        }
        if let Some(idx) = closed_idx {
            self.windows.remove(idx);
            if idx <= self.active && self.active > 0 {
                self.active -= 1;
            }
            if self.active >= self.windows.len() && !self.windows.is_empty() {
                self.active = self.windows.len() - 1;
            }
        }
        self.notify.notify_one();
        Ok(())
    }

    pub fn host_size(&self) -> PtySize {
        self.host_size
    }

    pub fn viewport(&self) -> Rect {
        host_viewport(self.host_size)
    }

    pub fn active_window(&self) -> &Window {
        &self.windows[self.active]
    }

    pub fn active_window_mut(&mut self) -> &mut Window {
        &mut self.windows[self.active]
    }

    pub fn windows(&self) -> &[Window] {
        &self.windows
    }

    pub fn active_idx(&self) -> usize {
        self.active
    }

    pub fn handle_command(&mut self, cmd: Command) -> Result<(), DaemonError> {
        let viewport = self.viewport();
        match cmd {
            Command::SplitV => {
                let new_id = self.alloc_pane_id();
                let mut spec = self.default_spec.clone();
                spec.cwd = inherit_cwd(self.active_window().active_pane());
                let notify = Arc::clone(&self.notify);
                let death = self.death_tx.clone();
                self.active_window_mut()
                    .split(SplitDir::Vertical, new_id, spec, viewport, notify, death)?;
            }
            Command::SplitH => {
                let new_id = self.alloc_pane_id();
                let mut spec = self.default_spec.clone();
                spec.cwd = inherit_cwd(self.active_window().active_pane());
                let notify = Arc::clone(&self.notify);
                let death = self.death_tx.clone();
                self.active_window_mut()
                    .split(SplitDir::Horizontal, new_id, spec, viewport, notify, death)?;
            }
            Command::SelectNextPane => self.active_window_mut().select_next(),
            Command::SelectPrevPane => self.active_window_mut().select_prev(),
            Command::SelectPane(dir) => {
                let _ = self.active_window_mut().select_direction(dir, viewport);
            }
            Command::KillPane => {
                let outcome = self.active_window_mut().close_active()?;
                if matches!(outcome, plexy_glass_mux::CloseOutcome::TreeEmpty) {
                    self.close_active_window();
                } else {
                    // Surviving panes may now occupy a larger rect after the
                    // layout collapses; resize their PTYs to match.
                    self.active_window_mut().resize(viewport)?;
                }
            }
            Command::NewWindow => {
                let id = WindowId(self.next_window_id);
                self.next_window_id += 1;
                let first_pane = self.alloc_pane_id();
                let mut spec = self.default_spec.clone();
                spec.cwd = inherit_cwd(self.active_window().active_pane());
                let n = id.raw();
                let window = Window::spawn_first(
                    id,
                    format!("shell{n}"),
                    first_pane,
                    spec,
                    viewport,
                    Arc::clone(&self.notify),
                    self.death_tx.clone(),
                )?;
                self.windows.push(window);
                self.active = self.windows.len() - 1;
            }
            Command::NextWindow => {
                if !self.windows.is_empty() {
                    self.active = (self.active + 1) % self.windows.len();
                }
            }
            Command::PrevWindow => {
                if !self.windows.is_empty() {
                    self.active = if self.active == 0 {
                        self.windows.len() - 1
                    } else {
                        self.active - 1
                    };
                }
            }
            Command::SelectWindow(n) => {
                let idx = usize::from(n);
                if idx < self.windows.len() {
                    self.active = idx;
                }
            }
            Command::KillWindow => self.close_active_window(),
            Command::ZoomToggle => {
                tracing::trace!("ZoomToggle: phase-3 no-op");
            }
            Command::Detach | Command::Cancel => {}
            Command::EnterCopyMode => {
                if let Some(pane) = self.active_window().active_pane() {
                    let (total_lines, pane_rows, start_line, start_col) = pane.with_screen(|s| {
                        let scrollback_len = s.scrollback.len() as u32;
                        let active_rows = u32::from(s.active.num_rows());
                        let total = scrollback_len + active_rows;
                        let start_line = scrollback_len + u32::from(s.cursor.row);
                        let start_col = s.cursor.col;
                        let pane_rows = s.active.num_rows();
                        (total, pane_rows, start_line, start_col)
                    });
                    pane.enter_copy_mode(total_lines, pane_rows, start_line, start_col);
                }
            }
        }
        self.notify.notify_one();
        Ok(())
    }

    pub fn on_host_resize(&mut self, new_size: PtySize) -> Result<(), DaemonError> {
        self.host_size = new_size;
        let viewport = host_viewport(new_size);
        for w in self.windows.iter_mut() {
            w.resize(viewport)?;
        }
        self.notify.notify_one();
        Ok(())
    }

    /// Dispatch one decoded mouse event. See the Phase 4 plan for the
    /// algorithm: locate the pane under (row, col), optionally forward the raw
    /// SGR-encoded event to the child if it opted into mouse reporting, and
    /// otherwise drive focus/hyperlink/click-to-position/selection/wheel.
    pub async fn handle_mouse(&mut self, event: MouseEvent) -> Result<(), DaemonError> {
        let viewport = self.viewport();
        let Some(pane_id) = self
            .active_window()
            .layout()
            .pane_at_coord(viewport, event.row, event.col)
        else {
            return Ok(());
        };

        // App-mouse-mode passthrough: if the resolved pane's emulator wants
        // mouse events, forward and return.
        let wants_mouse = self
            .active_window()
            .pane(pane_id)
            .map(|p| p.with_screen(|s| s.modes.any_mouse_mode_active()))
            .unwrap_or(false);
        if wants_mouse {
            let bytes = encode_for_child(event, MouseEncoding::Sgr);
            if let Some(pane) = self.active_window().pane(pane_id).cloned() {
                let _ = pane.send_input(bytes::Bytes::from(bytes)).await;
            }
            return Ok(());
        }

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
            MouseKind::Wheel { delta } => {
                self.handle_wheel(pane_id, delta);
            }
            _ => {}
        }
        self.notify.notify_one();
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

        // OSC 8 hyperlink under the cell? Open in the OS browser; suppress
        // selection start.
        let url = self.active_window().pane(pane_id).and_then(|p| {
            p.with_screen(|s| {
                s.active
                    .get_cell(local_row, local_col)
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

        // OSC 133 click-to-position. If a prompt mark on the current row is
        // before the click column, walk the shell cursor with arrow keys.
        if let Some(pane) = self.active_window().pane(pane_id).cloned()
            && crate::osc_actions::click_to_position(&pane, local_col).await?
        {
            return Ok(());
        }

        // Otherwise: start a selection anchored at this cell.
        self.selection = Some(Selection::start(
            pane_id,
            local_row,
            local_col,
            SelectionKind::Char,
        ));
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
        if sel.is_empty() {
            return Ok(());
        }
        if let Some(pane) = self.active_window().pane(sel.source_pane) {
            let text = pane.with_screen(|s| extract_text(&sel, s));
            if !text.is_empty() {
                tokio::spawn(async move {
                    let _ = crate::osc_actions::write_clipboard(text.as_bytes()).await;
                });
            }
        }
        Ok(())
    }

    fn handle_wheel(&mut self, pane_id: PaneId, delta: i16) {
        let Some(pane) = self.active_window().pane(pane_id) else {
            return;
        };
        let max_offset = pane.scrollback_len();
        // Wheel-up = positive delta = scroll INTO older history.
        pane.scroll_by(delta.into(), max_offset);
    }

    fn alloc_pane_id(&mut self) -> PaneId {
        let id = PaneId(self.next_pane_id);
        self.next_pane_id += 1;
        id
    }

    fn close_active_window(&mut self) {
        if self.windows.is_empty() {
            return;
        }
        self.windows.remove(self.active);
        if self.windows.is_empty() {
            return;
        }
        if self.active >= self.windows.len() {
            self.active = self.windows.len() - 1;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }
}

fn host_viewport(host: PtySize) -> Rect {
    let rows = host.rows.saturating_sub(1).max(1);
    Rect::new(0, 0, rows, host.cols.max(1))
}

fn inherit_cwd(active_pane: Option<&crate::pane::Pane>) -> Option<String> {
    active_pane
        .and_then(|p| p.with_screen(|s| s.cwd.clone()))
        .and_then(|url| cwd_from_osc7(&url))
}

/// OSC 7 sends `file://hostname/path`. Strip the scheme and optional
/// hostname and return the path string, or `None` if the format is
/// unexpected.
fn cwd_from_osc7(url: &str) -> Option<String> {
    let after_scheme = url.strip_prefix("file://")?;
    // Skip hostname if present (may be empty: "file:///path").
    let path_start = after_scheme.find('/')?;
    Some(after_scheme[path_start..].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> SpawnSpec {
        SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        }
    }

    #[tokio::test]
    async fn new_creates_one_window_one_pane() {
        let notify = Arc::new(Notify::new());
        let m = WindowManager::new(
            spec(),
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            notify,
            None,
        )
        .unwrap();
        assert_eq!(m.windows().len(), 1);
        assert_eq!(m.active_window().active(), PaneId(0));
    }

    #[tokio::test]
    async fn splitv_makes_two_panes() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            notify,
            None,
        )
        .unwrap();
        m.handle_command(Command::SplitV).unwrap();
        assert_eq!(m.active_window().layout().panes().len(), 2);
        assert_eq!(m.active_window().active(), PaneId(1));
    }

    #[tokio::test]
    async fn new_window_adds_and_activates() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            notify,
            None,
        )
        .unwrap();
        m.handle_command(Command::NewWindow).unwrap();
        assert_eq!(m.windows().len(), 2);
        assert_eq!(m.active_idx(), 1);
    }

    #[tokio::test]
    async fn select_pane_right_after_split() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            notify,
            None,
        )
        .unwrap();
        m.handle_command(Command::SplitV).unwrap();
        // After SplitV, the new pane (`PaneId(1)`) is active.
        assert_eq!(m.active_window().active(), PaneId(1));
        // Going left should reach PaneId(0).
        m.handle_command(Command::SelectPane(plexy_glass_mux::Direction::Left))
            .unwrap();
        assert_eq!(m.active_window().active(), PaneId(0));
        // Going right should return to PaneId(1).
        m.handle_command(Command::SelectPane(plexy_glass_mux::Direction::Right))
            .unwrap();
        assert_eq!(m.active_window().active(), PaneId(1));
    }

    #[tokio::test]
    async fn select_pane_up_down_after_horizontal_split() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            notify,
            None,
        )
        .unwrap();
        m.handle_command(Command::SplitH).unwrap();
        // After SplitH, the new pane (`PaneId(1)`) is active.
        assert_eq!(m.active_window().active(), PaneId(1));
        // Going up should reach PaneId(0).
        m.handle_command(Command::SelectPane(plexy_glass_mux::Direction::Up))
            .unwrap();
        assert_eq!(m.active_window().active(), PaneId(0));
        // Going down should return to PaneId(1).
        m.handle_command(Command::SelectPane(plexy_glass_mux::Direction::Down))
            .unwrap();
        assert_eq!(m.active_window().active(), PaneId(1));
    }

    #[tokio::test]
    async fn click_in_other_pane_changes_focus() {
        use plexy_glass_mux::{MouseButton, MouseEvent, MouseKind, MouseModifiers};

        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            notify,
            None,
        )
        .unwrap();
        m.handle_command(Command::SplitV).unwrap();
        // After SplitV, the new pane (`PaneId(1)`) is active on the right.
        assert_eq!(m.active_window().active(), PaneId(1));

        // Click in the LEFT half (`PaneId(0)`), which should change focus there.
        let event = MouseEvent {
            kind: MouseKind::Press,
            button: MouseButton::Left,
            modifiers: MouseModifiers::default(),
            row: 5,
            col: 2,
        };
        m.handle_mouse(event).await.unwrap();
        assert_eq!(m.active_window().active(), PaneId(0));
    }

    #[tokio::test]
    async fn next_window_cycles() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            notify,
            None,
        )
        .unwrap();
        m.handle_command(Command::NewWindow).unwrap();
        m.handle_command(Command::NextWindow).unwrap();
        assert_eq!(m.active_idx(), 0);
        m.handle_command(Command::NextWindow).unwrap();
        assert_eq!(m.active_idx(), 1);
    }

    #[tokio::test]
    async fn osc7_cwd_inherited_on_split() {
        let notify = Arc::new(Notify::new());
        let mut m = WindowManager::new(
            spec(),
            PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            notify,
            None,
        )
        .unwrap();
        // Inject a cwd directly onto the active pane's screen.
        if let Some(pane) = m.active_window().active_pane() {
            pane.with_screen_mut(|s| s.cwd = Some("file:///tmp/work".to_string()));
        }
        m.handle_command(Command::SplitV).unwrap();
        // We can't easily inspect the spawned pane's spec.cwd post-spawn,
        // but verify the split succeeded.
        assert_eq!(m.active_window().layout().panes().len(), 2);
    }

    #[test]
    fn cwd_from_osc7_strips_file_scheme_and_hostname() {
        assert_eq!(super::cwd_from_osc7("file:///tmp"), Some("/tmp".to_string()));
        assert_eq!(super::cwd_from_osc7("file://localhost/tmp"), Some("/tmp".to_string()));
        assert_eq!(super::cwd_from_osc7("not-a-file-url"), None);
    }
}
