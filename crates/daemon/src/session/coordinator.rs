//! Render coordinator and related helpers extracted from `session.rs`.

use super::Session;
use plexy_glass_mux::VirtualScreen;
use std::sync::{Arc, atomic::Ordering};
use tokio::sync::watch;

/// Per-pane data captured under the window-manager lock, owned so the borrowed
/// `PaneView`s handed to the compositor don't keep the lock held during
/// `compose`.
struct OwnedPane {
    id: plexy_glass_mux::PaneId,
    rect: plexy_glass_mux::Rect,
    screen: plexy_glass_emulator::Screen,
    is_active: bool,
    scroll: u32,
    copy_mode: Option<plexy_glass_mux::CopyMode>,
    block_mode: Option<plexy_glass_mux::BlockMode>,
    name: Option<String>,
}

pub(super) async fn render_coordinator(
    session: Arc<Session>,
    frame_tx: watch::Sender<Arc<VirtualScreen>>,
) {
    use plexy_glass_mux::{PaneView, StatusLine};
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
            let _ = frame_tx.send(Arc::new(VirtualScreen::blank(host.rows, host.cols)));
            break;
        }

        // Set true when the monitor drain emitted an alert message under the
        // WM lock below; the TTL-expiry repaint wake is scheduled after the
        // lock releases (scheduling re-borrows the session and must not nest
        // under the WM guard, and `Session::set_status_message`, which would,
        // deadlocks here because it re-locks the WM).
        let monitor_drain;
        let frame = {
            let mut m = session.window_manager.lock().await;
            if m.is_empty() {
                let host = m.host_size();
                let _ = frame_tx.send(Arc::new(VirtualScreen::blank(host.rows, host.cols)));
                break;
            }
            // Sole drainer of the per-pane activity/bell signals → per-window
            // sticky flags. Must run before any immutable borrow of `m` below.
            // Emits monitor-alert messages on a flag's false→true edge.
            monitor_drain = m.update_monitor_flags();
            let host = m.host_size();
            let viewport = m.viewport();
            let win = m.active_window();
            let layout = win.layout();
            let active_id = win.active();
            let marked_pane = m.marked_pane();
            let pane_drag = m.pane_drag_roles();
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
                        block_mode: pane.with_block_mode(|bm| bm.clone()),
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
                    block_mode: p.block_mode.as_ref(),
                    title: p.name.as_deref(),
                    marked: marked_pane == Some(p.id),
                    drag_role: match pane_drag {
                        Some((src, _)) if src == p.id => plexy_glass_mux::PaneDragRole::Source,
                        Some((_, Some(tgt))) if tgt == p.id => plexy_glass_mux::PaneDragRole::Target,
                        _ => plexy_glass_mux::PaneDragRole::None,
                    },
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
            let session_name = session.name();
            let attached_clients = session.clients.lock().await.len() as u8;
            let auto_rename = session.config_snapshot().auto_rename;
            let windows_data: Vec<plexy_glass_status::WindowSummary> = m
                .windows()
                .iter()
                .map(|w| plexy_glass_status::WindowSummary {
                    name: w.display_name(auto_rename),
                    activity: w.activity_flag(),
                    bell: w.bell_flag(),
                    done: w.done_flag(),
                    silence: w.silence_flag(),
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
            // Any-client-armed aggregate; same WM→clients lock order as the
            // `attached_clients` read above.
            let prefix_active = session.any_prefix_armed().await;
            let ctx = plexy_glass_status::EvalContext {
                session_name: &session_name,
                windows: &windows_data,
                active_window: m.active_idx(),
                attached_clients,
                prefix_active,
                active_pane_cwd: active_pane_cwd.as_deref(),
                copy_mode_active,
                sync_active,
                zoom_active,
                dragging_window: m.dragging_window_idx(),
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

            let glyphs =
                plexy_glass_status::GlyphSet::for_tier(session.config_snapshot().glyph_tier);
            // Flow the window list (middle zone) into the left powerline run so
            // the window names get the same arrows/caps as session/prefix instead
            // of rendering as flat blocks. Each window becomes its own group (the
            // window-list widget emits them together) so arrows appear BETWEEN
            // windows. The right cluster stays edge-anchored; middle is now empty.
            let mut left_groups = snap.left;
            left_groups.extend(snap.middle.into_iter().flatten().map(|seg| vec![seg]));
            let left = plexy_glass_status::powerline_zone(
                left_groups,
                plexy_glass_status::Cluster::Left,
                glyphs,
            );
            let right = plexy_glass_status::powerline_zone(
                snap.right,
                plexy_glass_status::Cluster::Right,
                glyphs,
            );

            // Clickable regions, computed from the FINAL painted segments
            // (powerline arrows/padding included) so window-name clicks land on
            // the right window: the left run paints at col 0 and the right cluster
            // is edge-anchored at `cols - right_w`. Arrow/padding cells carry no
            // `click_action`, so they're skipped.
            fn zone_hits(
                segments: &[plexy_glass_status::Segment],
                start: u16,
            ) -> Vec<plexy_glass_status::StatusHit> {
                let mut out = Vec::new();
                let mut col = start;
                for seg in segments {
                    let w = plexy_glass_emulator::display_width(&seg.text);
                    if let Some(action) = seg.click_action {
                        out.push(plexy_glass_status::StatusHit {
                            col_range: col..col.saturating_add(w),
                            action,
                        });
                    }
                    col = col.saturating_add(w);
                }
                out
            }
            let right_w = right
                .iter()
                .map(|s| plexy_glass_emulator::display_width(&s.text))
                .fold(0u16, |a, w| a.saturating_add(w));
            let mut hits = zone_hits(&left, 0);
            hits.extend(zone_hits(&right, host_size.cols.saturating_sub(right_w)));
            m.set_status_hits(hits);

            let status = StatusLine { left, middle: Vec::new(), right };
            let selection = m.selection().cloned();
            // Transient status-line message (cleared lazily here when expired);
            // peek the severity before taking the text so it can be styled.
            let message_severity = m.active_severity();
            let message: Option<(String, crate::window_manager::Severity)> = m
                .take_active_message()
                .map(|t| (t.to_string(), message_severity));

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
                Some(plexy_glass_mux::Overlay::History(state)) => {
                    Some(plexy_glass_mux::OverlayView::History { state })
                }
                Some(plexy_glass_mux::Overlay::Hint(state)) => {
                    let cfg = session.config_snapshot();
                    Some(plexy_glass_mux::OverlayView::Hint {
                        state,
                        colors: hint_colors(&cfg),
                    })
                }
                None => None,
            };

            let popup_view = popup_owned.as_ref().map(|(screen, title, rect)| {
                plexy_glass_mux::PopupView { rect: *rect, screen, title }
            });

            // Build the block-border color pair from the session's current config
            // so that live-reload updates apply for free on the next compose call.
            let block_colors = block_border_colors(&session.config_snapshot());
            let block_select = block_select_color(&session.config_snapshot());
            let chrome = chrome_colors(&session.config_snapshot());

            // Resolve the transient message's glyph + colors from the current
            // config (severity → palette color + tier glyph), mirroring how the
            // block colors are pre-resolved so the compositor stays palette-free.
            let message_view = message.as_ref().map(|(text, severity)| {
                let cfg = session.config_snapshot();
                let (fg, bg) = message_colors(&cfg, *severity);
                plexy_glass_mux::MessageView {
                    text: text.as_str(),
                    glyph: severity.glyph(cfg.glyph_tier),
                    fg,
                    bg,
                }
            });

            plexy_glass_mux::compositor::compose(
                &views,
                (host.rows, host.cols),
                Some(&status),
                placement,
                selection.as_ref(),
                overlay_view.as_ref(),
                message_view,
                popup_view.as_ref(),
                block_colors.as_ref(),
                block_select,
                chrome,
            )
        };
        // The WM lock is released; if the drain set an alert message, schedule
        // its TTL-expiry repaint wake now (see `update_monitor_flags`).
        if monitor_drain.alert_edge {
            session.schedule_status_expiry_wake();
        }
        // Desktop notifications for qualifying completions (off the WM lock).
        if !monitor_drain.notifications.is_empty() {
            let cfg = session.config_snapshot();
            let nt = &cfg.notifications;
            if nt.enabled {
                let (attached, any_focused, focus_reported) = session.client_attention().await;
                // Unknown focus (terminal doesn't report ?1004) counts as focused,
                // so we never fire a false toast; a reported FocusOut makes the
                // active window "unattended" too, which is what the user asked for.
                let terminal_focused = !focus_reported || any_focused;
                let title = format!("plexy-glass: {}", session.name());
                for p in &monitor_drain.notifications {
                    let attended = attached > 0 && p.is_active_window && terminal_focused;
                    if should_notify(nt.enabled, nt.min_duration_ms, p.event.duration_ms, attended) {
                        notify_desktop(title.clone(), notification_body(p));
                    }
                }
            }
        }
        let _ = frame_tx.send(Arc::new(frame));
    }
    session.closing.store(true, Ordering::SeqCst);
    // frame_tx drops here; subscribers will see frame_rx.changed() return Err
    // and exit their loops, which closes their sockets and lets clients restore.
}

/// Pure desktop-notification policy: notify only for an enabled, **unattended**
/// completion whose duration is known and ≥ the threshold. `attended` means a
/// client is attached AND the completing window is the active one.
fn should_notify(enabled: bool, min_ms: u32, duration_ms: Option<u32>, attended: bool) -> bool {
    enabled && !attended && duration_ms.is_some_and(|d| d >= min_ms)
}

/// Notification body: `"✓ cargo build · exit 0 · 2m03s"` (command best-effort;
/// falls back to the window when the command can't be extracted).
fn notification_body(p: &crate::window_manager::PendingNotification) -> String {
    use plexy_glass_mux::blocks::format_duration;
    let dur = p.event.duration_ms.map(format_duration).unwrap_or_default();
    let glyph = match p.event.exit {
        Some(0) => "\u{2713} ", // ✓
        Some(_) => "\u{2717} ", // ✗
        None => "",
    };
    let exit = match p.event.exit {
        Some(c) => format!(" \u{b7} exit {c}"),
        None => String::new(),
    };
    match &p.event.command {
        Some(cmd) => format!("{glyph}{cmd}{exit} \u{b7} {dur}"),
        None => format!("window {} ({}){exit} \u{b7} {dur}", p.window_index + 1, p.window_name),
    }
}

/// The platform notifier command + args for `(title, body)`. Pure, for testing.
///
/// macOS: `osascript`. A bare CLI binary has no app bundle, so the modern
/// notification API (and `notify-rust`'s deprecated `NSUserNotification` path)
/// silently drop its toasts; `osascript`'s `display notification` routes through
/// Script Editor (a signed, authorized app) and actually shows. Title/body are
/// passed as `argv` (`on run argv`), never interpolated into the script, so no
/// AppleScript/shell injection from command text.
///
/// Linux: `notify-send` (libnotify), title then body as args.
fn notification_argv(macos: bool, title: &str, body: &str) -> (&'static str, Vec<String>) {
    if macos {
        (
            "osascript",
            vec![
                "-e".into(),
                "on run argv".into(),
                "-e".into(),
                "display notification (item 1 of argv) with title (item 2 of argv)".into(),
                "-e".into(),
                "end run".into(),
                body.to_string(),  // item 1 of argv
                title.to_string(), // item 2 of argv
            ],
        )
    } else {
        ("notify-send", vec![title.to_string(), body.to_string()])
    }
}

/// Fire a desktop notification by spawning the platform notifier, off any lock.
/// Failures (notifier missing, no desktop session) are logged and swallowed,
/// never propagated and never panicking. The child is reaped asynchronously so
/// it can't leave a zombie. Not called by tests (it would pop a real toast).
fn notify_desktop(title: String, body: String) {
    let (program, args) = notification_argv(cfg!(target_os = "macos"), &title, &body);
    match tokio::process::Command::new(program)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
        Err(e) => tracing::warn!(error = %e, program, "desktop notification failed to spawn"),
    }
}

/// Substitute the `prefix` token (word-wise, case-insensitive) with the
/// configured prefix string and rejoin with single spaces.
///
/// Edge: if the configured prefix string is not a valid single chord, the
/// keymap already fell back to Ctrl+a at build time (and warned), but here we
/// substitute the raw configured string verbatim. The config is already broken
/// and has been warned about, so this is acceptable display drift; keep it
/// honest.
fn substitute_prefix_token(keys: &str, prefix: &str) -> String {
    let parts: Vec<&str> = keys
        .split_whitespace()
        .map(|tok| if tok.eq_ignore_ascii_case("prefix") { prefix } else { tok })
        .collect();
    parts.join(" ")
}

/// Build the effective keybinding list for the help overlay: the built-in
/// defaults (when `inherit_defaults`) overlaid with the user's bindings, later
/// bindings overriding earlier ones by key chord, preserving first-seen order.
fn build_help_lines(config: &plexy_glass_config::Config) -> Vec<(String, String)> {
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
    let prefix = &km.prefix;
    let mut ordered: Vec<(String, String)> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    if km.inherit_defaults {
        for b in plexy_glass_config::built_in_keymap().bindings {
            let resolved = substitute_prefix_token(&b.keys, prefix);
            upsert(&mut ordered, &mut index, &resolved, &b.command);
        }
    }
    for b in &km.bindings {
        let resolved = substitute_prefix_token(&b.keys, prefix);
        upsert(&mut ordered, &mut index, &resolved, &b.command);
    }
    // Orientation header: the three things a first-timer most needs (what the
    // prefix is, how to leave without killing their work, and where config
    // lives), prepended above the binding table and kept out of the dedup index
    // so the binding loop can't clobber the annotated detach line.
    // Note: the key column must never contain the literal word "prefix" (an
    // unresolved-token guard checks for it), so the first row keys on the
    // resolved prefix string itself, not the label "Prefix".
    let mut lines: Vec<(String, String)> = vec![
        (prefix.clone(), "the prefix — press it, then a key below".to_string()),
        (format!("{prefix} d"), "Detach — the session keeps running".to_string()),
        ("Config".to_string(), config_dir_hint()),
        (String::new(), String::new()),
    ];
    lines.extend(ordered);
    lines
}

/// Platform location of `config.kdl`, for the help-overlay orientation header.
fn config_dir_hint() -> String {
    #[cfg(target_os = "macos")]
    {
        "~/Library/Application Support/plexy-glass/config.kdl".to_string()
    }
    #[cfg(not(target_os = "macos"))]
    {
        "~/.config/plexy-glass/config.kdl".to_string()
    }
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
        "history" => "History palette",
        "hints" => "Hint mode",
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
        "prev_prompt" => "Previous command",
        "next_prompt" => "Next command",
        "copy_output" => "Copy last output",
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

// ── Block-border color resolution ─────────────────────────────────────────────

/// Fallback `Rgb` values for "ok" and "alert" from the built-in palette
/// (crates/config/src/default.rs line 28, 31).
/// Used when `resolve_color` fails to resolve a user-supplied name/hex.
const DEFAULT_OK_RGB: (u8, u8, u8) = (0x87, 0xa9, 0x87); // #87a987
const DEFAULT_ALERT_RGB: (u8, u8, u8) = (0xc4, 0x74, 0x6e); // #c4746e
const DEFAULT_SELECT_RGB: (u8, u8, u8) = (0xdc, 0xa5, 0x61); // #dca561

/// Resolve the hint-mode label/match colors from config.
pub(super) fn hint_colors(cfg: &plexy_glass_config::Config) -> plexy_glass_mux::HintColors {
    let resolve = |name: &str, def: (u8, u8, u8)| {
        let rgb = plexy_glass_status::resolve_color(name, &cfg.palette).unwrap_or(
            plexy_glass_status::Rgb { r: def.0, g: def.1, b: def.2 },
        );
        plexy_glass_emulator::Color::Rgb(rgb.r, rgb.g, rgb.b)
    };
    plexy_glass_mux::HintColors {
        label_fg: resolve(&cfg.hints.label_fg, (29, 28, 25)),
        label_bg: resolve(&cfg.hints.label_bg, (196, 178, 138)),
        match_fg: resolve(&cfg.hints.match_fg, (135, 169, 135)),
    }
}

/// Resolve the palette-driven chrome colors (pane border rings + overlay box
/// styling) from config, so the rings and overlay boxes match the theme instead
/// of clashing fixed ANSI indices. Mirrors [`block_border_colors`]: each name
/// resolves via `resolve_color`, falling back to the built-in palette default.
pub(super) fn chrome_colors(cfg: &plexy_glass_config::Config) -> plexy_glass_mux::ChromeColors {
    let resolve = |name: &str, def: (u8, u8, u8)| {
        let rgb = plexy_glass_status::resolve_color(name, &cfg.palette)
            .unwrap_or(plexy_glass_status::Rgb { r: def.0, g: def.1, b: def.2 });
        plexy_glass_emulator::Color::Rgb(rgb.r, rgb.g, rgb.b)
    };
    plexy_glass_mux::ChromeColors {
        rings: plexy_glass_mux::RingColors {
            active: resolve("highlight", (0xb6, 0x92, 0x7b)),
            marked: resolve("warn", (0xc4, 0xb2, 0x8a)),
            drag_source: resolve("info", (0x94, 0x9f, 0xb5)),
            drag_target: resolve("ok", DEFAULT_OK_RGB),
        },
        overlay_border: resolve("accent", (0x73, 0x7c, 0x73)),
        overlay_title: resolve("highlight", (0xb6, 0x92, 0x7b)),
        overlay_footer: resolve("muted", (0xb6, 0x92, 0x7b)),
        overlay_bg: resolve("bg_bar", (0x28, 0x27, 0x27)),
    }
}

/// Resolve `(fg, bg)` for a transient status-line message of the given severity.
/// `fg` is the severity's palette color; `bg` is `bg_bar`, so the bar blends
/// with the themed status bar instead of the harsh global inverse. Fallbacks
/// match the built-in kanagawa-dragon palette (crates/config/src/default.rs).
pub(super) fn message_colors(
    cfg: &plexy_glass_config::Config,
    severity: crate::window_manager::Severity,
) -> (plexy_glass_emulator::Color, plexy_glass_emulator::Color) {
    use crate::window_manager::Severity;
    let resolve = |name: &str, def: (u8, u8, u8)| {
        let rgb = plexy_glass_status::resolve_color(name, &cfg.palette).unwrap_or(
            plexy_glass_status::Rgb { r: def.0, g: def.1, b: def.2 },
        );
        plexy_glass_emulator::Color::Rgb(rgb.r, rgb.g, rgb.b)
    };
    let fg_def = match severity {
        Severity::Info => (0x94, 0x9f, 0xb5),    // info
        Severity::Success => DEFAULT_OK_RGB,     // ok
        Severity::Warn => (0xc4, 0xb2, 0x8a),    // warn
        Severity::Error => DEFAULT_ALERT_RGB,    // alert
    };
    let fg = resolve(severity.palette_key(), fg_def);
    let bg = resolve("bg_bar", (0x28, 0x27, 0x27)); // bg_bar
    (fg, bg)
}

/// Resolve the block-mode selection-bracket color from config. Always returns a
/// color (unlike [`block_border_colors`], which is `None` when blocks are
/// disabled), since the bracket is independent of the block-status feature.
pub(super) fn block_select_color(
    cfg: &plexy_glass_config::Config,
) -> plexy_glass_emulator::Color {
    let rgb = plexy_glass_status::resolve_color(&cfg.blocks.select_color, &cfg.palette)
        .unwrap_or(plexy_glass_status::Rgb {
            r: DEFAULT_SELECT_RGB.0,
            g: DEFAULT_SELECT_RGB.1,
            b: DEFAULT_SELECT_RGB.2,
        });
    plexy_glass_emulator::Color::Rgb(rgb.r, rgb.g, rgb.b)
}

/// Build an `Option<BlockBorderColors>` from the session's current config.
///
/// Returns `None` when `blocks.enabled` is `false`. Otherwise resolves each
/// color name/hex via `resolve_color`; if resolution fails, falls back to the
/// built-in palette defaults so the feature stays enabled even when the config
/// contains an unrecognised color string.
pub(super) fn block_border_colors(
    cfg: &plexy_glass_config::Config,
) -> Option<plexy_glass_mux::BlockBorderColors> {
    if !cfg.blocks.enabled {
        return None;
    }
    let palette = &cfg.palette;
    // resolve_color failed (bad palette name or malformed hex) → fall back to
    // the hard-coded default so the feature keeps painting.
    let ok_rgb = plexy_glass_status::resolve_color(&cfg.blocks.ok_color, palette)
        .unwrap_or(plexy_glass_status::Rgb {
            r: DEFAULT_OK_RGB.0,
            g: DEFAULT_OK_RGB.1,
            b: DEFAULT_OK_RGB.2,
        });
    let fail_rgb = plexy_glass_status::resolve_color(&cfg.blocks.fail_color, palette)
        .unwrap_or(plexy_glass_status::Rgb {
            r: DEFAULT_ALERT_RGB.0,
            g: DEFAULT_ALERT_RGB.1,
            b: DEFAULT_ALERT_RGB.2,
        });
    Some(plexy_glass_mux::BlockBorderColors {
        ok: plexy_glass_emulator::Color::Rgb(ok_rgb.r, ok_rgb.g, ok_rgb.b),
        fail: plexy_glass_emulator::Color::Rgb(fail_rgb.r, fail_rgb.g, fail_rgb.b),
        duration_threshold_ms: cfg
            .blocks
            .duration
            .then_some(cfg.blocks.duration_threshold_ms),
        sticky_header: cfg.blocks.sticky_header,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexy_glass_config::{KeymapBinding, built_in_default};

    fn binding(keys: &str, command: &str) -> KeymapBinding {
        KeymapBinding { keys: keys.into(), command: command.into() }
    }

    #[test]
    fn should_notify_policy_matrix() {
        // enabled, min_ms, duration, attended
        assert!(should_notify(true, 30_000, Some(30_000), false), "long + unattended → notify");
        assert!(should_notify(true, 30_000, Some(45_000), false));
        assert!(!should_notify(true, 30_000, Some(29_999), false), "below threshold");
        assert!(!should_notify(true, 30_000, Some(60_000), true), "attended → suppress");
        assert!(!should_notify(false, 0, Some(99_999), false), "disabled");
        assert!(!should_notify(true, 30_000, None, false), "no duration → never");
        assert!(should_notify(true, 0, Some(1), false), "min 0 notifies any unattended");
    }

    #[test]
    fn notification_argv_macos_uses_osascript_with_argv() {
        let (prog, args) = notification_argv(true, "plexy-glass: web", "\u{2713} cargo build \u{b7} 2m03s");
        assert_eq!(prog, "osascript");
        // Title/body are the trailing argv (body = item 1, title = item 2), never
        // interpolated into the script, that's the injection-safe contract.
        assert_eq!(args[args.len() - 2], "\u{2713} cargo build \u{b7} 2m03s");
        assert_eq!(args[args.len() - 1], "plexy-glass: web");
        assert!(args.contains(&"display notification (item 1 of argv) with title (item 2 of argv)".to_string()));
    }

    #[test]
    fn notification_argv_linux_uses_notify_send() {
        let (prog, args) = notification_argv(false, "T", "B");
        assert_eq!(prog, "notify-send");
        assert_eq!(args, vec!["T".to_string(), "B".to_string()]);
    }

    #[test]
    fn notification_body_formats_command_exit_duration() {
        use crate::window::CompletionEvent;
        let p = crate::window_manager::PendingNotification {
            window_index: 1,
            window_name: "api".into(),
            is_active_window: false,
            event: CompletionEvent {
                exit: Some(1),
                duration_ms: Some(45_000),
                command: Some("cargo test".into()),
            },
        };
        let body = notification_body(&p);
        assert!(body.contains("cargo test"), "command: {body:?}");
        assert!(body.contains("exit 1"), "exit: {body:?}");
        assert!(body.contains("45s"), "duration: {body:?}");
        assert!(body.contains('\u{2717}'), "fail glyph: {body:?}");
        // No command → window fallback.
        let p2 = crate::window_manager::PendingNotification {
            event: CompletionEvent { command: None, ..p.event.clone() },
            ..p
        };
        assert!(notification_body(&p2).contains("window 2 (api)"), "fallback to window");
    }

    /// Test 1: default config, the output contains ("Ctrl+a c", "New window") and
    /// NO key string containing the word "prefix" (case-insensitive).
    #[test]
    fn default_config_has_no_prefix_token_in_keys() {
        let cfg = built_in_default();
        let lines = build_help_lines(&cfg);

        // Must contain the resolved form of the canonical "new window" binding.
        assert!(
            lines.iter().any(|(k, v)| k == "Ctrl+a c" && v == "New window"),
            "expected (\"Ctrl+a c\", \"New window\") in help lines; got:\n{lines:?}"
        );

        // No key column may still carry the raw token.
        for (keys, _) in &lines {
            for tok in keys.split_whitespace() {
                assert!(
                    !tok.eq_ignore_ascii_case("prefix"),
                    "found unresolved 'prefix' token in help key string: {keys:?}"
                );
            }
        }
    }

    /// Test 2: custom prefix "Ctrl+b": "prefix c" → "Ctrl+b c", a user
    /// binding "prefix H" → "Ctrl+b H", and an absolute binding "Ctrl+x q" stays
    /// verbatim.
    #[test]
    fn custom_prefix_substituted_in_help_lines() {
        let mut cfg = built_in_default();
        cfg.keymap.prefix = "Ctrl+b".into();
        // Add user bindings: one prefix-relative and one absolute.
        cfg.keymap.bindings.push(binding("prefix H", "resize_pane_left"));
        cfg.keymap.bindings.push(binding("Ctrl+x q", "detach"));

        let lines = build_help_lines(&cfg);

        assert!(
            lines.iter().any(|(k, v)| k == "Ctrl+b c" && v == "New window"),
            "expected (\"Ctrl+b c\", \"New window\"); got:\n{lines:?}"
        );
        assert!(
            lines.iter().any(|(k, _)| k == "Ctrl+b H"),
            "expected \"Ctrl+b H\" in help lines; got:\n{lines:?}"
        );
        assert!(
            lines.iter().any(|(k, _)| k == "Ctrl+x q"),
            "expected absolute \"Ctrl+x q\" unchanged; got:\n{lines:?}"
        );

        // Still no raw token.
        for (keys, _) in &lines {
            for tok in keys.split_whitespace() {
                assert!(
                    !tok.eq_ignore_ascii_case("prefix"),
                    "found unresolved 'prefix' token in help key string: {keys:?}"
                );
            }
        }
    }

    // ── block_border_colors unit tests ────────────────────────────────────────

    /// `enabled #false` → None regardless of colors.
    #[test]
    fn block_border_colors_disabled_returns_none() {
        let mut cfg = built_in_default();
        cfg.blocks.enabled = false;
        assert!(
            block_border_colors(&cfg).is_none(),
            "expected None when blocks.enabled = false"
        );
    }

    /// Default config resolves "ok" and "alert" from the built-in palette.
    #[test]
    fn chrome_colors_map_each_role_to_the_right_palette_key() {
        use plexy_glass_emulator::Color::Rgb;
        // Pins each role → palette RGB from the built-in kanagawa-dragon palette,
        // so a swapped key (active↔marked, source↔target, border/title/bg) is
        // caught here (the painting tests build colors from literals and can't).
        let c = chrome_colors(&built_in_default());
        assert_eq!(c.rings.active, Rgb(0xb6, 0x92, 0x7b), "active ring = highlight");
        assert_eq!(c.rings.marked, Rgb(0xc4, 0xb2, 0x8a), "marked ring = warn");
        assert_eq!(c.rings.drag_source, Rgb(0x94, 0x9f, 0xb5), "drag source = info");
        assert_eq!(c.rings.drag_target, Rgb(0x87, 0xa9, 0x87), "drag target = ok");
        assert_eq!(c.overlay_border, Rgb(0x73, 0x7c, 0x73), "overlay border = accent");
        assert_eq!(c.overlay_title, Rgb(0xb6, 0x92, 0x7b), "overlay title = highlight");
        assert_eq!(c.overlay_footer, Rgb(0xb6, 0x92, 0x7b), "overlay footer = muted");
        assert_eq!(c.overlay_bg, Rgb(0x28, 0x27, 0x27), "overlay bg = bg_bar");
    }

    #[test]
    fn message_colors_map_severity_to_the_right_palette_key() {
        use crate::window_manager::Severity;
        use plexy_glass_emulator::Color::Rgb;
        let cfg = built_in_default();
        let bg = Rgb(0x28, 0x27, 0x27); // bg_bar for every severity
        assert_eq!(message_colors(&cfg, Severity::Info), (Rgb(0x94, 0x9f, 0xb5), bg));
        assert_eq!(message_colors(&cfg, Severity::Success), (Rgb(0x87, 0xa9, 0x87), bg));
        assert_eq!(message_colors(&cfg, Severity::Warn), (Rgb(0xc4, 0xb2, 0x8a), bg));
        assert_eq!(message_colors(&cfg, Severity::Error), (Rgb(0xc4, 0x74, 0x6e), bg));
    }

    #[test]
    fn block_border_colors_defaults_resolve_correctly() {
        let cfg = built_in_default();
        let colors = block_border_colors(&cfg).expect("expected Some with default config");
        assert_eq!(
            colors.ok,
            plexy_glass_emulator::Color::Rgb(0x87, 0xa9, 0x87),
            "ok color should be #87a987"
        );
        assert_eq!(
            colors.fail,
            plexy_glass_emulator::Color::Rgb(0xc4, 0x74, 0x6e),
            "fail color should be #c4746e"
        );
    }

    /// The hardcoded fallback constants must track the built-in palette's
    /// `ok`/`alert` entries, so a palette change can't silently desync the
    /// bad-config fallback path.
    #[test]
    fn fallback_constants_match_built_in_palette() {
        let palette = &built_in_default().palette;
        let ok = plexy_glass_status::resolve_color("ok", palette).expect("palette has ok");
        let alert = plexy_glass_status::resolve_color("alert", palette).expect("palette has alert");
        assert_eq!((ok.r, ok.g, ok.b), DEFAULT_OK_RGB);
        assert_eq!((alert.r, alert.g, alert.b), DEFAULT_ALERT_RGB);
    }

    /// A bad `ok_color` falls back to the default ok `Rgb`; the feature stays enabled.
    #[test]
    fn block_border_colors_bad_ok_color_falls_back_to_default() {
        let mut cfg = built_in_default();
        cfg.blocks.ok_color = "not-a-valid-color".to_string();
        let colors = block_border_colors(&cfg).expect("expected Some even with bad ok_color");
        // Falls back to the hard-coded default #87a987.
        assert_eq!(
            colors.ok,
            plexy_glass_emulator::Color::Rgb(0x87, 0xa9, 0x87),
            "bad ok_color must fall back to default #87a987"
        );
    }

    /// A bad `fail_color` falls back to the default alert `Rgb`; the feature stays
    /// enabled.
    #[test]
    fn block_border_colors_bad_fail_color_falls_back_to_default() {
        let mut cfg = built_in_default();
        cfg.blocks.fail_color = "##invalid".to_string();
        let colors = block_border_colors(&cfg).expect("expected Some even with bad fail_color");
        assert_eq!(
            colors.fail,
            plexy_glass_emulator::Color::Rgb(0xc4, 0x74, 0x6e),
            "bad fail_color must fall back to default #c4746e"
        );
    }

    /// A custom hex color resolves correctly.
    #[test]
    fn block_border_colors_custom_hex_resolves() {
        let mut cfg = built_in_default();
        cfg.blocks.ok_color = "#aabbcc".to_string();
        cfg.blocks.fail_color = "#001122".to_string();
        let colors = block_border_colors(&cfg).expect("expected Some with valid hex colors");
        assert_eq!(
            colors.ok,
            plexy_glass_emulator::Color::Rgb(0xaa, 0xbb, 0xcc),
            "custom ok hex #aabbcc should resolve"
        );
        assert_eq!(
            colors.fail,
            plexy_glass_emulator::Color::Rgb(0x00, 0x11, 0x22),
            "custom fail hex #001122 should resolve"
        );
    }

    /// A custom palette name resolves via the config's palette.
    #[test]
    fn block_border_colors_custom_palette_name_resolves() {
        let mut cfg = built_in_default();
        // Add a custom palette entry.
        cfg.palette.entries.insert("my_green".to_string(), "#00ff00".to_string());
        cfg.blocks.ok_color = "my_green".to_string();
        let colors = block_border_colors(&cfg).expect("expected Some with custom palette name");
        assert_eq!(
            colors.ok,
            plexy_glass_emulator::Color::Rgb(0x00, 0xff, 0x00),
            "custom palette name 'my_green' should resolve to #00ff00"
        );
    }
}
