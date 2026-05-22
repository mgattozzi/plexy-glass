//! A `Window` owns a set of `Pane`s laid out in a binary split tree.

use crate::{error::DaemonError, pane::Pane};
use plexy_glass_mux::{
    CloseOutcome, LayoutError, LayoutTree, PaneId, Rect, SplitDir, SplitPosition, WindowId,
};
use plexy_glass_protocol::{PtySize, SpawnSpec};
use std::collections::{HashMap, VecDeque};

pub struct Window {
    pub id: WindowId,
    pub name: String,
    /// When true, input sent to the active pane is also broadcast to all other
    /// panes in this window (sync-panes mode). Defaults to false; toggled by
    /// `Command::ToggleSyncPanes`.
    pub sync_input: bool,
    panes: HashMap<PaneId, Pane>,
    layout: LayoutTree,
    active: PaneId,
    focus_history: VecDeque<PaneId>,
}

impl Window {
    // Window construction needs the full set of plumbing arguments; bundling
    // them into a struct would obscure the call sites and complicate borrows.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_first(
        id: WindowId,
        name: String,
        first_pane_id: PaneId,
        spec: SpawnSpec,
        rect: Rect,
        output_notify: std::sync::Arc<tokio::sync::Notify>,
        death_tx: Option<tokio::sync::mpsc::Sender<PaneId>>,
        config: std::sync::Arc<plexy_glass_config::Config>,
    ) -> Result<Self, DaemonError> {
        let size = PtySize {
            rows: rect.rows,
            cols: rect.cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pane = Pane::spawn(first_pane_id, spec, size, output_notify, death_tx, config)?;
        let mut panes = HashMap::new();
        panes.insert(first_pane_id, pane);
        Ok(Self {
            id,
            name,
            sync_input: false,
            panes,
            layout: LayoutTree::single(first_pane_id),
            active: first_pane_id,
            focus_history: VecDeque::new(),
        })
    }

    pub fn active(&self) -> PaneId {
        self.active
    }

    pub fn active_pane(&self) -> Option<&Pane> {
        self.panes.get(&self.active)
    }

    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.get(&id)
    }

    pub fn panes(&self) -> impl Iterator<Item = (&PaneId, &Pane)> {
        self.panes.iter()
    }

    pub fn layout(&self) -> &LayoutTree {
        &self.layout
    }

    /// Split the active pane in `dir`. The new pane appears After the existing
    /// one and becomes active.
    // Same rationale as `spawn_first`, this is pane-creation plumbing.
    #[allow(clippy::too_many_arguments)]
    pub fn split(
        &mut self,
        dir: SplitDir,
        new_pane_id: PaneId,
        spec: SpawnSpec,
        viewport: Rect,
        output_notify: std::sync::Arc<tokio::sync::Notify>,
        death_tx: Option<tokio::sync::mpsc::Sender<PaneId>>,
        config: std::sync::Arc<plexy_glass_config::Config>,
    ) -> Result<(), DaemonError> {
        self.layout
            .split(self.active, dir, new_pane_id, SplitPosition::After)
            .map_err(|e| DaemonError::Io(std::io::Error::other(format!("layout: {e}"))))?;
        let rect = self
            .layout
            .rect_of(new_pane_id, viewport)
            .ok_or_else(|| DaemonError::Io(std::io::Error::other("new pane rect missing")))?;
        let size = PtySize {
            rows: rect.rows,
            cols: rect.cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pane = Pane::spawn(new_pane_id, spec, size, output_notify, death_tx, config)?;
        self.panes.insert(new_pane_id, pane);
        self.focus_history.push_back(self.active);
        self.active = new_pane_id;
        self.resize(viewport)?;
        Ok(())
    }

    pub fn close_pane(&mut self, id: PaneId) -> Result<CloseOutcome, DaemonError> {
        let outcome = self.layout.close(id);
        self.panes.remove(&id);
        if id == self.active {
            // Collect history first to avoid simultaneous borrows of self.
            let alive_history: Vec<PaneId> = self
                .focus_history
                .iter()
                .rev()
                .filter(|p| self.panes.contains_key(p))
                .copied()
                .collect();
            self.active = alive_history
                .first()
                .copied()
                .or_else(|| self.layout.panes().into_iter().next())
                .unwrap_or(PaneId(0));
        }
        self.focus_history.retain(|p| self.panes.contains_key(p));
        Ok(outcome)
    }

    pub fn close_active(&mut self) -> Result<CloseOutcome, DaemonError> {
        self.close_pane(self.active)
    }

    pub fn select_next(&mut self) {
        let panes = self.layout.panes();
        let Some(idx) = panes.iter().position(|p| *p == self.active) else {
            return;
        };
        if let Some(next) = panes.get((idx + 1) % panes.len()) {
            self.focus_history.push_back(self.active);
            self.active = *next;
        }
    }

    pub fn select_prev(&mut self) {
        let panes = self.layout.panes();
        let Some(idx) = panes.iter().position(|p| *p == self.active) else {
            return;
        };
        let prev_idx = if idx == 0 { panes.len() - 1 } else { idx - 1 };
        if let Some(prev) = panes.get(prev_idx) {
            self.focus_history.push_back(self.active);
            self.active = *prev;
        }
    }

    /// Make `target` the active pane (no-op if it is already active or not in
    /// this window). Mirrors the focus-history update used by `select_next` /
    /// `select_prev` so `close_pane` can fall back to the previously focused
    /// pane.
    pub fn focus(&mut self, target: PaneId) {
        if self.panes.contains_key(&target) && target != self.active {
            self.focus_history.push_back(self.active);
            self.active = target;
        }
    }

    pub fn select_direction(
        &mut self,
        dir: plexy_glass_mux::Direction,
        viewport: Rect,
    ) -> Result<(), LayoutError> {
        if let Some(target) = self.layout.next_in_direction(self.active, viewport, dir) {
            self.focus_history.push_back(self.active);
            self.active = target;
        }
        Ok(())
    }

    pub fn resize(&mut self, viewport: Rect) -> Result<(), DaemonError> {
        for (id, pane) in self.panes.iter() {
            if let Some(rect) = self.layout.rect_of(*id, viewport) {
                let new_rows = rect.rows.max(1);
                let size = PtySize {
                    rows: new_rows,
                    cols: rect.cols.max(1),
                    pixel_width: 0,
                    pixel_height: 0,
                };
                pane.resize(size)?;
                pane.on_size_changed(new_rows);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notify() -> std::sync::Arc<tokio::sync::Notify> {
        std::sync::Arc::new(tokio::sync::Notify::new())
    }

    fn cfg() -> std::sync::Arc<plexy_glass_config::Config> {
        std::sync::Arc::new(plexy_glass_config::built_in_default())
    }

    fn shell_spec() -> SpawnSpec {
        SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        }
    }

    #[tokio::test]
    async fn spawn_first_creates_one_pane() {
        let viewport = Rect::new(0, 0, 24, 80);
        let w = Window::spawn_first(
            WindowId(0),
            "w0".into(),
            PaneId(0),
            shell_spec(),
            viewport,
            notify(),
            None,
            cfg(),
        )
        .expect("spawn");
        assert_eq!(w.active(), PaneId(0));
        assert_eq!(w.layout().panes(), vec![PaneId(0)]);
    }

    #[tokio::test]
    async fn split_adds_pane_and_makes_active() {
        let viewport = Rect::new(0, 0, 24, 80);
        let mut w = Window::spawn_first(
            WindowId(0),
            "w0".into(),
            PaneId(0),
            shell_spec(),
            viewport,
            notify(),
            None,
            cfg(),
        )
        .unwrap();
        w.split(SplitDir::Vertical, PaneId(1), shell_spec(), viewport, notify(), None, cfg())
            .expect("split");
        assert_eq!(w.active(), PaneId(1));
        assert!(w.layout().panes().contains(&PaneId(0)));
        assert!(w.layout().panes().contains(&PaneId(1)));
    }

    #[tokio::test]
    async fn close_active_promotes_focus_history() {
        let viewport = Rect::new(0, 0, 24, 80);
        let mut w = Window::spawn_first(
            WindowId(0),
            "w0".into(),
            PaneId(0),
            shell_spec(),
            viewport,
            notify(),
            None,
            cfg(),
        )
        .unwrap();
        w.split(SplitDir::Vertical, PaneId(1), shell_spec(), viewport, notify(), None, cfg())
            .unwrap();
        let outcome = w.close_active().unwrap();
        assert_eq!(outcome, CloseOutcome::SiblingPromoted);
        assert_eq!(w.active(), PaneId(0));
    }
}
