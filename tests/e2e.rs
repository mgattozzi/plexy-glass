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
        .write_all(b"echo HEL-LO; exit\n")
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

    // Ask the inner shell to print its idea of the size.
    writer.write_all(b"stty size; exit\n").expect("write stty");

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
    writer.write_all(&[0x02, b'%']).expect("split");
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
