use super::*;

fn spec() -> SpawnSpec {
    SpawnSpec {
        program: "/bin/cat".into(),
        args: vec![],
        env: vec![],
        cwd: None,
    }
}

fn cfg() -> Arc<plexy_glass_config::Config> {
    Arc::new(plexy_glass_config::built_in_default())
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
        cfg(),
    )
    .unwrap();
    assert_eq!(m.windows().len(), 1);
    assert_eq!(m.active_window().active(), PaneId(0));
}

#[tokio::test]
async fn new_window_default_spec_is_shell_not_first_command() {
    // A session whose first pane runs a non-shell command (here `/bin/cat`,
    // standing in for `claude`) must still open the SHELL for new windows and
    // splits, not re-run that command. Regression: `default_spec` used to be a
    // clone of `first_spec`.
    let notify = Arc::new(Notify::new());
    let m = WindowManager::new(
        spec(), // program = `/bin/cat` (a non-shell first command)
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    assert_eq!(m.default_spec.program, crate::declared::default_shell());
    assert_ne!(m.default_spec.program, "/bin/cat");
    assert!(m.default_spec.args.is_empty());
}

#[tokio::test]
async fn status_message_set_and_lazy_expire() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    assert_eq!(m.take_active_message(), None);
    m.set_status_message("no session: foo".into());
    assert_eq!(m.take_active_message(), Some("no session: foo"));
    // Force expiry and confirm it clears in place on the next read.
    m.status_message.as_mut().unwrap().expires_at = Instant::now() - Duration::from_secs(1);
    assert_eq!(m.take_active_message(), None);
    assert!(m.status_message.is_none(), "expired message cleared in place");
    // A newer message replaces the prior one.
    m.set_status_message("first".into());
    m.set_status_message("second".into());
    assert_eq!(m.take_active_message(), Some("second"));
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
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
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
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
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
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
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
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
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
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
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
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
    m.handle_command(Command::NewWindow).unwrap();
    m.handle_command(Command::NextWindow).unwrap();
    assert_eq!(m.active_idx(), 0);
    m.handle_command(Command::NextWindow).unwrap();
    assert_eq!(m.active_idx(), 1);
}

#[tokio::test]
async fn split_cwd_is_window_home_base_not_active_pane_cwd() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
    m.set_window_home_cwd(0, Some("/home/base".into()));
    // The active pane reports a DIFFERENT live cwd via OSC 7, and it must be ignored.
    if let Some(pane) = m.active_window().active_pane() {
        pane.with_screen_mut(|s| s.cwd = Some("file:///somewhere/else".to_string()));
    }
    // A split spawns at the window home base, never the active pane's cd location.
    assert_eq!(m.split_cwd().as_deref(), Some("/home/base"));
    // And the split still succeeds structurally.
    m.handle_command(Command::SplitV).unwrap();
    assert_eq!(m.active_window().layout().panes().len(), 2);
}

#[tokio::test]
async fn toggle_sync_panes_flips_the_flag() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    assert!(!m.active_window().sync_input);
    m.handle_command(Command::ToggleSyncPanes).unwrap();
    assert!(m.active_window().sync_input);
    m.handle_command(Command::ToggleSyncPanes).unwrap();
    assert!(!m.active_window().sync_input);
}

fn okey(k: plexy_glass_mux::Key) -> plexy_glass_mux::KeyEvent {
    plexy_glass_mux::KeyEvent::plain(k)
}

fn type_str(m: &mut WindowManager, s: &str) {
    for c in s.chars() {
        m.handle_overlay_key(&okey(plexy_glass_mux::Key::Char(c)));
    }
}

#[tokio::test]
async fn session_picker_overlay_commits_switch_session() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    m.open_session_picker(vec![
        PickerEntry { name: "main".into(), label: "main".into(), is_current: true },
        PickerEntry { name: "work".into(), label: "work".into(), is_current: false },
    ]);
    assert!(matches!(
        m.overlay(),
        Some(plexy_glass_mux::Overlay::SessionPicker { .. })
    ));
    // Down to "work", then Enter → SwitchSession("work"); overlay closes.
    m.handle_overlay_key(&okey(plexy_glass_mux::Key::Arrow(plexy_glass_mux::Direction::Down)));
    let r = m.handle_overlay_key(&okey(plexy_glass_mux::Key::Enter));
    assert!(matches!(r, OverlayKeyResult::SwitchSession(ref n) if n == "work"));
    assert!(m.overlay().is_none(), "picker closes on commit");
}

#[tokio::test]
async fn rename_window_overlay_commits_name() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    let orig = m.active_window().name.clone();
    m.handle_command(Command::RenameWindow).unwrap();
    // The prompt seeds with the current name for in-place editing.
    match m.overlay() {
        Some(plexy_glass_mux::Overlay::Rename { buf, .. }) => assert_eq!(*buf, orig),
        other => panic!("expected a rename overlay, got {other:?}"),
    }
    // Clear the seeded name, then type a fresh one.
    for _ in 0..orig.chars().count() {
        m.handle_overlay_key(&okey(plexy_glass_mux::Key::Backspace));
    }
    type_str(&mut m, "build");
    // A real rename reports `Committed` so the connection persists it.
    assert_eq!(
        m.handle_overlay_key(&okey(plexy_glass_mux::Key::Enter)),
        OverlayKeyResult::Committed
    );
    assert!(m.overlay().is_none(), "overlay closed on commit");
    assert_eq!(m.active_window().name, "build");
}

#[tokio::test]
async fn rename_window_escape_cancels_without_change() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    let orig = m.active_window().name.clone();
    m.handle_command(Command::RenameWindow).unwrap();
    type_str(&mut m, "zzz");
    m.handle_overlay_key(&okey(plexy_glass_mux::Key::Escape));
    assert!(m.overlay().is_none());
    assert_eq!(m.active_window().name, orig, "cancel leaves the name unchanged");
}

#[tokio::test]
async fn rename_pane_overlay_sets_pane_name() {
    let mut m = make_two_pane_manager().await;
    let active = m.active_window().active();
    m.handle_command(Command::RenamePane).unwrap();
    type_str(&mut m, "logs");
    m.handle_overlay_key(&okey(plexy_glass_mux::Key::Enter));
    assert!(m.overlay().is_none());
    assert_eq!(
        m.active_window().pane(active).and_then(|p| p.name()).as_deref(),
        Some("logs")
    );
}

#[tokio::test]
async fn empty_rename_commit_is_noop_but_closes() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    let orig = m.active_window().name.clone();
    m.handle_command(Command::RenameWindow).unwrap();
    // Clear the seeded name entirely, then commit empty.
    for _ in 0..orig.chars().count() {
        m.handle_overlay_key(&okey(plexy_glass_mux::Key::Backspace));
    }
    // Empty commit closes the overlay but changes nothing, so it must NOT
    // report Committed (no needless persist).
    assert_eq!(
        m.handle_overlay_key(&okey(plexy_glass_mux::Key::Enter)),
        OverlayKeyResult::Redraw
    );
    assert!(m.overlay().is_none());
    assert_eq!(m.active_window().name, orig, "empty commit does not rename");
}

#[tokio::test]
async fn show_help_opens_and_dismisses() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    m.handle_command(Command::ShowHelp).unwrap();
    assert!(matches!(m.overlay(), Some(plexy_glass_mux::Overlay::Help { .. })));
    m.handle_overlay_key(&okey(plexy_glass_mux::Key::Char('q')));
    assert!(m.overlay().is_none());
}

async fn make_two_pane_manager() -> WindowManager {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh"); // splits must not depend on `$SHELL`
    m.handle_command(Command::SplitV).unwrap();
    m
}

#[tokio::test]
async fn select_layout_puts_active_pane_in_the_main_slot() {
    let mut m = make_two_pane_manager().await;
    m.handle_command(Command::SplitH).unwrap(); // 3 panes
    let active = m.active_window().active();
    m.handle_command(Command::SelectLayout(plexy_glass_mux::LayoutPreset::MainVertical))
        .unwrap();
    let vp = m.viewport();
    let rects: Vec<(PaneId, plexy_glass_mux::Rect)> = m
        .active_window()
        .layout()
        .dfs_leaves()
        .into_iter()
        .map(|p| (p, m.active_window().layout().rect_of(p, vp).unwrap()))
        .collect();
    let (widest, widest_rect) = rects
        .iter()
        .max_by_key(|(_, r)| r.cols)
        .copied()
        .unwrap();
    assert_eq!(widest, active, "active pane takes the main slot: {rects:?}");
    assert!(
        widest_rect.cols > vp.cols / 2,
        "main pane has the major share: {widest_rect:?}"
    );
    // Focus unchanged; PTYs resized to the new rects.
    assert_eq!(m.active_window().active(), active);
    let (rows, cols) = m
        .active_window()
        .pane(active)
        .unwrap()
        .with_screen(|s| (s.active.num_rows(), s.active.num_cols()));
    assert_eq!((rows, cols), (widest_rect.rows, widest_rect.cols));
}

#[tokio::test]
async fn next_layout_cycles_in_order_and_remembers() {
    use plexy_glass_mux::LayoutPreset;
    let mut m = make_two_pane_manager().await;
    m.handle_command(Command::NextLayout).unwrap();
    assert_eq!(m.active_window().last_preset, Some(LayoutPreset::EvenHorizontal));
    m.handle_command(Command::NextLayout).unwrap();
    assert_eq!(m.active_window().last_preset, Some(LayoutPreset::EvenVertical));
    // A manual split does NOT reset the cycle position.
    m.handle_command(Command::SplitV).unwrap();
    m.handle_command(Command::NextLayout).unwrap();
    assert_eq!(m.active_window().last_preset, Some(LayoutPreset::MainHorizontal));
}

#[tokio::test]
async fn select_layout_clears_zoom() {
    let mut m = make_two_pane_manager().await;
    m.handle_command(Command::ZoomToggle).unwrap();
    assert!(m.active_window().is_zoomed());
    m.handle_command(Command::SelectLayout(plexy_glass_mux::LayoutPreset::Tiled))
        .unwrap();
    assert!(!m.active_window().is_zoomed());
}

#[tokio::test]
async fn select_layout_single_pane_is_noop_but_remembers() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    m.handle_command(Command::SelectLayout(plexy_glass_mux::LayoutPreset::Tiled))
        .unwrap();
    assert_eq!(m.active_window().layout().panes().len(), 1);
    assert_eq!(m.active_window().last_preset, Some(plexy_glass_mux::LayoutPreset::Tiled));
}

fn gutter_col_for(m: &WindowManager) -> u16 {
    let vp = m.viewport();
    m.active_window()
        .layout()
        .rect_of(PaneId(0), vp)
        .map(|r| r.col + r.cols)
        .unwrap_or(0)
}

#[tokio::test]
async fn border_press_starts_resize_drag() {
    let mut m = make_two_pane_manager().await;
    let gutter = gutter_col_for(&m);
    let event = MouseEvent {
        kind: MouseKind::Press,
        button: MouseButton::Left,
        modifiers: plexy_glass_mux::MouseModifiers::default(),
        row: 5,
        col: gutter,
    };
    m.handle_mouse(event).await.unwrap();
    assert!(m.resize_drag.is_some());
}

#[tokio::test]
async fn classify_click_count_increments_within_window() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    let event = MouseEvent {
        kind: MouseKind::Press,
        button: MouseButton::Left,
        modifiers: plexy_glass_mux::MouseModifiers::default(),
        row: 5,
        col: 5,
    };
    assert_eq!(m.classify_click_count(PaneId(0), &event), 1);
    assert_eq!(m.classify_click_count(PaneId(0), &event), 2);
    assert_eq!(m.classify_click_count(PaneId(0), &event), 3);
    // Clamps at 3.
    assert_eq!(m.classify_click_count(PaneId(0), &event), 3);
}

#[tokio::test]
async fn classify_click_count_resets_on_target_change() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    let mut event = MouseEvent {
        kind: MouseKind::Press,
        button: MouseButton::Left,
        modifiers: plexy_glass_mux::MouseModifiers::default(),
        row: 5,
        col: 5,
    };
    assert_eq!(m.classify_click_count(PaneId(0), &event), 1);
    event.col = 10; // different target
    assert_eq!(m.classify_click_count(PaneId(0), &event), 1);
}

#[tokio::test]
async fn resize_drag_release_clears_state() {
    let mut m = make_two_pane_manager().await;
    let gutter = gutter_col_for(&m);
    m.handle_mouse(MouseEvent {
        kind: MouseKind::Press,
        button: MouseButton::Left,
        modifiers: plexy_glass_mux::MouseModifiers::default(),
        row: 5,
        col: gutter,
    })
    .await
    .unwrap();
    assert!(m.resize_drag.is_some());
    m.handle_mouse(MouseEvent {
        kind: MouseKind::Release,
        button: MouseButton::Left,
        modifiers: plexy_glass_mux::MouseModifiers::default(),
        row: 5,
        col: gutter,
    })
    .await
    .unwrap();
    assert!(m.resize_drag.is_none());
}

// Regression: a pane running an app that enabled mouse reporting (less, hx,
// a TUI) must still be focusable by left-clicking it. Previously Rule 5
// forwarded *every* event to the child before the focus-on-click rule could
// run, so a click on such a pane never changed focus.
#[tokio::test]
async fn left_click_focuses_non_active_pane_even_with_app_mouse_mode() {
    let mut m = make_two_pane_manager().await;
    let active = m.active_window().active();
    let other = if active == PaneId(0) { PaneId(1) } else { PaneId(0) };
    // Simulate less/hx: the non-active pane's app enabled mouse reporting.
    m.active_window()
        .pane(other)
        .unwrap()
        .with_screen_mut(|s| s.modes.insert(plexy_glass_emulator::Modes::MOUSE_BTN));
    assert!(m.pane_has_any_mouse_mode(other));
    // Click in the middle of the other pane (avoiding borders), translating
    // the logical rect back to physical coords via the status-bar offset.
    let vp = m.viewport();
    let rect = m.active_window().layout().rect_of(other, vp).unwrap();
    let event = MouseEvent {
        kind: MouseKind::Press,
        button: MouseButton::Left,
        modifiers: plexy_glass_mux::MouseModifiers::default(),
        row: rect.row + rect.rows / 2 + m.pane_row_offset,
        col: rect.col + rect.cols / 2,
    };
    m.handle_mouse(event).await.unwrap();
    assert_eq!(
        m.active_window().active(),
        other,
        "left-clicking a pane with app mouse mode on should focus it"
    );
}

// SP4: with the status bar on top, mouse events (physical coords) must be
// shifted up by one row before hitting the logical pane/border layout.
// A physical click on the logical gutter row is *inside* a pane; the
// border lives one physical row lower.
#[tokio::test]
async fn status_top_offset_shifts_border_hit_row() {
    use plexy_glass_mux::{MouseButton, MouseEvent, MouseKind, MouseModifiers};

    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
    m.handle_command(Command::SplitH).unwrap(); // stacked top/bottom panes
    let vp = m.viewport();
    let r0 = m.active_window().layout().rect_of(PaneId(0), vp).unwrap();
    let r1 = m.active_window().layout().rect_of(PaneId(1), vp).unwrap();
    let top = if r0.row <= r1.row { r0 } else { r1 };
    let gutter_row = top.row + top.rows; // logical border row between panes

    // Status bar on top → pane band shifted down one physical row.
    m.set_status_layout(Some(0), 1);
    let press = |row| MouseEvent {
        kind: MouseKind::Press,
        button: MouseButton::Left,
        modifiers: MouseModifiers::default(),
        row,
        col: 4,
    };

    // Physical gutter_row → logical gutter_row-1 (inside top pane): no drag.
    m.handle_mouse(press(gutter_row)).await.unwrap();
    assert!(
        m.resize_drag.is_none(),
        "physical gutter row is inside a pane under top placement"
    );
    // One physical row lower → logical border row: starts a resize drag.
    m.handle_mouse(press(gutter_row + 1)).await.unwrap();
    assert!(
        m.resize_drag.is_some(),
        "physical gutter_row+1 maps to the logical border under top placement"
    );
}

// K8: deterministic resize propagation (no PTY/timing). Proves
// on_host_resize → Window::resize → Pane::resize → emulator resize for
// every pane. The flaky e2e resize tests were the only prior coverage.
#[tokio::test]
async fn on_host_resize_propagates_to_all_panes() {
    let mut m = make_two_pane_manager().await; // 24x80, vertical split
    m.on_host_resize(PtySize { rows: 40, cols: 120, pixel_width: 0, pixel_height: 0 })
        .unwrap();
    let vp = m.viewport();
    // The layout region is inset by the pane frame: full width (120) minus
    // the two outer frame columns.
    assert_eq!(vp.cols, 118, "viewport width did not update (120 - 2 frame cols)");
    let win = m.active_window();
    let panes = win.layout().panes();
    assert_eq!(panes.len(), 2);
    for id in panes {
        let rect = win.layout().rect_of(id, vp).expect("rect");
        let (er, ec) = win
            .pane(id)
            .unwrap()
            .with_screen(|s| (s.active.num_rows(), s.active.num_cols()));
        assert_eq!(er, rect.rows, "pane {id:?} emulator rows != layout rect");
        assert_eq!(ec, rect.cols, "pane {id:?} emulator cols != layout rect");
    }
}

// K9: mouse precedence ladder. The wheel rung routes to the active pane.
#[tokio::test]
async fn wheel_event_routes_to_active_pane_without_panic() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    let pane = m.active_window().layout().panes()[0];
    let before = m.active_window().pane(pane).unwrap().scroll_offset();
    m.handle_mouse(MouseEvent {
        kind: MouseKind::Wheel { delta: 3 },
        button: MouseButton::None,
        modifiers: plexy_glass_mux::MouseModifiers::default(),
        row: 2,
        col: 2,
    })
    .await
    .unwrap();
    // No scrollback yet → offset clamps to 0; the rung routed without error.
    let after = m.active_window().pane(pane).unwrap().scroll_offset();
    assert!(after >= before);
}

// K9: mouse precedence ladder. A status-bar click dispatches its action.
#[tokio::test]
async fn status_bar_click_dispatches_select_window() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
    m.handle_command(Command::NewWindow).unwrap(); // 2 windows, active = 1
    assert_eq!(m.active_idx(), 1);
    m.set_status_layout(Some(23), 0);
    m.set_status_hits(vec![plexy_glass_status::StatusHit {
        col_range: 0..5,
        action: plexy_glass_status::ClickAction::SelectWindow(0),
    }]);
    m.handle_mouse(MouseEvent {
        kind: MouseKind::Press,
        button: MouseButton::Left,
        modifiers: plexy_glass_mux::MouseModifiers::default(),
        row: 23,
        col: 2,
    })
    .await
    .unwrap();
    assert_eq!(m.active_idx(), 0, "status-bar click did not select window 0");
}

// ---- SP1: pane zoom ----

#[tokio::test]
async fn zoom_toggle_sets_and_clears_zoomed() {
    let mut m = make_two_pane_manager().await;
    let active = m.active_window().active();
    m.handle_command(Command::ZoomToggle).unwrap();
    assert_eq!(m.active_window().zoomed, Some(active));
    m.handle_command(Command::ZoomToggle).unwrap();
    assert!(m.active_window().zoomed.is_none());
}

#[tokio::test]
async fn splitting_clears_zoom() {
    let mut m = make_two_pane_manager().await;
    m.handle_command(Command::ZoomToggle).unwrap();
    assert!(m.active_window().is_zoomed());
    m.handle_command(Command::SplitV).unwrap();
    assert!(!m.active_window().is_zoomed(), "split must clear zoom");
}

#[tokio::test]
async fn zoom_resizes_pane_to_full_then_restores() {
    let mut m = make_two_pane_manager().await; // 24x80 vertical split
    let active = m.active_window().active();
    let vp = m.viewport();
    m.handle_command(Command::ZoomToggle).unwrap();
    let zoomed_cols = m
        .active_window()
        .pane(active)
        .unwrap()
        .with_screen(|s| s.active.num_cols());
    assert_eq!(zoomed_cols, vp.cols, "zoomed pane should span full width");
    m.handle_command(Command::ZoomToggle).unwrap();
    let restored_cols = m
        .active_window()
        .pane(active)
        .unwrap()
        .with_screen(|s| s.active.num_cols());
    assert!(restored_cols < vp.cols, "unzoom should restore the split width");
}

// ---- SP2: pane resize keybindings ----

#[tokio::test]
async fn resize_pane_right_widens_active_pane() {
    let mut m = make_two_pane_manager().await; // vertical split; active = second (right)
    let vp = m.viewport();
    let active = m.active_window().active();
    let before = m.active_window().layout().rect_of(active, vp).unwrap().cols;
    m.handle_command(Command::ResizePane(plexy_glass_mux::Direction::Right)).unwrap();
    let after = m.active_window().layout().rect_of(active, vp).unwrap().cols;
    assert!(after >= before, "active pane should not shrink when growing right");
}

#[tokio::test]
async fn resize_single_pane_is_noop() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    m.handle_command(Command::ResizePane(plexy_glass_mux::Direction::Left)).unwrap();
    assert_eq!(m.active_window().layout().panes().len(), 1);
}

// ---- SP3: last-window / last-pane ----

#[tokio::test]
async fn last_window_toggle_returns_to_previous() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
    m.handle_command(Command::NewWindow).unwrap(); // active = 1, last = 0
    assert_eq!(m.active_idx(), 1);
    m.handle_command(Command::SelectLastWindow).unwrap();
    assert_eq!(m.active_idx(), 0, "should return to window 0");
    m.handle_command(Command::SelectLastWindow).unwrap();
    assert_eq!(m.active_idx(), 1, "toggling again returns to window 1");
}

#[tokio::test]
async fn last_window_noop_when_single() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    m.handle_command(Command::SelectLastWindow).unwrap();
    assert_eq!(m.active_idx(), 0);
}

#[tokio::test]
async fn last_pane_returns_to_previous() {
    let mut m = make_two_pane_manager().await; // 2 panes, active = second
    let second = m.active_window().active();
    let panes = m.active_window().layout().panes();
    let first = *panes.iter().find(|p| **p != second).unwrap();
    m.active_window_mut().focus(first);
    m.handle_command(Command::SelectLastPane).unwrap();
    assert_eq!(m.active_window().active(), second, "last-pane returns to previous pane");
}

// SP6: killing the active window must keep `last_active_window` valid.
// Regression for a stale index that pointed past the shifted window list.
#[tokio::test]
async fn kill_window_keeps_last_active_index_valid() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
    m.handle_command(Command::NewWindow).unwrap(); // W1, active=1, last=0
    m.handle_command(Command::NewWindow).unwrap(); // W2, active=2, last=1
    m.handle_command(Command::SelectWindow(0)).unwrap(); // active=0, last=Some(2) -> W2
    assert_eq!(m.active_idx(), 0);
    let w2_id = m.windows()[2].id;
    // Kill W0 (active). windows shift to [W1, W2]; last (was index 2)
    // must follow W2 to its new index 1, not dangle at 2.
    m.handle_command(Command::KillWindow).unwrap();
    assert_eq!(m.windows().len(), 2);
    m.handle_command(Command::SelectLastWindow).unwrap();
    assert_eq!(m.windows()[m.active_idx()].id, w2_id, "toggle lands on the original W2");
}

// SP6: a window removed by pane-death (its last pane exited) must shift
// `last_active_window` the same way `active` is shifted.
#[tokio::test]
async fn pane_death_window_removal_keeps_last_active_valid() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
    m.handle_command(Command::NewWindow).unwrap(); // W1 (pane 1)
    m.handle_command(Command::NewWindow).unwrap(); // W2 (pane 2)
    m.handle_command(Command::SelectWindow(0)).unwrap(); // active=0, last=Some(2) -> W2
    let w2_id = m.windows()[2].id;
    // W1's sole pane (PaneId(1)) dies, so window index 1 is removed.
    m.handle_pane_death(PaneId(1)).unwrap();
    assert_eq!(m.windows().len(), 2);
    // last (was index 2, after the removed index 1) must follow W2 to
    // index 1 so the toggle still reaches it.
    m.handle_command(Command::SelectLastWindow).unwrap();
    assert_eq!(m.windows()[m.active_idx()].id, w2_id, "toggle lands on the original W2");
}

// ----- choose-tree -----

fn mk_mgr() -> WindowManager {
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        Arc::new(Notify::new()),
        None,
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh"); // new windows/splits must not depend on `$SHELL`
    m
}

fn tnode(session: &str, window: Option<u32>, pane: Option<u32>, depth: u8) -> TreeNode {
    TreeNode {
        session: session.into(),
        window: window.map(WindowId),
        pane: pane.map(PaneId),
        depth,
        label: String::new(),
        name: String::new(),
        index: 0,
        is_current: false,
    }
}

fn key(c: char) -> KeyEvent {
    KeyEvent::new(plexy_glass_mux::Key::Char(c), plexy_glass_mux::Modifiers::empty())
}

#[tokio::test]
async fn open_tree_then_enter_switches_and_closes() {
    let mut m = mk_mgr();
    m.open_tree(vec![tnode("work", None, None, 0), tnode("work", Some(0), None, 1)]);
    assert!(matches!(m.overlay(), Some(Overlay::Tree(_))));
    let r = m
        .handle_overlay_key(&KeyEvent::new(plexy_glass_mux::Key::Enter, plexy_glass_mux::Modifiers::empty()));
    assert!(matches!(r, OverlayKeyResult::Tree(TreeAction::Switch { .. })));
    assert!(m.overlay().is_none(), "switch closes the overlay");
}

#[tokio::test]
async fn tree_kill_keeps_overlay_open() {
    let mut m = mk_mgr();
    m.open_tree(vec![
        tnode("work", None, None, 0),
        tnode("work", Some(0), None, 1),
        tnode("work", Some(0), Some(5), 2),
    ]);
    m.handle_overlay_key(&key('j'));
    m.handle_overlay_key(&key('j')); // select the pane node
    assert!(matches!(m.handle_overlay_key(&key('x')), OverlayKeyResult::Redraw));
    let r = m.handle_overlay_key(&key('y'));
    assert!(matches!(
        r,
        OverlayKeyResult::Tree(TreeAction::KillPane { pane: PaneId(5), .. })
    ));
    assert!(m.overlay().is_some(), "kill keeps the overlay open");
}

#[tokio::test]
async fn by_id_helpers_hit_and_miss() {
    let mut m = mk_mgr();
    m.handle_command(Command::SplitV).unwrap(); // window 0: panes {0,1}, active 1
    m.handle_command(Command::NewWindow).unwrap(); // window 1 (pane 2), active window 1

    // Real cross-window focus: from window 1, focus pane 0 (in window 0).
    assert!(m.focus_pane_by_id(PaneId(0)));
    assert_eq!(m.active_idx(), 0, "focus switched to pane 0's window");
    assert_eq!(m.windows()[0].active(), PaneId(0), "pane 0 became its window's active pane");
    assert!(!m.focus_pane_by_id(PaneId(999)));

    assert!(m.select_window_by_id(WindowId(1)));
    assert_eq!(m.active_idx(), 1);
    assert!(!m.select_window_by_id(WindowId(99)));

    assert!(m.rename_window_by_id(WindowId(0), "renamed".into()));
    assert_eq!(m.windows()[0].name, "renamed");
    assert!(!m.rename_window_by_id(WindowId(99), "x".into()));

    // Rename a pane and read the stored name back.
    assert!(m.rename_pane_by_id(PaneId(0), "p".into()));
    assert_eq!(m.windows()[0].pane(PaneId(0)).unwrap().name(), Some("p".to_string()));
    assert!(!m.rename_pane_by_id(PaneId(999), "p".into()));

    assert!(m.kill_window_panes(WindowId(1)));
    assert!(!m.kill_window_panes(WindowId(99)));
    assert!(!m.kill_pane_child(PaneId(999)));
}

#[tokio::test]
async fn by_id_helpers_clear_zoom() {
    let mut m = mk_mgr();
    m.handle_command(Command::SplitV).unwrap(); // window 0: panes {0,1}, active 1

    // `focus_pane_by_id` clears the TARGET window's zoom so the pane is visible.
    m.handle_command(Command::ZoomToggle).unwrap();
    assert!(m.windows()[0].is_zoomed());
    assert!(m.focus_pane_by_id(PaneId(0)));
    assert!(!m.windows()[0].is_zoomed(), "focus_pane_by_id unzooms the target window");

    // `select_window_by_id` clears the leaving window's zoom before switching.
    m.handle_command(Command::NewWindow).unwrap(); // window 1 active
    m.select_window_by_id(WindowId(0)); // back to window 0
    m.handle_command(Command::ZoomToggle).unwrap();
    assert!(m.windows()[0].is_zoomed());
    m.select_window_by_id(WindowId(1));
    assert!(!m.windows()[0].is_zoomed(), "select_window_by_id unzooms before switching");
}

// ----- break / join / swap -----

#[tokio::test]
async fn mark_pane_toggles_and_death_clears() {
    let mut m = mk_mgr();
    m.handle_command(Command::SplitV).unwrap(); // panes 0,1; active 1
    assert_eq!(m.marked_pane(), None);
    m.handle_command(Command::MarkPane).unwrap();
    assert_eq!(m.marked_pane(), Some(PaneId(1)));
    m.handle_command(Command::MarkPane).unwrap();
    assert_eq!(m.marked_pane(), None, "re-marking the active pane toggles off");
    m.handle_command(Command::MarkPane).unwrap();
    assert_eq!(m.marked_pane(), Some(PaneId(1)));
    m.handle_pane_death(PaneId(1)).unwrap();
    assert_eq!(m.marked_pane(), None, "a dead pane is unmarked");
}

#[tokio::test]
async fn break_pane_moves_active_into_new_window() {
    let mut m = mk_mgr();
    m.handle_command(Command::SplitV).unwrap(); // window 0: panes 0,1; active 1
    m.handle_command(Command::BreakPane).unwrap();
    assert_eq!(m.windows().len(), 2);
    assert_eq!(m.windows()[0].layout().panes(), vec![PaneId(0)]);
    assert_eq!(m.active_idx(), 1);
    assert_eq!(m.active_window().active(), PaneId(1));
    assert_eq!(m.last_active_window, Some(0));
}

#[tokio::test]
async fn break_pane_single_pane_is_noop_with_status() {
    let mut m = mk_mgr();
    m.handle_command(Command::BreakPane).unwrap();
    assert_eq!(m.windows().len(), 1, "single pane is not broken out");
    assert_eq!(m.take_active_message(), Some("only pane in window"));
}

#[tokio::test]
async fn swap_pane_exchanges_with_neighbor() {
    let mut m = mk_mgr();
    m.handle_command(Command::SplitV).unwrap(); // panes 0,1; active 1
    let vp = m.viewport();
    let before = m.active_window().layout().rect_of(PaneId(1), vp).unwrap();
    m.handle_command(Command::SwapPane(false)).unwrap(); // swap with previous (pane 0)
    assert_eq!(m.active_window().active(), PaneId(1), "active id unchanged");
    let after = m.active_window().layout().rect_of(PaneId(1), vp).unwrap();
    assert_ne!(before, after, "active pane's content moved to the other slot");
}

#[tokio::test]
async fn join_pane_moves_marked_and_removes_empty_source() {
    let mut m = mk_mgr(); // W0 (WindowId 0), pane 0
    m.handle_command(Command::NewWindow).unwrap(); // W1 (WindowId 1), pane 1, active
    m.handle_command(Command::SelectWindow(0)).unwrap(); // active W0
    m.handle_command(Command::MarkPane).unwrap(); // marked = pane 0
    m.handle_command(Command::SelectWindow(1)).unwrap(); // active W1
    m.handle_command(Command::JoinPane(SplitDir::Vertical)).unwrap();
    assert_eq!(m.windows().len(), 1, "emptied source window removed");
    let panes = m.active_window().layout().panes();
    assert!(panes.contains(&PaneId(0)) && panes.contains(&PaneId(1)));
    assert_eq!(m.active_window().active(), PaneId(0), "joined pane is active");
    assert_eq!(m.marked_pane(), None, "mark cleared after join");
}

#[tokio::test]
async fn join_pane_fixes_active_and_last_active_after_source_removal() {
    let mut m = mk_mgr(); // W0 idx0
    m.handle_command(Command::NewWindow).unwrap(); // W1 idx1
    m.handle_command(Command::NewWindow).unwrap(); // W2 idx2, active
    m.handle_command(Command::SelectWindow(0)).unwrap(); // active W0
    m.handle_command(Command::MarkPane).unwrap(); // marked = pane 0 (W0, a lower index)
    m.handle_command(Command::SelectWindow(1)).unwrap(); // active W1
    m.handle_command(Command::SelectWindow(2)).unwrap(); // active W2, last = W1
    let w2_id = m.windows()[m.active_idx()].id;
    let w1_id = m.windows()[1].id;
    m.handle_command(Command::JoinPane(SplitDir::Vertical)).unwrap(); // W0 empties → removed
    assert_eq!(m.windows()[m.active_idx()].id, w2_id, "active stays W2 across removal");
    m.handle_command(Command::SelectLastWindow).unwrap();
    assert_eq!(m.windows()[m.active_idx()].id, w1_id, "last-active follows W1 across removal");
}

#[tokio::test]
async fn join_pane_source_survives_when_multipane() {
    let mut m = mk_mgr(); // W0 idx0 pane0
    m.handle_command(Command::NewWindow).unwrap(); // W1 idx1 pane1, active
    m.handle_command(Command::SplitV).unwrap(); // W1 panes {1,2}, active 2
    let half = m.windows()[1].pane(PaneId(1)).unwrap().with_screen(|s| s.active.num_cols());
    m.handle_command(Command::MarkPane).unwrap(); // marked = pane 2 (W1)
    m.handle_command(Command::SelectWindow(0)).unwrap(); // active W0
    m.handle_command(Command::JoinPane(SplitDir::Vertical)).unwrap();
    assert_eq!(m.windows().len(), 2, "multi-pane source survives");
    let active_panes = m.active_window().layout().panes();
    assert!(active_panes.contains(&PaneId(0)) && active_panes.contains(&PaneId(2)));
    assert_eq!(m.windows()[1].layout().panes(), vec![PaneId(1)], "source keeps its other pane");
    let full = m.windows()[1].pane(PaneId(1)).unwrap().with_screen(|s| s.active.num_cols());
    assert!(full > half, "surviving source pane was resized wider ({half} -> {full})");
    assert_eq!(m.marked_pane(), None);
}

#[tokio::test]
async fn swap_marked_pane_same_window_then_cross_window_status() {
    let mut m = mk_mgr();
    m.handle_command(Command::SplitV).unwrap(); // W0 panes 0,1; active 1
    let vp = m.viewport();
    m.handle_command(Command::MarkPane).unwrap(); // marked = 1
    m.handle_command(Command::SelectPane(plexy_glass_mux::Direction::Left)).unwrap(); // active 0
    let before = m.active_window().layout().rect_of(PaneId(1), vp).unwrap();
    m.handle_command(Command::SwapMarkedPane).unwrap();
    let after = m.active_window().layout().rect_of(PaneId(1), vp).unwrap();
    assert_ne!(before, after, "same-window swap exchanged slots");
    assert_eq!(m.marked_pane(), Some(PaneId(1)), "mark preserved across swap");

    // Cross-window: marked stays in W0; move active to a new window → status.
    m.handle_command(Command::NewWindow).unwrap();
    m.handle_command(Command::SwapMarkedPane).unwrap();
    assert_eq!(
        m.take_active_message(),
        Some("marked pane is in another window — use join")
    );
}

#[tokio::test]
async fn kill_pane_and_kill_window_clear_mark() {
    // KillPane on the marked pane clears the mark (no death-channel round-trip).
    let mut m = mk_mgr();
    m.handle_command(Command::SplitV).unwrap(); // panes 0,1; active 1
    m.handle_command(Command::MarkPane).unwrap(); // marked = 1 (active)
    m.handle_command(Command::KillPane).unwrap();
    assert_eq!(m.marked_pane(), None, "killing the marked pane clears the mark");

    // KillWindow clears a mark on any pane in the removed window.
    let mut m = mk_mgr();
    m.handle_command(Command::SplitV).unwrap(); // W0: panes 0,1
    m.handle_command(Command::NewWindow).unwrap(); // W1, active
    m.handle_command(Command::SelectWindow(0)).unwrap(); // active W0
    m.handle_command(Command::MarkPane).unwrap(); // marks a W0 pane
    m.handle_command(Command::KillWindow).unwrap(); // removes W0
    assert_eq!(m.marked_pane(), None, "killing the marked pane's window clears the mark");
}

#[tokio::test]
async fn structural_commands_clear_zoom() {
    for cmd in [Command::SwapPane(false), Command::BreakPane] {
        let mut m = mk_mgr();
        m.handle_command(Command::SplitV).unwrap();
        m.handle_command(Command::ZoomToggle).unwrap();
        assert!(m.active_window().is_zoomed());
        m.handle_command(cmd.clone()).unwrap();
        assert!(!m.active_window().is_zoomed(), "{cmd:?} must clear zoom");
    }
    // Join/SwapMarked need a same-window marked pane.
    for cmd in [Command::JoinPane(SplitDir::Vertical), Command::SwapMarkedPane] {
        let mut m = mk_mgr();
        m.handle_command(Command::SplitV).unwrap(); // panes 0,1; active 1
        m.handle_command(Command::SelectPane(plexy_glass_mux::Direction::Left)).unwrap(); // active 0
        m.handle_command(Command::MarkPane).unwrap(); // marked 0
        m.handle_command(Command::SelectPane(plexy_glass_mux::Direction::Right)).unwrap(); // active 1
        m.handle_command(Command::ZoomToggle).unwrap();
        assert!(m.active_window().is_zoomed());
        m.handle_command(cmd.clone()).unwrap();
        assert!(!m.active_window().is_zoomed(), "{cmd:?} must clear zoom");
    }
}

#[tokio::test]
async fn swap_pane_next_and_prev_are_directional() {
    let setup = || {
        let mut m = mk_mgr();
        m.handle_command(Command::SplitV).unwrap();
        m.handle_command(Command::SplitV).unwrap(); // leaves [0,1,2], active 2
        m
    };
    // prev: active 2 swaps with pane 1.
    let mut m = setup();
    let vp = m.viewport();
    let r1 = m.active_window().layout().rect_of(PaneId(1), vp).unwrap();
    m.handle_command(Command::SwapPane(false)).unwrap();
    assert_eq!(m.active_window().layout().rect_of(PaneId(2), vp).unwrap(), r1, "prev swaps 2<->1");
    // next: active 2 wraps to pane 0.
    let mut m = setup();
    let r0 = m.active_window().layout().rect_of(PaneId(0), vp).unwrap();
    m.handle_command(Command::SwapPane(true)).unwrap();
    assert_eq!(m.active_window().layout().rect_of(PaneId(2), vp).unwrap(), r0, "next wraps 2<->0");
}

#[tokio::test]
async fn swap_pane_single_pane_is_noop() {
    let mut m = mk_mgr();
    m.handle_command(Command::SwapPane(true)).unwrap();
    assert_eq!(m.active_window().layout().panes(), vec![PaneId(0)]);
    assert_eq!(m.active_window().active(), PaneId(0));
}

#[tokio::test]
async fn break_pane_names_window_from_pane_name() {
    let mut m = mk_mgr();
    m.handle_command(Command::SplitV).unwrap(); // panes 0,1; active 1
    assert!(m.rename_pane_by_id(PaneId(1), "editor".into()));
    m.handle_command(Command::BreakPane).unwrap();
    assert_eq!(m.windows().len(), 2);
    assert_eq!(m.active_window().name, "editor", "new window named from the pane");
}

#[tokio::test]
async fn join_pane_same_window_reorders() {
    let mut m = mk_mgr();
    m.handle_command(Command::SplitV).unwrap(); // panes 0,1; active 1
    m.handle_command(Command::SelectPane(plexy_glass_mux::Direction::Left)).unwrap(); // active 0
    m.handle_command(Command::MarkPane).unwrap(); // marked 0
    m.handle_command(Command::SelectPane(plexy_glass_mux::Direction::Right)).unwrap(); // active 1
    m.handle_command(Command::JoinPane(SplitDir::Horizontal)).unwrap();
    assert_eq!(m.windows().len(), 1, "same-window join keeps one window");
    let panes = m.active_window().layout().panes();
    assert_eq!(panes.len(), 2);
    assert!(panes.contains(&PaneId(0)) && panes.contains(&PaneId(1)));
    assert_eq!(m.active_window().active(), PaneId(0), "joined pane is active");
    assert_eq!(m.marked_pane(), None);
}

#[tokio::test]
async fn join_pane_marked_equals_active_is_status_noop() {
    let mut m = mk_mgr();
    m.handle_command(Command::SplitV).unwrap(); // active 1
    m.handle_command(Command::MarkPane).unwrap(); // marked = active = 1
    let before = m.active_window().layout().panes().len();
    m.handle_command(Command::JoinPane(SplitDir::Vertical)).unwrap();
    assert_eq!(m.active_window().layout().panes().len(), before, "no structural change");
    assert_eq!(m.take_active_message(), Some("marked pane is the active pane"));
}

#[tokio::test]
async fn mark_survives_break_then_swap_is_cross_window() {
    let mut m = mk_mgr();
    m.handle_command(Command::SplitV).unwrap(); // panes 0,1; active 1
    m.handle_command(Command::MarkPane).unwrap(); // marked = 1 (active)
    m.handle_command(Command::BreakPane).unwrap(); // pane 1 → new window
    assert_eq!(m.marked_pane(), Some(PaneId(1)), "mark survives a break (pane still lives)");
    m.handle_command(Command::SelectWindow(0)).unwrap(); // active W0 (pane 0)
    m.handle_command(Command::SwapMarkedPane).unwrap();
    assert_eq!(
        m.take_active_message(),
        Some("marked pane is in another window — use join")
    );
}

#[tokio::test]
async fn buffer_picker_paste_closes_delete_stays_open() {
    let mut m = mk_mgr();
    m.open_buffer_picker(vec![
        plexy_glass_mux::BufferEntry { name: "buffer1".into(), preview: "a".into() },
        plexy_glass_mux::BufferEntry { name: "buffer0".into(), preview: "b".into() },
    ]);
    assert!(matches!(m.overlay(), Some(Overlay::BufferPicker(_))));
    // `d` deletes the selected buffer and keeps the overlay open.
    let r = m.handle_overlay_key(&key('d'));
    assert!(
        matches!(&r, OverlayKeyResult::Buffer(plexy_glass_mux::BufferAction::Delete(n)) if n == "buffer1"),
        "got {r:?}"
    );
    assert!(m.overlay().is_some(), "delete keeps the overlay open");
    // Enter pastes the now-selected buffer and closes.
    let r = m.handle_overlay_key(&KeyEvent::new(
        plexy_glass_mux::Key::Enter,
        plexy_glass_mux::Modifiers::empty(),
    ));
    assert!(
        matches!(&r, OverlayKeyResult::Buffer(plexy_glass_mux::BufferAction::Paste(n)) if n == "buffer0"),
        "got {r:?}"
    );
    assert!(m.overlay().is_none(), "paste closes the overlay");
}

// ----- activity / bell monitoring -----

#[tokio::test]
async fn toggle_monitor_commands_flip_and_message() {
    let mut m = mk_mgr();
    // Defaults: `monitor_activity` off, `monitor_bell` on.
    assert!(!m.active_window().monitor_activity());
    assert!(m.active_window().monitor_bell());
    m.handle_command(Command::ToggleMonitorActivity).unwrap();
    assert_eq!(m.take_active_message(), Some("monitor-activity on"));
    assert!(m.active_window().monitor_activity());
    m.handle_command(Command::ToggleMonitorActivity).unwrap();
    assert_eq!(m.take_active_message(), Some("monitor-activity off"));
    m.handle_command(Command::ToggleMonitorBell).unwrap();
    assert_eq!(m.take_active_message(), Some("monitor-bell off"));
    assert!(!m.active_window().monitor_bell());
}

#[tokio::test]
async fn update_monitor_flags_clears_active_window_alerts() {
    let mut m = mk_mgr();
    m.active_window_mut().set_bell(); // a stale alert on the (current) window
    m.active_window_mut().set_activity();
    m.update_monitor_flags();
    assert!(!m.active_window().bell_flag(), "current window's bell cleared");
    assert!(!m.active_window().activity_flag(), "current window's activity cleared");
}

#[tokio::test]
async fn update_monitor_flags_sets_background_activity_then_clears_on_switch() {
    let mut m = mk_mgr(); // window 0
    m.handle_command(Command::NewWindow).unwrap(); // window 1 (active)
    m.handle_command(Command::ToggleMonitorActivity).unwrap(); // monitor on for window 1
    m.handle_command(Command::SelectWindow(0)).unwrap(); // window 0 active; window 1 background
    // Generate output in the background window's pane (cat echoes it).
    let pid = m.windows()[1].layout().panes()[0];
    m.windows()[1]
        .pane(pid)
        .unwrap()
        .send_input(bytes::Bytes::from_static(b"x\n"))
        .await
        .unwrap();
    // Drain into the sticky flag (the coordinator's per-frame step).
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        m.update_monitor_flags();
        if m.windows()[1].activity_flag() {
            break;
        }
        if Instant::now() > deadline {
            panic!("background activity never flagged");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!m.active_window().activity_flag(), "the current window is never flagged");
    // Switching to the flagged window clears it on the next update.
    m.handle_command(Command::SelectWindow(1)).unwrap();
    m.update_monitor_flags();
    assert!(!m.windows()[1].activity_flag(), "flag cleared once the window is current");
}

#[tokio::test]
async fn update_monitor_flags_sets_background_bell_from_a_real_bel() {
    // Exercises the full emulator-BEL → pane-bell → window-flag chain with a
    // real BEL (not a faked atomic). monitor_bell is on by default.
    //
    // Window 1 must run the test's `cat` spec, NOT `Command::NewWindow`
    // (which deliberately spawns `$SHELL`): cat echoes the typed BEL byte
    // verbatim within milliseconds, whereas an interactive login shell
    // only emits a BEL if its line editor happens to beep on ^G, and that
    // comes after fork/exec + sourcing the user's rc files, which under
    // full-suite load occasionally exceeded the 5s deadline (the old
    // flake) and breaks outright under a different/misconfigured $SHELL.
    let mut m = mk_mgr(); // window 0
    m.new_window_with_spec(spec(), "w1".into()).unwrap(); // window 1 (active), runs `cat`
    m.handle_command(Command::SelectWindow(0)).unwrap(); // window 0 active; window 1 background
    let pid = m.windows()[1].layout().panes()[0];
    // The trailing newline flushes cat's line so it OUTPUTS the raw BEL (the
    // line-discipline echo may render the input as "^G").
    m.windows()[1]
        .pane(pid)
        .unwrap()
        .send_input(bytes::Bytes::from_static(b"\x07\n"))
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        m.update_monitor_flags();
        if m.windows()[1].bell_flag() {
            break;
        }
        if Instant::now() > deadline {
            panic!("background bell never flagged window 1");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!m.active_window().bell_flag(), "the current window is never bell-flagged");
}

#[tokio::test]
async fn open_popup_sets_state_with_derived_size() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
    assert!(!m.has_popup());
    m.handle_command(Command::OpenPopup { command: None }).unwrap();
    assert!(m.has_popup());
    let rect = plexy_glass_mux::popup_rect(m.viewport());
    let (rows, cols) = m
        .popup()
        .unwrap()
        .pane
        .with_screen(|s| (s.active.num_rows(), s.active.num_cols()));
    assert_eq!((rows, cols), (rect.rows - 2, rect.cols - 2));
    assert_eq!(m.popup().unwrap().title, "popup");
}

#[tokio::test]
async fn open_popup_is_last_wins_and_close_clears() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
    m.handle_command(Command::OpenPopup { command: Some("sleep 600".into()) }).unwrap();
    assert_eq!(m.popup().unwrap().title, "sleep 600");
    m.handle_command(Command::OpenPopup { command: None }).unwrap();
    assert_eq!(m.popup().unwrap().title, "popup", "second open replaces the first");
    m.handle_command(Command::ClosePopup).unwrap();
    assert!(!m.has_popup());
    // Idempotent.
    m.handle_command(Command::ClosePopup).unwrap();
    assert!(!m.has_popup());
}

#[tokio::test]
async fn input_target_pane_prefers_popup() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
    let layout_pane = m.active_window().active();
    assert_eq!(
        m.input_target_pane().map(|p| p.id()),
        Some(layout_pane),
        "no popup: input targets the active layout pane"
    );
    m.handle_command(Command::OpenPopup { command: None }).unwrap();
    let popup_id = m.popup().unwrap().pane.id();
    assert_eq!(
        m.input_target_pane().map(|p| p.id()),
        Some(popup_id),
        "popup open: it is modal and owns user input"
    );
    m.handle_command(Command::ClosePopup).unwrap();
    assert_eq!(
        m.input_target_pane().map(|p| p.id()),
        Some(layout_pane),
        "popup closed: input returns to the active layout pane"
    );
}

#[tokio::test]
async fn paste_gate_reads_the_popup_pane_mode_not_the_layout_pane() {
    // Regression: the Ctrl+a ] / choose-buffer paste gate used to read
    // `BRACKETED_PASTE` from the active LAYOUT pane even while a popup was
    // open, but the bytes go to the popup (`handle_input_bytes` is
    // popup-first), so the wrong pane's mode decided the wrapping. The
    // gate decision must be computed from
    // `input_target_pane().wants_bracketed_paste()`.
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
    m.handle_command(Command::OpenPopup { command: None }).unwrap();
    // The popup app turns bracketed paste ON; the layout pane has it OFF.
    m.popup().unwrap().pane.with_screen_mut(|s| {
        s.modes.insert(plexy_glass_emulator::Modes::BRACKETED_PASTE);
    });
    let layout = m.active_window().active_pane().unwrap();
    assert!(!layout.wants_bracketed_paste(), "layout pane: mode off");
    assert!(
        m.input_target_pane().unwrap().wants_bracketed_paste(),
        "while a popup is open, the paste gate's input is the POPUP's mode"
    );
    m.handle_command(Command::ClosePopup).unwrap();
    assert!(
        !m.input_target_pane().unwrap().wants_bracketed_paste(),
        "popup closed: the gate reads the layout pane's mode again"
    );
}

#[tokio::test]
async fn popup_cwd_prefers_live_osc7_then_home_base() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    m.set_window_home_cwd(0, Some("/home/base".into()));
    // No live OSC-7 cwd → home base.
    assert_eq!(m.popup_cwd().as_deref(), Some("/home/base"));
    // Live OSC-7 cwd wins (documented divergence from split_cwd).
    if let Some(pane) = m.active_window().active_pane() {
        pane.with_screen_mut(|s| s.cwd = Some("file:///live/here".to_string()));
    }
    assert_eq!(m.popup_cwd().as_deref(), Some("/live/here"));
    assert_eq!(m.split_cwd().as_deref(), Some("/home/base"), "splits unaffected");
}

#[tokio::test]
async fn popup_pane_death_closes_popup_only() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
    m.handle_command(Command::OpenPopup { command: None }).unwrap();
    let popup_id = m.popup().unwrap().pane.id();
    m.handle_pane_death(popup_id).unwrap();
    assert!(!m.has_popup());
    assert_eq!(m.windows().len(), 1, "layout untouched by popup death");
}

#[tokio::test]
async fn last_window_death_also_closes_popup() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
    m.handle_command(Command::OpenPopup { command: None }).unwrap();
    // The only layout pane dies → session is ending; popup must not orphan.
    m.handle_pane_death(PaneId(0)).unwrap();
    assert!(m.is_empty());
    assert!(!m.has_popup());
}

#[tokio::test]
async fn kill_window_emptying_session_also_closes_popup() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
    m.handle_command(Command::OpenPopup { command: None }).unwrap();
    m.handle_command(Command::KillWindow).unwrap();
    assert!(m.is_empty());
    assert!(!m.has_popup());
}

#[tokio::test]
async fn host_resize_resizes_popup_pane() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh"); // spawns must not depend on `$SHELL`
    m.handle_command(Command::OpenPopup { command: None }).unwrap();
    m.on_host_resize(PtySize { rows: 40, cols: 120, pixel_width: 0, pixel_height: 0 })
        .unwrap();
    let rect = plexy_glass_mux::popup_rect(m.viewport());
    let (rows, cols) = m
        .popup()
        .unwrap()
        .pane
        .with_screen(|s| (s.active.num_rows(), s.active.num_cols()));
    assert_eq!((rows, cols), (rect.rows - 2, rect.cols - 2));
}

#[tokio::test]
async fn popup_swallows_clicks_outside_and_keeps_focus() {
    let mut m = make_two_pane_manager().await;
    let focused_before = m.active_window().active();
    let other = if focused_before == PaneId(0) { PaneId(1) } else { PaneId(0) };
    m.handle_command(Command::OpenPopup { command: None }).unwrap();
    // Click squarely inside the OTHER layout pane (outside the popup box).
    // Geometry check: viewport is (1,1,21,78), so the popup box spans rows
    // 3..=18, cols 9..=70. SplitV focuses the new right pane, so `other` is
    // the left pane at col 1, left of the popup's left edge (col 9). (Even
    // if `other` were the right pane, row rect.row+1 == 2 is above the box.)
    let vp = m.viewport();
    let rect = m.active_window().layout().rect_of(other, vp).unwrap();
    let popup_box = plexy_glass_mux::popup_rect(vp);
    assert!(
        rect.col < popup_box.col || rect.row + 1 < popup_box.row,
        "test premise: click target must be outside the popup box"
    );
    m.handle_mouse(MouseEvent {
        kind: MouseKind::Press,
        button: MouseButton::Left,
        modifiers: plexy_glass_mux::MouseModifiers::default(),
        row: rect.row + 1 + m.pane_row_offset,
        col: rect.col,
    })
    .await
    .unwrap();
    assert_eq!(m.active_window().active(), focused_before, "popup is modal: no focus change");
    assert!(m.has_popup(), "popup still open");
    assert!(m.resize_drag.is_none(), "no border drag starts under a popup");
}

#[tokio::test]
async fn popup_swallows_interior_click_when_child_has_no_mouse_mode() {
    let mut m = make_two_pane_manager().await;
    let focused_before = m.active_window().active();
    m.handle_command(Command::OpenPopup { command: None }).unwrap();
    // Box center is genuinely interior: for a (1,1,21,78) viewport the box
    // is rows 3..=18 / cols 9..=70, so (11, 40) sits inside the border. It
    // also happens to sit on the SplitV gutter, and without Rule 0 this press
    // would start a resize drag, so the drag/focus asserts below bite.
    let rect = plexy_glass_mux::popup_rect(m.viewport());
    m.handle_mouse(MouseEvent {
        kind: MouseKind::Press,
        button: MouseButton::Left,
        modifiers: plexy_glass_mux::MouseModifiers::default(),
        row: rect.row + rect.rows / 2 + m.pane_row_offset,
        col: rect.col + rect.cols / 2,
    })
    .await
    .unwrap();
    assert!(m.has_popup());
    assert!(m.selection().is_none(), "no layout selection starts under a popup");
    assert!(m.resize_drag.is_none(), "no border drag starts under a popup");
    assert_eq!(m.active_window().active(), focused_before, "no focus change under a popup");
}

#[tokio::test]
async fn open_popup_clears_in_flight_resize_drag() {
    let mut m = make_two_pane_manager().await;
    let gutter = gutter_col_for(&m);
    // Start a border drag...
    m.handle_mouse(MouseEvent {
        kind: MouseKind::Press,
        button: MouseButton::Left,
        modifiers: plexy_glass_mux::MouseModifiers::default(),
        row: 5,
        col: gutter,
    })
    .await
    .unwrap();
    assert!(m.resize_drag.is_some(), "premise: drag started");
    // ...then a popup opens (e.g. via a keybinding) mid-drag.
    m.handle_command(Command::OpenPopup { command: None }).unwrap();
    assert!(m.resize_drag.is_none(), "popup open must drop the frozen drag");
    assert!(m.selection().is_none());
}
