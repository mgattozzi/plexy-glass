//! A `Window` owns a set of `Pane`s laid out in a binary split tree.

use crate::{error::DaemonError, pane::Pane};
use plexy_glass_mux::{
    CloseOutcome, LayoutError, LayoutTree, PaneId, Rect, SplitDir, SplitPosition, WindowId,
};
use plexy_glass_protocol::{PtySize, SpawnSpec};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

pub struct Window {
    pub id: WindowId,
    pub name: String,
    /// When true, input sent to the active pane is also broadcast to all other
    /// panes in this window (sync-panes mode). Defaults to false; toggled by
    /// `Command::ToggleSyncPanes`.
    pub sync_input: bool,
    /// When `Some`, the named pane is zoomed: it renders at the full viewport
    /// and other panes are hidden. This is a view overlay (the layout tree is
    /// NOT mutated), so unzoom restores exactly. Cleared by any structural
    /// change.
    pub zoomed: Option<PaneId>,
    /// The window's permanent home base: panes/splits created here spawn at this
    /// cwd. Resolved once at construction (window cwd → session cwd, expanded);
    /// `None` = no anchor (daemon cwd). Not mutated after construction.
    pub home_cwd: Option<String>,
    /// The preset most recently applied to this window, if any (the cursor
    /// for `next_layout` cycling). Runtime-only (not persisted), and manual
    /// splits and resizes deliberately do not reset it.
    pub last_preset: Option<plexy_glass_mux::LayoutPreset>,
    panes: HashMap<PaneId, Pane>,
    layout: LayoutTree,
    active: PaneId,
    focus_history: VecDeque<PaneId>,
    /// Monitor options (tmux's `monitor-activity` / `monitor-bell`, plus our
    /// `monitor-command`). When on, a background occurrence sets the
    /// corresponding sticky flag below.
    monitor_activity: bool,
    monitor_bell: bool,
    /// Monitor command completion (OSC 133;D blocks). When on, a completed
    /// command block in a background pane sets the sticky `done` flag.
    monitor_command: bool,
    /// Sticky alert flags: set by `WindowManager::update_monitor_flags` when this
    /// (non-current) window had activity / a bell; cleared when it becomes the
    /// current window. Surfaced as `#`/`!` in the status window-list.
    activity: bool,
    bell: bool,
    /// Sticky command-completion flag: `Some(true)` = completed OK (`✓`),
    /// `Some(false)` = completed with a nonzero exit (`✗`), `None` = no
    /// completion pending. Cleared on view like activity/bell.
    done: Option<bool>,
    /// Per-pane last-seen `Screen.blocks_completed` snapshot. Updated
    /// UNCONDITIONALLY every drain for every pane (so a toggle-on never
    /// backlogs history); a counter DECREASE (RIS) re-baselines silently.
    block_baselines: HashMap<PaneId, u64>,
    /// Silence threshold (tmux's `monitor-silence`): `Some(N)` = alert after N
    /// of no output; `None` = off. Set by `:monitor-silence <secs>`.
    monitor_silence: Option<Duration>,
    /// Last instant this window produced output, updated in the drain whenever
    /// the activity signal fired. Seeds to construction time so a window that
    /// never emits anything still ages toward its silence threshold.
    last_output: Instant,
    /// Sticky silence flag: set when the silence threshold is crossed in a
    /// background window; surfaced as `~` in the status window-list, cleared on
    /// view like activity/bell.
    silence: bool,
    /// Episode latch: fire at most once per silence EPISODE. Set when the
    /// silence edge fires, reset ONLY when output resumes. Viewing clears the
    /// `silence` FLAG but NOT this latch, so a view-while-still-silent does not
    /// re-fire on the next tick (a 1 Hz loop); output resuming re-arms it.
    silence_fired: bool,
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
            zoomed: None,
            home_cwd: None,
            last_preset: None,
            panes,
            layout: LayoutTree::single(first_pane_id),
            active: first_pane_id,
            focus_history: VecDeque::new(),
            monitor_activity: false,
            monitor_bell: true,
            monitor_command: false,
            activity: false,
            bell: false,
            done: None,
            block_baselines: HashMap::new(),
            monitor_silence: None,
            last_output: Instant::now(),
            silence: false,
            silence_fired: false,
        })
    }

    pub fn active(&self) -> PaneId {
        self.active
    }

    pub fn is_zoomed(&self) -> bool {
        self.zoomed.is_some()
    }

    /// Toggle zoom on the active pane. Returns the new zoom state.
    pub fn toggle_zoom(&mut self) -> bool {
        if self.zoomed.is_some() {
            self.zoomed = None;
        } else {
            self.zoomed = Some(self.active);
        }
        self.zoomed.is_some()
    }

    /// Clear zoom if set. Returns true if it was zoomed.
    pub fn clear_zoom(&mut self) -> bool {
        self.zoomed.take().is_some()
    }

    /// The most recently focused pane other than the current active one, if
    /// it still exists. Used by `select_last_pane`.
    pub fn last_pane(&self) -> Option<PaneId> {
        self.focus_history
            .iter()
            .rev()
            .find(|p| **p != self.active && self.panes.contains_key(p))
            .copied()
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

    /// Mutable layout access for in-place ratio updates (drag-resize).
    pub fn layout_mut(&mut self) -> &mut LayoutTree {
        &mut self.layout
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
        self.split_at(
            self.active,
            dir,
            new_pane_id,
            spec,
            viewport,
            output_notify,
            death_tx,
            config,
        )
    }

    /// Like `split`, but splits an arbitrary `target_pane_id` instead of the
    /// active one. The active pane stays the same. Used by session restore
    /// to rebuild a saved layout depth-first.
    #[allow(clippy::too_many_arguments)]
    pub fn split_at(
        &mut self,
        target_pane_id: PaneId,
        dir: SplitDir,
        new_pane_id: PaneId,
        spec: SpawnSpec,
        viewport: Rect,
        output_notify: std::sync::Arc<tokio::sync::Notify>,
        death_tx: Option<tokio::sync::mpsc::Sender<PaneId>>,
        config: std::sync::Arc<plexy_glass_config::Config>,
    ) -> Result<(), DaemonError> {
        self.layout
            .split(target_pane_id, dir, new_pane_id, SplitPosition::After)
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
        let pane = match Pane::spawn(new_pane_id, spec, size, output_notify, death_tx, config) {
            Ok(p) => p,
            Err(e) => {
                // Roll the layout node back: a failed spawn must not leave a
                // dangling leaf with no pane behind it (it would render as a
                // dead region and corrupt close/focus bookkeeping).
                let _ = self.layout.close(new_pane_id);
                return Err(e);
            }
        };
        self.panes.insert(new_pane_id, pane);
        self.focus_history.push_back(self.active);
        self.active = new_pane_id;
        self.resize(viewport)?;
        Ok(())
    }

    pub fn close_pane(&mut self, id: PaneId) -> Result<CloseOutcome, DaemonError> {
        let outcome = self.layout.close(id);
        // Kill the removed pane's child and cancel any pipe-pane consumer.
        // Dropping the pane alone does NOT terminate the child (the detached
        // reader thread holds the PTY master open until the child exits), so a
        // synchronous close (Ctrl+a x / Ctrl+a &) must do what `kill_child`
        // does for the death-channel paths, or it leaks both the shell and the
        // pipe consumer.
        if let Some(pane) = self.panes.remove(&id) {
            pane.kill_child();
        }
        // Drop a zoom overlay that pointed at the closed pane so it never
        // outlives its target (a dangling zoom renders a blank viewport).
        if self.zoomed == Some(id) {
            self.zoomed = None;
        }
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

    /// Remove `id` from the layout and the pane map and RETURN the live `Pane`
    /// (it is NOT killed, the caller adopts it into another window). Mirrors
    /// `close_pane`'s sibling-promotion / active-fixup / focus-history retain /
    /// dangling-zoom clear, but hands the pane back. Returns `None` if `id` is not
    /// in this window. Does not resize (the caller resizes the surviving window,
    /// as every `close_pane` caller does).
    pub fn detach_pane(&mut self, id: PaneId) -> Option<Pane> {
        let pane = self.panes.remove(&id)?;
        self.layout.close(id);
        if self.zoomed == Some(id) {
            self.zoomed = None;
        }
        if id == self.active {
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
        Some(pane)
    }

    /// Slot-preserving occupant swap: the layout leaf that held `old` now
    /// holds `new_pane`, and every piece of bookkeeping that pointed at `old`
    /// (`active`, every `focus_history` entry, the zoom overlay) is rewritten
    /// to the new pane's id, so focus and zoom follow the SLOT. Contrast
    /// `detach_pane`, which removes the slot (sibling promotion) and moves
    /// focus away; here the tree shape never changes. Returns the displaced
    /// `Pane` (live, not killed). When `old` is not in this window, returns
    /// `Err(new_pane)` with the window untouched, so no `Pane` is ever dropped
    /// on any path: each one is either returned or inserted into the pane map.
    /// Does not resize (the caller resizes, as with `detach_pane`).
    pub fn swap_occupant(&mut self, old: PaneId, new_pane: Pane) -> Result<Pane, Pane> {
        match self.take_pane(old) {
            Some(old_pane) => {
                self.install_in_slot(old, new_pane);
                Ok(old_pane)
            }
            None => Err(new_pane),
        }
    }

    /// Map-only removal of `id`'s `Pane`: the layout leaf, `active`, the
    /// focus history, and zoom still reference `id` afterwards. This is the
    /// first half of the cross-window swap choreography, so the caller MUST
    /// follow with `install_in_slot(id, …)` (directly or via `swap_occupant`)
    /// to fill the hole before the window is observed again.
    pub(crate) fn take_pane(&mut self, id: PaneId) -> Option<Pane> {
        self.panes.remove(&id)
    }

    /// Second half of the slot swap: install `pane` in the slot that holds
    /// `old_slot` (whose map entry was already removed by `take_pane` /
    /// `swap_occupant`): replace the layout leaf and rewrite `active`,
    /// every `focus_history` entry, and `zoomed` from `old_slot` to the new
    /// pane's id. Slot-preserving: the tree shape never changes.
    pub(crate) fn install_in_slot(&mut self, old_slot: PaneId, pane: Pane) {
        let new_id = pane.id();
        let replaced = self.layout.replace_leaf(old_slot, new_id);
        debug_assert!(replaced, "install_in_slot: no leaf for {old_slot:?}");
        self.panes.insert(new_id, pane);
        if self.active == old_slot {
            self.active = new_id;
        }
        for p in self.focus_history.iter_mut() {
            if *p == old_slot {
                *p = new_id;
            }
        }
        if self.zoomed == Some(old_slot) {
            self.zoomed = Some(new_id);
        }
    }

    /// Split `target` in `dir` and place an EXISTING `pane` (keeping its id) in
    /// the new slot; insert it, make it active, and resize. Errors if `target`
    /// is not in the layout.
    pub fn adopt_split(
        &mut self,
        target: PaneId,
        dir: SplitDir,
        pane: Pane,
        viewport: Rect,
    ) -> Result<(), DaemonError> {
        let id = pane.id();
        self.layout
            .split(target, dir, id, SplitPosition::After)
            .map_err(|e| DaemonError::Io(std::io::Error::other(format!("layout: {e}"))))?;
        self.panes.insert(id, pane);
        self.focus_history.push_back(self.active);
        self.active = id;
        self.resize(viewport)?;
        Ok(())
    }

    /// Build a new window whose single pane is an existing `pane` (break-pane).
    pub fn from_pane(id: WindowId, name: String, pane: Pane) -> Self {
        let pid = pane.id();
        let mut panes = HashMap::new();
        panes.insert(pid, pane);
        Self {
            id,
            name,
            sync_input: false,
            zoomed: None,
            home_cwd: None,
            last_preset: None,
            panes,
            layout: LayoutTree::single(pid),
            active: pid,
            focus_history: VecDeque::new(),
            monitor_activity: false,
            monitor_bell: true,
            monitor_command: false,
            activity: false,
            bell: false,
            done: None,
            block_baselines: HashMap::new(),
            monitor_silence: None,
            last_output: Instant::now(),
            silence: false,
            silence_fired: false,
        }
    }

    /// Toggle monitor-activity; returns the new state.
    pub fn toggle_monitor_activity(&mut self) -> bool {
        self.monitor_activity = !self.monitor_activity;
        self.monitor_activity
    }

    /// Toggle monitor-bell; returns the new state.
    pub fn toggle_monitor_bell(&mut self) -> bool {
        self.monitor_bell = !self.monitor_bell;
        self.monitor_bell
    }

    /// Toggle monitor-command; returns the new state.
    pub fn toggle_monitor_command(&mut self) -> bool {
        self.monitor_command = !self.monitor_command;
        self.monitor_command
    }

    pub fn monitor_activity(&self) -> bool {
        self.monitor_activity
    }

    pub fn monitor_bell(&self) -> bool {
        self.monitor_bell
    }

    pub fn monitor_command(&self) -> bool {
        self.monitor_command
    }

    /// Set (or clear, with `None`) the silence threshold. Clearing also resets
    /// the sticky flag and the episode latch so a re-arm starts clean.
    pub fn set_monitor_silence(&mut self, threshold: Option<Duration>) {
        self.monitor_silence = threshold;
        if threshold.is_none() {
            self.silence = false;
            self.silence_fired = false;
        }
    }

    pub fn monitor_silence(&self) -> Option<Duration> {
        self.monitor_silence
    }

    pub fn activity_flag(&self) -> bool {
        self.activity
    }

    pub fn bell_flag(&self) -> bool {
        self.bell
    }

    /// The sticky command-completion flag: `Some(true)` = `✓`, `Some(false)` =
    /// `✗`, `None` = none. Surfaced as `✓`/`✗` in the status window-list.
    pub fn done_flag(&self) -> Option<bool> {
        self.done
    }

    /// The sticky silence flag (`~`).
    pub fn silence_flag(&self) -> bool {
        self.silence
    }

    /// Clear the sticky alert flags (the window became current). Clears the
    /// silence FLAG but NOT the `silence_fired` episode latch: viewing a
    /// still-silent window must not let the next tick re-fire (a 1 Hz loop);
    /// only resuming output (`note_drain_output`) resets the latch.
    pub fn clear_alerts(&mut self) {
        self.activity = false;
        self.bell = false;
        self.done = None;
        self.silence = false;
    }

    /// Set the sticky activity flag (called by the manager for a background
    /// window with monitor-activity on). Returns whether this set was a
    /// false→true EDGE. The drain emits an alert message only on the edge, so
    /// a chatty pane (whose atomic re-arms every frame while the sticky flag
    /// merely stays true) is messaged once, not per frame.
    pub fn set_activity(&mut self) -> bool {
        let edge = !self.activity;
        self.activity = true;
        edge
    }

    /// Set the sticky bell flag; returns whether this set was a false→true edge.
    pub fn set_bell(&mut self) -> bool {
        let edge = !self.bell;
        self.bell = true;
        edge
    }

    /// Read-and-clear every pane's activity/bell signal, OR-ing the results.
    /// Always drains (so signals never backlog), regardless of monitor options.
    pub fn drain_pane_alerts(&mut self) -> (bool, bool) {
        let (mut acted, mut belled) = (false, false);
        for id in self.layout.panes() {
            if let Some(p) = self.panes.get(&id) {
                acted |= p.take_activity();
                belled |= p.take_bell();
            }
        }
        (acted, belled)
    }

    /// Fold this drain's activity signal into silence-timing bookkeeping.
    /// Called every drain for every window (so timing is uniform regardless of
    /// monitor/active state): when output fired, refresh `last_output` and
    /// reset the episode latch so the NEXT silence episode can fire again.
    pub fn note_drain_output(&mut self, acted: bool) {
        if acted {
            self.last_output = Instant::now();
            self.silence_fired = false;
        }
    }

    /// Silence-tick check: if this window monitors silence and has produced no
    /// output for at least the threshold AND has not already fired this
    /// episode, set the sticky `~` flag and latch the episode. Returns whether
    /// this was a fresh silence EDGE (so the tick notifies only on an edge).
    /// The caller must exclude the active window (an idle active window would
    /// otherwise flicker at 1 Hz: tick sets the flag → render clears it → tick
    /// re-fires).
    pub fn check_silence(&mut self, now: Instant) -> bool {
        let Some(threshold) = self.monitor_silence else {
            return false;
        };
        if self.silence_fired {
            return false;
        }
        if now.duration_since(self.last_output) >= threshold {
            self.silence = true;
            self.silence_fired = true;
            true
        } else {
            false
        }
    }

    /// Set the sticky command-completion flag from a block's exit outcome
    /// (`Some(code)` → `✓` for 0 / `✗` for nonzero; `None` codeless → `✓`).
    /// Returns whether this was a false→`Some` EDGE so the drain messages once
    /// per completion, not while the flag stays sticky.
    pub fn set_done(&mut self, exit: Option<i32>) -> bool {
        let ok = exit.map(|c| c == 0).unwrap_or(true);
        let edge = self.done.is_none();
        self.done = Some(ok);
        edge
    }

    /// Compare every pane's live `blocks_completed` counter against its
    /// per-pane baseline and fold the result into the sticky `done` flag.
    ///
    /// Baselines update UNCONDITIONALLY for every pane regardless of monitor /
    /// active state (the same "always drains, never backlogs" convention as
    /// `drain_pane_alerts`), so a toggle-on starts from the current counter and
    /// never replays history. A counter DECREASE (a RIS reset the screen)
    /// re-baselines silently and never alerts. On an INCREASE, the flag/edge is
    /// set only when this is a monitored non-active window (`record_flag`); the
    /// active or unmonitored case still advances the baseline but reports
    /// nothing.
    ///
    /// Returns the alert outcome to message on (`Some(exit)`) when an edge fired
    /// for a flagged window: `exit` is the latest completed block's exit payload
    /// (`None` for a codeless `133;D`). The caller formats the message.
    pub fn drain_command_completion(&mut self, record_flag: bool) -> Option<Option<i32>> {
        // Prune baselines for panes that no longer exist (break/swap/kill).
        let live: std::collections::HashSet<PaneId> = self.layout.panes().into_iter().collect();
        self.block_baselines.retain(|id, _| live.contains(id));

        let mut completed_exit: Option<Option<i32>> = None;
        for id in self.layout.panes() {
            let Some(p) = self.panes.get(&id) else { continue };
            let (count, exit) = p.with_screen(|s| (s.blocks_completed, s.last_block_exit));
            let baseline = self.block_baselines.entry(id).or_insert(0);
            if count > *baseline {
                // A new block (or several) completed since the last drain. We
                // surface the latest exit payload, since the most recent outcome
                // is what the user cares about.
                completed_exit = Some(exit);
            }
            // Update the baseline unconditionally (increase OR decrease/RIS).
            *baseline = count;
        }
        // Flag + edge only for a monitored non-active window; the active or
        // unmonitored case still advances the baseline above but reports
        // nothing.
        match completed_exit {
            Some(exit) if record_flag && self.set_done(exit) => Some(exit),
            _ => None,
        }
    }

    /// The active pane's DFS-leaf neighbor (wrapping): the next leaf if `next`,
    /// else the previous. `None` when there is only one pane.
    pub fn neighbor_leaf(&self, next: bool) -> Option<PaneId> {
        let leaves = self.layout.dfs_leaves();
        if leaves.len() < 2 {
            return None;
        }
        let i = leaves.iter().position(|p| *p == self.active)?;
        let n = leaves.len();
        let j = if next { (i + 1) % n } else { (i + n - 1) % n };
        Some(leaves[j])
    }

    pub fn is_layout_empty(&self) -> bool {
        self.layout.is_empty()
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
            // A zoomed pane fills the whole viewport regardless of its latent
            // layout rect; hidden panes still track their layout rect so an
            // un-zoom restores the split instantly at the correct size.
            let rect = if self.zoomed == Some(*id) {
                viewport
            } else {
                match self.layout.rect_of(*id, viewport) {
                    Some(r) => r,
                    None => continue,
                }
            };
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

    #[tokio::test]
    async fn resize_keeps_zoomed_pane_at_full_viewport() {
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
        assert!(w.toggle_zoom(), "active pane is now zoomed");
        // A subsequent host resize must keep the zoomed pane at the full
        // viewport, not collapse it back to its split rect.
        let new_vp = Rect::new(0, 0, 40, 100);
        w.resize(new_vp).unwrap();
        let (rows, cols) = w
            .pane(PaneId(1))
            .unwrap()
            .with_screen(|s| (s.active.num_rows(), s.active.num_cols()));
        assert_eq!((rows, cols), (40, 100), "zoomed pane must track the full viewport");
    }

    #[tokio::test]
    async fn close_pane_clears_zoom_when_zoomed_pane_dies() {
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
        assert!(w.toggle_zoom());
        assert!(w.is_zoomed());
        let outcome = w.close_pane(PaneId(1)).unwrap();
        assert_eq!(outcome, CloseOutcome::SiblingPromoted);
        assert!(!w.is_zoomed(), "zoom must clear when its target pane is closed");
    }

    fn two_pane_window() -> Window {
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
            .unwrap(); // active = PaneId(1); leaves [0, 1]
        w
    }

    #[tokio::test]
    async fn detach_pane_returns_live_pane_and_promotes_sibling() {
        let mut w = two_pane_window();
        let pane = w.detach_pane(PaneId(0)).expect("pane present");
        assert_eq!(pane.id(), PaneId(0), "returns the live (un-killed) pane");
        assert!(!w.is_layout_empty());
        assert_eq!(w.layout().panes(), vec![PaneId(1)]);
        assert_eq!(w.active(), PaneId(1));
        assert!(w.detach_pane(PaneId(99)).is_none(), "absent pane → None");
        pane.kill_child(); // tidy the moved-out child
    }

    #[tokio::test]
    async fn detach_last_pane_empties_layout() {
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
        let pane = w.detach_pane(PaneId(0)).expect("present");
        assert!(w.is_layout_empty());
        pane.kill_child();
    }

    #[tokio::test]
    async fn adopt_split_inserts_existing_pane_and_activates() {
        let viewport = Rect::new(0, 0, 24, 80);
        let mut src = two_pane_window();
        let moved = src.detach_pane(PaneId(1)).expect("present"); // PaneId(1)
        let mut dst = Window::spawn_first(
            WindowId(1),
            "dst".into(),
            PaneId(2),
            shell_spec(),
            viewport,
            notify(),
            None,
            cfg(),
        )
        .unwrap();
        dst.adopt_split(PaneId(2), SplitDir::Vertical, moved, viewport).unwrap();
        assert_eq!(dst.active(), PaneId(1), "adopted pane becomes active");
        assert!(dst.layout().panes().contains(&PaneId(1)));
        assert!(dst.layout().panes().contains(&PaneId(2)));
    }

    #[tokio::test]
    async fn from_pane_builds_single_pane_window() {
        let mut src = two_pane_window();
        let moved = src.detach_pane(PaneId(1)).expect("present");
        let w = Window::from_pane(WindowId(5), "broken".into(), moved);
        assert_eq!(w.id, WindowId(5));
        assert_eq!(w.name, "broken");
        assert_eq!(w.active(), PaneId(1));
        assert_eq!(w.layout().panes(), vec![PaneId(1)]);
    }

    /// A live standalone `Pane` (detached from a throwaway window) for
    /// `swap_occupant` tests.
    fn donor_pane(id: PaneId) -> Pane {
        let viewport = Rect::new(0, 0, 24, 80);
        let mut w = Window::spawn_first(
            WindowId(99),
            "donor".into(),
            id,
            shell_spec(),
            viewport,
            notify(),
            None,
            cfg(),
        )
        .unwrap();
        w.detach_pane(id).expect("donor pane present")
    }

    #[tokio::test]
    async fn swap_occupant_replaces_slot_and_returns_old_pane() {
        let mut w = two_pane_window(); // panes {0,1}, active 1
        let viewport = Rect::new(0, 0, 24, 80);
        let r0 = w.layout().rect_of(PaneId(0), viewport).unwrap();
        let Ok(old) = w.swap_occupant(PaneId(0), donor_pane(PaneId(9))) else {
            panic!("pane 0 present")
        };
        assert_eq!(old.id(), PaneId(0), "displaced pane returned live");
        assert!(w.pane(PaneId(9)).is_some() && w.pane(PaneId(0)).is_none());
        assert_eq!(
            w.layout().rect_of(PaneId(9), viewport),
            Some(r0),
            "new pane occupies the old slot's rect"
        );
        assert_eq!(w.active(), PaneId(1), "active untouched when swapping a non-active slot");
        old.kill_child();
    }

    #[tokio::test]
    async fn swap_occupant_rewrites_active_and_zoom() {
        let mut w = two_pane_window(); // active 1
        assert!(w.toggle_zoom()); // zoomed = Some(1)
        let Ok(old) = w.swap_occupant(PaneId(1), donor_pane(PaneId(9))) else {
            panic!("pane 1 present")
        };
        assert_eq!(old.id(), PaneId(1));
        assert_eq!(w.active(), PaneId(9), "focus follows the slot");
        assert_eq!(w.zoomed, Some(PaneId(9)), "zoom follows the slot");
        old.kill_child();
    }

    #[tokio::test]
    async fn swap_occupant_rewrites_focus_history_observable_via_close() {
        // Build h = [0, 1, 2, 0] with active 2: the NEWEST history entry is
        // pane 0, the slot being swapped. After the swap (0 → 9) the close of
        // the active pane must fall back to 9; without the rewrite the dead
        // entry 0 would be filtered and focus would fall to 1 instead.
        let viewport = Rect::new(0, 0, 24, 80);
        let mut w = two_pane_window(); // h=[0], active 1
        w.split(SplitDir::Vertical, PaneId(2), shell_spec(), viewport, notify(), None, cfg())
            .unwrap(); // h=[0,1], active 2
        w.focus(PaneId(0)); // h=[0,1,2], active 0
        w.focus(PaneId(2)); // h=[0,1,2,0], active 2
        let Ok(old) = w.swap_occupant(PaneId(0), donor_pane(PaneId(9))) else {
            panic!("pane 0 present")
        };
        w.close_pane(PaneId(2)).unwrap();
        assert_eq!(w.active(), PaneId(9), "fallback focus uses the rewritten history entry");
        old.kill_child();
    }

    #[tokio::test]
    async fn swap_occupant_absent_old_returns_new_pane_untouched() {
        let mut w = two_pane_window();
        let Err(back) = w.swap_occupant(PaneId(42), donor_pane(PaneId(9))) else {
            panic!("absent old pane → Err")
        };
        assert_eq!(back.id(), PaneId(9), "new pane handed back, not dropped");
        assert!(w.pane(PaneId(9)).is_none(), "window untouched");
        assert_eq!(w.layout().panes(), vec![PaneId(0), PaneId(1)]);
        assert_eq!(w.active(), PaneId(1));
        back.kill_child();
    }

    #[tokio::test]
    async fn neighbor_leaf_wraps_and_handles_single_pane() {
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
        assert_eq!(w.neighbor_leaf(true), None, "single pane has no neighbor");
        w.split(SplitDir::Vertical, PaneId(1), shell_spec(), viewport, notify(), None, cfg())
            .unwrap(); // active = PaneId(1), leaves [0, 1]
        assert_eq!(w.neighbor_leaf(true), Some(PaneId(0)));
        assert_eq!(w.neighbor_leaf(false), Some(PaneId(0)));
    }
}
