use super::*;
use plexy_glass_mux::{
    Command, HintAction, HintKind, HintState, HintTarget, KeyEvent, MouseButton, MouseEvent,
    MouseKind, PickerEntry, TreeAction, TreeNode,
};
use crate::window_manager::OverlayKeyResult;

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
    m.set_status_message("no session: foo".into(), Severity::Error);
    assert_eq!(m.take_active_message(), Some("no session: foo"));
    assert_eq!(m.active_severity(), Severity::Error);
    // Force expiry and confirm it clears in place on the next read.
    m.status_message.as_mut().unwrap().expires_at = Instant::now() - Duration::from_secs(1);
    assert_eq!(m.take_active_message(), None);
    assert!(m.status_message.is_none(), "expired message cleared in place");
    // A newer message replaces the prior one.
    m.set_status_message("first".into(), Severity::Info);
    m.set_status_message("second".into(), Severity::Success);
    assert_eq!(m.take_active_message(), Some("second"));
    assert_eq!(m.active_severity(), Severity::Success);
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

/// A plain click (press + release, no drag) on the cursor's own row injects
/// arrow keys to reposition the shell cursor (even with no OSC 133 prompt
/// mark) and the synthesized arrows reach the child. A drag, by contrast,
/// becomes a selection and injects nothing. Exercises the full `handle_mouse`
/// ladder with a `/bin/cat` child that echoes whatever it receives.
#[tokio::test]
async fn click_release_on_cursor_row_repositions_without_a_mark() {
    use bytes::Bytes;
    use plexy_glass_mux::{MouseButton, MouseEvent, MouseKind, MouseModifiers};

    async fn read_for(rx: &mut tokio::sync::broadcast::Receiver<Bytes>, ms: u64) -> Vec<u8> {
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_millis(ms);
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let step = (deadline - now).min(Duration::from_millis(150));
            match tokio::time::timeout(step, rx.recv()).await {
                Ok(Ok(chunk)) => out.extend_from_slice(&chunk),
                Ok(Err(_)) => break,          // channel closed or lagged
                Err(_) if !out.is_empty() => break, // idle after data → done
                Err(_) => {}                  // still idle, keep waiting to deadline
            }
        }
        out
    }
    let press = |row, col| MouseEvent {
        kind: MouseKind::Press,
        button: MouseButton::Left,
        modifiers: MouseModifiers::default(),
        row,
        col,
    };
    let at = |kind, row, col| MouseEvent {
        kind,
        button: MouseButton::Left,
        modifiers: MouseModifiers::default(),
        row,
        col,
    };

    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(), // `/bin/cat` echoes input back as output
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    let pane = m.active_window().pane(PaneId(0)).cloned().unwrap();
    // Cursor at (row 5, col 8), NO PROMPT_END mark, primary screen.
    pane.with_screen_mut(|s| {
        s.cursor.row = 5;
        s.cursor.col = 8;
    });

    // Physical (6,6) → pane-local (5,5): the frame inset is one cell on each
    // side (viewport origin (1,1)), and the status bar is unset (offset 0).
    let mut rx = pane.subscribe_output();
    m.handle_mouse(press(6, 6)).await.unwrap();
    m.handle_mouse(at(MouseKind::Release, 6, 6)).await.unwrap();
    // cursor col 8 → click col 5 = 3 graphemes left → three "\x1b[D" Left
    // arrows. cat echoes them back (its cooked PTY renders ESC as caret
    // notation, so match the arrow terminators rather than the raw ESC form).
    let echoed = read_for(&mut rx, 2000).await;
    let lefts = echoed.iter().filter(|&&b| b == b'D').count();
    let rights = echoed.iter().filter(|&&b| b == b'C').count();
    assert_eq!((lefts, rights), (3, 0), "expected 3 Left arrows, got {echoed:?}");

    // A DRAG (press, move to a different cell, release) must NOT reposition, it is
    // a selection. No arrows should be injected.
    pane.with_screen_mut(|s| {
        s.cursor.row = 5;
        s.cursor.col = 8;
    });
    let mut rx2 = pane.subscribe_output();
    m.handle_mouse(press(6, 6)).await.unwrap();
    m.handle_mouse(at(MouseKind::Move, 6, 9)).await.unwrap();
    m.handle_mouse(at(MouseKind::Release, 6, 9)).await.unwrap();
    let dragged = read_for(&mut rx2, 400).await;
    assert!(
        !dragged.iter().any(|&b| b == b'D' || b == b'C'),
        "a drag must select, not inject cursor-movement arrows, got {dragged:?}"
    );

    // A one-cell pointer DRIFT during a click (within the dead-zone) must still
    // reposition and not degrade into a one-character selection/copy. Press (5,5),
    // nudge one cell to (5,6), release → repositions to the anchor col 5.
    pane.with_screen_mut(|s| {
        s.cursor.row = 5;
        s.cursor.col = 8;
    });
    let mut rx3 = pane.subscribe_output();
    m.handle_mouse(press(6, 6)).await.unwrap();
    m.handle_mouse(at(MouseKind::Move, 6, 7)).await.unwrap();
    m.handle_mouse(at(MouseKind::Release, 6, 7)).await.unwrap();
    let jittered = read_for(&mut rx3, 2000).await;
    assert_eq!(
        jittered.iter().filter(|&&b| b == b'D').count(),
        3,
        "a one-cell drift must still reposition (dead-zone), got {jittered:?}"
    );

    let _ = pane.send_input(Bytes::from_static(&[0x04])).await; // EOF
}

/// Regression: the click dead-zone must NOT swallow an explicit word/line
/// selection whose span is ≤ 1 column. Double-clicking a TWO-character word
/// (e.g. `ls`) yields anchor..head with Δcol == 1, which `is_click()` reports
/// true, but it is a word selection (double-click), so it must still copy.
#[tokio::test]
async fn double_click_on_a_two_char_word_still_copies() {
    use plexy_glass_mux::{MouseButton, MouseEvent, MouseKind, MouseModifiers};
    let ev = |kind, row, col| MouseEvent {
        kind,
        button: MouseButton::Left,
        modifiers: MouseModifiers::default(),
        row,
        col,
    };

    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    let pane = m.active_window().pane(PaneId(0)).cloned().unwrap();
    // "  ls  " on row 5; cursor parked elsewhere so the first click's reposition
    // path is a no-op (wrong row) and can't interfere.
    pane.with_screen_mut(|s| {
        s.cursor.row = 0;
        s.cursor.col = 0;
        for (i, ch) in "  ls  ".chars().enumerate() {
            s.active.rows[5].cells[i].grapheme = ch.to_string().into();
        }
    });
    assert_eq!(m.take_active_message(), None);

    // Double-click 'l' at physical (6,3) → pane-local (5,2). Two press/release
    // pairs in quick succession → the second press classifies as count==2 →
    // word_at selects "ls".
    for _ in 0..2 {
        m.handle_mouse(ev(MouseKind::Press, 6, 3)).await.unwrap();
        m.handle_mouse(ev(MouseKind::Release, 6, 3)).await.unwrap();
    }
    let msg = m.take_active_message();
    assert!(
        msg.is_some_and(|s| s.starts_with("copied")),
        "double-clicking a 2-char word must copy it, not be swallowed by the dead-zone"
    );
}

/// A mouse-reporting child (editor, pager, Claude Code's click-to-move) must
/// receive clicks in ITS OWN coordinate space, not the viewport's. Regression:
/// `forward_mouse_to_pane` used to encode the raw viewport event, so a click in
/// a split/offset pane reported a cell offset by the pane's position and the
/// child's click-to-move missed.
/// A pane's PTY must carry the host's real cell size (pixels per cell), so a
/// child scales inline graphics and answers CSI 14/16/18t correctly, not the
/// emulator's 10×20 fallback. Regression: the host pixel dims were dropped at
/// the PtySize→Rect→PtySize boundary, leaving every pane at the fallback.
#[tokio::test]
async fn panes_inherit_real_host_cell_size_pixels() {
    let notify = Arc::new(Notify::new());
    // Host cell = 720/80 × 432/24 = 9×18 (distinct from the 10×20 fallback).
    let host = PtySize { rows: 24, cols: 80, pixel_width: 720, pixel_height: 432 };
    let mut m = WindowManager::new(spec(), host, notify, None, cfg()).unwrap();
    let pane = m.active_window().pane(PaneId(0)).cloned().unwrap();
    assert_eq!(pane.with_screen(|s| s.cell_pixels()), (9, 18), "construction must relay host cell size");
    m.on_host_resize(PtySize { rows: 24, cols: 80, pixel_width: 800, pixel_height: 480 })
        .unwrap();
    assert_eq!(pane.with_screen(|s| s.cell_pixels()), (10, 20), "host resize must update the cell size");
}

/// Copy-mode mouse must use pane-local coordinates (CopyMode::handle_mouse
/// treats them as such). Regression: handle_copy_mode_mouse forwarded the raw
/// viewport event, so a click in an offset pane set the copy cursor off by the
/// pane rect origin (~40 columns in a right split, past the pane's width).
#[tokio::test]
async fn copy_mode_mouse_uses_pane_local_coords() {
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
    m.set_default_program("/bin/sh");
    m.handle_command(Command::SplitV).unwrap();
    let active = m.active_window().active(); // right pane, offset from origin
    let pane = m.active_window().pane(active).cloned().unwrap();
    let rect = m.active_window().layout().rect_of(active, m.viewport()).unwrap();
    assert!(rect.col > 1, "right pane should be offset from the viewport origin");
    pane.enter_copy_mode(100, rect.rows, 0, 0);

    m.handle_mouse(MouseEvent {
        kind: MouseKind::Press,
        button: MouseButton::Left,
        modifiers: MouseModifiers::default(),
        row: rect.row + 2,
        col: rect.col + 5,
    })
    .await
    .unwrap();

    let col = pane.with_copy_mode(|cm| cm.cursor.1).unwrap();
    assert_eq!(col, 5, "copy-mode cursor col must be pane-local (5), not viewport ({})", rect.col + 5);
}

#[tokio::test]
async fn forwarded_mouse_uses_pane_local_coords() {
    use bytes::Bytes;
    use plexy_glass_emulator::Modes;
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
    m.set_default_program("/bin/cat"); // splits echo what they receive
    m.handle_command(Command::SplitV).unwrap();
    let active = m.active_window().active(); // right pane, offset from origin
    let pane = m.active_window().pane(active).cloned().unwrap();
    // The child enabled `?1000` + `?1006` (a TUI); Rule 5 will forward the click.
    pane.with_screen_mut(|s| {
        s.modes.insert(Modes::MOUSE_BTN);
        s.modes.insert(Modes::MOUSE_SGR);
    });
    let rect = m.active_window().layout().rect_of(active, m.viewport()).unwrap();
    assert!(rect.col > 1, "right pane should be offset from the viewport origin");

    let mut rx = pane.subscribe_output();
    m.handle_mouse(MouseEvent {
        kind: MouseKind::Press,
        button: MouseButton::Left,
        modifiers: MouseModifiers::default(),
        row: rect.row + 3,
        col: rect.col + 5,
    })
    .await
    .unwrap();

    let mut out = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(1500);
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        match tokio::time::timeout((deadline - now).min(Duration::from_millis(150)), rx.recv()).await
        {
            Ok(Ok(c)) => out.extend_from_slice(&c),
            Ok(Err(_)) => break,
            Err(_) if !out.is_empty() => break,
            Err(_) => {}
        }
    }
    // Pane-local (row 3, col 5) → SGR `ESC[<0;6;4M` (col+1;row+1). The raw
    // viewport coords (col ≈ rect.col+6) would not contain this.
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains(";6;4M"), "child must get pane-local SGR coords ;6;4M, got {s:?}");
    let _ = pane.send_input(Bytes::from_static(&[0x04])).await;
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

/// Push `n` blank rows into the active pane's scrollback, setting
/// `PROMPT_START` on the given absolute lines. Marks live on the rows
/// themselves, so injecting scrollback rows directly exercises the same scan
/// the real OSC 133 path feeds.
fn inject_scrollback_prompts(m: &WindowManager, n: usize, prompts: &[usize]) {
    use plexy_glass_emulator::{Row, RowMark};
    let pane = m.active_window().active_pane().unwrap();
    pane.with_screen_mut(|s| {
        let cols = s.active.num_cols();
        for i in 0..n {
            let mut row = Row::blank(cols);
            if prompts.contains(&i) {
                row.mark.set(RowMark::PROMPT_START);
            }
            s.scrollback.push(row);
        }
    });
}

fn active_scroll_offset(m: &WindowManager) -> u32 {
    m.active_window().active_pane().unwrap().scroll_offset()
}

// Offset math, pinned: at offset N the compositor shows N scrollback rows
// above the grid, so the top visible absolute line is `scrollback_len - N`.
// With `scrollback_len` = 10 and prompts at absolute lines 2 and 6, putting
// prompt L at the viewport top means offset = 10 - L.
#[tokio::test]
async fn prev_prompt_pins_target_to_viewport_top_and_clamps_at_oldest() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    inject_scrollback_prompts(&m, 10, &[2, 6]);
    // Live (offset 0, top = 10): prev prompt is 6 → offset 10-6 = 4.
    m.handle_command(Command::PrevPrompt).unwrap();
    assert_eq!(active_scroll_offset(&m), 4);
    // Top = 6: prev prompt strictly above is 2 → offset 8.
    m.handle_command(Command::PrevPrompt).unwrap();
    assert_eq!(active_scroll_offset(&m), 8);
    // Top = 2 (the oldest prompt): nothing above → no-op, no wraparound.
    m.handle_command(Command::PrevPrompt).unwrap();
    assert_eq!(active_scroll_offset(&m), 8);
}

#[tokio::test]
async fn next_prompt_walks_forward_then_snaps_to_live() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    inject_scrollback_prompts(&m, 10, &[2, 6]);
    let pane = m.active_window().active_pane().unwrap();
    pane.set_scroll_offset(8, 10); // top = 2 (the oldest prompt)
    // Next prompt below 2 is 6 → offset 4.
    m.handle_command(Command::NextPrompt).unwrap();
    assert_eq!(active_scroll_offset(&m), 4);
    // Top = 6 (the newest prompt): past it → live (offset 0).
    m.handle_command(Command::NextPrompt).unwrap();
    assert_eq!(active_scroll_offset(&m), 0);
    // Already live: stays live.
    m.handle_command(Command::NextPrompt).unwrap();
    assert_eq!(active_scroll_offset(&m), 0);
}

#[tokio::test]
async fn next_prompt_snaps_to_live_when_the_prompt_is_in_the_grid() {
    use plexy_glass_emulator::RowMark;
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    inject_scrollback_prompts(&m, 10, &[2]);
    let pane = m.active_window().active_pane().unwrap();
    // A prompt on grid row 3 = absolute line 13 (> scrollback_len 10): the
    // target offset would be negative; it saturates to live.
    pane.with_screen_mut(|s| s.active.rows[3].mark.set(RowMark::PROMPT_START));
    pane.set_scroll_offset(6, 10); // top = 4: next prompt is the grid one
    m.handle_command(Command::NextPrompt).unwrap();
    assert_eq!(active_scroll_offset(&m), 0);
}

#[tokio::test]
async fn prev_prompt_lands_target_at_top_under_a_fold() {
    use plexy_glass_emulator::{Row, RowMark};
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 6, cols: 20, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    // Scrollback: block0 (p@0, out@1,2), block1 (p@3, out@4), block2 (p@5).
    {
        let pane = m.active_window().active_pane().unwrap();
        pane.with_screen_mut(|s| {
            let cols = s.active.num_cols();
            let mut push = |flag: Option<u8>| {
                let mut r = Row::blank(cols);
                if let Some(f) = flag {
                    r.mark.set(f);
                }
                s.scrollback.push(r);
            };
            push(Some(RowMark::PROMPT_START)); // 0
            push(Some(RowMark::OUTPUT_START)); // 1
            push(None); // 2
            push(Some(RowMark::PROMPT_START)); // 3
            push(Some(RowMark::OUTPUT_START)); // 4
            push(Some(RowMark::PROMPT_START)); // 5
            plexy_glass_mux::blocks::set_block_folded(s, 0, true); // hide unified 1,2
        });
        // Scroll so block1's prompt (unified 3) is at the top.
        let (off, max) = pane.with_screen(|s| {
            let r = s.active.num_rows();
            (
                plexy_glass_mux::blocks::scroll_offset_for_top(s, r, 3),
                plexy_glass_mux::blocks::max_scroll_offset(s, r),
            )
        });
        pane.set_scroll_offset(off, max);
    }
    let top_line = |m: &WindowManager| {
        let p = m.active_window().active_pane().unwrap();
        let off = p.scroll_offset();
        p.with_screen(|s| {
            plexy_glass_mux::blocks::scroll_line_at(s, s.active.num_rows(), off, 0)
        })
    };
    assert_eq!(top_line(&m), 3, "setup: block1 prompt at the top");
    // Prev-prompt jumps to block0's prompt (unified 0), which must land at the
    // top exactly despite the fold (unified 1,2) below it.
    m.handle_command(Command::PrevPrompt).unwrap();
    assert_eq!(top_line(&m), 0, "prev-prompt lands the target at the top under a fold");
}

#[tokio::test]
async fn fold_via_block_mode_dispatch_persists_after_exit() {
    let notify = Arc::new(Notify::new());
    let m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    let pane = m.active_window().active_pane().unwrap();
    // Two blocks so block 0 (prompt 0 + output 1) is completed/foldable.
    pane.with_screen_mut(|s| {
        use plexy_glass_emulator::RowMark;
        s.active.rows[0].mark.set(RowMark::PROMPT_START);
        s.active.rows[1].mark.set(RowMark::OUTPUT_START);
        s.active.rows[3].mark.set(RowMark::PROMPT_START);
    });
    let screen = pane.with_screen(|s| s.clone());
    pane.enter_block_mode(plexy_glass_mux::BlockMode::new_for(&screen, 24).unwrap());
    // Apply the fold exactly as the connection dispatch does.
    pane.with_screen_mut(|s| plexy_glass_mux::blocks::toggle_block_fold(s, 0));
    assert!(pane.with_screen(|s| s.active.rows[0].mark.is_folded()), "block folded");
    // Leaving block mode must NOT clear the fold, that's the whole point.
    pane.exit_block_mode();
    assert!(pane.with_screen(|s| s.active.rows[0].mark.is_folded()), "fold persists after exit");
}

#[tokio::test]
async fn block_scroll_without_marks_prev_noops_and_next_goes_live() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    inject_scrollback_prompts(&m, 10, &[]); // scrollback, but no prompts
    m.handle_command(Command::PrevPrompt).unwrap();
    assert_eq!(active_scroll_offset(&m), 0, "prev with no marks is a no-op");
    m.active_window().active_pane().unwrap().set_scroll_offset(5, 10);
    m.handle_command(Command::PrevPrompt).unwrap();
    assert_eq!(active_scroll_offset(&m), 5, "prev keeps a manual scroll");
    m.handle_command(Command::NextPrompt).unwrap();
    assert_eq!(active_scroll_offset(&m), 0, "next past the newest snaps live");
}

#[tokio::test]
async fn block_scroll_does_not_clear_zoom() {
    let mut m = make_two_pane_manager().await;
    m.handle_command(Command::ZoomToggle).unwrap();
    assert!(m.active_window().is_zoomed());
    m.handle_command(Command::PrevPrompt).unwrap();
    m.handle_command(Command::NextPrompt).unwrap();
    assert!(m.active_window().is_zoomed(), "view-only verbs keep zoom");
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

#[tokio::test]
async fn resize_drag_move_changes_the_split_border() {
    let mut m = make_two_pane_manager().await; // vertical split: pane 0 | pane 1
    let gutter = gutter_col_for(&m);
    let vp = m.viewport();
    let before = m.active_window().layout().rect_of(PaneId(0), vp).unwrap();
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
    // Drag the gutter several columns right → the left pane widens.
    m.handle_mouse(MouseEvent {
        kind: MouseKind::Move,
        button: MouseButton::Left,
        modifiers: plexy_glass_mux::MouseModifiers::default(),
        row: 5,
        col: gutter + 5,
    })
    .await
    .unwrap();
    let after = m.active_window().layout().rect_of(PaneId(0), vp).unwrap();
    assert!(after.cols > before.cols, "drag right widened pane 0: {before:?} -> {after:?}");
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
        kind: MouseKind::Wheel { delta: 3, horizontal: false },
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

#[tokio::test]
async fn pane_death_of_active_middle_window_focuses_next() {
    let mut m = mk_mgr(); // W0 (pane 0)
    m.handle_command(Command::NewWindow).unwrap(); // W1 (pane 1)
    m.handle_command(Command::NewWindow).unwrap(); // W2 (pane 2)
    m.handle_command(Command::SelectWindow(1)).unwrap(); // active middle window
    let w2_id = m.windows()[2].id;
    // The active middle window's sole pane dies via the death channel → the
    // window is removed. Focus must land on the NEXT window (W2), matching
    // `KillWindow`'s tmux-standard policy, not the previous window.
    m.handle_pane_death(PaneId(1)).unwrap();
    assert_eq!(m.windows().len(), 2);
    assert_eq!(
        m.windows()[m.active_idx()].id,
        w2_id,
        "focus follows the next window when the active middle window dies"
    );
}

#[tokio::test]
async fn cross_window_swap_into_monitored_window_does_not_replay_done() {
    // A pane that ran commands (blocks_completed > 0) swapped into a background
    // monitor-command window must NOT fire a spurious "done" alert, because its
    // block baseline is seeded from its live counter at install time.
    let mut m = mk_mgr(); // W0: pane 0, active
    m.handle_command(Command::ToggleMonitorCommand).unwrap(); // monitor-command on W0
    m.handle_command(Command::MarkPane).unwrap(); // marked = pane 0 (in W0)
    m.new_window_with_spec(spec(), "api".into()).unwrap(); // W1: pane 1, active
    // pane 1 completed commands before being moved.
    set_block_counter(&m, 1, 3, Some(0));
    let _ = m.update_monitor_flags().alert_edge; // establish baselines (W1 active, W0 background)
    // Swap marked(pane 0 @ W0) <-> active(pane 1 @ W1): pane 1 (blocks=3) lands
    // in the background, monitored W0.
    m.handle_command(Command::SwapMarkedPane).unwrap();
    assert!(
        m.windows()[0].pane(PaneId(1)).is_some(),
        "the blocks>0 pane moved into the monitored background window"
    );
    let _ = m.take_active_message();
    assert!(
        !m.update_monitor_flags().alert_edge,
        "a moved pane must not replay its prior completion as a done alert"
    );
    assert_eq!(m.windows()[0].done_flag(), None, "no spurious done flag");
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
async fn rename_tree_session_rewrites_subtree_and_rekeys_folds() {
    use plexy_glass_mux::NodeKey;
    let mut m = mk_mgr();
    // Two sessions: "work" is NOT last, so its subtree is bounded by "other"'s
    // depth-0 row (exercises the descendant scan).
    m.open_tree(vec![
        tnode("work", None, None, 0),
        tnode("work", Some(0), None, 1),
        tnode("work", Some(0), Some(5), 2),
        tnode("other", None, None, 0),
        tnode("other", Some(0), None, 1),
    ]);
    // Collapse work's window so a NodeKey::Window fold exists to be re-keyed.
    m.handle_overlay_key(&key('j')); // select the window row
    m.handle_overlay_key(&key('h')); // collapse it

    m.rename_tree_session("work", "dev");

    let Some(Overlay::Tree(state)) = m.overlay() else { panic!("expected tree overlay") };
    // Session row + its descendants now point at the new name.
    assert_eq!(state.nodes[0].name, "dev");
    assert_eq!(state.nodes[0].session, "dev");
    assert_eq!(state.nodes[1].session, "dev");
    assert_eq!(state.nodes[2].session, "dev");
    assert!(state.nodes[0].label.contains("dev"), "label re-derived: {:?}", state.nodes[0].label);
    // The other session is untouched.
    assert_eq!(state.nodes[3].session, "other");
    // The collapsed window fold was re-keyed to the new session name.
    assert!(
        state
            .collapsed
            .contains(&NodeKey::Window { session: "dev".into(), window: WindowId(0) }),
        "collapsed fold re-keyed: {:?}",
        state.collapsed
    );
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
async fn swap_marked_pane_same_window_then_cross_window_swaps() {
    let mut m = mk_mgr();
    m.handle_command(Command::SplitV).unwrap(); // W0 panes 0,1; active 1
    let vp = m.viewport();
    m.handle_command(Command::MarkPane).unwrap(); // marked = 1
    m.status_message = None; // clear the "marked pane" confirmation
    m.handle_command(Command::SelectPane(plexy_glass_mux::Direction::Left)).unwrap(); // active 0
    let before = m.active_window().layout().rect_of(PaneId(1), vp).unwrap();
    m.handle_command(Command::SwapMarkedPane).unwrap();
    let after = m.active_window().layout().rect_of(PaneId(1), vp).unwrap();
    assert_ne!(before, after, "same-window swap exchanged slots");
    assert_eq!(m.marked_pane(), Some(PaneId(1)), "mark preserved across swap");

    // Cross-window: marked stays in W0; move active to a new window → the
    // panes exchange slots across windows (the old "use join" refusal is gone).
    m.handle_command(Command::NewWindow).unwrap(); // W1: pane 2, active
    m.handle_command(Command::SwapMarkedPane).unwrap();
    assert_eq!(m.take_active_message(), None);
    assert!(m.windows()[1].pane(PaneId(1)).is_some(), "M moved into the active window");
    assert!(m.windows()[0].pane(PaneId(2)).is_some(), "A moved into the other window");
    assert_eq!(m.marked_pane(), Some(PaneId(1)), "mark preserved across the cross-window swap");
}

#[tokio::test]
async fn swap_marked_cross_window_exchanges_slots() {
    let mut m = mk_mgr(); // W0: pane 0
    m.handle_command(Command::SplitV).unwrap(); // W0 {0,1}, active 1
    m.handle_command(Command::NewWindow).unwrap(); // W1: pane 2, active
    m.handle_command(Command::SplitV).unwrap(); // W1 {2,3}, active 3
    m.handle_command(Command::MarkPane).unwrap(); // marked = 3
    m.status_message = None; // clear the "marked pane" confirmation
    m.handle_command(Command::SelectWindow(0)).unwrap(); // active W0, pane 1
    let vp = m.viewport();
    let slot_a = m.windows()[0].layout().rect_of(PaneId(1), vp).unwrap();
    let slot_m = m.windows()[1].layout().rect_of(PaneId(3), vp).unwrap();
    m.handle_command(Command::SwapMarkedPane).unwrap();
    // Occupants exchanged: W0 now holds {0,3}, W1 holds {2,1}.
    assert!(m.windows()[0].pane(PaneId(3)).is_some());
    assert!(m.windows()[0].pane(PaneId(1)).is_none());
    assert!(m.windows()[1].pane(PaneId(1)).is_some());
    assert!(m.windows()[1].pane(PaneId(3)).is_none());
    let w0_panes = m.windows()[0].layout().panes();
    assert!(w0_panes.contains(&PaneId(0)) && w0_panes.contains(&PaneId(3)));
    let w1_panes = m.windows()[1].layout().panes();
    assert!(w1_panes.contains(&PaneId(2)) && w1_panes.contains(&PaneId(1)));
    // Each pane sits in the other's old slot (shape preserved).
    assert_eq!(m.windows()[0].layout().rect_of(PaneId(3), vp), Some(slot_a));
    assert_eq!(m.windows()[1].layout().rect_of(PaneId(1), vp), Some(slot_m));
    // The mark is preserved and points at M, now in the active window.
    assert_eq!(m.marked_pane(), Some(PaneId(3)));
    assert!(m.active_window().pane(PaneId(3)).is_some());
    assert_eq!(m.take_active_message(), None, "no refusal message");
}

#[tokio::test]
async fn swap_marked_cross_window_focus_follows_slot() {
    let mut m = mk_mgr(); // W0: pane 0
    m.handle_command(Command::NewWindow).unwrap(); // W1: pane 1, active
    m.handle_command(Command::MarkPane).unwrap(); // marked = 1 (W1's active)
    m.handle_command(Command::SelectWindow(0)).unwrap(); // active W0, pane 0
    m.handle_command(Command::SwapMarkedPane).unwrap();
    // W0's active slot now holds M; W1's active slot (was M) now holds A.
    assert_eq!(m.windows()[0].active(), PaneId(1), "active window focuses M");
    assert_eq!(m.windows()[1].active(), PaneId(0), "other window's focus follows the slot");
}

#[tokio::test]
async fn swap_marked_cross_window_zoom_follows_slot_in_other_window() {
    let mut m = mk_mgr(); // W0: pane 0
    m.handle_command(Command::NewWindow).unwrap(); // W1: pane 1, active
    m.handle_command(Command::MarkPane).unwrap(); // marked = 1
    m.handle_command(Command::SelectWindow(0)).unwrap(); // active W0
    // Zoom the OTHER window on M directly (chords can't: leaving a window
    // clears its zoom). The active window's zoom is pre-cleared by
    // `command_clears_zoom`, so only the other window's remap is reachable.
    m.windows[1].zoomed = Some(PaneId(1));
    m.handle_command(Command::SwapMarkedPane).unwrap();
    assert_eq!(
        m.windows()[1].zoomed,
        Some(PaneId(0)),
        "the zoomed SLOT keeps showing its occupant"
    );
    assert_eq!(m.windows()[0].zoomed, None, "active window zoom stays cleared");
}

#[tokio::test]
async fn swap_marked_cross_window_then_close_falls_back_sanely() {
    let mut m = mk_mgr(); // W0: pane 0
    m.handle_command(Command::SplitV).unwrap(); // W0 {0,1}, active 1
    m.handle_command(Command::NewWindow).unwrap(); // W1: pane 2, active
    m.handle_command(Command::MarkPane).unwrap(); // marked = 2
    m.handle_command(Command::SelectWindow(0)).unwrap(); // active W0, pane 1
    m.handle_command(Command::SwapMarkedPane).unwrap(); // W0 {0,2}, active 2
    assert_eq!(m.active_window().active(), PaneId(2));
    // Close the new active: focus must fall back to a live pane (the
    // rewritten history must not resurrect the departed pane 1).
    m.handle_command(Command::KillPane).unwrap();
    let active = m.active_window().active();
    assert!(
        m.active_window().layout().panes().contains(&active),
        "fallback focus {active:?} must be a live pane"
    );
    assert_eq!(active, PaneId(0));
    assert_eq!(m.marked_pane(), None, "killing the marked pane clears the mark");
}

#[tokio::test]
async fn swap_marked_cross_window_resizes_both_windows() {
    let mut m = mk_mgr(); // W0: pane 0 (full viewport)
    m.handle_command(Command::NewWindow).unwrap(); // W1: pane 1
    m.handle_command(Command::SplitV).unwrap(); // W1 {1,2}, active 2 (half width)
    m.handle_command(Command::MarkPane).unwrap(); // marked = 2
    m.handle_command(Command::SelectWindow(0)).unwrap(); // active W0, pane 0
    let vp = m.viewport();
    m.handle_command(Command::SwapMarkedPane).unwrap();
    // M (pane 2) now fills W0's full-viewport slot; A (pane 0) sits in W1's
    // half-width slot. Both PTYs must match their new rects immediately.
    let r2 = m.windows()[0].layout().rect_of(PaneId(2), vp).unwrap();
    let c2 = m.windows()[0].pane(PaneId(2)).unwrap().with_screen(|s| s.active.num_cols());
    assert_eq!(c2, r2.cols, "M resized to the full-viewport slot");
    assert_eq!(c2, vp.cols);
    let r0 = m.windows()[1].layout().rect_of(PaneId(0), vp).unwrap();
    let c0 = m.windows()[1].pane(PaneId(0)).unwrap().with_screen(|s| s.active.num_cols());
    assert_eq!(c0, r0.cols, "A resized to the half-width slot");
    assert!(c0 < vp.cols);
}

#[tokio::test]
async fn swap_marked_no_mark_is_status_noop() {
    let mut m = mk_mgr();
    m.handle_command(Command::SplitV).unwrap();
    m.handle_command(Command::SwapMarkedPane).unwrap();
    assert_eq!(m.take_active_message(), Some("no marked pane"));
}

#[tokio::test]
async fn swap_marked_equals_active_is_noop() {
    let mut m = mk_mgr();
    m.handle_command(Command::SplitV).unwrap(); // panes 0,1; active 1
    m.handle_command(Command::MarkPane).unwrap(); // marked = active = 1
    m.status_message = None; // clear the "marked pane" confirmation
    let vp = m.viewport();
    let before = m.active_window().layout().rect_of(PaneId(1), vp);
    m.handle_command(Command::SwapMarkedPane).unwrap();
    assert_eq!(m.active_window().layout().rect_of(PaneId(1), vp), before);
    assert_eq!(m.marked_pane(), Some(PaneId(1)), "mark untouched");
    assert_eq!(m.take_active_message(), None);
}

#[tokio::test]
async fn swap_marked_vanished_mark_is_cleared_silently() {
    let mut m = mk_mgr();
    m.handle_command(Command::SplitV).unwrap(); // panes 0,1; active 1
    // Force a dangling mark (the kill paths normally clear it first).
    m.marked_pane = Some(PaneId(77));
    let before = m.active_window().layout().panes();
    m.handle_command(Command::SwapMarkedPane).unwrap();
    assert_eq!(m.marked_pane(), None, "vanished mark cleared");
    assert_eq!(m.active_window().layout().panes(), before, "no structural change");
    assert_eq!(m.take_active_message(), None, "cleared silently");
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
    m.status_message = None; // clear the "marked pane" confirmation
    m.handle_command(Command::BreakPane).unwrap(); // pane 1 → new window
    assert_eq!(m.marked_pane(), Some(PaneId(1)), "mark survives a break (pane still lives)");
    m.handle_command(Command::SelectWindow(0)).unwrap(); // active W0 (pane 0)
    m.handle_command(Command::SwapMarkedPane).unwrap();
    // Cross-window swap: the broken-out pane comes back to W0's slot and
    // pane 0 takes its place in the new window.
    assert_eq!(m.take_active_message(), None);
    assert_eq!(m.windows()[0].layout().panes(), vec![PaneId(1)]);
    assert_eq!(m.windows()[1].layout().panes(), vec![PaneId(0)]);
    assert_eq!(m.marked_pane(), Some(PaneId(1)));
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
async fn mark_pane_confirms_set_and_clear() {
    let mut m = mk_mgr();
    m.handle_command(Command::MarkPane).unwrap();
    assert_eq!(m.take_active_message(), Some("marked pane"));
    assert_eq!(m.active_severity(), Severity::Success);
    // Marking the same pane again clears it, with a neutral notice.
    m.handle_command(Command::MarkPane).unwrap();
    assert_eq!(m.take_active_message(), Some("mark cleared"));
    assert_eq!(m.active_severity(), Severity::Info);
}

#[tokio::test]
async fn kill_window_flashes_named_window() {
    let mut m = mk_mgr();
    m.handle_command(Command::NewWindow).unwrap(); // 2 windows, active index 1 (window 2)
    m.status_message = None; // clear any incidental message
    // Capture the name the way production does, so we assert the *named* half too
    // (not just the "killed window 2" prefix, which would pass on empty parens).
    let auto_rename = m.config.auto_rename;
    let name = m.active_window().display_name(auto_rename);
    m.handle_command(Command::KillWindow).unwrap();
    let msg = m.take_active_message().expect("kill-window flashes a message");
    assert_eq!(msg, format!("killed window 2 ({name})"), "names the killed window");
    assert_eq!(m.active_severity(), Severity::Success);
}

#[tokio::test]
async fn welcome_overlay_opens_and_any_key_dismisses() {
    let mut m = mk_mgr();
    m.open_welcome();
    assert!(matches!(m.overlay(), Some(plexy_glass_mux::Overlay::Welcome)));
    // Any key dismisses the modal (it's a "press any key to continue" banner).
    let r = m.handle_overlay_key(&key('x'));
    assert_eq!(r, OverlayKeyResult::Redraw);
    assert!(m.overlay().is_none(), "any key closes the welcome modal");
}

#[tokio::test]
async fn update_monitor_flags_clears_active_window_alerts() {
    let mut m = mk_mgr();
    m.active_window_mut().set_bell(); // a stale alert on the (current) window
    m.active_window_mut().set_activity();
    let _ = m.update_monitor_flags().alert_edge;
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
        let _ = m.update_monitor_flags().alert_edge;
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
    let _ = m.update_monitor_flags().alert_edge;
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
        let _ = m.update_monitor_flags().alert_edge;
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

/// Drive output into the background window's pane until `update_monitor_flags`
/// reports an edge (returns true) or the deadline passes. Returns whether an
/// edge fired and (if so) the message it set. Pumps a fresh byte each poll so
/// the activity atomic keeps re-arming.
async fn drive_until_alert(m: &mut WindowManager, bg_idx: usize) -> Option<String> {
    let pid = m.windows()[bg_idx].layout().panes()[0];
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        m.windows()[bg_idx]
            .pane(pid)
            .unwrap()
            .send_input(bytes::Bytes::from_static(b"x\n"))
            .await
            .unwrap();
        if m.update_monitor_flags().alert_edge {
            return m.take_active_message().map(str::to_string);
        }
        if Instant::now() > deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn activity_edge_in_background_window_emits_message() {
    let mut m = mk_mgr(); // window 0 (`cat`)
    m.new_window_with_spec(spec(), "api".into()).unwrap(); // window 1 (active), `cat`
    m.handle_command(Command::ToggleMonitorActivity).unwrap(); // monitor on for window 1
    m.handle_command(Command::SelectWindow(0)).unwrap(); // window 0 active; window 1 background
    let msg = drive_until_alert(&mut m, 1).await;
    assert_eq!(
        msg.as_deref(),
        Some("activity in window 2 (api)"),
        "background activity edge fires a 1-based message with the window name"
    );
    assert!(m.windows()[1].activity_flag(), "the sticky flag is set");
}

#[tokio::test]
async fn activity_in_active_window_emits_no_message() {
    let mut m = mk_mgr(); // window 0 (`cat`), active
    m.handle_command(Command::ToggleMonitorActivity).unwrap(); // monitor on for the active window
    // Generate output in the ACTIVE window's pane.
    let pid = m.windows()[0].layout().panes()[0];
    m.windows()[0]
        .pane(pid)
        .unwrap()
        .send_input(bytes::Bytes::from_static(b"x\n"))
        .await
        .unwrap();
    // Drain a few times; the active window never flags and never emits an edge
    // alert (`update_monitor_flags` returns false → no alert message set).
    for _ in 0..5 {
        assert!(!m.update_monitor_flags().alert_edge, "active window emits no alert message");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!m.active_window().activity_flag(), "active window is never flagged");
}

#[tokio::test]
async fn activity_does_not_re_message_while_sticky() {
    let mut m = mk_mgr();
    m.new_window_with_spec(spec(), "api".into()).unwrap(); // window 1, active
    m.handle_command(Command::ToggleMonitorActivity).unwrap();
    m.handle_command(Command::SelectWindow(0)).unwrap();
    // First edge fires.
    assert!(drive_until_alert(&mut m, 1).await.is_some(), "first edge messages");
    // Subsequent drains, even with continued output, do NOT re-message while
    // the sticky flag stays true (the edge already happened).
    let pid = m.windows()[1].layout().panes()[0];
    for _ in 0..5 {
        m.windows()[1]
            .pane(pid)
            .unwrap()
            .send_input(bytes::Bytes::from_static(b"y\n"))
            .await
            .unwrap();
        assert!(!m.update_monitor_flags().alert_edge, "no re-message while the flag stays sticky");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn activity_re_messages_after_view_clears_and_re_edges() {
    let mut m = mk_mgr();
    m.new_window_with_spec(spec(), "api".into()).unwrap(); // window 1, active
    m.handle_command(Command::ToggleMonitorActivity).unwrap();
    m.handle_command(Command::SelectWindow(0)).unwrap();
    assert!(drive_until_alert(&mut m, 1).await.is_some(), "first edge messages");
    let _ = m.take_active_message();
    // View window 1 (clears its flag), then go back to window 0.
    m.handle_command(Command::SelectWindow(1)).unwrap();
    let _ = m.update_monitor_flags().alert_edge; // clears the now-active window's flag
    assert!(!m.windows()[1].activity_flag(), "viewing cleared the flag");
    m.handle_command(Command::SelectWindow(0)).unwrap();
    // A fresh edge after clearing must message again.
    let msg = drive_until_alert(&mut m, 1).await;
    assert_eq!(
        msg.as_deref(),
        Some("activity in window 2 (api)"),
        "a fresh edge after view-clear messages again"
    );
}

// ----- command-completion monitoring -----

/// Set the background pane's completed-block counter + exit directly on the
/// screen (the OSC-133;D → counter path is covered by the emulator's own
/// tests; here we exercise the drain's reaction to a counter change without
/// timing on `cat` echo).
fn set_block_counter(m: &WindowManager, win: usize, count: u64, exit: Option<i32>) {
    let pid = m.windows()[win].layout().panes()[0];
    m.windows()[win].pane(pid).unwrap().with_screen_mut(|s| {
        s.blocks_completed = count;
        s.last_block_exit = exit;
    });
}

#[tokio::test]
async fn monitor_command_toggle_flips_and_messages() {
    let mut m = mk_mgr();
    assert!(!m.active_window().monitor_command(), "off by default");
    m.handle_command(Command::ToggleMonitorCommand).unwrap();
    assert_eq!(m.take_active_message(), Some("monitor-command on"));
    assert!(m.active_window().monitor_command());
    m.handle_command(Command::ToggleMonitorCommand).unwrap();
    assert_eq!(m.take_active_message(), Some("monitor-command off"));
    assert!(!m.active_window().monitor_command());
}

#[tokio::test]
async fn command_completion_nonzero_exit_flags_failed_and_messages() {
    let mut m = mk_mgr(); // window 0
    m.new_window_with_spec(spec(), "api".into()).unwrap(); // window 1, active
    m.handle_command(Command::ToggleMonitorCommand).unwrap(); // monitor-command on for window 1
    m.handle_command(Command::SelectWindow(0)).unwrap(); // window 0 active; window 1 background
    let _ = m.update_monitor_flags().alert_edge; // baseline window 1 at 0
    // A command completes in the background window with a nonzero exit.
    set_block_counter(&m, 1, 1, Some(1));
    assert!(m.update_monitor_flags().alert_edge, "completion edge fires a message");
    assert_eq!(m.take_active_message(), Some("done in window 2 (api): exit 1"));
    assert_eq!(m.windows()[1].done_flag(), Some(false), "✗ flag (failed)");
}

#[tokio::test]
async fn command_completion_exit_zero_flags_ok_and_messages() {
    let mut m = mk_mgr();
    m.new_window_with_spec(spec(), "api".into()).unwrap();
    m.handle_command(Command::ToggleMonitorCommand).unwrap();
    m.handle_command(Command::SelectWindow(0)).unwrap();
    let _ = m.update_monitor_flags().alert_edge;
    set_block_counter(&m, 1, 1, Some(0));
    assert!(m.update_monitor_flags().alert_edge);
    assert_eq!(m.take_active_message(), Some("done in window 2 (api): exit 0"));
    assert_eq!(m.windows()[1].done_flag(), Some(true), "✓ flag (ok)");
}

#[tokio::test]
async fn update_monitor_flags_collects_pending_notifications() {
    // Completion events are collected for EVERY window regardless of the
    // monitor-command flag (the notification policy is applied by the coordinator).
    let mut m = mk_mgr(); // window 0
    m.new_window_with_spec(spec(), "api".into()).unwrap(); // window 1, active
    m.handle_command(Command::SelectWindow(0)).unwrap(); // window 0 active; 1 background
    let _ = m.update_monitor_flags().alert_edge; // baselines
    // A block completes in the BACKGROUND window (monitor-command never toggled).
    set_block_counter(&m, 1, 1, Some(0));
    let pid = m.windows()[1].layout().panes()[0];
    m.windows()[1].pane(pid).unwrap().with_screen_mut(|s| s.last_block_duration = Some(45_000));
    let drain = m.update_monitor_flags();
    let n = drain
        .notifications
        .iter()
        .find(|n| n.window_index == 1)
        .expect("completion event for the background window, flag or not");
    assert!(!n.is_active_window);
    assert_eq!(n.event.exit, Some(0));
    assert_eq!(n.event.duration_ms, Some(45_000));
}

#[tokio::test]
async fn command_completion_codeless_d_flags_ok_with_no_exit_clause() {
    let mut m = mk_mgr();
    m.new_window_with_spec(spec(), "api".into()).unwrap();
    m.handle_command(Command::ToggleMonitorCommand).unwrap();
    m.handle_command(Command::SelectWindow(0)).unwrap();
    let _ = m.update_monitor_flags().alert_edge;
    // Codeless D: counter incremented but no exit payload.
    set_block_counter(&m, 1, 1, None);
    assert!(m.update_monitor_flags().alert_edge);
    assert_eq!(
        m.take_active_message(),
        Some("done in window 2 (api)"),
        "codeless completion → no exit clause"
    );
    assert_eq!(m.windows()[1].done_flag(), Some(true), "✓ flag (outcome unknown)");
}

#[tokio::test]
async fn command_completion_in_active_window_neither_flags_nor_messages() {
    let mut m = mk_mgr(); // window 0, active
    m.handle_command(Command::ToggleMonitorCommand).unwrap(); // monitor-command on for the active window
    let _ = m.update_monitor_flags().alert_edge; // baseline at 0
    set_block_counter(&m, 0, 1, Some(1)); // completion in the ACTIVE window
    assert!(!m.update_monitor_flags().alert_edge, "active window emits no completion message");
    assert_eq!(m.active_window().done_flag(), None, "active window never flagged");
}

#[tokio::test]
async fn command_completion_toggle_on_after_history_does_not_backlog() {
    let mut m = mk_mgr();
    m.new_window_with_spec(spec(), "api".into()).unwrap(); // window 1, active
    m.handle_command(Command::SelectWindow(0)).unwrap();
    // Completed history accumulates BEFORE monitor-command is on; the baseline
    // still advances every drain.
    set_block_counter(&m, 1, 3, Some(0));
    let _ = m.update_monitor_flags().alert_edge;
    let _ = m.update_monitor_flags().alert_edge;
    // Now turn monitoring on for window 1 (via its own active-window toggle).
    m.handle_command(Command::SelectWindow(1)).unwrap();
    m.handle_command(Command::ToggleMonitorCommand).unwrap();
    let _ = m.take_active_message();
    m.handle_command(Command::SelectWindow(0)).unwrap();
    // No NEW completion since toggle-on → no alert (no history replay).
    assert!(!m.update_monitor_flags().alert_edge, "toggle-on does not backlog completed history");
    assert_eq!(m.windows()[1].done_flag(), None);
}

#[tokio::test]
async fn command_completion_counter_decrease_re_baselines_silently() {
    let mut m = mk_mgr();
    m.new_window_with_spec(spec(), "api".into()).unwrap(); // window 1, active
    m.handle_command(Command::ToggleMonitorCommand).unwrap();
    m.handle_command(Command::SelectWindow(0)).unwrap();
    set_block_counter(&m, 1, 5, Some(0));
    let _ = m.update_monitor_flags().alert_edge; // baseline at 5
    let _ = m.take_active_message();
    // A RIS resets the screen → counter drops below the baseline.
    set_block_counter(&m, 1, 0, None);
    assert!(
        !m.update_monitor_flags().alert_edge,
        "a counter decrease (RIS) re-baselines silently, never alerts"
    );
    // View window 1 to clear its sticky done flag, then return to window 0. A
    // fresh completion after the reset fires from the new (0) baseline.
    m.handle_command(Command::SelectWindow(1)).unwrap();
    let _ = m.update_monitor_flags().alert_edge; // clears the now-active window's done flag
    assert_eq!(m.windows()[1].done_flag(), None, "viewing cleared the done flag");
    m.handle_command(Command::SelectWindow(0)).unwrap();
    let _ = m.update_monitor_flags().alert_edge; // re-baseline window 1 at 0 after the view
    set_block_counter(&m, 1, 1, Some(2));
    assert!(m.update_monitor_flags().alert_edge, "post-reset completion alerts from the new baseline");
    assert_eq!(m.take_active_message(), Some("done in window 2 (api): exit 2"));
}

// ----- silence monitoring -----

#[tokio::test]
async fn monitor_silence_set_and_clear_messages() {
    let mut m = mk_mgr();
    m.handle_command(Command::SetMonitorSilence(Some(30))).unwrap();
    assert_eq!(m.take_active_message(), Some("monitor-silence 30s"));
    assert_eq!(m.active_window().monitor_silence(), Some(Duration::from_secs(30)));
    m.handle_command(Command::SetMonitorSilence(None)).unwrap();
    assert_eq!(m.take_active_message(), Some("monitor-silence off"));
    assert_eq!(m.active_window().monitor_silence(), None);
}

#[tokio::test]
async fn any_silence_monitored_reflects_arm_state() {
    let mut m = mk_mgr();
    assert!(!m.any_silence_monitored(), "no monitors by default");
    m.handle_command(Command::SetMonitorSilence(Some(5))).unwrap();
    assert!(m.any_silence_monitored(), "armed after set");
    m.handle_command(Command::SetMonitorSilence(None)).unwrap();
    assert!(!m.any_silence_monitored(), "disarmed after clear");
}

#[tokio::test(start_paused = true)]
async fn silence_edge_in_background_window_flags_and_messages() {
    let mut m = mk_mgr(); // window 0
    m.new_window_with_spec(spec(), "build".into()).unwrap(); // window 1, active
    // Inject a small threshold directly (sub-second control without the
    // 1s-minimum parse path), then switch away so window 1 is background.
    m.windows_mut()[1].set_monitor_silence(Some(Duration::from_millis(80)));
    m.handle_command(Command::SelectWindow(0)).unwrap();
    // Before the threshold elapses: no edge. start_paused = true means no real
    // time has passed yet (tokio::time::Instant::now() has not advanced).
    assert!(!m.check_silence_alerts(), "no silence before the threshold");
    tokio::time::advance(Duration::from_millis(120)).await;
    assert!(m.check_silence_alerts(), "silence edge fires past the threshold");
    assert_eq!(m.take_active_message(), Some("silence in window 2 (build)"));
    assert!(m.windows()[1].silence_flag(), "the ~ flag is set");
    // The episode latch prevents re-firing on the next tick while still silent.
    assert!(!m.check_silence_alerts(), "no re-fire within the same silence episode");
}

#[tokio::test(start_paused = true)]
async fn silence_active_window_never_fires() {
    let mut m = mk_mgr(); // window 0, active
    m.active_window_mut().set_monitor_silence(Some(Duration::from_millis(50)));
    tokio::time::advance(Duration::from_millis(90)).await;
    // The active window is excluded (else an idle active window flickers 1 Hz).
    assert!(!m.check_silence_alerts(), "active window never silence-fires");
    assert!(!m.active_window().silence_flag());
}

#[tokio::test(start_paused = true)]
async fn silence_output_resets_timer_and_latch() {
    let mut m = mk_mgr();
    m.new_window_with_spec(spec(), "build".into()).unwrap(); // window 1, active
    m.windows_mut()[1].set_monitor_silence(Some(Duration::from_millis(80)));
    m.handle_command(Command::SelectWindow(0)).unwrap();
    tokio::time::advance(Duration::from_millis(120)).await;
    assert!(m.check_silence_alerts(), "first silence episode fires");
    let _ = m.take_active_message();
    // Output resumes: refresh `last_output` + reset the latch.
    m.windows_mut()[1].note_drain_output(true);
    // Right after output, no silence (timer reset) and no re-fire.
    assert!(!m.check_silence_alerts(), "output reset the timer");
    // A NEW silence episode after output resumes can fire again.
    tokio::time::advance(Duration::from_millis(120)).await;
    assert!(m.check_silence_alerts(), "a new silence episode after output re-fires");
    assert_eq!(m.take_active_message(), Some("silence in window 2 (build)"));
}

#[tokio::test(start_paused = true)]
async fn silence_viewing_clears_flag_but_not_latch() {
    let mut m = mk_mgr();
    m.new_window_with_spec(spec(), "build".into()).unwrap(); // window 1, active
    m.windows_mut()[1].set_monitor_silence(Some(Duration::from_millis(80)));
    m.handle_command(Command::SelectWindow(0)).unwrap();
    tokio::time::advance(Duration::from_millis(120)).await;
    assert!(m.check_silence_alerts(), "silence fires");
    let _ = m.take_active_message();
    assert!(m.windows()[1].silence_flag());
    // View window 1: `clear_alerts` clears the ~ FLAG but not the episode latch.
    m.handle_command(Command::SelectWindow(1)).unwrap();
    let _ = m.update_monitor_flags().alert_edge; // clears the now-active window's flag
    assert!(!m.windows()[1].silence_flag(), "viewing cleared the ~ flag");
    // Go back to window 0; the window is STILL silent (no output happened), so
    // the latch must prevent a re-fire (no 1 Hz flag loop).
    m.handle_command(Command::SelectWindow(0)).unwrap();
    assert!(!m.check_silence_alerts(), "no re-fire while still silent (latch held)");
    assert!(!m.windows()[1].silence_flag(), "flag stays clear without a re-fire");
}

#[tokio::test(start_paused = true)]
async fn silence_disable_clears_flag_and_threshold() {
    let mut m = mk_mgr();
    m.new_window_with_spec(spec(), "build".into()).unwrap();
    m.windows_mut()[1].set_monitor_silence(Some(Duration::from_millis(60)));
    m.handle_command(Command::SelectWindow(0)).unwrap();
    tokio::time::advance(Duration::from_millis(100)).await;
    assert!(m.check_silence_alerts());
    let _ = m.take_active_message();
    assert!(m.windows()[1].silence_flag());
    // Disabling (0/None) clears the threshold, the flag, and the latch.
    m.windows_mut()[1].set_monitor_silence(None);
    assert_eq!(m.windows()[1].monitor_silence(), None);
    assert!(!m.windows()[1].silence_flag(), "disable cleared the ~ flag");
    assert!(!m.check_silence_alerts(), "disabled window never fires");
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

// ----- J4: block-aware mouse (prompt-row click-to-jump) -----
//
// Viewport geometry (24×80 WM, status bar at bottom):
//   host_viewport → Rect { row: 1, col: 1, rows: 21, cols: 78 }
//   single pane gets the full viewport rect.
//   pane_at_coord requires physical row ∈ [1, 22) and col ∈ [1, 79).
//   local_row = physical_row - pane_rect.row = physical_row - 1.
//   local_col = physical_col - pane_rect.col = physical_col - 1.
//   So physical row = pane_local_row + 1; use col >= 2 for safety.

/// Plain (unmodified) left press. `row` and `col` are PHYSICAL coords.
fn plain_left_press(row: u16, col: u16) -> MouseEvent {
    MouseEvent {
        kind: MouseKind::Press,
        button: MouseButton::Left,
        modifiers: plexy_glass_mux::MouseModifiers::default(),
        row,
        col,
    }
}

/// Shift+left press. `row` and `col` are PHYSICAL coords.
fn shift_left_press(row: u16, col: u16) -> MouseEvent {
    MouseEvent {
        kind: MouseKind::Press,
        button: MouseButton::Left,
        modifiers: plexy_glass_mux::MouseModifiers { shift: true, alt: false, ctrl: false },
        row,
        col,
    }
}

// J4-T1: scrolled pane + left press on a viewport row showing a PROMPT_START
// line → scroll_offset becomes scrollback_len - that_line (row at top).
//
// sb=10, offset=8 → top=2. pane-local row 1 → abs line 3 (the prompt).
// Expected: offset = 10 - 3 = 7 (prompt at viewport top).
#[tokio::test]
async fn prompt_click_while_scrolled_jumps_to_viewport_top() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    // 10 scrollback rows; prompt at absolute line 3.
    inject_scrollback_prompts(&m, 10, &[3]);
    // Scroll so top visible absolute line = 2 (offset = 10 - 2 = 8).
    // Pane-local row 1 → abs line 3 (the prompt).
    let pane = m.active_window().active_pane().unwrap().clone();
    pane.set_scroll_offset(8, 10);
    assert_eq!(pane.scroll_offset(), 8);

    // Physical row 2 → pane-local row 1 → abs line 3 (the prompt).
    m.handle_mouse(plain_left_press(2, 5)).await.unwrap();

    // Expected new offset: sb - abs_line = 10 - 3 = 7.
    assert_eq!(pane.scroll_offset(), 7, "prompt row should be at viewport top (offset 7)");
}

// J4-T2: same setup, press on a non-prompt row → behavior unchanged (selection
// starts; offset does not change).
//
// sb=10, offset=8 → top=2. pane-local row 0 → abs line 2 (NOT a prompt).
#[tokio::test]
async fn non_prompt_click_while_scrolled_leaves_offset_unchanged() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    // 10 scrollback rows; prompt at absolute line 3 only.
    inject_scrollback_prompts(&m, 10, &[3]);
    let pane = m.active_window().active_pane().unwrap().clone();
    pane.set_scroll_offset(8, 10);

    // Physical row 1 → pane-local row 0 → abs line 2 (NOT a prompt).
    m.handle_mouse(plain_left_press(1, 5)).await.unwrap();

    // Offset must be unchanged; a selection started instead.
    assert_eq!(pane.scroll_offset(), 8, "offset unchanged on non-prompt click");
    assert!(m.selection().is_some(), "selection started on non-prompt click");
}

// J4-T3: offset 0 (live view) + prompt row under cursor → untouched (no
// scroll change; existing live behavior, the scroll_offset > 0 guard fires).
#[tokio::test]
async fn prompt_click_at_live_view_does_not_change_offset() {
    use plexy_glass_emulator::RowMark;
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    // Set PROMPT_START on active grid row 0 (= abs line scrollback_len + 0).
    m.active_window()
        .active_pane()
        .unwrap()
        .with_screen_mut(|s| s.active.rows[0].mark.set(RowMark::PROMPT_START));
    // offset is already 0 (live view).
    assert_eq!(active_scroll_offset(&m), 0);

    // Physical row 1 → pane-local row 0 → abs line 0 (grid prompt).
    // `scroll_offset` == 0 so the prompt-jump guard is not entered.
    m.handle_mouse(plain_left_press(1, 5)).await.unwrap();

    // No scroll change (the `scroll_offset > 0` guard was false).
    assert_eq!(active_scroll_offset(&m), 0, "live view: offset stays 0");
}

// J4-T4: shift+left-press on a scrolled prompt row with an active selection →
// selection extends (shift+click precedence wins), offset unchanged.
#[tokio::test]
async fn shift_click_on_scrolled_prompt_row_extends_selection_not_jumps() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    inject_scrollback_prompts(&m, 10, &[3]);
    let pane = m.active_window().active_pane().unwrap().clone();
    pane.set_scroll_offset(8, 10);

    // Seed a selection with a plain click (physical row 1 → local row 0 →
    // abs line 2, non-prompt) so the selection starts.
    m.handle_mouse(plain_left_press(1, 2)).await.unwrap();
    assert!(m.selection().is_some(), "premise: selection seeded");
    // Reset the offset so we're still scrolled back for the next step.
    pane.set_scroll_offset(8, 10);

    // Shift+click on physical row 2 → local row 1 → abs line 3 (the prompt).
    m.handle_mouse(shift_left_press(2, 5)).await.unwrap();

    // Shift+click fires the extend branch BEFORE the prompt-jump rung.
    assert_eq!(pane.scroll_offset(), 8, "shift+click: offset unchanged");
    assert!(m.selection().is_some(), "shift+click: selection still active");
}

// J4-T5: prompt line in the GRID portion of a scrolled view → offset saturates
// to 0 (snaps to live, accepted).
//
// sb=5, offset=2 → top=3.
// Grid row 0 is abs line 5.  Pane-local row 2 → abs line 5.
// New offset = sb.saturating_sub(5) = 5 - 5 = 0 → snaps live.
#[tokio::test]
async fn prompt_click_on_grid_portion_snaps_to_live() {
    use plexy_glass_emulator::RowMark;
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    // 5 scrollback rows; a prompt is on active grid row 0 (abs line 5).
    inject_scrollback_prompts(&m, 5, &[]);
    m.active_window()
        .active_pane()
        .unwrap()
        .with_screen_mut(|s| s.active.rows[0].mark.set(RowMark::PROMPT_START));
    let pane = m.active_window().active_pane().unwrap().clone();
    // Scroll so top = abs line 3 (offset = 5 - 3 = 2).
    // Pane-local row 2 → abs line 5 (grid row 0, the prompt).
    pane.set_scroll_offset(2, 5);

    // Physical row 3 → pane-local row 2 → abs line 5.
    m.handle_mouse(plain_left_press(3, 5)).await.unwrap();

    // sb.saturating_sub(5) = 0 → snaps to live.
    assert_eq!(pane.scroll_offset(), 0, "grid-portion prompt click saturates to live");
}

// J4-T6: pane with app mouse mode on → passthrough unaffected (event forwarded
// to child, no scroll change). Rule 5 fires before J4's rung.
#[tokio::test]
async fn app_mouse_mode_passthrough_unaffected_by_prompt_jump() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    inject_scrollback_prompts(&m, 10, &[3]);
    let pane = m.active_window().active_pane().unwrap().clone();
    pane.set_scroll_offset(8, 10);

    // Turn on app mouse mode so Rule 5 (passthrough) fires.
    pane.with_screen_mut(|s| s.modes.insert(plexy_glass_emulator::Modes::MOUSE_BTN));
    assert!(m.pane_has_any_mouse_mode(m.active_window().active()));

    // Physical row 2 → pane-local row 1 → abs line 3 (the prompt row).
    m.handle_mouse(plain_left_press(2, 5)).await.unwrap();

    // Rule 5 forwarded the event, so J4's rung never ran and the offset is unchanged.
    assert_eq!(pane.scroll_offset(), 8, "app mouse mode: passthrough, no offset change");
}

// J4-T7: double-click on the now-relocated content is just another click
// (no panic, sane state). First click jumps; second press at the same physical
// position now sees a different abs line → falls through to selection logic.
#[tokio::test]
async fn double_click_after_prompt_jump_does_not_panic() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    inject_scrollback_prompts(&m, 10, &[3]);
    let pane = m.active_window().active_pane().unwrap().clone();
    pane.set_scroll_offset(8, 10);

    // First click: physical row 2 → local row 1 → abs line 3 (prompt) →
    // jumps to offset 7 (prompt at top).
    m.handle_mouse(plain_left_press(2, 5)).await.unwrap();
    assert_eq!(pane.scroll_offset(), 7, "first click: offset updated to 7");

    // Second click at the same physical position: offset is now 7, top = 3,
    // local row 1 → abs line 4 (NOT a prompt) → falls through to selection.
    // Must not panic.
    m.handle_mouse(plain_left_press(2, 5)).await.unwrap();
    // State is sane (no panic). Offset unchanged from 7; selection started.
    assert_eq!(pane.scroll_offset(), 7, "second click: offset unchanged");
    assert!(m.selection().is_some(), "second click: selection started");
}

#[tokio::test]
async fn hint_overlay_pick_returns_copy() {
    let notify = Arc::new(Notify::new());
    let mut wm = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    let targets = vec![
        HintTarget { start: (0, 0), text: "alpha".into(), kind: HintKind::Sha },
    ];
    // Single target + single-char alphabet → label "a".
    let state = HintState::new(targets, "asdf");
    wm.open_hints(state);
    assert!(wm.overlay().is_some(), "overlay is open after open_hints");

    // Press 'a' (lowercase) → matches label "a" → Copy action.
    let res = wm.handle_overlay_key(&KeyEvent::plain(plexy_glass_mux::Key::Char('a')));
    match res {
        OverlayKeyResult::Hint(pick) => {
            assert_eq!(pick.text, "alpha");
            assert_eq!(pick.action, HintAction::Copy);
        }
        other => panic!("expected Hint(Copy), got {other:?}"),
    }
    assert!(wm.overlay().is_none(), "overlay closes on pick");
}

#[tokio::test]
async fn move_window_forward_keeps_active_following_window() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh");
    m.handle_command(Command::NewWindow).unwrap(); // W1
    m.handle_command(Command::NewWindow).unwrap(); // W2, active = 2
    let ids: Vec<_> = m.windows().iter().map(|w| w.id).collect(); // [w0, w1, w2]
    assert_eq!(m.active_idx(), 2);

    // Move w0 (front) to slot 2.
    assert!(m.move_window(0, 2));
    let after: Vec<_> = m.windows().iter().map(|w| w.id).collect();
    assert_eq!(after, vec![ids[1], ids[2], ids[0]], "remove(0)+insert(2)");
    // active was w2 → now at index 1; it must still be active.
    assert_eq!(m.windows()[m.active_idx()].id, ids[2]);
}

#[tokio::test]
async fn move_window_backward_and_to_end() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh");
    m.handle_command(Command::NewWindow).unwrap();
    m.handle_command(Command::NewWindow).unwrap();
    let ids: Vec<_> = m.windows().iter().map(|w| w.id).collect();

    // backward: w2 → slot 0
    assert!(m.move_window(2, 0));
    assert_eq!(
        m.windows().iter().map(|w| w.id).collect::<Vec<_>>(),
        vec![ids[2], ids[0], ids[1]]
    );

    // "past the end" clamps to the last slot: w2 (now at 0) → end
    assert!(m.move_window(0, 99));
    assert_eq!(
        m.windows().iter().map(|w| w.id).collect::<Vec<_>>(),
        vec![ids[0], ids[1], ids[2]]
    );
}

#[tokio::test]
async fn move_window_keeps_unrelated_active_window() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh");
    m.handle_command(Command::NewWindow).unwrap();
    m.handle_command(Command::NewWindow).unwrap();
    m.handle_command(Command::SelectWindow(1)).unwrap(); // active = W1
    let ids: Vec<_> = m.windows().iter().map(|w| w.id).collect();

    // Move W0 to the end; W1 (active) shifts but stays active.
    assert!(m.move_window(0, 2));
    assert_eq!(m.windows()[m.active_idx()].id, ids[1]);
}

#[tokio::test]
async fn move_window_noops() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh");
    // single window
    assert!(!m.move_window(0, 0));
    m.handle_command(Command::NewWindow).unwrap();
    // same slot
    assert!(!m.move_window(1, 1));
    // out of range `from`
    assert!(!m.move_window(5, 0));
}

#[tokio::test]
async fn move_window_by_id_resolves_source() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh");
    m.handle_command(Command::NewWindow).unwrap();
    m.handle_command(Command::NewWindow).unwrap();
    let ids: Vec<_> = m.windows().iter().map(|w| w.id).collect();
    assert!(m.move_window_by_id(ids[0], 2));
    assert_eq!(
        m.windows().iter().map(|w| w.id).collect::<Vec<_>>(),
        vec![ids[1], ids[2], ids[0]]
    );
    // unknown id is a no-op
    assert!(!m.move_window_by_id(plexy_glass_mux::WindowId(9999), 0));
}

// ---- Tab drag reorder (Task 4) ----

async fn three_tab_manager() -> WindowManager {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh");
    m.handle_command(Command::NewWindow).unwrap();
    m.handle_command(Command::NewWindow).unwrap(); // 3 windows, active = 2
    m.set_status_layout(Some(23), 0);
    m.set_status_hits(vec![
        plexy_glass_status::StatusHit {
            col_range: 0..5,
            action: plexy_glass_status::ClickAction::SelectWindow(0),
        },
        plexy_glass_status::StatusHit {
            col_range: 5..10,
            action: plexy_glass_status::ClickAction::SelectWindow(1),
        },
        plexy_glass_status::StatusHit {
            col_range: 10..15,
            action: plexy_glass_status::ClickAction::SelectWindow(2),
        },
    ]);
    m
}

fn mev(kind: MouseKind, row: u16, col: u16, alt: bool) -> MouseEvent {
    MouseEvent {
        kind,
        button: MouseButton::Left,
        modifiers: plexy_glass_mux::MouseModifiers { shift: false, alt, ctrl: false },
        row,
        col,
    }
}

#[tokio::test]
async fn alt_drag_reorders_window_to_drop_slot() {
    let mut m = three_tab_manager().await;
    let ids: Vec<_> = m.windows().iter().map(|w| w.id).collect();

    // Alt-press tab 0 → drag begins; highlight points at index 0.
    m.handle_mouse(mev(MouseKind::Press, 23, 2, true)).await.unwrap();
    assert_eq!(m.dragging_window_idx(), Some(0), "drag started on tab 0");

    // Release over tab 2 → move W0 to slot 2.
    m.handle_mouse(mev(MouseKind::Release, 23, 12, false)).await.unwrap();
    assert_eq!(m.dragging_window_idx(), None, "drag cleared on release");
    assert_eq!(
        m.windows().iter().map(|w| w.id).collect::<Vec<_>>(),
        vec![ids[1], ids[2], ids[0]]
    );
}

#[tokio::test]
async fn plain_press_still_selects_no_drag() {
    let mut m = three_tab_manager().await;
    m.handle_mouse(mev(MouseKind::Press, 23, 2, false)).await.unwrap();
    assert_eq!(m.dragging_window_idx(), None, "no drag without the modifier");
    assert_eq!(m.active_idx(), 0, "plain click selected tab 0");
}

#[tokio::test]
async fn alt_drag_release_on_same_tab_is_noop() {
    let mut m = three_tab_manager().await;
    let ids: Vec<_> = m.windows().iter().map(|w| w.id).collect();
    m.handle_mouse(mev(MouseKind::Press, 23, 2, true)).await.unwrap();
    m.handle_mouse(mev(MouseKind::Release, 23, 3, false)).await.unwrap(); // same tab 0
    assert_eq!(m.windows().iter().map(|w| w.id).collect::<Vec<_>>(), ids);
    assert_eq!(m.dragging_window_idx(), None);
}

#[tokio::test]
async fn alt_drag_release_off_status_row_aborts() {
    let mut m = three_tab_manager().await;
    let ids: Vec<_> = m.windows().iter().map(|w| w.id).collect();
    m.handle_mouse(mev(MouseKind::Press, 23, 2, true)).await.unwrap();
    // Move stays in drag; release on a pane row (5) aborts with no reorder.
    m.handle_mouse(mev(MouseKind::Move, 10, 8, false)).await.unwrap();
    assert_eq!(m.dragging_window_idx(), Some(0), "still dragging after a move");
    m.handle_mouse(mev(MouseKind::Release, 5, 8, false)).await.unwrap();
    assert_eq!(m.windows().iter().map(|w| w.id).collect::<Vec<_>>(), ids);
    assert_eq!(m.dragging_window_idx(), None);
}

#[tokio::test]
async fn alt_drag_release_right_of_tabs_moves_to_end() {
    let mut m = three_tab_manager().await;
    let ids: Vec<_> = m.windows().iter().map(|w| w.id).collect();
    m.handle_mouse(mev(MouseKind::Press, 23, 7, true)).await.unwrap(); // grab tab 1
    m.handle_mouse(mev(MouseKind::Release, 23, 40, false)).await.unwrap(); // past all tabs
    assert_eq!(
        m.windows().iter().map(|w| w.id).collect::<Vec<_>>(),
        vec![ids[0], ids[2], ids[1]]
    );
}

// I1: a popup opened mid alt-drag must clear the in-flight tab_drag so the
// Release that Rule 0 swallows cannot perform a phantom reorder after close.
#[tokio::test]
async fn open_popup_clears_in_flight_tab_drag() {
    let mut m = three_tab_manager().await;
    // Alt-press tab 0 → drag begins.
    m.handle_mouse(mev(MouseKind::Press, 23, 2, true)).await.unwrap();
    assert!(m.dragging_window_idx().is_some(), "premise: tab drag started");
    // A popup opens mid-drag (e.g. via a keybinding from another client).
    m.handle_command(Command::OpenPopup { command: None }).unwrap();
    assert!(m.dragging_window_idx().is_none(), "popup open must clear the frozen tab drag");
}

// I2: a popup opened mid alt-drag must clear the in-flight pane_drag so the
// Release that Rule 0 swallows cannot perform a phantom pane swap after close.
#[tokio::test]
async fn open_popup_clears_in_flight_pane_drag() {
    let mut m = make_two_pane_manager().await; // `PaneId(0)`, `PaneId(1)`; active = 1
    let vp = m.viewport();
    let r0 = m.active_window().layout().rect_of(PaneId(0), vp).unwrap();
    let (cr, cc) = (r0.row + r0.rows / 2, r0.col + r0.cols / 2);
    // Alt-press inside pane 0 → pane drag begins.
    m.handle_mouse(mev(MouseKind::Press, cr, cc, true)).await.unwrap();
    assert!(m.pane_drag_roles().is_some(), "premise: pane drag started");
    // A popup opens mid-drag (e.g. via a keybinding from another client).
    m.handle_command(Command::OpenPopup { command: None }).unwrap();
    assert!(m.pane_drag_roles().is_none(), "popup open must clear the frozen pane drag");
}

// M1: `last_active_window` re-follows its window by id after `move_window`.
#[tokio::test]
async fn move_window_keeps_last_active_valid_by_id() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh");
    m.handle_command(Command::NewWindow).unwrap(); // W1, active=1, last=Some(0)
    m.handle_command(Command::NewWindow).unwrap(); // W2, active=2, last=Some(1)
    // SelectWindow(0) sets last = the previously active window (W2 at index 2).
    m.handle_command(Command::SelectWindow(0)).unwrap(); // active=0, last=Some(2)=W2
    let pre_last_id = m.windows()[2].id; // capture W2's id before the reorder
    // Move W0 (active) to slot 2 → order becomes [W1, W2, W0].
    // W2 shifts to index 1; `last_active_window` must follow it there.
    assert!(m.move_window(0, 2));
    // Trigger `SelectLastWindow` to exercise the last-active follow path.
    m.handle_command(Command::SelectLastWindow).unwrap();
    assert_eq!(
        m.windows()[m.active_idx()].id,
        pre_last_id,
        "last_active re-follows W2 to its new index after move_window"
    );
}

// ── Pane-swap drag (Task 2) ─────────────────────────────────────────────────

#[tokio::test]
async fn alt_drag_swaps_panes_and_focuses_source() {
    let mut m = make_two_pane_manager().await; // `PaneId(0)`, `PaneId(1)`; active = 1
    let vp = m.viewport();
    let r0 = m.active_window().layout().rect_of(PaneId(0), vp).unwrap();
    let r1 = m.active_window().layout().rect_of(PaneId(1), vp).unwrap();
    let (c0r, c0c) = (r0.row + r0.rows / 2, r0.col + r0.cols / 2);
    let (c1r, c1c) = (r1.row + r1.rows / 2, r1.col + r1.cols / 2);

    // Alt-press in pane 0 → drag begins, source = pane 0.
    m.handle_mouse(mev(MouseKind::Press, c0r, c0c, true)).await.unwrap();
    assert_eq!(m.pane_drag_roles(), Some((PaneId(0), None)));

    // Move into pane 1 → target updates.
    m.handle_mouse(mev(MouseKind::Move, c1r, c1c, false)).await.unwrap();
    assert_eq!(m.pane_drag_roles(), Some((PaneId(0), Some(PaneId(1)))));

    // Release in pane 1 → swap + focus source + clear.
    m.handle_mouse(mev(MouseKind::Release, c1r, c1c, false)).await.unwrap();
    assert_eq!(m.pane_drag_roles(), None, "drag cleared on release");
    // Slots swapped occupants: pane 0's old position now holds pane 1, and vice versa.
    assert_eq!(m.active_window().layout().pane_at_coord(vp, c0r, c0c), Some(PaneId(1)));
    assert_eq!(m.active_window().layout().pane_at_coord(vp, c1r, c1c), Some(PaneId(0)));
    assert_eq!(m.active_window().active(), PaneId(0), "focus follows dragged pane");
}

#[tokio::test]
async fn plain_press_in_pane_does_not_start_drag() {
    let mut m = make_two_pane_manager().await; // active = 1
    let vp = m.viewport();
    let r0 = m.active_window().layout().rect_of(PaneId(0), vp).unwrap();
    m.handle_mouse(mev(MouseKind::Press, r0.row + 1, r0.col + 1, false)).await.unwrap();
    assert_eq!(m.pane_drag_roles(), None, "no drag without the modifier");
    assert_eq!(m.active_window().active(), PaneId(0), "plain click focused pane 0");
}

#[tokio::test]
async fn alt_drag_release_on_same_pane_is_noop() {
    let mut m = make_two_pane_manager().await;
    let vp = m.viewport();
    let r1 = m.active_window().layout().rect_of(PaneId(1), vp).unwrap();
    let (cr, cc) = (r1.row + r1.rows / 2, r1.col + r1.cols / 2);
    let before = m.active_window().layout().dfs_leaves();
    m.handle_mouse(mev(MouseKind::Press, cr, cc, true)).await.unwrap();
    m.handle_mouse(mev(MouseKind::Release, cr, cc, false)).await.unwrap();
    assert_eq!(m.active_window().layout().dfs_leaves(), before, "no swap on same pane");
    assert_eq!(m.pane_drag_roles(), None);
}

#[tokio::test]
async fn alt_drag_release_off_content_aborts() {
    let mut m = make_two_pane_manager().await;
    let vp = m.viewport();
    let r0 = m.active_window().layout().rect_of(PaneId(0), vp).unwrap();
    let before = m.active_window().layout().dfs_leaves();
    m.handle_mouse(mev(MouseKind::Press, r0.row + 1, r0.col + 1, true)).await.unwrap();
    // Release far off the grid → pane_at_coord None → abort.
    m.handle_mouse(mev(MouseKind::Release, 250, 250, false)).await.unwrap();
    assert_eq!(m.active_window().layout().dfs_leaves(), before, "no swap off content");
    assert_eq!(m.pane_drag_roles(), None);
}

#[tokio::test]
async fn alt_drag_preempts_child_mouse_mode() {
    let mut m = make_two_pane_manager().await; // active = PaneId(1)
    // Turn on mouse reporting in the active pane so a plain press would forward
    // to the child; the Alt-press must still start the swap drag.
    m.active_window()
        .pane(PaneId(1))
        .unwrap()
        .with_screen_mut(|s| s.modes.insert(plexy_glass_emulator::Modes::MOUSE_BTN));
    assert!(m.pane_has_any_mouse_mode(PaneId(1)));
    let vp = m.viewport();
    let r1 = m.active_window().layout().rect_of(PaneId(1), vp).unwrap();
    m.handle_mouse(mev(MouseKind::Press, r1.row + 1, r1.col + 1, true)).await.unwrap();
    assert_eq!(m.pane_drag_roles().map(|(s, _)| s), Some(PaneId(1)), "alt-press pre-empts child");
}

// M2: `move_window` clamp-collapses-to-noop (to=99 clamps to 2 == from) returns
// false.
#[tokio::test]
async fn move_window_clamp_to_noop() {
    let notify = Arc::new(Notify::new());
    let mut m = WindowManager::new(
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        notify,
        None,
        cfg(),
    )
    .unwrap();
    m.set_default_program("/bin/sh");
    m.handle_command(Command::NewWindow).unwrap();
    m.handle_command(Command::NewWindow).unwrap(); // 3 windows: indices 0, 1, 2
    let ids: Vec<_> = m.windows().iter().map(|w| w.id).collect();
    // from=2, to=99 clamps to 2; from==to → no-op.
    assert!(!m.move_window(2, 99));
    assert_eq!(
        m.windows().iter().map(|w| w.id).collect::<Vec<_>>(),
        ids,
        "order unchanged on clamp-to-noop"
    );
}

#[test]
fn severity_maps_to_palette_key_and_tier_glyph() {
    use plexy_glass_config::GlyphTier;
    assert_eq!(Severity::Info.palette_key(), "info");
    assert_eq!(Severity::Success.palette_key(), "ok");
    assert_eq!(Severity::Warn.palette_key(), "warn");
    assert_eq!(Severity::Error.palette_key(), "alert");

    assert_eq!(Severity::Success.glyph(GlyphTier::Unicode), "✓");
    assert_eq!(Severity::Error.glyph(GlyphTier::Nerd), "✗");
    // ascii tier degrades to plain letters (no tofu on a basic font).
    assert_eq!(Severity::Success.glyph(GlyphTier::Ascii), "+");
    assert_eq!(Severity::Error.glyph(GlyphTier::Ascii), "x");
    assert_eq!(Severity::Info.glyph(GlyphTier::Ascii), "i");
    assert_eq!(Severity::Warn.glyph(GlyphTier::Ascii), "!");
}
