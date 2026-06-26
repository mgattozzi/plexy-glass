use super::{Severity, WindowManager};
use crate::{error::DaemonError, window::Window};
use plexy_glass_mux::{Command, SplitDir, WindowId};
use std::sync::Arc;

impl WindowManager {
    pub fn handle_command(&mut self, cmd: Command) -> Result<(), DaemonError> {
        let viewport = self.viewport();
        // Any structural / navigation command clears a zoom overlay first
        // (zoom is a view of one pane; changing the layout or focus ends it).
        if command_clears_zoom(&cmd) {
            self.clear_zoom_restore()?;
        }
        match cmd {
            Command::SplitV | Command::SplitH => {
                let dir = if matches!(cmd, Command::SplitV) {
                    SplitDir::Vertical
                } else {
                    SplitDir::Horizontal
                };
                let new_id = self.alloc_pane_id();
                let mut spec = self.default_spec.clone();
                spec.cwd = self.split_cwd();
                let notify = Arc::clone(&self.notify);
                let death = self.death_tx.clone();
                let config = Arc::clone(&self.config);
                self.active_window_mut().split(
                    dir, new_id, spec, viewport, notify, death, config, None,
                )?;
            }
            Command::SelectNextPane => self.active_window_mut().select_next(),
            Command::SelectPrevPane => self.active_window_mut().select_prev(),
            Command::SelectPane(dir) => {
                let _ = self.active_window_mut().select_direction(dir, viewport);
            }
            Command::KillPane => {
                // KillPane drops the active pane synchronously (no death-channel
                // round-trip), so clear a mark that pointed at it here, otherwise
                // it would dangle until some later event.
                let killed = self.active_window().active();
                let outcome = self.active_window_mut().close_active()?;
                if matches!(outcome, plexy_glass_mux::CloseOutcome::TreeEmpty) {
                    self.close_active_window();
                } else {
                    // Surviving panes may now occupy a larger rect after the
                    // layout collapses; resize their PTYs to match.
                    self.active_window_mut().resize(viewport)?;
                }
                if self.marked_pane == Some(killed) {
                    self.marked_pane = None;
                }
            }
            Command::NewWindow => {
                let id = WindowId(self.next_window_id);
                self.next_window_id += 1;
                let first_pane = self.alloc_pane_id();
                let mut spec = self.default_spec.clone();
                let home = self.session_cwd.clone();
                spec.cwd = home.clone();
                // Empty name → auto-named: the window derives its name from the
                // active pane (running command → cwd → shell) until a manual
                // rename pins it. See `Window::display_name`.
                let mut window = Window::spawn_first(
                    id,
                    String::new(),
                    first_pane,
                    spec,
                    viewport,
                    Arc::clone(&self.notify),
                    self.death_tx.clone(),
                    Arc::clone(&self.config),
                    None,
                )?;
                window.home_cwd = home;
                self.windows.push(window);
                self.last_active_window = Some(self.active);
                self.active = self.windows.len() - 1;
            }
            Command::NextWindow => {
                if !self.windows.is_empty() {
                    let idx = (self.active + 1) % self.windows.len();
                    self.switch_to_window(idx);
                }
            }
            Command::PrevWindow => {
                if !self.windows.is_empty() {
                    let idx = if self.active == 0 {
                        self.windows.len() - 1
                    } else {
                        self.active - 1
                    };
                    self.switch_to_window(idx);
                }
            }
            Command::SelectWindow(n) => {
                self.switch_to_window(usize::from(n));
            }
            Command::SelectLastWindow => {
                if let Some(prev) = self.last_active_window {
                    self.switch_to_window(prev);
                }
            }
            Command::SelectLastPane => {
                if let Some(p) = self.active_window().last_pane() {
                    self.active_window_mut().focus(p);
                }
            }
            Command::MarkPane => {
                let a = self.active_window().active();
                if self.marked_pane == Some(a) {
                    self.marked_pane = None;
                    self.set_status_message("mark cleared".into(), Severity::Info);
                } else {
                    self.marked_pane = Some(a);
                    self.set_status_message("marked pane".into(), Severity::Success);
                }
            }
            Command::BreakPane => {
                if self.active_window().layout().panes().len() < 2 {
                    self.set_status_message("only pane in window".into(), Severity::Info);
                } else {
                    let active = self.active_window().active();
                    // invariant: the active pane is always in its window.
                    let pane = self
                        .active_window_mut()
                        .detach_pane(active)
                        .expect("active pane present");
                    self.active_window_mut().resize(viewport)?; // surviving source
                    let name =
                        pane.name().unwrap_or_else(|| format!("shell{}", self.next_window_id));
                    let id = WindowId(self.next_window_id);
                    self.next_window_id += 1;
                    let mut w = Window::from_pane(id, name, pane);
                    w.resize(viewport)?;
                    self.windows.push(w);
                    self.last_active_window = Some(self.active);
                    self.active = self.windows.len() - 1;
                }
            }
            Command::SwapPane(next) => {
                let active = self.active_window().active();
                if let Some(other) = self.active_window().neighbor_leaf(next) {
                    let w = self.active_window_mut();
                    w.layout_mut().swap_panes(active, other);
                    w.resize(viewport)?;
                }
            }
            Command::JoinPane(dir) => {
                if let Some(marked) = self.marked_pane {
                    let act_idx = self.active;
                    let act_pane = self.windows[act_idx].active();
                    if marked == act_pane {
                        self.set_status_message("marked pane is the active pane".into(), Severity::Info);
                    } else if let Some(src_idx) =
                        self.windows.iter().position(|w| w.pane(marked).is_some())
                    {
                        let act_wid = self.windows[act_idx].id;
                        if let Some(pane) = self.windows[src_idx].detach_pane(marked) {
                            if self.windows[src_idx].is_layout_empty() {
                                self.windows.remove(src_idx);
                                // invariant: the active window is never the emptied
                                // source, `marked != act_pane` (guarded above) means
                                // the active window keeps act_pane and stays alive.
                                self.active = self
                                    .windows
                                    .iter()
                                    .position(|w| w.id == act_wid)
                                    .expect("active window survives a join");
                                self.fixup_last_active_after_removal(src_idx);
                            } else {
                                // Source survives with a promoted layout, so resize it.
                                self.windows[src_idx].resize(viewport)?;
                            }
                            let act_idx = self.active;
                            let act_pane = self.windows[act_idx].active();
                            self.windows[act_idx]
                                .adopt_split(act_pane, dir, pane, viewport)?;
                            self.marked_pane = None;
                        }
                    } else {
                        self.marked_pane = None; // marked pane vanished
                    }
                } else {
                    self.set_status_message("no marked pane".into(), Severity::Info);
                }
            }
            Command::SwapMarkedPane => {
                if let Some(marked) = self.marked_pane {
                    let a = self.active_window().active();
                    if marked != a {
                        if self.active_window().pane(marked).is_some() {
                            let w = self.active_window_mut();
                            w.layout_mut().swap_panes(a, marked);
                            w.resize(viewport)?;
                        } else if let Some(other_idx) =
                            self.windows.iter().position(|w| w.pane(marked).is_some())
                        {
                            // Cross-window: exchange the slot occupants. Both
                            // layout shapes are preserved; focus/zoom follow
                            // the slot (the window helpers rewrite them) and
                            // the mark stays on M. Choreography: a map-only
                            // take of A breaks the ownership cycle, then one
                            // slot-replace per window, so no `Pane` is dropped
                            // on any path.
                            let act_idx = self.active;
                            // invariant: the active pane is always in its window.
                            let pane_a = self.windows[act_idx]
                                .take_pane(a)
                                .expect("active pane present");
                            match self.windows[other_idx].swap_occupant(marked, pane_a) {
                                Ok(pane_m) => {
                                    self.windows[act_idx].install_in_slot(a, pane_m);
                                    // The slots' rects differ, so size both
                                    // windows' PTYs to their new rects now.
                                    self.windows[act_idx].resize(viewport)?;
                                    self.windows[other_idx].resize(viewport)?;
                                }
                                Err(pane_a) => {
                                    // Unreachable single-threaded: the scan
                                    // above just found `marked` in that
                                    // window. Restore A's slot rather than
                                    // drop the pane.
                                    self.windows[act_idx].install_in_slot(a, pane_a);
                                }
                            }
                        } else {
                            self.marked_pane = None; // marked pane vanished
                        }
                    }
                } else {
                    self.set_status_message("no marked pane".into(), Severity::Info);
                }
            }
            Command::ToggleMonitorActivity => {
                let on = self.active_window_mut().toggle_monitor_activity();
                self.set_status_message(
                    format!("monitor-activity {}", if on { "on" } else { "off" }),
                    Severity::Info,
                );
            }
            Command::ToggleMonitorBell => {
                let on = self.active_window_mut().toggle_monitor_bell();
                self.set_status_message(
                    format!("monitor-bell {}", if on { "on" } else { "off" }),
                    Severity::Info,
                );
            }
            Command::ToggleMonitorCommand => {
                let on = self.active_window_mut().toggle_monitor_command();
                self.set_status_message(
                    format!("monitor-command {}", if on { "on" } else { "off" }),
                    Severity::Info,
                );
            }
            Command::SetMonitorSilence(secs) => {
                let threshold = secs.map(std::time::Duration::from_secs);
                self.active_window_mut().set_monitor_silence(threshold);
                self.set_status_message(
                    match secs {
                        Some(n) => format!("monitor-silence {n}s"),
                        None => "monitor-silence off".to_string(),
                    },
                    Severity::Info,
                );
            }
            Command::ResizePane(dir) => {
                let active = self.active_window().active();
                const STEP: i32 = 3;
                let (axis, delta) = match dir {
                    plexy_glass_mux::Direction::Left => (SplitDir::Vertical, -STEP),
                    plexy_glass_mux::Direction::Right => (SplitDir::Vertical, STEP),
                    plexy_glass_mux::Direction::Up => (SplitDir::Horizontal, -STEP),
                    plexy_glass_mux::Direction::Down => (SplitDir::Horizontal, STEP),
                };
                self.active_window_mut()
                    .layout_mut()
                    .resize_split(active, axis, delta, viewport);
                self.active_window_mut().resize(viewport)?;
            }
            Command::KillWindow => {
                // Capture the identity before the window is gone so the post-hoc
                // flash can name it (a fat-fingered `kill-window` is alarming
                // without acknowledgement; this is the lightweight alternative to
                // a blocking confirm).
                let idx = self.active;
                let name = self.active_window().display_name(self.config.auto_rename);
                self.close_active_window();
                self.set_status_message(
                    format!("killed window {} ({name})", idx + 1),
                    Severity::Success,
                );
            }
            Command::RenameWindow => self.open_rename_window(),
            Command::RenamePane => self.open_rename_pane(),
            Command::ShowHelp => self.open_help(),
            Command::ZoomToggle => {
                self.active_window_mut().toggle_zoom();
                // The zoom-aware resize handles both directions: it sizes a
                // newly-zoomed pane to the full viewport, or restores every
                // pane to its layout rect on un-zoom.
                self.active_window_mut().resize(viewport)?;
            }
            Command::ToggleSyncPanes => {
                let win = self.active_window_mut();
                win.sync_input = !win.sync_input;
            }
            Command::Detach | Command::Cancel => {}
            Command::ReloadConfig => {
                // Handled by serve_attach (needs registry access).
            }
            Command::CommandPrompt => {
                // Opened at the connection layer (needs the live session list
                // for Tab-completion); see serve_attach.
            }
            Command::ChooseSession => {
                // Opened at the connection layer (needs the live session list);
                // see serve_attach.
            }
            Command::ChooseTree => {
                // Opened at the connection layer (needs the live session list);
                // see serve_attach.
            }
            Command::History => {
                // Opened at the connection layer (needs the registry to walk
                // every session's blocks); see serve_attach.
            }
            Command::Hints => {
                // Opened at the connection layer (scans the active pane's grid
                // and builds hint targets); see serve_attach.
            }
            Command::PasteBuffer | Command::ChooseBuffer => {
                // Handled at the connection layer (needs the registry's paste
                // buffers); see serve_attach.
            }
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
            Command::OpenPopup { command } => {
                self.open_popup(command)?;
            }
            Command::ClosePopup => {
                self.close_popup();
            }
            Command::SelectLayout(preset) => {
                self.apply_layout_preset(preset)?;
            }
            Command::NextLayout => {
                self.next_layout()?;
            }
            // Block-scroll verbs operate on the wheel-scroll offset of the
            // ACTIVE pane (a popup swallows these chords at the connection
            // layer, so input target == active pane whenever they fire). If
            // the pane is in copy mode the chord still fires, since the
            // keymap consumes keys BEFORE copy-mode routing (only
            // PassThrough keys reach the copy-mode handler), but the offset
            // change is invisible until copy mode exits (the compositor
            // renders the copy-mode viewport instead; see `effective_scroll`).
            //
            // Viewport math (pinned by tests): at offset N the compositor
            // shows N scrollback rows above the grid, so the top visible
            // absolute line is `scrollback_len - N` (N=0 → grid row 0, i.e.
            // line `scrollback_len`).
            Command::PrevPrompt => {
                if let Some(pane) = self.active_window().active_pane() {
                    let offset = pane.scroll_offset();
                    // Fold-aware, visible-space: find the prompt above the current
                    // top line and the offset that lands it exactly at the top.
                    let target = pane.with_screen(|s| {
                        let rows = s.active.num_rows();
                        let top = plexy_glass_mux::blocks::scroll_line_at(s, rows, offset, 0);
                        plexy_glass_mux::prev_prompt_line(s, top).map(|t| {
                            (
                                plexy_glass_mux::blocks::scroll_offset_for_top(s, rows, t),
                                plexy_glass_mux::blocks::max_scroll_offset(s, rows),
                            )
                        })
                    });
                    // No prompt above → no-op (no wraparound).
                    if let Some((off, max)) = target {
                        pane.set_scroll_offset(off, max);
                    }
                }
            }
            Command::NextPrompt => {
                if let Some(pane) = self.active_window().active_pane() {
                    let offset = pane.scroll_offset();
                    let (off, max) = pane.with_screen(|s| {
                        let rows = s.active.num_rows();
                        let top = plexy_glass_mux::blocks::scroll_line_at(s, rows, offset, 0);
                        // Past the newest prompt, or one already in the live view
                        // (offset_for_top saturates to 0), snaps to live.
                        let off = plexy_glass_mux::next_prompt_line(s, top)
                            .map_or(0, |t| plexy_glass_mux::blocks::scroll_offset_for_top(s, rows, t));
                        (off, plexy_glass_mux::blocks::max_scroll_offset(s, rows))
                    });
                    pane.set_scroll_offset(off, max);
                }
            }
            Command::EnterBlockMode | Command::CopyOutput => {
                // Handled at the connection layer (needs the registry's paste
                // buffers / clipboard, or the session for the no-blocks status
                // message); see `serve_attach` / `run_connection_verb`.
            }
        }
        self.notify.notify_one();
        Ok(())
    }
}

/// Whether a command should clear an active zoom overlay before running.
/// Structural (split/kill/new-window) and navigation (window/pane switch,
/// resize) commands end zoom; `ZoomToggle`, sync-toggle, copy-mode, detach,
/// cancel, and reload do not. The block verbs (`PrevPrompt`/`NextPrompt`/
/// `CopyOutput`) are view-only, they scroll or read the zoomed pane itself,
/// so ending zoom would be hostile (like wheel scrolling, which also keeps it).
fn command_clears_zoom(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::SplitV
            | Command::SplitH
            | Command::KillPane
            | Command::KillWindow
            | Command::NewWindow
            | Command::NextWindow
            | Command::PrevWindow
            | Command::SelectWindow(_)
            | Command::SelectNextPane
            | Command::SelectPrevPane
            | Command::SelectPane(_)
            | Command::ResizePane(_)
            | Command::SelectLastWindow
            | Command::SelectLastPane
            | Command::BreakPane
            | Command::JoinPane(_)
            | Command::SwapPane(_)
            | Command::SwapMarkedPane
            | Command::SelectLayout(_)
            | Command::NextLayout
    )
}
