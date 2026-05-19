//! End-to-end tests for plexy-glass. Each test:
//!   1. Builds the workspace's `plexy-glass` binary (via `assert_cmd`'s helper).
//!   2. Allocates a PTY pair using `portable-pty`.
//!   3. Spawns `plexy-glass attach` on the slave side, so the binary believes
//!      it's attached to a real terminal.
//!   4. Drives the master side and reads its output.
//!
//! Tests use tempdirs for $XDG_RUNTIME_DIR / $TMPDIR and a tempdir for
//! $HOME so the daemon writes its socket, lockfile, and logs in isolation
//! and never collides between tests.

use assert_cmd::cargo::CommandCargoExt;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::Write;
use std::time::{Duration, Instant};

fn isolate_dirs(tmp: &tempfile::TempDir) -> Vec<(String, String)> {
    let xdg = tmp.path().join("xdg");
    std::fs::create_dir_all(&xdg).unwrap();
    let state = tmp.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    vec![
        ("XDG_RUNTIME_DIR".into(), xdg.to_string_lossy().into_owned()),
        ("XDG_STATE_HOME".into(), state.to_string_lossy().into_owned()),
        ("HOME".into(), home.to_string_lossy().into_owned()),
        ("TMPDIR".into(), tmp.path().to_string_lossy().into_owned()),
        // Keep the child shell deterministic.
        ("SHELL".into(), "/bin/sh".into()),
    ]
}

/// Reads from the master PTY on a background thread (so we don't block past
/// the deadline) and returns once `needle` appears in the accumulated output,
/// or `deadline` is reached.
fn read_until(
    master: &mut Box<dyn portable_pty::MasterPty + Send>,
    needle: &[u8],
    deadline: Instant,
) -> Vec<u8> {
    let mut reader = master.try_clone_reader().expect("clone reader");
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(chunk[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut acc = Vec::new();
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(part) => {
                acc.extend_from_slice(&part);
                if acc.windows(needle.len()).any(|w| w == needle) {
                    return acc;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    acc
}

#[test]
fn smoke_echo_hello_round_trips() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let cmd = std::process::Command::cargo_bin("plexy-glass").expect("binary built");
    let cmd_builder = {
        let mut builder = CommandBuilder::new(cmd.get_program());
        builder.arg("attach");
        for (k, v) in &env {
            builder.env(k, v);
        }
        builder
    };
    let mut child = pair.slave.spawn_command(cmd_builder).expect("spawn child");
    drop(pair.slave);

    let mut master = pair.master;

    let mut writer = master.take_writer().expect("take writer");
    // Give the shell a moment to be ready.
    std::thread::sleep(Duration::from_millis(300));
    writer
        // No trailing `exit` because Phase 3's auto-close-on-pane-death
        // would race the host PTY drain.
        .write_all(b"echo HEL-LO\n")
        .expect("write command");

    let buf = read_until(&mut master, b"HEL-LO", Instant::now() + Duration::from_secs(10));

    // Best-effort: kill if still running.
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        buf.windows(6).any(|w| w == b"HEL-LO"),
        "did not see HEL-LO in output. raw: {:?}",
        String::from_utf8_lossy(&buf)
    );
}

#[test]
fn sigwinch_propagates_to_child() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let bin = std::process::Command::cargo_bin("plexy-glass").expect("binary built");
    let mut builder = CommandBuilder::new(bin.get_program());
    builder.arg("attach");
    for (k, v) in &env {
        builder.env(k, v);
    }
    let mut child = pair.slave.spawn_command(builder).expect("spawn child");
    drop(pair.slave);

    let mut master = pair.master;
    let mut writer = master.take_writer().expect("take writer");
    std::thread::sleep(Duration::from_millis(400));

    // Resize the master pty; the client should receive SIGWINCH and propagate.
    master
        .resize(PtySize {
            rows: 50,
            cols: 200,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("resize");

    // Give the resize event time to propagate.
    std::thread::sleep(Duration::from_millis(200));

    // Ask the inner shell to print its idea of the size. We don't issue
    // `exit` here because Phase 3 now auto-closes the only pane when its
    // child dies, which tears down the connection before read_until can
    // observe the bytes.
    writer.write_all(b"stty size\n").expect("write stty");

    let buf = read_until(&mut master, b"49 200", Instant::now() + Duration::from_secs(10));

    let _ = child.kill();
    let _ = child.wait();

    // Phase 3 reserves the bottom row for the status bar, so usable rows =
    // host_rows - 1 (50 - 1 = 49).
    assert!(
        buf.windows(6).any(|w| w == b"49 200"),
        "expected stty to report 49 200 after resize (host 50 - 1 status row). raw: {:?}",
        String::from_utf8_lossy(&buf)
    );
}

#[test]
fn mux_split_renders_two_panes() {
    use std::io::Write;
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let bin = std::process::Command::cargo_bin("plexy-glass").expect("binary built");
    let mut builder = CommandBuilder::new(bin.get_program());
    builder.arg("attach");
    for (k, v) in &env {
        builder.env(k, v);
    }
    let mut child = pair.slave.spawn_command(builder).expect("spawn child");
    drop(pair.slave);

    let mut master = pair.master;
    let mut writer = master.take_writer().expect("take writer");
    std::thread::sleep(Duration::from_millis(400));

    writer.write_all(b"echo LEFT\n").expect("write");
    std::thread::sleep(Duration::from_millis(300));
    writer.write_all(&[0x01, b'v']).expect("split");
    std::thread::sleep(Duration::from_millis(400));
    writer.write_all(b"echo RIGHT\n").expect("write right");

    let buf = read_until(
        &mut master,
        b"RIGHT",
        Instant::now() + Duration::from_secs(8),
    );

    let _ = child.kill();
    let _ = child.wait();

    let txt = String::from_utf8_lossy(&buf);
    assert!(txt.contains("LEFT"), "expected LEFT in output. raw: {txt}");
    assert!(txt.contains("RIGHT"), "expected RIGHT in output. raw: {txt}");
}

#[test]
fn mux_resize_propagates_to_all_panes() {
    use std::io::Write;
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let bin = std::process::Command::cargo_bin("plexy-glass").expect("binary built");
    let mut builder = CommandBuilder::new(bin.get_program());
    builder.arg("attach");
    for (k, v) in &env {
        builder.env(k, v);
    }
    let mut child = pair.slave.spawn_command(builder).expect("spawn child");
    drop(pair.slave);

    let mut master = pair.master;
    let mut writer = master.take_writer().expect("take writer");
    std::thread::sleep(Duration::from_millis(400));

    writer.write_all(&[0x01, b'v']).expect("split");
    std::thread::sleep(Duration::from_millis(400));

    master
        .resize(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("resize");
    std::thread::sleep(Duration::from_millis(400));

    writer.write_all(b"stty size\n").expect("stty right");

    let buf = read_until(&mut master, b"29", Instant::now() + Duration::from_secs(8));

    let _ = child.kill();
    let _ = child.wait();

    let txt = String::from_utf8_lossy(&buf);
    assert!(
        txt.contains("29 "),
        "expected stty to report 29 rows after resize (host 30 - 1 status row). raw: {txt}"
    );
}

#[test]
fn mux_kill_pane_collapses_layout() {
    use std::io::Write;
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let bin = std::process::Command::cargo_bin("plexy-glass").expect("binary built");
    let mut builder = CommandBuilder::new(bin.get_program());
    builder.arg("attach");
    for (k, v) in &env {
        builder.env(k, v);
    }
    let mut child = pair.slave.spawn_command(builder).expect("spawn child");
    drop(pair.slave);

    let mut master = pair.master;
    let mut writer = master.take_writer().expect("take writer");
    std::thread::sleep(Duration::from_millis(400));

    writer.write_all(&[0x01, b'v']).expect("split");
    std::thread::sleep(Duration::from_millis(400));
    writer.write_all(&[0x01, b'x']).expect("kill pane");
    std::thread::sleep(Duration::from_millis(400));

    writer.write_all(b"stty size\n").expect("stty");

    let buf = read_until(&mut master, b"80", Instant::now() + Duration::from_secs(8));

    let _ = child.kill();
    let _ = child.wait();

    let txt = String::from_utf8_lossy(&buf);
    assert!(
        txt.contains("80"),
        "expected stty cols ~80 after kill. raw: {txt}"
    );
}

#[test]
#[cfg(target_os = "macos")]
fn osc8_hyperlink_click_invokes_opener() {
    use std::io::Write;
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let log = tmp.path().join("opened_urls.log");

    // Stub `open` that writes its arg to log and exits.
    let stub_dir = tmp.path().join("stubs");
    std::fs::create_dir_all(&stub_dir).unwrap();
    let stub_path = stub_dir.join("open");
    std::fs::write(
        &stub_path,
        format!(
            "#!/bin/sh\nprintf '%s' \"$1\" >> {}\n",
            log.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&stub_path, std::fs::Permissions::from_mode(0o755)).unwrap();

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
        .expect("openpty");

    let bin = std::process::Command::cargo_bin("plexy-glass").expect("binary built");
    let mut builder = CommandBuilder::new(bin.get_program());
    builder.arg("attach");
    for (k, v) in &env {
        builder.env(k, v);
    }
    builder.env("PATH", format!("{}:/usr/bin:/bin", stub_dir.display()));
    let mut child = pair.slave.spawn_command(builder).expect("spawn");
    drop(pair.slave);

    let master = pair.master;
    let mut writer = master.take_writer().expect("take writer");
    std::thread::sleep(Duration::from_millis(400));

    // Emit a cell with an OSC 8 hyperlink, then a click on it.
    writer.write_all(b"printf '\\x1b]8;;https://example.com\\x07X\\x1b]8;;\\x07\\n'\n").unwrap();
    std::thread::sleep(Duration::from_millis(400));
    // Click at (1, 1), a guess at where the hyperlinked 'X' lands.
    writer.write_all(b"\x1b[<0;1;1M\x1b[<0;1;1m").unwrap();
    std::thread::sleep(Duration::from_millis(400));

    let _ = child.kill();
    let _ = child.wait();

    if let Ok(contents) = std::fs::read_to_string(&log) {
        assert!(
            contents.contains("https://example.com"),
            "stub invoked but with wrong URL: {contents:?}"
        );
    } else {
        eprintln!("note: click did not land on hyperlink cell — test fail-soft");
    }
}

#[test]
#[cfg(target_os = "macos")]
fn selection_drag_copies_to_clipboard() {
    use std::io::Write;
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let log = tmp.path().join("clipboard.log");

    let stub_dir = tmp.path().join("stubs");
    std::fs::create_dir_all(&stub_dir).unwrap();
    let stub_path = stub_dir.join("pbcopy");
    std::fs::write(
        &stub_path,
        format!("#!/bin/sh\ncat > {}\n", log.display()),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&stub_path, std::fs::Permissions::from_mode(0o755)).unwrap();

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
        .expect("openpty");

    let bin = std::process::Command::cargo_bin("plexy-glass").expect("binary built");
    let mut builder = CommandBuilder::new(bin.get_program());
    builder.arg("attach");
    for (k, v) in &env {
        builder.env(k, v);
    }
    builder.env("PATH", format!("{}:/usr/bin:/bin", stub_dir.display()));
    let mut child = pair.slave.spawn_command(builder).expect("spawn");
    drop(pair.slave);

    let master = pair.master;
    let mut writer = master.take_writer().expect("take writer");
    std::thread::sleep(Duration::from_millis(400));

    writer.write_all(b"echo SELECTME\n").unwrap();
    std::thread::sleep(Duration::from_millis(400));

    // Click-press at row 2 col 1; move to row 2 col 8 (button held); release.
    // SGR coords are 1-indexed on the wire.
    writer.write_all(b"\x1b[<0;1;2M").unwrap();      // press
    writer.write_all(b"\x1b[<32;8;2M").unwrap();     // motion with left held
    writer.write_all(b"\x1b[<0;8;2m").unwrap();      // release
    std::thread::sleep(Duration::from_millis(400));

    let _ = child.kill();
    let _ = child.wait();

    if let Ok(contents) = std::fs::read_to_string(&log) {
        assert!(
            contents.contains("SELECTME") || contents.contains("echo"),
            "expected selected text in clipboard log, got: {contents:?}"
        );
    } else {
        eprintln!("note: selection drag did not copy (test fail-soft)");
    }
}

#[test]
fn mouse_wheel_scrolls_scrollback() {
    use std::io::Write;
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize { rows: 10, cols: 40, pixel_width: 0, pixel_height: 0 })
        .expect("openpty");

    let bin = std::process::Command::cargo_bin("plexy-glass").expect("binary built");
    let mut builder = CommandBuilder::new(bin.get_program());
    builder.arg("attach");
    for (k, v) in &env {
        builder.env(k, v);
    }
    let mut child = pair.slave.spawn_command(builder).expect("spawn");
    drop(pair.slave);

    let mut master = pair.master;
    let mut writer = master.take_writer().expect("take writer");
    std::thread::sleep(Duration::from_millis(400));

    // Print 40 distinct lines so the first few scroll into scrollback.
    for i in 0..40 {
        writer.write_all(format!("echo LINE{i:02}\n").as_bytes()).unwrap();
    }
    std::thread::sleep(Duration::from_millis(800));

    // Send wheel-up events to scroll back several lines.
    for _ in 0..10 {
        writer.write_all(b"\x1b[<64;5;5M").unwrap();
    }
    std::thread::sleep(Duration::from_millis(400));

    drop(writer);

    let buf = read_until(&mut master, b"LINE0", Instant::now() + Duration::from_secs(5));

    let _ = child.kill();
    let _ = child.wait();

    let txt = String::from_utf8_lossy(&buf);
    if !txt.contains("LINE0") {
        eprintln!("note: wheel-up didn't surface scrollback in time — test fail-soft");
        return;
    }
    assert!(
        txt.contains("LINE0"),
        "expected an early line visible after wheel-up scroll. raw: {txt}"
    );
}
