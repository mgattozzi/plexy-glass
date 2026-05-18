//! Owns all windows for one attached client.

use crate::{error::DaemonError, window::Window};
use plexy_glass_mux::{Command, PaneId, Rect, SplitDir, WindowId};
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
        })
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
                let spec = self.default_spec.clone();
                let notify = Arc::clone(&self.notify);
                let death = self.death_tx.clone();
                self.active_window_mut()
                    .split(SplitDir::Vertical, new_id, spec, viewport, notify, death)?;
            }
            Command::SplitH => {
                let new_id = self.alloc_pane_id();
                let spec = self.default_spec.clone();
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
                let spec = self.default_spec.clone();
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
}
