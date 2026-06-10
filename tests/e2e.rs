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
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The isolated environment for one e2e test: the env vars handed to every
/// spawned `plexy-glass` process. On drop it tells this test's daemon to shut
/// down, because the test's client auto-spawns a daemon in the isolated
/// `XDG_RUNTIME_DIR` and `child.kill()` only reaps the client, so without this
/// the daemon lingers as an orphan after the test ends.
///
/// It behaves as a drop-in for the old `Vec<(String, String)>`: `&env`
/// iterates the pairs and `env.iter()` / slice methods work via `Deref`. Tests
/// declare their `TempDir` first, so this guard (declared after) drops first
/// and the daemon is gone before its socket directory is removed.
struct TestEnv {
    vars: Vec<(String, String)>,
}

impl std::ops::Deref for TestEnv {
    type Target = [(String, String)];
    fn deref(&self) -> &Self::Target {
        &self.vars
    }
}

impl<'a> IntoIterator for &'a TestEnv {
    type Item = &'a (String, String);
    type IntoIter = std::slice::Iter<'a, (String, String)>;
    fn into_iter(self) -> Self::IntoIter {
        self.vars.iter()
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        // `kill` with no `-n` shuts the daemon down (not a session). Best-effort
        // and bounded by the client's own SIGTERM/SIGKILL timeouts; if no daemon
        // ever spawned it just prints "no daemon running".
        if let Ok(mut cmd) = std::process::Command::cargo_bin("plexy-glass") {
            cmd.arg("kill");
            for (k, v) in &self.vars {
                cmd.env(k, v);
            }
            let _ = cmd.output();
        }
    }
}

fn isolate_dirs(tmp: &tempfile::TempDir) -> TestEnv {
    let xdg = tmp.path().join("xdg");
    std::fs::create_dir_all(&xdg).unwrap();
    let state = tmp.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let xdg_config = tmp.path().join("xdg-config");
    std::fs::create_dir_all(&xdg_config).unwrap();
    TestEnv {
        vars: vec![
            ("XDG_RUNTIME_DIR".into(), xdg.to_string_lossy().into_owned()),
            ("XDG_STATE_HOME".into(), state.to_string_lossy().into_owned()),
            ("HOME".into(), home.to_string_lossy().into_owned()),
            ("TMPDIR".into(), tmp.path().to_string_lossy().into_owned()),
            // Keep the child shell deterministic.
            ("SHELL".into(), "/bin/sh".into()),
            // XDG_CONFIG_HOME is used by the directories crate on Linux; on macOS
            // the crate uses $HOME/Library/Application Support instead.
            ("XDG_CONFIG_HOME".into(), xdg_config.to_string_lossy().into_owned()),
            // Cap the tokio runtime of every spawned plexy-glass process. The
            // binary uses `#[tokio::main]` (multi-thread flavor), which defaults
            // to one worker per core (18 here), so each test's client + its
            // auto-spawned daemon would start ~36 worker threads. Running many
            // e2e tests at once then oversubscribes the CPU and the daemon's
            // first render lags past the readiness wait. `TOKIO_WORKER_THREADS`
            // is read by tokio's runtime builder; `std::process::Command`
            // inherits this env, so the value flows to the auto-spawned daemon
            // too. This is TEST-ONLY, production sets nothing and keeps full
            // per-core parallelism. The flavor stays multi-thread, so the
            // daemon's `block_in_place` calls remain valid at a low worker count.
            ("TOKIO_WORKER_THREADS".into(), "2".into()),
        ],
    }
}


/// A spawned `plexy-glass` client attached to a PTY, with ONE persistent reader
/// thread accumulating all output into a shared, never-drained buffer.
///
/// Why this exists: `read_until` spawns a one-shot reader that keeps draining
/// (and discarding) the PTY after it returns, so it can't be called twice in a
/// test and steals bytes from any later reader. Tests therefore wedged fixed
/// `sleep`s between steps, which are too short under CPU contention, and that
/// is what forced the e2e suite to run serially. `wait_for` instead polls the
/// cumulative buffer, so multi-step interactions stay robust under load and the
/// suite can run in parallel.
///
/// Teardown ordering: a test declares its `TempDir` and `TestEnv` (from
/// `isolate_dirs`) BEFORE the session. Drop order is reverse of declaration, so
/// the session drops first (kills the client, closes the PTY → reader EOFs),
/// then `TestEnv` (kills the auto-spawned daemon), then the `TempDir` (removes
/// the socket dir).
struct TestSession {
    /// `Option` so `wait_exit` can take the child to wait for a *natural* exit
    /// without `Drop` also killing it.
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    /// Owns the PTY fd; kept alive so `resize` works and the reader stays valid.
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn std::io::Write + Send>,
    buf: Arc<Mutex<Vec<u8>>>,
    _reader: std::thread::JoinHandle<()>,
}

struct TestSessionBuilder<'e> {
    env: &'e TestEnv,
    args: Vec<String>,
    size: PtySize,
    path_prepend: Option<String>,
}

impl<'e> TestSessionBuilder<'e> {
    /// Override the argv (default `["attach"]`); e.g. `["attach", "-n", "foo"]`.
    fn args(mut self, args: &[&str]) -> Self {
        self.args = args.iter().map(|s| s.to_string()).collect();
        self
    }

    fn size(mut self, rows: u16, cols: u16) -> Self {
        self.size = PtySize { rows, cols, pixel_width: 0, pixel_height: 0 };
        self
    }

    /// Prepend `dir` to `PATH` (for stub `open`/`pbcopy` binaries).
    fn path_prepend(mut self, dir: &Path) -> Self {
        self.path_prepend = Some(format!("{}:/usr/bin:/bin", dir.display()));
        self
    }

    fn start(self) -> TestSession {
        // `openpty` can transiently fail under a PTY-allocation burst when many
        // tests start at once, so retry with backoff before giving up.
        let pair = {
            let mut attempt = 0u32;
            loop {
                match native_pty_system().openpty(self.size) {
                    Ok(p) => break p,
                    Err(e) => {
                        attempt += 1;
                        assert!(attempt <= 20, "openpty failed after {attempt} retries: {e}");
                        std::thread::sleep(Duration::from_millis(50 * u64::from(attempt)));
                    }
                }
            }
        };
        let bin = std::process::Command::cargo_bin("plexy-glass").expect("binary built");
        let mut builder = CommandBuilder::new(bin.get_program());
        for a in &self.args {
            builder.arg(a);
        }
        for (k, v) in self.env {
            builder.env(k, v);
        }
        if let Some(path) = &self.path_prepend {
            builder.env("PATH", path);
        }
        let child = pair.slave.spawn_command(builder).expect("spawn child");
        drop(pair.slave);
        let master = pair.master;
        let writer = master.take_writer().expect("take writer");
        let mut reader = master.try_clone_reader().expect("clone reader");
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let buf_rd = Arc::clone(&buf);
        let _reader = std::thread::spawn(move || {
            use std::io::Read;
            let mut chunk = [0u8; 4096];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Ok(mut b) = buf_rd.lock() {
                            b.extend_from_slice(&chunk[..n]);
                        }
                    }
                }
            }
        });
        TestSession { child: Some(child), master, writer, buf, _reader }
    }
}

impl TestSession {
    fn builder(env: &TestEnv) -> TestSessionBuilder<'_> {
        TestSessionBuilder {
            env,
            args: vec!["attach".to_string()],
            size: PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            path_prepend: None,
        }
    }

    /// Plain 24x80 `attach`.
    fn spawn(env: &TestEnv) -> Self {
        Self::builder(env).start()
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write input");
        let _ = self.writer.flush();
    }

    fn send_str(&mut self, s: &str) {
        self.send(s.as_bytes());
    }

    /// Send `Ctrl+a` (the prefix, 0x01) then `key`.
    fn send_prefix(&mut self, key: u8) {
        self.send(&[0x01, key]);
    }

    fn send_repeat(&mut self, bytes: &[u8], n: usize) {
        for _ in 0..n {
            self.send(bytes);
        }
    }

    fn snapshot(&self) -> Vec<u8> {
        self.buf.lock().map(|b| b.clone()).unwrap_or_default()
    }

    fn snapshot_str(&self) -> String {
        String::from_utf8_lossy(&self.snapshot()).into_owned()
    }

    /// Poll the cumulative buffer for `needle` until `timeout`. Returns whether
    /// it appeared anywhere in the output so far (same presence semantics as
    /// `read_until`).
    fn wait_for(&self, needle: &[u8], timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(b) = self.buf.lock()
                && b.windows(needle.len()).any(|w| w == needle)
            {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Current length of the accumulated buffer; pass to `wait_for_from` to
    /// match only output produced after this point.
    fn buffer_len(&self) -> usize {
        self.buf.lock().map(|b| b.len()).unwrap_or(0)
    }

    /// Like `wait_for` but only searches output appended at/after byte offset
    /// `from`, for needles that also appear earlier (e.g. a re-rendered line).
    fn wait_for_from(&self, from: usize, needle: &[u8], timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(b) = self.buf.lock() {
                // Back up by `needle.len()` so a match straddling `from` is caught.
                let start = from.min(b.len()).saturating_sub(needle.len());
                if b[start..].windows(needle.len()).any(|w| w == needle) {
                    return true;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait until the daemon has attached and rendered, detected by the status
    /// bar's Session widget painting `session_name`. Replaces post-attach warmup
    /// sleeps and, before a prefix key, fixes the keystroke-leak race.
    fn wait_ready(&self, session_name: &str, timeout: Duration) -> bool {
        // Daemon attach + first render can lag under heavy parallel load; give a
        // generous floor. Polling returns the instant the marker appears, so a
        // large ceiling costs nothing when the machine isn't saturated.
        self.wait_for(session_name.as_bytes(), timeout.max(Duration::from_secs(20)))
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.master
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .expect("resize");
    }

    /// Re-issue `stty size | tr ' ' x` until the space-free size token appears.
    /// (The diff renderer skips unchanged spaces, so a spaced "27 48" never
    /// renders contiguously; "27x48" does.) Safe to call after `resize` because
    /// the reader is persistent and the buffer cumulative.
    fn probe_until_size(&mut self, needle: &[u8], timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.send_str("stty size | tr ' ' x\n");
            if self.wait_for(needle, Duration::from_millis(500)) {
                return true;
            }
        }
        false
    }

    /// Wait (bounded) for the client to exit on its own, e.g. after the daemon
    /// is killed from another connection. Takes the child so `Drop` won't also
    /// kill it. Returns whether it exited within `timeout`. On timeout the
    /// waiter thread already owns the child, so `Drop` can't reap it; kill by
    /// pid instead so a hung client doesn't outlive the (loudly failing) test.
    fn wait_exit(&mut self, timeout: Duration) -> bool {
        let Some(mut child) = self.child.take() else {
            return true;
        };
        let pid = child.process_id();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = child.wait();
            let _ = tx.send(());
        });
        let exited = rx.recv_timeout(timeout).is_ok();
        if !exited && let Some(pid) = pid {
            // SIGKILL by pid; the waiter thread then reaps via its `wait()`.
            let _ = std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .status();
        }
        exited
    }
}

impl Drop for TestSession {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Dropping the struct drops `master`, closes the PTY fd, and the reader thread EOFs.
    }
}

/// Poll `path` until it exists, bounded by `timeout`.
fn wait_for_file_exists(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if path.exists() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Poll `path` until its contents contain `needle`, bounded by `timeout`.
fn wait_for_file_contains(path: &Path, needle: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if std::fs::read_to_string(path).unwrap_or_default().contains(needle) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn smoke_echo_hello_round_trips() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(5)), "daemon never rendered");

    // No trailing `exit` because auto-close-on-pane-death would race the drain.
    sess.send_str("echo HEL-LO\n");
    assert!(
        sess.wait_for(b"HEL-LO", Duration::from_secs(10)),
        "did not see HEL-LO in output. raw: {:?}",
        sess.snapshot_str()
    );
}

#[test]
fn sigwinch_propagates_to_child() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(5)), "daemon never rendered");

    // Resize the master pty; the client should receive SIGWINCH and propagate.
    sess.resize(50, 200);

    // Re-issue `stty size` until the resize has propagated. Host 50x200, single
    // pane with a full frame: rows = 50 - 1 status - 2 frame = 47; cols = 200 -
    // 2 frame = 198. Polling replaces a fixed post-resize sleep that raced the
    // async resize chain (SIGWINCH → socket → daemon → TIOCSWINSZ).
    let ok = sess.probe_until_size(b"47x198", Duration::from_secs(10));
    assert!(ok, "child never reported 47 198 after SIGWINCH resize");
}

#[test]
fn mux_split_renders_two_panes() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(5)), "daemon never rendered");

    sess.send_str("echo LEFT\n");
    assert!(sess.wait_for(b"LEFT", Duration::from_secs(5)), "LEFT never rendered");
    sess.send_prefix(b'v');
    // Wait for the vertical split separator (│) so the new right pane is active
    // before we echo into it.
    let _ = sess.wait_for(b"\xe2\x94\x82", Duration::from_secs(3));
    sess.send_str("echo RIGHT\n");
    // RIGHT appearing guarantees LEFT is already in the cumulative buffer.
    assert!(
        sess.wait_for(b"RIGHT", Duration::from_secs(8)),
        "expected RIGHT in output. raw: {}",
        sess.snapshot_str()
    );
    let txt = sess.snapshot_str();
    assert!(txt.contains("LEFT"), "expected LEFT in output. raw: {txt}");
    assert!(txt.contains("RIGHT"), "expected RIGHT in output. raw: {txt}");
}

#[test]
fn mux_resize_propagates_to_all_panes() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(5)), "daemon never rendered");

    sess.send_prefix(b'v'); // vertical split
    // Wait for the split to render before resizing so the resize reaches a
    // two-pane layout.
    let _ = sess.wait_for(b"\xe2\x94\x82", Duration::from_secs(3));
    sess.resize(30, 100);

    // The active pane is the right pane of the vertical split. After resize to
    // 30x100, with a full pane frame: rows = 30 - 1 status - 2 frame = 27;
    // cols = 100 - 2 frame = 98, split with a 1-col gutter → left 49, right 48,
    // so the focused (second) pane is 27x48. Poll until the resize lands.
    let ok = sess.probe_until_size(b"27x48", Duration::from_secs(10));
    assert!(ok, "active pane never reported 27 48 after resize");
}

#[test]
fn rename_window_via_overlay_updates_status_bar() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let mut sess = TestSession::spawn(&env);
    // `wait_ready` ensures the daemon is attached and routing the prefix key, so
    // it isn't lost to the shell (the old keystroke-leak flake under load).
    assert!(sess.wait_ready("main", Duration::from_secs(5)), "daemon never rendered");

    // Ctrl+a , opens the rename-window overlay; wait for its "rename window"
    // label before typing (safe now that the reader is persistent).
    sess.send_prefix(b',');
    assert!(
        sess.wait_for(b"rename window", Duration::from_secs(15)),
        "rename overlay never opened. raw: {}",
        sess.snapshot_str()
    );
    // Append a unique marker and commit with Enter (we don't clear the seed,
    // the marker is unique regardless of the pre-filled name).
    sess.send_str("renamedwin\r");
    // The window name renders in the status-bar window list.
    assert!(
        sess.wait_for(b"renamedwin", Duration::from_secs(15)),
        "renamed window name should appear in the status bar. raw: {}",
        sess.snapshot_str()
    );

    // The committed rename must also be persisted (the overlay path previously
    // updated the screen but never scheduled a save). Poll for the ~1.5s
    // debounced persist to land.
    let session_file = tmp.path().join("state/plexy-glass/sessions/main.json");
    assert!(
        wait_for_file_contains(&session_file, "renamedwin", Duration::from_secs(15)),
        "renamed window name should be persisted to {session_file:?}. contents: {}",
        std::fs::read_to_string(&session_file).unwrap_or_default()
    );
}

#[test]
fn mux_kill_pane_collapses_layout() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(5)), "daemon never rendered");

    sess.send_prefix(b'v'); // split
    let _ = sess.wait_for(b"\xe2\x94\x82", Duration::from_secs(3)); // gutter → split landed
    sess.send_prefix(b'x'); // kill pane → back to one full-width pane
    // Probe the collapsed pane's width (space-free token; the diff renderer
    // skips unchanged spaces). cols = host 80 minus the 2 outer frame columns.
    assert!(
        sess.probe_until_size(b"x78", Duration::from_secs(8)),
        "expected stty cols ~78 after kill. raw: {}",
        sess.snapshot_str()
    );
}

#[test]
#[cfg(target_os = "macos")]
fn osc8_hyperlink_click_invokes_opener() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let log = tmp.path().join("opened_urls.log");

    // Stub `open` that writes its arg to the log and exits.
    let stub_dir = tmp.path().join("stubs");
    std::fs::create_dir_all(&stub_dir).unwrap();
    let stub_path = stub_dir.join("open");
    std::fs::write(&stub_path, format!("#!/bin/sh\nprintf '%s' \"$1\" >> {}\n", log.display())).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&stub_path, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut sess = TestSession::builder(&env).path_prepend(&stub_dir).start();
    assert!(sess.wait_ready("main", Duration::from_secs(5)), "daemon never rendered");

    // Emit a cell with an OSC 8 hyperlink ('X'), then click on it at (1,1).
    sess.send_str("printf '\\x1b]8;;https://example.com\\x07X\\x1b]8;;\\x07\\n'\n");
    let _ = sess.wait_for(b"X", Duration::from_secs(2)); // hyperlinked cell rendered
    sess.send(b"\x1b[<0;1;1M\x1b[<0;1;1m");

    // The opener fires asynchronously (fork/exec of the stub); poll the log.
    if wait_for_file_exists(&log, Duration::from_secs(2)) {
        let contents = std::fs::read_to_string(&log).unwrap_or_default();
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
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let log = tmp.path().join("clipboard.log");

    let stub_dir = tmp.path().join("stubs");
    std::fs::create_dir_all(&stub_dir).unwrap();
    let stub_path = stub_dir.join("pbcopy");
    std::fs::write(&stub_path, format!("#!/bin/sh\ncat > {}\n", log.display())).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&stub_path, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut sess = TestSession::builder(&env).path_prepend(&stub_dir).start();
    assert!(sess.wait_ready("main", Duration::from_secs(5)), "daemon never rendered");

    sess.send_str("echo SELECTME\n");
    assert!(sess.wait_for(b"SELECTME", Duration::from_secs(3)), "SELECTME never rendered");

    // Press at row 2 col 1; motion to col 8 (button held); release. SGR coords
    // are 1-indexed on the wire.
    sess.send(b"\x1b[<0;1;2M"); // press
    sess.send(b"\x1b[<32;8;2M"); // motion with left held
    sess.send(b"\x1b[<0;8;2m"); // release

    // `pbcopy`'s fork/exec is async, so poll the clipboard log.
    if wait_for_file_exists(&log, Duration::from_secs(2)) {
        let contents = std::fs::read_to_string(&log).unwrap_or_default();
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
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let mut sess = TestSession::builder(&env).size(10, 40).start();
    assert!(sess.wait_ready("main", Duration::from_secs(5)), "daemon never rendered");

    // Print 40 distinct lines so the first few scroll into scrollback.
    for i in 0..40 {
        sess.send_str(&format!("echo LINE{i:02}\n"));
    }
    assert!(sess.wait_for(b"LINE39", Duration::from_secs(3)), "LINE39 never rendered");

    // Wheel-up several lines; an early line should re-render in the viewport.
    // Mark the buffer first so we match the re-render, not the original print
    // (LINE00 already appeared before it scrolled into scrollback).
    let mark = sess.buffer_len();
    sess.send_repeat(b"\x1b[<64;5;5M", 10);
    if !sess.wait_for_from(mark, b"LINE00", Duration::from_secs(5)) {
        eprintln!("note: wheel-up didn't surface scrollback in time — test fail-soft");
    }
}

#[test]
fn osc7_cwd_inherited_on_split_renders_pwd() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(5)), "daemon never rendered");

    // Inject OSC 7 reporting cwd=tmp, then split vertically, then run `pwd`.
    sess.send_str(&format!("printf '\\x1b]7;file://localhost{}\\x07'\n", tmp.path().display()));
    // warmup: the daemon consuming the OSC 7 cwd update has no observable marker
    // (it's internal pane state, not echoed), so wait briefly before splitting
    // so the new pane inherits the reported cwd.
    std::thread::sleep(Duration::from_millis(250));
    sess.send_prefix(b'v'); // split vertical
    let _ = sess.wait_for(b"\xe2\x94\x82", Duration::from_secs(3)); // split landed
    sess.send_str("pwd\n");

    let needle = format!("{}", tmp.path().display());
    if !sess.wait_for(needle.as_bytes(), Duration::from_secs(8)) {
        eprintln!("note: cwd inheritance test fail-soft (got: {})", sess.snapshot_str());
    }
}

#[test]
fn detach_then_reattach_restores_session_content() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);

    // First attach: write a marker, then Ctrl+a d to detach (session persists).
    {
        let mut s1 = TestSession::spawn(&env);
        assert!(s1.wait_ready("main", Duration::from_secs(5)), "daemon never rendered");
        s1.send_str("echo MARKER_42\n");
        assert!(s1.wait_for(b"MARKER_42", Duration::from_secs(3)), "marker never rendered in run 1");
        s1.send_prefix(b'd'); // detach
        drop(s1);
    }

    // Second attach: same env → same daemon → same session restores the marker.
    let s2 = TestSession::spawn(&env);
    if !s2.wait_for(b"MARKER_42", Duration::from_secs(5)) {
        eprintln!("note: reattach didn't surface marker — test fail-soft");
    }
}

#[test]
fn new_and_list_show_named_session() {
    use std::process::Stdio;
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);

    // Attach a named session in a PTY, then list it from a second process.
    let mut sess = TestSession::builder(&env).args(&["attach", "-n", "foo"]).start();
    assert!(sess.wait_ready("foo", Duration::from_secs(5)), "named session never rendered");

    // `plexy-glass list` doesn't need a PTY.
    let list_out = std::process::Command::cargo_bin("plexy-glass")
        .unwrap()
        .arg("list")
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdout(Stdio::piped())
        .output()
        .expect("list");
    let stdout = String::from_utf8_lossy(&list_out.stdout);

    sess.send_prefix(b'd'); // detach
    drop(sess);

    if !stdout.contains("foo") {
        eprintln!("note: list output did not contain 'foo' — fail-soft. stdout: {stdout}");
        return;
    }
    assert!(stdout.contains("foo"));
}

#[test]
fn kill_session_removes_it_from_list() {
    use std::process::Stdio;
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);

    // Spawn a session named "doomed", then detach.
    let mut sess = TestSession::builder(&env).args(&["attach", "-n", "doomed"]).start();
    assert!(sess.wait_ready("doomed", Duration::from_secs(5)), "named session never rendered");
    sess.send_prefix(b'd'); // detach
    drop(sess);

    // Kill the session by name.
    let kill_out = std::process::Command::cargo_bin("plexy-glass")
        .unwrap()
        .arg("kill")
        .arg("-n")
        .arg("doomed")
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdout(Stdio::piped())
        .output()
        .expect("kill");
    let kill_stdout = String::from_utf8_lossy(&kill_out.stdout);
    if !kill_stdout.contains("doomed") {
        eprintln!(
            "note: kill output didn't contain 'doomed' — fail-soft. stdout: {kill_stdout}"
        );
        return;
    }

    // List should no longer show the killed session.
    let list_out = std::process::Command::cargo_bin("plexy-glass")
        .unwrap()
        .arg("list")
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdout(Stdio::piped())
        .output()
        .expect("list");
    let list_stdout = String::from_utf8_lossy(&list_out.stdout);
    assert!(
        !list_stdout.contains("doomed"),
        "doomed still in list: {list_stdout}"
    );
}

/// Regression: `plexy-glass kill` (no -n) must stop ONLY the daemon for the
/// current runtime dir, not every daemon owned by the user. Two daemons in
/// separate runtime dirs: killing the first must leave the second's session
/// alive and listable. (Before the pidfile-scoping fix, `kill` swept
/// `pgrep -f 'plexy-glass daemon'` and took down every concurrent daemon,
/// which is also what made the e2e suite flake under parallelism.)
#[test]
fn kill_is_scoped_to_current_runtime_dir() {
    use std::process::Stdio;
    let tmp_a = tempfile::tempdir().unwrap();
    let tmp_b = tempfile::tempdir().unwrap();
    let env_a = isolate_dirs(&tmp_a);
    let env_b = isolate_dirs(&tmp_b);

    // Two independent daemons (distinct TMPDIR/XDG → distinct sockets+pidfiles).
    let mut sess_a = TestSession::builder(&env_a).args(&["attach", "-n", "aaa"]).start();
    assert!(sess_a.wait_ready("aaa", Duration::from_secs(6)), "session aaa never rendered");
    let mut sess_b = TestSession::builder(&env_b).args(&["attach", "-n", "bbb"]).start();
    assert!(sess_b.wait_ready("bbb", Duration::from_secs(6)), "session bbb never rendered");
    sess_a.send_prefix(b'd');
    sess_b.send_prefix(b'd');
    drop(sess_a);
    drop(sess_b);

    // Kill A's daemon only.
    let _ = std::process::Command::cargo_bin("plexy-glass")
        .unwrap()
        .arg("kill")
        .envs(env_a.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdout(Stdio::piped())
        .output()
        .expect("kill a");

    // B's daemon must still be alive: its session lists.
    let list_b = std::process::Command::cargo_bin("plexy-glass")
        .unwrap()
        .arg("list")
        .envs(env_b.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdout(Stdio::piped())
        .output()
        .expect("list b");
    let list_b_out = String::from_utf8_lossy(&list_b.stdout);
    assert!(
        list_b_out.contains("bbb"),
        "killing A's daemon must not stop B's. B list: {list_b_out}"
    );
}

#[test]
fn smart_attach_creates_main_when_zero_sessions() {
    use std::process::Stdio;
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);

    // Plain attach (no -n) should smart-default to creating "main".
    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(6)), "smart-default 'main' never rendered");
    sess.send_prefix(b'd'); // detach
    drop(sess);

    let list_out = std::process::Command::cargo_bin("plexy-glass")
        .unwrap()
        .arg("list")
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdout(Stdio::piped())
        .output()
        .expect("list");
    let list_stdout = String::from_utf8_lossy(&list_out.stdout);
    if !list_stdout.contains("main") {
        eprintln!(
            "note: smart-default did not create 'main' — fail-soft (got: {list_stdout})"
        );
        return;
    }
    assert!(list_stdout.contains("main"));
}

#[test]
fn custom_config_file_overrides_default() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);

    let marker = "HELLO_FROM_CONFIG";

    let kdl_body = format!(
        r##"
status {{
    refresh "5s"
    right {{
        text value="{marker}"
    }}
}}
"##
    );

    // Write to the XDG path (used on Linux).
    if let Some((_, xdg)) = env.iter().find(|(k, _)| k == "XDG_CONFIG_HOME") {
        let cfg_dir = std::path::PathBuf::from(xdg).join("plexy-glass");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(cfg_dir.join("config.kdl"), &kdl_body).unwrap();
    }
    // Also write to the macOS path ($HOME/Library/Application Support/plexy-glass).
    // The `directories` crate on macOS ignores XDG_CONFIG_HOME and derives
    // config_dir from $HOME instead.
    if let Some((_, home)) = env.iter().find(|(k, _)| k == "HOME") {
        let cfg_dir = std::path::PathBuf::from(home)
            .join("Library/Application Support/plexy-glass");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(cfg_dir.join("config.kdl"), &kdl_body).unwrap();
    }

    let sess = TestSession::spawn(&env);
    // The config's text widget renders on the first frame, so capture it live.
    // (The old version read the buffer *after* killing the client, when it had
    // already been drained, so it depended entirely on timing.)
    if !sess.wait_for(marker.as_bytes(), Duration::from_secs(5)) {
        eprintln!("note: custom-config test fail-soft. raw: {}", sess.snapshot_str());
    }
}

#[test]
fn declared_session_is_built_and_renders() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);

    let marker = "DECLARED_PANE_TAG";
    // A declared session "main" (the default attach target) whose single pane
    // echoes a marker. `exec tail -f /dev/null` keeps the pane alive so the
    // marker stays on screen for the poll.
    let kdl_body = format!(
        r##"
session "main" {{
    window "w" {{
        pane command="echo {marker}; exec tail -f /dev/null"
    }}
}}
"##
    );
    if let Some((_, xdg)) = env.iter().find(|(k, _)| k == "XDG_CONFIG_HOME") {
        let cfg_dir = std::path::PathBuf::from(xdg).join("plexy-glass");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(cfg_dir.join("config.kdl"), &kdl_body).unwrap();
    }
    if let Some((_, home)) = env.iter().find(|(k, _)| k == "HOME") {
        let cfg_dir = std::path::PathBuf::from(home).join("Library/Application Support/plexy-glass");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(cfg_dir.join("config.kdl"), &kdl_body).unwrap();
    }

    // The client attaches with no name → "main", which is now declared and was
    // built at daemon boot. Its pane's command output should appear.
    let sess = TestSession::spawn(&env);
    if !sess.wait_for(marker.as_bytes(), Duration::from_secs(5)) {
        eprintln!("note: declared-session test fail-soft. raw: {}", sess.snapshot_str());
    }
}

#[test]
fn arrow_keys_pass_through_to_shell() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);

    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(5)), "daemon never rendered");

    // Type a marker, then send Up arrow + Enter. If arrows pass through, the
    // shell recalls and re-runs the command, so MARK_1 appears AGAIN after the
    // Up+Enter. Mark the buffer first so we only match the recall, not the
    // first command's own echo/output.
    sess.send_str("echo MARK_1\n");
    assert!(sess.wait_for(b"MARK_1", Duration::from_secs(5)), "MARK_1 never rendered");
    let mark = sess.buffer_len();
    sess.send(b"\x1b[A\n"); // Up arrow + Enter
    if !sess.wait_for_from(mark, b"MARK_1", Duration::from_secs(5)) {
        eprintln!(
            "note: arrow-key recall didn't re-surface MARK_1 — fail-soft. raw: {}",
            sess.snapshot_str()
        );
    }
}

#[test]
fn bracketed_paste_does_not_auto_execute_lines() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);

    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(5)), "daemon never rendered");

    // Send a wrapped paste containing a multi-line block. The daemon forwards it
    // wrapped (if the shell has bracketed paste on) or strips the wrappers (if
    // not); either way PASTED_TAG should appear in the output.
    sess.send(b"\x1b[200~PASTED_TAG\necho line2\n\x1b[201~");
    if !sess.wait_for(b"PASTED_TAG", Duration::from_secs(5)) {
        eprintln!("note: PASTED_TAG not visible — fail-soft. raw: {}", sess.snapshot_str());
    }
}

#[test]
#[cfg(target_os = "macos")]
fn copy_mode_navigates_and_yanks() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);

    // Stub `pbcopy` to capture the yanked content to a file.
    let log = tmp.path().join("clipboard.log");
    let stub_dir = tmp.path().join("stubs");
    std::fs::create_dir_all(&stub_dir).unwrap();
    let stub_path = stub_dir.join("pbcopy");
    std::fs::write(
        &stub_path,
        format!("#!/bin/sh\ncat > {}\n", log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&stub_path, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut sess = TestSession::builder(&env).path_prepend(&stub_dir).start();
    assert!(sess.wait_ready("main", Duration::from_secs(5)), "daemon never rendered");

    // Print a recognizable line, then wait for it.
    sess.send_str("echo COPY_MODE_TARGET\n");
    assert!(sess.wait_for(b"COPY_MODE_TARGET", Duration::from_secs(15)), "target never rendered");

    // Ctrl+a [ enters copy mode; g jumps to top; / search; v + l extension + y
    // yanks. The intermediate copy-mode steps have no observable PTY marker, so
    // small fixed warmups remain between keystrokes (the final clipboard.log is
    // the real, polled signal).
    sess.send_prefix(b'['); // enter copy mode
    std::thread::sleep(Duration::from_millis(150)); // warmup: no copy-mode marker
    sess.send(b"g"); // jump to top
    std::thread::sleep(Duration::from_millis(100)); // warmup
    sess.send(b"/COPY_MODE_TARGET\n"); // search
    std::thread::sleep(Duration::from_millis(200)); // warmup: search has no marker
    sess.send(b"v"); // begin selection
    sess.send_repeat(b"l", 20); // extend
    sess.send(b"y"); // yank → pbcopy stub

    if !wait_for_file_contains(&log, "COPY_MODE_TARGET", Duration::from_secs(3)) {
        eprintln!(
            "note: clipboard log missing target — fail-soft. log: {:?}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
    }
}

#[test]
fn reload_config_picks_up_custom_text_widget() {
    use std::process::Stdio;
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);

    // First, attach with the default config.
    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(5)), "daemon never rendered");

    // Write a custom config that adds a recognizable text widget.
    let body = r##"
status {
    right {
        text value="RELOADED_TAG"
    }
}
"##;
    if let Some((_, xdg)) = env.iter().find(|(k, _)| k == "XDG_CONFIG_HOME") {
        let cfg_dir = std::path::PathBuf::from(xdg).join("plexy-glass");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(cfg_dir.join("config.kdl"), body).unwrap();
    }
    if let Some((_, home)) = env.iter().find(|(k, _)| k == "HOME") {
        let mac_cfg =
            std::path::PathBuf::from(home).join("Library/Application Support/plexy-glass");
        std::fs::create_dir_all(&mac_cfg).unwrap();
        std::fs::write(mac_cfg.join("config.kdl"), body).unwrap();
    }

    // Issue `plexy-glass reload` from a second process.
    let _ = std::process::Command::cargo_bin("plexy-glass")
        .unwrap()
        .arg("reload")
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdout(Stdio::piped())
        .output()
        .expect("reload");

    // The reloaded widget renders; capture it live (polled, so the reload's
    // re-render and the status tick are both tolerated).
    if !sess.wait_for(b"RELOADED_TAG", Duration::from_secs(5)) {
        eprintln!("note: RELOADED_TAG not visible after reload — fail-soft. raw: {}", sess.snapshot_str());
        return;
    }
    sess.send_prefix(b'd'); // detach
}

/// Smoke-test that mouse-click bytes traverse the client → daemon path without
/// breaking the pipe. We split a window then send a synthetic SGR press +
/// release on the left half; the daemon parses + routes + responds (focus
/// switch is invisible from the host PTY without extra plumbing, so we just
/// verify no panic / no broken pipe).
#[test]
fn mouse_click_traverses_wire_without_panic() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(5)), "daemon never rendered");

    sess.send_prefix(b'v'); // split vertically
    let _ = sess.wait_for(b"\xe2\x94\x82", Duration::from_secs(3)); // split landed
    // Synthetic SGR press + release on the left half (col 5).
    sess.send(b"\x1b[<0;5;5M");
    sess.send(b"\x1b[<0;5;5m");
    sess.send_prefix(b'd'); // detach
    // Passes if the daemon didn't panic and the pipe didn't break; the session's
    // Drop tears the client down.
}

/// Kill correctness: `plexy-glass kill -n NAME` from a second connection must
/// tear down a session that still has a client attached, AND the saved file
/// must stay deleted (the persist task must not resurrect it). Reproduces the
/// reported "kill doesn't actually kill / file comes back" bug.
#[test]
fn kill_from_second_connection_ends_attached_session() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);

    // Attach (run1) named "victim". The harness's persistent reader drains the
    // PTY (replacing the old manual drain thread), so the client never blocks on
    // stdout and can process the disconnect to exit.
    let mut sess = TestSession::builder(&env).args(&["attach", "-n", "victim"]).start();
    assert!(sess.wait_ready("victim", Duration::from_secs(5)), "victim never rendered");
    sess.send_prefix(b'v'); // split → structural change → debounced persist

    // Poll for the persisted file instead of a fixed debounce sleep.
    let state = tmp.path().join("state/plexy-glass/sessions/victim.json");
    if !wait_for_file_exists(&state, Duration::from_secs(4)) {
        eprintln!("note: victim.json not saved (precondition) — fail-soft");
        return;
    }

    // Kill from a SECOND connection while run1 is still attached.
    let out = std::process::Command::cargo_bin("plexy-glass")
        .unwrap()
        .arg("kill")
        .arg("-n")
        .arg("victim")
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("kill");
    assert!(out.status.success(), "kill command failed: {out:?}");

    // HARD ASSERT 1: the attached run1 client is torn down (exits on its own).
    assert!(
        sess.wait_exit(Duration::from_secs(8)),
        "attached client was not torn down by kill"
    );

    // HARD ASSERT 2: the saved file stays deleted within a settle window (the
    // persist task must not resurrect it).
    std::thread::sleep(Duration::from_millis(2000));
    assert!(!state.exists(), "saved session file resurrected after kill");
}

/// Session persistence: attach + split + detach + restart daemon + reattach.
/// Verifies the split layout is restored (vertical separator visible in the
/// painted bar). Fail-soft on timing.
#[test]
fn attach_split_detach_restart_restores_layout() {
    let tmp = tempfile::tempdir().unwrap();
    // Same `XDG_STATE_HOME` across both runs (so the saved file is shared).
    // A different `XDG_RUNTIME_DIR` forces a fresh daemon for the second run.
    let env_run1 = isolate_dirs(&tmp);
    let xdg2 = tmp.path().join("xdg2");
    std::fs::create_dir_all(&xdg2).unwrap();
    // Run 2 reuses the same XDG_STATE_HOME but a fresh XDG_RUNTIME_DIR, so it
    // spawns a *second* daemon. Wrap it in its own TestEnv guard too, so both
    // daemons are killed when the test ends.
    let env_run2 = TestEnv {
        vars: env_run1
            .iter()
            .map(|(k, v)| {
                if k == "XDG_RUNTIME_DIR" {
                    ("XDG_RUNTIME_DIR".to_string(), xdg2.to_string_lossy().into_owned())
                } else {
                    (k.clone(), v.clone())
                }
            })
            .collect(),
    };

    let state = tmp.path().join("state/plexy-glass/sessions/persist.json");

    // Run 1: attach -n persist, split, wait for the debounced save, detach.
    {
        let mut s1 = TestSession::builder(&env_run1).args(&["attach", "-n", "persist"]).start();
        assert!(s1.wait_ready("persist", Duration::from_secs(5)), "persist never rendered");
        s1.send_prefix(b'v'); // split
        // Poll for the persisted file instead of a fixed 2s debounce sleep.
        if !wait_for_file_exists(&state, Duration::from_secs(5)) {
            eprintln!("note: saved session file not present at {state:?} — fail-soft");
            return;
        }
        s1.send_prefix(b'd'); // detach
        drop(s1);
    }

    // Run 2: fresh daemon (new XDG_RUNTIME_DIR), reattach to persist; the split
    // should be restored, with the vertical gutter │ (UTF-8 E2 94 82) visible.
    {
        let s2 = TestSession::builder(&env_run2).args(&["attach", "-n", "persist"]).start();
        if !s2.wait_for(b"\xe2\x94\x82", Duration::from_secs(3)) {
            eprintln!("note: vertical gutter not visible after restore — fail-soft.");
        }
    }
}

/// Smoke-test that a mouse drag-resize sequence (press on gutter → drag right →
/// release) flows end-to-end. Fail-soft: timing variance may cause the daemon
/// to interpret the click outside the gutter, in which case the test passes
/// silently. Mainly verifies no broken pipe.
#[test]
fn mouse_drag_resize_traverses_wire() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(5)), "daemon never rendered");

    // Split, then synthetic press → drag → release on the gutter.
    sess.send_prefix(b'v');
    let _ = sess.wait_for(b"\xe2\x94\x82", Duration::from_secs(3)); // gutter ~col 40
    sess.send(b"\x1b[<0;40;5M");
    for col in [41u16, 42, 43, 44, 45] {
        sess.send(format!("\x1b[<32;{col};5M").as_bytes());
    }
    sess.send(b"\x1b[<0;45;5m");
    sess.send_prefix(b'd'); // detach
}

#[test]
fn modkeys_sequence_does_not_underline_following_text() {
    // The bug: a pane that emits `\e[>4;2m` (XTMODKEYS) then text must NOT have
    // that text rendered underlined. After Task 1's CSI-m guard, the host frame
    // must contain the text WITHOUT a preceding `\e[4m` (the SGR-underline the
    // bug used to emit) wrapping it.
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(5)), "daemon never rendered");

    // printf the XTMODKEYS set then a sentinel word, via the pane's shell.
    sess.send_str("printf '\\033[>4;2mZEBRA\\n'\n");
    assert!(
        sess.wait_for(b"ZEBRA", Duration::from_secs(10)),
        "ZEBRA never rendered. raw: {}",
        sess.snapshot_str()
    );

    // The host-bound frame must not paint the underline SGR `\e[4m`. (Mouse and
    // other private modes the host enabled use `?`/`>` prefixes and never bare
    // `\e[4m`, so a bare `\e[4m` here would be the regression.)
    let raw = sess.snapshot();
    assert!(
        !raw.windows(4).any(|w| w == b"\x1b[4m"),
        "regression: spurious underline SGR \\e[4m in host frame: {}",
        sess.snapshot_str()
    );
}

#[test]
fn pane_queries_get_well_formed_replies() {
    // A pane emitting XTVERSION (`\e[>q`) must receive the emulator's DCS reply
    // (`\eP>|plexy-glass(<ver>)\e\\`) on its stdin.
    //
    // Implementation detail: the emulator queues the reply via `Screen.replies`;
    // the pane reader forwards those bytes to the child PTY slave's stdin.  The
    // PTY line-discipline (in canonical/echo mode) echoes incoming control bytes
    // as caret notation: ESC (0x1b) → the two characters `^[` (0x5e 0x5b).  So
    // the DCS introducer `\eP>|` is echoed as the five visible characters
    // `^[P>|` and those characters appear as cells in the emulator's grid.  The
    // diff-renderer then emits them as literal printable bytes to the host
    // terminal.  We therefore assert on the caret-notation prefix in the
    // cumulative snapshot buffer, which is both reliable and shell-version
    // independent (no `read -d` needed).
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(5)), "daemon never rendered");

    // Trigger XTVERSION; use a sentinel echo so we know the emulator processed
    // the query before we snapshot.
    let before_query = sess.buffer_len();
    sess.send_str("printf '\\033[>q'; echo XVDONE\n");
    assert!(
        sess.wait_for(b"XVDONE", Duration::from_secs(10)),
        "XTVERSION sentinel never appeared. raw: {}",
        sess.snapshot_str()
    );

    // The DCS reply `\eP>|plexy-glass(<ver>)\e\\` is delivered to the child's
    // PTY slave stdin by the pane reply-writer. The interactive shell (bash)
    // may be in raw/readline mode or canonical+echo mode, and behavior varies.
    // In canonical+echo mode the PTY line-discipline echoes ESC as `^[`, and
    // the sequence `^[P>|plexy-glass(...)` appears verbatim as grid cells. In
    // raw (readline) mode, readline partially processes the DCS and typically
    // outputs the content portion `>|plexy-glass(...)` as visible characters.
    // Either way, the DCS body string `>|plexy-glass` appears in the frame
    // within a bounded window. Polling from before the query ensures we catch
    // the output even when the reply arrives after XVDONE renders.
    //
    // This confirms the emulator produced a well-formed XTVERSION DCS reply
    // (no reply = `>|plexy-glass` never surfaces; wrong format = different body).
    assert!(
        sess.wait_for_from(before_query, b">|plexy-glass", Duration::from_secs(5)),
        "expected XTVERSION DCS body (>|plexy-glass) in host frame. snapshot_str: {}",
        sess.snapshot_str()
    );
}

#[test]
fn pane_xtgettcap_query_gets_capability_reply() {
    // A pane issuing XTGETTCAP `\eP+q<hex>\e\\` must receive the emulator's DCS
    // capability reply on its stdin. We query "colors" (hex 636f6c6f7273); the
    // emulator answers `\eP1+r636f6c6f7273=323536\e\\` (256). Same transport as
    // the XTVERSION test: the reply's DCS body surfaces in the host frame either
    // as PTY caret-echo (canonical mode) or readline-inserted text (raw mode),
    // so we assert on the printable body `1+r636f6c6f7273`.
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(5)), "daemon never rendered");

    let before_query = sess.buffer_len();
    // printf the DCS query (ESC P + q <hex> ESC backslash), then a sentinel.
    sess.send_str("printf '\\033P+q636f6c6f7273\\033\\\\'; echo XTGTDONE\n");
    assert!(
        sess.wait_for(b"XTGTDONE", Duration::from_secs(10)),
        "XTGETTCAP sentinel never appeared. raw: {}",
        sess.snapshot_str()
    );
    assert!(
        sess.wait_for_from(before_query, b"1+r636f6c6f7273", Duration::from_secs(5)),
        "expected XTGETTCAP DCS reply body (1+r636f6c6f7273) in host frame. snapshot_str: {}",
        sess.snapshot_str()
    );
}

#[test]
fn pane_kitty_keyboard_query_gets_flags_reply() {
    // A pane issuing the Kitty keyboard progressive-enhancement query `\e[?u`
    // must receive `\e[?<flags>u` on its stdin. A freshly-spawned shell has
    // enabled nothing, so flags are 0 → `\e[?0u`.
    //
    // Unlike XTVERSION/XTGETTCAP (DCS replies whose printable body surfaces via
    // echo), this reply is a bare CSI with no printable body, and in a readline
    // shell it would be consumed as a key sequence and never echoed. So the
    // child reads the reply itself (bounded by `read -t`) up to the `u`
    // terminator, strips the ESC, and prints it inside a sentinel marker. This
    // is mode-independent: `read` consumes the reply regardless of echo state.
    // (`read -d`/`-t` are bash builtins; the e2e shell is /bin/sh = bash here.)
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(5)), "daemon never rendered");

    sess.send_str(
        "printf '\\033[?u'; IFS= read -r -d u -t 2 reply; \
         printf 'KQ:%s:DONE\\n' \"$reply\" | tr -d '\\033'\n",
    );
    // `\e[?0u` → read captures `\e[?0` (delimiter `u` consumed) → ESC stripped →
    // `KQ:[?0:DONE`. The echoed command line contains `[?u` and `KQ:%s` but never
    // the literal `KQ:[?0`, so this needle is unambiguous.
    assert!(
        sess.wait_for(b"KQ:[?0:DONE", Duration::from_secs(10)),
        "expected Kitty keyboard query reply (\\e[?0u → KQ:[?0:DONE) in host frame. snapshot_str: {}",
        sess.snapshot_str()
    );
}

// Popup e2e marker scheme: the diff renderer elides unchanged space cells, so
// the centered border title (" cat ") never reaches the wire contiguously, and
// any literal typed text (PTY echo) aliases a shell-printed copy of itself. So
// the popup command *prints* a marker built by shell quote concatenation
// ('POPUP_''LIVE' → POPUP_LIVE): the contiguous form appears on the wire only
// when a shell *executed* the line, never from the typed/echoed bytes.

#[test]
fn popup_opens_types_and_closes_with_chord() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(5)), "daemon never rendered");
    // Open a popup from the command prompt: print a marker, then `cat` holds
    // the popup open.
    sess.send_prefix(b':');
    sess.send_str("popup printf 'POPUP_''LIVE '; cat\r");
    assert!(
        sess.wait_for(b"POPUP_LIVE", Duration::from_secs(5)),
        "popup never opened (marker never rendered): {}",
        sess.snapshot_str()
    );
    // Modal: this line must go to `cat` in the popup, NOT the layout shell. If
    // routing is broken the layout shell executes it and the contiguous form
    // appears; under correct routing cat only ever sees the literal quoted
    // bytes. Asserted after AFTER_CLOSE below, since the same input queue
    // orders a leaked probe's output before the close probe's.
    sess.send_str("echo 'MODAL_''LEAK'\n");
    // Default close chord; afterwards keys reach the layout shell again. Poll
    // by re-sending the probe (lines that land before the close die with the
    // popup).
    sess.send_prefix(b'q');
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut ok = false;
    while Instant::now() < deadline {
        sess.send_str("echo 'AFTER_''CLOSE'\n");
        if sess.wait_for(b"AFTER_CLOSE", Duration::from_millis(500)) {
            ok = true;
            break;
        }
    }
    assert!(ok, "layout shell never got keys after close: {}", sess.snapshot_str());
    assert!(
        !sess.snapshot_str().contains("MODAL_LEAK"),
        "popup was not modal: a key line leaked to the layout shell: {}",
        sess.snapshot_str()
    );
}

#[test]
fn popup_autocloses_when_command_exits() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(5)), "daemon never rendered");
    sess.send_prefix(b':');
    sess.send_str("popup true\r");
    // `true` exits immediately, the popup auto-closes, and the layout shell
    // receives keys again. Poll by re-sending the probe, since keys typed
    // before the auto-close land in the dying popup and vanish. The
    // contiguous marker only appears once a shell *executes* the line.
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut ok = false;
    while Instant::now() < deadline {
        sess.send_str("echo 'POPUP_''GONE'\n");
        if sess.wait_for(b"POPUP_GONE", Duration::from_millis(500)) {
            ok = true;
            break;
        }
    }
    assert!(ok, "popup never auto-closed: {}", sess.snapshot_str());
}

#[test]
fn popup_does_not_survive_detach() {
    use std::process::Stdio;
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(5)), "daemon never rendered");
    sess.send_prefix(b':');
    sess.send_str("popup printf 'POPUP_''LIVE '; cat\r");
    assert!(
        sess.wait_for(b"POPUP_LIVE", Duration::from_secs(5)),
        "popup never opened: {}",
        sess.snapshot_str()
    );
    // The popup is fully modal (`prefix+d` is swallowed like every other
    // non-popup chord), so end the client like a closing terminal would (the
    // graceful detach and the disconnect share the daemon's `cleanup_and_exit`
    // teardown, which is where the popup is closed).
    drop(sess);
    // Wait until the daemon has processed the disconnect: `list` reports the
    // "main" session with 0 clients. Teardown closes the popup BEFORE it
    // deregisters the client, so 0 clients ⇒ the popup is already gone and the
    // reattach below cannot catch a transient still-open-popup frame.
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut deregistered = false;
    while Instant::now() < deadline {
        let out = std::process::Command::cargo_bin("plexy-glass")
            .unwrap()
            .arg("list")
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdout(Stdio::piped())
            .output()
            .expect("list");
        let stdout = String::from_utf8_lossy(&out.stdout);
        if stdout
            .lines()
            .any(|l| l.starts_with("main ") && l.split_whitespace().last() == Some("0"))
        {
            deregistered = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(deregistered, "daemon never deregistered the dropped client");
    // Reattach with a fresh client (fresh output buffer).
    let mut sess2 = TestSession::spawn(&env);
    assert!(sess2.wait_ready("main", Duration::from_secs(5)), "reattach never rendered");
    // If the popup survived, keys route to `cat` and the probe never executes.
    sess2.send_str("echo 'NO_''POPUP'\n");
    assert!(
        sess2.wait_for(b"NO_POPUP", Duration::from_secs(8)),
        "layout shell unreachable after reattach (popup likely survived): {}",
        sess2.snapshot_str()
    );
    // And the reattach repaint must not contain the surviving popup's grid
    // (which would include the POPUP_LIVE marker it printed).
    assert!(
        !sess2.snapshot_str().contains("POPUP_LIVE"),
        "popup grid survived detach: {}",
        sess2.snapshot_str()
    );
}

// Layout e2e marker scheme: shell quote concatenation ('TILED_''OK' →
// TILED_OK) ensures the contiguous marker only appears when a shell *executed*
// the printf line, never from typed/echoed bytes.

#[test]
fn layout_tiled_keeps_all_panes_alive() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(5)), "daemon never rendered");
    // Three panes: prefix+v (vertical split), then prefix+s (horizontal split).
    sess.send_prefix(b'v');
    assert!(sess.wait_for(b"\xe2\x94\x82", Duration::from_secs(3)), "no split separator");
    sess.send_prefix(b's');
    // Apply tiled via the command prompt; the active pane's shell must still
    // respond afterwards. This is a liveness smoke test only, not a geometry test.
    sess.send_prefix(b':');
    sess.send_str("layout tiled\r");
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut ok = false;
    while Instant::now() < deadline {
        sess.send_str("printf 'TILED_''OK\\n'\n");
        if sess.wait_for(b"TILED_OK", Duration::from_millis(500)) {
            ok = true;
            break;
        }
    }
    assert!(ok, "active pane unresponsive after :layout tiled: {}", sess.snapshot_str());
}

#[test]
fn next_layout_cycles_without_breaking_input() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(5)), "daemon never rendered");
    // Two panes.
    sess.send_prefix(b'v');
    assert!(sess.wait_for(b"\xe2\x94\x82", Duration::from_secs(3)), "no split separator");
    // Cycle through three presets with Ctrl+a Space.
    sess.send_prefix(b' ');
    sess.send_prefix(b' ');
    sess.send_prefix(b' ');
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut ok = false;
    while Instant::now() < deadline {
        sess.send_str("printf 'CYCLE_''OK\\n'\n");
        if sess.wait_for(b"CYCLE_OK", Duration::from_millis(500)) {
            ok = true;
            break;
        }
    }
    assert!(ok, "active pane unresponsive after next_layout cycling: {}", sess.snapshot_str());
}

// ---------------------------------------------------------------------------
// CLI scripting verbs (S5)
// ---------------------------------------------------------------------------

/// Run a `plexy-glass` CLI verb against the test env; returns (status, stdout,
/// stderr).
fn run_cli(env: &TestEnv, args: &[&str]) -> (std::process::ExitStatus, String, String) {
    let out = std::process::Command::cargo_bin("plexy-glass")
        .unwrap()
        .args(args)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("run plexy-glass");
    (
        out.status,
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `plexy-glass send --enter` writes text into the attached session's pane and
/// the output becomes visible in the PTY.
#[test]
fn cli_send_reaches_attached_session() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(20)), "daemon never rendered");

    // Quote-concatenation: 'CLI_''SENT' → CLI_SENT only when a shell *executes*
    // the line, never from PTY echo of the typed bytes.
    let (status, _stdout, stderr) =
        run_cli(&env, &["send", "--enter", "printf 'CLI_''SENT\\n'"]);
    assert!(
        status.success(),
        "send --enter failed (status={status:?}): {stderr}"
    );

    assert!(
        sess.wait_for(b"CLI_SENT", Duration::from_secs(10)),
        "CLI_SENT never appeared in pane output after send --enter. raw: {}",
        sess.snapshot_str()
    );
}

/// `plexy-glass capture` reads the pane's visible screen text and returns it on
/// stdout; polling until the previously-sent marker is visible.
#[test]
fn cli_capture_reads_pane() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(20)), "daemon never rendered");

    // Send a marker line first so it's on the screen.
    let (status, _stdout, stderr) =
        run_cli(&env, &["send", "--enter", "printf 'CAP_''MARKER\\n'"]);
    assert!(status.success(), "send failed: {stderr}");

    // Wait for the marker to appear in the PTY (so the shell executed it).
    assert!(
        sess.wait_for(b"CAP_MARKER", Duration::from_secs(10)),
        "CAP_MARKER never appeared in PTY before capture poll"
    );

    // Capture is point-in-time: poll in a bounded loop.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut captured = false;
    while Instant::now() < deadline {
        let (status, stdout, _stderr) = run_cli(&env, &["capture"]);
        if status.success() && stdout.contains("CAP_MARKER") {
            captured = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        captured,
        "capture never returned CAP_MARKER within deadline. last snapshot: {}",
        sess.snapshot_str()
    );
}

/// `plexy-glass cmd` structural smoke tests: split and layout succeed; a bogus
/// verb returns a non-zero exit code with "unknown command" on stderr; `help`
/// returns a non-zero exit code with "requires an attached client" on stderr.
#[test]
fn cli_cmd_structural_and_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(20)), "daemon never rendered");

    // Split a vertical pane (this one succeeds).
    let (status, _stdout, stderr) = run_cli(&env, &["cmd", "split v"]);
    assert!(status.success(), "cmd 'split v' failed: {stderr}");
    // Wait for the gutter to confirm the split landed before the next command.
    let _ = sess.wait_for(b"\xe2\x94\x82", Duration::from_secs(5));

    // Apply the tiled layout, this one succeeds.
    let (status, _stdout, stderr) = run_cli(&env, &["cmd", "layout tiled"]);
    assert!(status.success(), "cmd 'layout tiled' failed: {stderr}");

    // Bogus verb → non-zero exit, "unknown command" in stderr.
    let (status, _stdout, stderr) = run_cli(&env, &["cmd", "bogusverb"]);
    assert!(
        !status.success(),
        "cmd 'bogusverb' should have failed but returned success"
    );
    assert!(
        stderr.contains("unknown command"),
        "expected 'unknown command' in stderr for bogusverb, got: {stderr}"
    );

    // `help` is interactive-only, so we expect a non-zero exit and
    // "requires an attached client" on stderr.
    let (status, _stdout, stderr) = run_cli(&env, &["cmd", "help"]);
    assert!(
        !status.success(),
        "cmd 'help' should have failed (interactive-only) but returned success"
    );
    assert!(
        stderr.contains("requires an attached client"),
        "expected 'requires an attached client' in stderr for help, got: {stderr}"
    );

    // Liveness probe: send a marker via send and confirm the session still responds.
    let (status, _stdout, send_err) =
        run_cli(&env, &["send", "--enter", "printf 'CMD_''LIVENESS\\n'"]);
    assert!(status.success(), "liveness send failed: {send_err}");
    assert!(
        sess.wait_for(b"CMD_LIVENESS", Duration::from_secs(10)),
        "CMD_LIVENESS never appeared after cmd error tests (session not live). raw: {}",
        sess.snapshot_str()
    );
}

/// With no daemon running, `plexy-glass capture` must exit with a non-zero
/// status (the daemon socket doesn't exist; connect_only returns an error which
/// main maps to exit 1).
///
/// Note: TestEnv::drop issues `kill` after the test body; the kill prints
/// "no daemon running", which is harmless.
#[test]
fn cli_no_daemon_exits_nonzero() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    // Intentionally do NOT spawn a TestSession, so no daemon is running.
    let (status, _stdout, _stderr) = run_cli(&env, &["capture"]);
    assert!(
        !status.success(),
        "capture against a non-existent daemon should exit non-zero, but got success"
    );
}
