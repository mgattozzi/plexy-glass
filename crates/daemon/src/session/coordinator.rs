//! Render coordinator and related helpers extracted from `session.rs`.

use super::Session;
use plexy_glass_mux::VirtualScreen;
use plexy_glass_protocol::PtySize;
use std::sync::{Arc, atomic::Ordering};
use tokio::sync::watch;

/// Per-pane data captured under the window-manager lock, owned so the borrowed
/// `PaneView`s handed to the compositor don't keep the lock held during
/// `compose`.
pub(super) struct OwnedPane {
    pub(super) id: plexy_glass_mux::PaneId,
    pub(super) rect: plexy_glass_mux::Rect,
    pub(super) screen: plexy_glass_emulator::Screen,
    pub(super) is_active: bool,
    pub(super) scroll: u32,
    pub(super) copy_mode: Option<plexy_glass_mux::CopyMode>,
    pub(super) name: Option<String>,
}

pub(super) async fn render_coordinator(
    session: Arc<Session>,
    frame_tx: watch::Sender<Arc<VirtualScreen>>,
) {
    use plexy_glass_mux::{Compositor, PaneView, StatusLine};
    use std::time::Duration;
    const DEBOUNCE: Duration = Duration::from_millis(16);

    loop {
        session.notify.notified().await;
        // Debounce a few notifications.
        let n = Arc::clone(&session.notify);
        let _ = tokio::time::timeout(DEBOUNCE, async move {
            loop {
                n.notified().await;
            }
        })
        .await;

        // Kill teardown: when the session is closing, emit a final blank
        // frame and exit so frame_tx drops and attached clients detach.
        if session.closing.load(Ordering::SeqCst) {
            let host = { session.window_manager.lock().await.host_size() };
            let _ = frame_tx.send(Arc::new(build_session_end_frame(host)));
            break;
        }

        let frame = {
            let mut m = session.window_manager.lock().await;
            if m.is_empty() {
                let host = m.host_size();
                let virt = build_session_end_frame(host);
                let _ = frame_tx.send(Arc::new(virt));
                break;
            }
            // Sole drainer of the per-pane activity/bell signals into the per-window
            // sticky flags. Must run before any immutable borrow of `m` below.
            m.update_monitor_flags();
            let host = m.host_size();
            let viewport = m.viewport();
            let win = m.active_window();
            let layout = win.layout();
            let active_id = win.active();
            let marked_pane = m.marked_pane();
            // Ignore a zoom that points at a pane that no longer exists, so a
            // momentarily-stale overlay falls back to rendering all panes
            // instead of a blank viewport.
            let zoomed = win.zoomed.filter(|zid| win.pane(*zid).is_some());

            // When zoomed, render ONLY the zoomed pane at the full viewport;
            // otherwise render every pane at its layout rect.
            let pane_ids: Vec<plexy_glass_mux::PaneId> = match zoomed {
                Some(zid) => vec![zid],
                None => layout.panes(),
            };
            let mut owned: Vec<OwnedPane> = Vec::with_capacity(pane_ids.len());
            for id in pane_ids {
                if let Some(pane) = win.pane(id) {
                    let rect = if zoomed == Some(id) {
                        viewport
                    } else {
                        match layout.rect_of(id, viewport) {
                            Some(r) => r,
                            None => continue,
                        }
                    };
                    owned.push(OwnedPane {
                        id,
                        rect,
                        screen: pane.with_screen(|s| s.clone()),
                        is_active: id == active_id,
                        scroll: pane.scroll_offset(),
                        copy_mode: pane.with_copy_mode(|cm| cm.clone()),
                        name: pane.name(),
                    });
                }
            }
            let views: Vec<PaneView> = owned
                .iter()
                .map(|p| PaneView {
                    id: p.id,
                    rect: p.rect,
                    screen: &p.screen,
                    is_active: p.is_active,
                    scroll_offset: p.scroll,
                    copy_mode: p.copy_mode.as_ref(),
                    title: p.name.as_deref(),
                    marked: marked_pane == Some(p.id),
                })
                .collect();

            // Snapshot the floating popup for this frame (screen + title + rect).
            let popup_owned: Option<(plexy_glass_emulator::Screen, String, plexy_glass_mux::Rect)> =
                m.popup().map(|p| {
                    (
                        p.pane.with_screen(|s| s.clone()),
                        p.title.clone(),
                        plexy_glass_mux::popup_rect(m.viewport()),
                    )
                });

            // Build event-driven widget context, refresh, snapshot.
            let session_name = session.name.clone();
            let attached_clients = session.clients.lock().await.len() as u8;
            let windows_data: Vec<plexy_glass_status::WindowSummary> = m
                .windows()
                .iter()
                .enumerate()
                .map(|(i, w)| plexy_glass_status::WindowSummary {
                    name: w.name.clone(),
                    active: i == m.active_idx(),
                    activity: w.activity_flag(),
                    bell: w.bell_flag(),
                })
                .collect();
            let active_pane_cwd = m
                .active_window()
                .active_pane()
                .and_then(|p| p.with_screen(|s| s.cwd.clone()));
            let copy_mode_active = m
                .active_window()
                .active_pane()
                .map(|p| p.is_in_copy_mode())
                .unwrap_or(false);
            let sync_active = m.active_window().sync_input;
            let zoom_active = m.active_window().is_zoomed();
            let ctx = plexy_glass_status::EvalContext {
                session_name: &session_name,
                windows: &windows_data,
                active_window: m.active_idx(),
                attached_clients,
                prefix_active: false,
                active_pane_cwd: active_pane_cwd.as_deref(),
                copy_mode_active,
                sync_active,
                zoom_active,
            };
            let engine = session.status_engine_snapshot();
            engine.refresh_event_driven(&ctx).await;
            // Also flush any interval widgets whose deadline has passed. On
            // the first render this populates widgets the tick task hasn't
            // had a chance to evaluate yet (initial next_due is None, so
            // they're all considered due); on subsequent renders it's a
            // cheap no-op when the tick task is keeping up.
            let _ = engine.refresh_due_intervals(&ctx).await;
            let snap = engine.snapshot().await;
            // Push clickable regions to the window manager so the next
            // status-bar click can dispatch the matching command (M10).
            let hits = snap.click_hits();
            let host_size = m.host_size();
            // Honor the configured status-bar position for both the click row
            // and the compositor placement.
            let placement = match session.config_snapshot().status.position {
                plexy_glass_config::Position::Top => plexy_glass_mux::StatusPlacement::Top,
                plexy_glass_config::Position::Bottom => plexy_glass_mux::StatusPlacement::Bottom,
            };
            let (status_row, pane_row_offset) = match placement {
                plexy_glass_mux::StatusPlacement::Top => (0u16, 1u16),
                plexy_glass_mux::StatusPlacement::Bottom => {
                    (host_size.rows.saturating_sub(1), 0u16)
                }
            };
            m.set_status_layout(Some(status_row), pane_row_offset);
            m.set_status_hits(hits);
            let status = StatusLine {
                left: snap.left.into_iter().flatten().collect(),
                middle: snap.middle.into_iter().flatten().collect(),
                right: snap.right.into_iter().flatten().collect(),
            };
            let selection = m.selection().cloned();
            // Transient status-line message (cleared lazily here when expired).
            let message: Option<String> = m.take_active_message().map(str::to_string);

            // Build the active overlay's render view (rename prompt / help).
            // `help_lines` is deferred-init so the Help view can borrow it.
            let help_lines: Vec<(String, String)>;
            let overlay_view = match m.overlay() {
                Some(plexy_glass_mux::Overlay::Rename { target, buf }) => {
                    let label = match target {
                        plexy_glass_mux::RenameTarget::Window => "rename window",
                        plexy_glass_mux::RenameTarget::Pane => "rename pane",
                    };
                    Some(plexy_glass_mux::OverlayView::RenamePrompt { label, buf })
                }
                Some(plexy_glass_mux::Overlay::Help { scroll }) => {
                    let cfg = session.config_snapshot();
                    help_lines = build_help_lines(&cfg);
                    Some(plexy_glass_mux::OverlayView::Help { lines: &help_lines, scroll: *scroll })
                }
                Some(plexy_glass_mux::Overlay::Command { buf, .. }) => {
                    Some(plexy_glass_mux::OverlayView::Command { buf })
                }
                Some(plexy_glass_mux::Overlay::SessionPicker { entries, filter, selected }) => {
                    Some(plexy_glass_mux::OverlayView::SessionPicker {
                        entries,
                        filter,
                        selected: *selected,
                    })
                }
                Some(plexy_glass_mux::Overlay::Tree(state)) => {
                    Some(plexy_glass_mux::OverlayView::Tree { state })
                }
                Some(plexy_glass_mux::Overlay::BufferPicker(state)) => {
                    Some(plexy_glass_mux::OverlayView::Buffer { state })
                }
                None => None,
            };

            let popup_view = popup_owned.as_ref().map(|(screen, title, rect)| {
                plexy_glass_mux::PopupView { rect: *rect, screen, title }
            });

            Compositor::compose(
                &views,
                (host.rows, host.cols),
                Some(&status),
                placement,
                selection.as_ref(),
                overlay_view.as_ref(),
                message.as_deref(),
                popup_view.as_ref(),
            )
        };
        let _ = frame_tx.send(Arc::new(frame));
    }
    session.closing.store(true, Ordering::SeqCst);
    // frame_tx drops here; subscribers will see frame_rx.changed() return Err
    // and exit their loops, which closes their sockets and lets clients restore.
}

pub(super) fn build_session_end_frame(host: PtySize) -> plexy_glass_mux::VirtualScreen {
    plexy_glass_mux::VirtualScreen::blank(host.rows, host.cols)
}

/// Build the effective keybinding list for the help overlay: the built-in
/// defaults (when `inherit_defaults`) overlaid with the user's bindings, later
/// bindings overriding earlier ones by key chord, preserving first-seen order.
pub(super) fn build_help_lines(config: &plexy_glass_config::Config) -> Vec<(String, String)> {
    fn upsert(
        ordered: &mut Vec<(String, String)>,
        index: &mut std::collections::HashMap<String, usize>,
        keys: &str,
        command: &str,
    ) {
        let entry = (keys.to_string(), command_label(command));
        if let Some(&i) = index.get(keys) {
            ordered[i] = entry;
        } else {
            index.insert(keys.to_string(), ordered.len());
            ordered.push(entry);
        }
    }
    let km = &config.keymap;
    let mut ordered: Vec<(String, String)> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    if km.inherit_defaults {
        for b in plexy_glass_config::built_in_keymap().bindings {
            upsert(&mut ordered, &mut index, &b.keys, &b.command);
        }
    }
    for b in &km.bindings {
        upsert(&mut ordered, &mut index, &b.keys, &b.command);
    }
    ordered
}

/// Friendly label for a keymap command string; falls back to the raw command.
fn command_label(command: &str) -> String {
    let label = match command {
        "new_window" => "New window",
        "split_v" => "Split vertical",
        "split_h" => "Split horizontal",
        "kill_pane" => "Kill pane",
        "kill_window" => "Kill window",
        "zoom_toggle" => "Zoom pane",
        "next_window" => "Next window",
        "prev_window" => "Previous window",
        "detach" => "Detach",
        "cancel" => "Cancel",
        "enter_copy_mode" => "Copy mode",
        "toggle_sync_panes" => "Toggle sync panes",
        "reload_config" => "Reload config",
        "select_next_pane" => "Next pane",
        "select_prev_pane" => "Previous pane",
        "select_pane_left" => "Focus pane left",
        "select_pane_right" => "Focus pane right",
        "select_pane_up" => "Focus pane up",
        "select_pane_down" => "Focus pane down",
        "resize_pane_left" => "Resize pane left",
        "resize_pane_right" => "Resize pane right",
        "resize_pane_up" => "Resize pane up",
        "resize_pane_down" => "Resize pane down",
        "select_last_window" => "Last window",
        "select_last_pane" => "Last pane",
        "rename_window" => "Rename window",
        "rename_pane" => "Rename pane",
        "show_help" => "Help",
        "command_prompt" => "Command prompt",
        "choose_session" => "Choose session",
        "choose_tree" => "Choose tree",
        "mark_pane" => "Mark pane",
        "break_pane" => "Break pane",
        "swap_pane_next" => "Swap pane next",
        "swap_pane_prev" => "Swap pane prev",
        "join_pane" => "Join pane",
        "swap_marked_pane" => "Swap marked pane",
        "paste_buffer" => "Paste buffer",
        "choose_buffer" => "Choose buffer",
        "toggle_monitor_activity" => "Monitor activity",
        "toggle_monitor_bell" => "Monitor bell",
        "popup" => "Popup (scratch shell)",
        "close_popup" => "Close popup",
        "next_layout" => "Next layout",
        other => {
            if let Some(n) = other
                .strip_prefix("select_window:")
                .and_then(|x| x.parse::<u32>().ok())
            {
                return format!("Select window {}", n + 1);
            }
            if let Some(cmd) = other.strip_prefix("popup:") {
                return format!("Popup: {cmd}");
            }
            if let Some(name) = other.strip_prefix("layout:") {
                return format!("Layout: {name}");
            }
            return other.to_string();
        }
    };
    label.to_string()
}
