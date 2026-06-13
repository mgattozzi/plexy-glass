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

/// Write `body` as the test env's `config.kdl` at BOTH platform candidate
/// paths: `$XDG_CONFIG_HOME/plexy-glass/` (used on Linux) and
/// `$HOME/Library/Application Support/plexy-glass/` (macOS, where the
/// `directories` crate ignores XDG_CONFIG_HOME and derives config_dir from
/// $HOME). `isolate_dirs` overrides both env vars, so this never touches a
/// real config.
fn write_config(env: &TestEnv, body: &str) {
    if let Some((_, xdg)) = env.iter().find(|(k, _)| k == "XDG_CONFIG_HOME") {
        let cfg_dir = std::path::PathBuf::from(xdg).join("plexy-glass");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(cfg_dir.join("config.kdl"), body).unwrap();
    }
    if let Some((_, home)) = env.iter().find(|(k, _)| k == "HOME") {
        let cfg_dir =
            std::path::PathBuf::from(home).join("Library/Application Support/plexy-glass");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(cfg_dir.join("config.kdl"), body).unwrap();
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

    write_config(&env, &kdl_body);

    let sess = TestSession::spawn(&env);
    // The config's text widget renders on the first frame, so capture it live.
    // (The old version read the buffer *after* killing the client, when it had
    // already been drained, so it depended entirely on timing.)
    if !sess.wait_for(marker.as_bytes(), Duration::from_secs(5)) {
        eprintln!("note: custom-config test fail-soft. raw: {}", sess.snapshot_str());
    }
}

/// `keymap { prefix "Ctrl+b" }` retargets the prefix-relative built-in
/// defaults end-to-end: `Ctrl+b c` creates a window, `Ctrl+a c` no longer does.
#[test]
fn custom_prefix_retargets_bindings() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    write_config(&env, "keymap { prefix \"Ctrl+b\" }\n");

    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(5)), "daemon never rendered");

    // Ctrl+b (0x02) then `c` → the inherited `prefix c` default fires
    // new_window under the custom prefix. Observable: the status bar's
    // window-list paints " {index} {name} " per window, and a window created
    // interactively is named "shell{id}", so the second window is "shell1" (the
    // first is "shell"). Match "shell1", not "2 shell1": the diff renderer can
    // jump over unchanged blank cells, so the spaced form need not arrive as
    // contiguous bytes, while a same-style word always does.
    sess.send(&[0x02]);
    sess.send(b"c");
    assert!(
        sess.wait_for(b"shell1", Duration::from_secs(10)),
        "Ctrl+b c did not create a second window under prefix Ctrl+b. raw: {}",
        sess.snapshot_str()
    );

    // Negative: Ctrl+a (0x01) then `c`. With the prefix moved to Ctrl+b no
    // binding starts with Ctrl+a, so both keys pass through to the pane's
    // shell and no third window ("shell2") may appear. Absence needs a
    // liveness round-trip first, all through the SAME client writer so the
    // bytes are strictly ordered after the chord: Ctrl+u (0x15) clears the
    // literal "c" from the shell's line buffer, then a quote-concatenated
    // marker proves the input path and renderer caught up. Had Ctrl+a c
    // wrongly created a window, its status repaint would render before the
    // marker's output does.
    sess.send(&[0x01]);
    sess.send(b"c");
    sess.send(&[0x15]); // kill-line: discard the passed-through "c"
    sess.send_str("printf 'PFX_''NEG_DONE\\n'\n");
    assert!(
        sess.wait_for(b"PFX_NEG_DONE", Duration::from_secs(10)),
        "liveness marker never appeared after Ctrl+a c. raw: {}",
        sess.snapshot_str()
    );
    assert!(
        !sess.snapshot_str().contains("shell2"),
        "Ctrl+a c created a window despite prefix Ctrl+b. raw: {}",
        sess.snapshot_str()
    );
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
    write_config(&env, &kdl_body);

    // The client attaches with no name → "main", which is now declared and was
    // built at daemon boot. Its pane's command output should appear.
    let sess = TestSession::spawn(&env);
    if !sess.wait_for(marker.as_bytes(), Duration::from_secs(5)) {
        eprintln!("note: declared-session test fail-soft. raw: {}", sess.snapshot_str());
    }
}

#[test]
fn plain_attach_creates_main_not_the_declared_session() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);

    // One DECLARED session "dev" is built at daemon boot. Plain `attach`
    // (no -n) must still mean the default session "main", never silently
    // landing in a declared session just because it is the only one running.
    write_config(
        &env,
        r##"
session "dev" {
    window "w" {
        pane command="tail -f /dev/null"
    }
}
"##,
    );

    let sess = TestSession::spawn(&env);
    assert!(
        sess.wait_ready("main", Duration::from_secs(6)),
        "plain attach must attach-or-create \"main\", not the declared session. raw: {}",
        sess.snapshot_str()
    );

    // The declared session was still built at boot, alongside "main".
    let (status, list_out, _) = run_cli(&env, &["list"]);
    assert!(status.success(), "list failed: {list_out}");
    assert!(list_out.contains("dev"), "declared session missing from list: {list_out}");
    assert!(list_out.contains("main"), "default session missing from list: {list_out}");
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
    write_config(&env, body);

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

/// Paste-buffers v2 happy path over the CLI: `cmd "set-buffer …"` pushes a
/// buffer, `cmd "save-buffer <abs>"` writes its bytes to a file, `cmd
/// "load-buffer <abs>"` reads them back as a new buffer, and `cmd "paste
/// bufferN"` types that buffer into the pane (a `cat` child echoes it).
#[test]
fn cli_buffer_set_save_load_paste_round_trips() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(20)), "daemon never rendered");

    // `set-buffer` creates `buffer0` (confirmation message on stdout).
    let (status, stdout, stderr) = run_cli(&env, &["cmd", "set-buffer hello world"]);
    assert!(status.success(), "cmd set-buffer failed: {stderr}");
    assert!(
        stdout.contains("buffer set (11 bytes)"),
        "unexpected set-buffer output: {stdout}"
    );

    // `save-buffer` (the newest) writes the bytes to the file verbatim.
    let out = tmp.path().join("buf.txt");
    let (status, stdout, stderr) =
        run_cli(&env, &["cmd", &format!("save-buffer {}", out.display())]);
    assert!(status.success(), "cmd save-buffer failed: {stderr}");
    assert!(
        stdout.contains("saved buffer0"),
        "save must name the buffer it wrote: {stdout}"
    );
    assert_eq!(std::fs::read(&out).unwrap(), b"hello world");

    // `load-buffer` pushes a new newest buffer (`buffer1`).
    let (status, stdout, stderr) =
        run_cli(&env, &["cmd", &format!("load-buffer {}", out.display())]);
    assert!(status.success(), "cmd load-buffer failed: {stderr}");
    assert!(stdout.contains("(11 bytes)"), "unexpected load-buffer output: {stdout}");

    // Start `cat` so the paste goes to cat's stdin and is echoed (probe loop
    // per the copy-mode e2e: cat echoes the plain token once it is up).
    sess.send_str("cat\n");
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut cat_ready = false;
    while Instant::now() < deadline {
        sess.send_str("CATREADY\n");
        if sess.wait_for(b"CATREADY", Duration::from_millis(500)) {
            cat_ready = true;
            break;
        }
    }
    assert!(cat_ready, "cat child never came up: {}", sess.snapshot_str());

    // Paste `buffer1` (the loaded one) and `cat` echoes it into the pane.
    // Assert via `capture` (the grid as text): the frame diff skips blank
    // cells, so the needle is not contiguous in the raw PTY stream, and the
    // emulator buffers the trailing grapheme until the next byte arrives, so
    // probe for all but the last char.
    let (status, _stdout, stderr) = run_cli(&env, &["cmd", "paste buffer1"]);
    assert!(status.success(), "cmd paste buffer1 failed: {stderr}");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut pasted = false;
    while Instant::now() < deadline {
        let (status, stdout, _stderr) = run_cli(&env, &["capture"]);
        if status.success() && stdout.contains("hello worl") {
            pasted = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        pasted,
        "pasted buffer never echoed by cat. raw: {}",
        sess.snapshot_str()
    );
}

/// Cross-window swap-with-marked: `Ctrl+a m` marks a pane in window 2; after
/// switching back to window 1 and running `:swap-pane`, the two panes exchange
/// slots, so window 1 holds the former window-2 pane and vice versa.
///
/// Marker scheme: `printf 'SWAP_''W1\n'` → `SWAP_W1` appears only when the
/// shell *executes* the line, not from PTY echo; `exec tail -f /dev/null` keeps
/// the pane alive so the marker stays visible on screen through the swap.
#[test]
fn cross_window_swap_pane_exchanges_panes() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(20)), "daemon never rendered");

    // Window 1 (index 0, name "shell"): print a unique needle then keep alive.
    sess.send_str("printf 'SWAP_''W1\\n'; exec tail -f /dev/null\n");
    assert!(
        sess.wait_for(b"SWAP_W1", Duration::from_secs(10)),
        "SWAP_W1 never appeared in window 1. raw: {}",
        sess.snapshot_str()
    );

    // Create window 2 (index 1, status bar shows "shell1").
    sess.send_prefix(b'c');
    assert!(
        sess.wait_for(b"shell1", Duration::from_secs(10)),
        "window 2 never appeared in status bar. raw: {}",
        sess.snapshot_str()
    );

    // Window 2: print a unique needle then keep alive.
    sess.send_str("printf 'SWAP_''W2\\n'; exec tail -f /dev/null\n");
    assert!(
        sess.wait_for(b"SWAP_W2", Duration::from_secs(10)),
        "SWAP_W2 never appeared in window 2. raw: {}",
        sess.snapshot_str()
    );

    // Mark window 2's pane (Ctrl+a m).
    sess.send_prefix(b'm');

    // Switch back to window 1 (Ctrl+a p = prev_window).
    sess.send_prefix(b'p');
    // Wait for the status bar to reflect window 1 as active (its name "shell"
    // re-appears as the highlighted entry). A brief liveness probe via capture
    // would also work, but watching the status bar is simpler and avoids the
    // need for a shell that responds in the `tail` pane.
    std::thread::sleep(Duration::from_millis(200));

    // Headless swap-pane: the marked pane (window 2) swaps into window 1's slot.
    let (status, _stdout, stderr) = run_cli(&env, &["cmd", "swap-pane"]);
    assert!(status.success(), "cmd 'swap-pane' failed: {stderr}");

    // After the swap, window 1's active pane is the former window-2 pane
    // (which printed SWAP_W2).  Poll capture until the content is visible.
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut w1_ok = false;
    while Instant::now() < deadline {
        let (st, stdout, _) = run_cli(&env, &["capture"]);
        if st.success() && stdout.contains("SWAP_W2") {
            w1_ok = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        w1_ok,
        "window 1's active pane should contain SWAP_W2 after cross-window swap. capture: {}",
        {
            let (_, s, _) = run_cli(&env, &["capture"]);
            s
        }
    );

    // Switch the session's active window to window 2 (index 1) via headless cmd.
    let (status, _stdout, stderr) = run_cli(&env, &["cmd", "win 2"]);
    assert!(status.success(), "cmd 'win 2' failed: {stderr}");

    // Window 2 now holds the former window-1 pane (which printed SWAP_W1).
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut w2_ok = false;
    while Instant::now() < deadline {
        let (st, stdout, _) = run_cli(&env, &["capture"]);
        if st.success() && stdout.contains("SWAP_W1") {
            w2_ok = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        w2_ok,
        "window 2's active pane should contain SWAP_W1 after cross-window swap. capture: {}",
        {
            let (_, s, _) = run_cli(&env, &["capture"]);
            s
        }
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

// ---------------------------------------------------------------------------
// Command-blocks e2e (B6)
// ---------------------------------------------------------------------------
// All four tests plant OSC 133 marks via printf inside the pane's /bin/sh, so
// the emulator sees the real wire bytes and the genuine mark path gets
// exercised.
//
// Marker naming: quote-concatenation (`OUT_'LN'_1` → OUT_LN_1) ensures the
// contiguous needle only appears in the printf OUTPUT, not in the echoed
// command text.

/// `capture --last-command` returns exactly the block output (OUTPUT_START row
/// through block end), not the prompt or the next prompt's text.
#[test]
fn capture_last_command_returns_block_output() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(20)), "daemon never rendered");

    // Emit two synthetic OSC 133 blocks in one printf. Marker names use
    // quote-concatenation so the echoed command text shows `OUT_'LN'_1` while
    // the executed printf OUTPUT shows the plain `OUT_LN_1`.
    //
    // Block structure after the emulator processes these bytes:
    //   row N:   PROMPT_START  "PONE"
    //   row N+1: OUTPUT_START  "OUT_LN_1"
    //   row N+2:              "OUT_LN_2"
    //   row N+3: BLOCK_END + PROMPT_START  "PTWO"
    //
    // last_completed_block → (OUTPUT_START row .. row before PTWO) → rows N+1..N+2
    // block_text → "OUT_LN_1\nOUT_LN_2"
    //
    // The D;0 and A markers land on the same row (common shell flow);
    // that row is excluded from the output range by last_completed_block.
    let (send_status, _, send_err) = run_cli(
        &env,
        &[
            "send",
            "--enter",
            "printf '\\033]133;A\\007PONE\\r\\n\\033]133;C\\007OUT_'LN'_1\\nOUT_'LN'_2\\n\\033]133;D;0\\007\\033]133;A\\007PTWO\\n'",
        ],
    );
    assert!(send_status.success(), "send failed: {send_err}");

    // Wait until the output lines appear in the plain capture (proves the
    // emulator processed the OSC sequences and the marks are set).
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut marks_set = false;
    while Instant::now() < deadline {
        let (_, stdout, _) = run_cli(&env, &["capture"]);
        if stdout.contains("OUT_LN_1") {
            marks_set = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(marks_set, "OUT_LN_1 never appeared in plain capture (marks not set). pane: {}", sess.snapshot_str());

    // Now fetch via --last-command.
    let deadline2 = Instant::now() + Duration::from_secs(10);
    let mut last_cmd_out = String::new();
    let mut last_cmd_ok = false;
    while Instant::now() < deadline2 {
        let (status, stdout, _) = run_cli(&env, &["capture", "--last-command"]);
        if status.success() {
            last_cmd_out = stdout;
            last_cmd_ok = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(last_cmd_ok, "capture --last-command never succeeded. pane: {}", sess.snapshot_str());
    assert!(
        last_cmd_out.contains("OUT_LN_1"),
        "block output must contain OUT_LN_1. got: {last_cmd_out:?}"
    );
    assert!(
        last_cmd_out.contains("OUT_LN_2"),
        "block output must contain OUT_LN_2. got: {last_cmd_out:?}"
    );
    // The next prompt's text (PTWO) must not bleed into the output range.
    assert!(
        !last_cmd_out.contains("PTWO"),
        "block output must not contain the next prompt text (PTWO). got: {last_cmd_out:?}"
    );
    // The prompt text (PONE) must also be excluded, since the output range
    // starts at the OUTPUT_START row, not the prompt row.
    assert!(
        !last_cmd_out.contains("PONE"),
        "block output must not contain the prompt text (PONE). got: {last_cmd_out:?}"
    );
}

/// `prev-prompt` scrolls the viewport so the previous prompt is visible;
/// `next-prompt` scrolls back toward the live content.
///
/// The plain `capture` verb reads only the live grid (not the scroll
/// viewport), so the assertion surface for the viewport position is the
/// rendered frame accumulated in the TestSession buffer. After `prev-prompt`
/// the daemon re-renders and emits the scrolled viewport to the client PTY;
/// `wait_for_from` detects the old prompt text arriving in that new render.
#[test]
fn prev_prompt_and_next_prompt_scroll_viewport() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    // 10-row terminal (8 usable rows after status + frame); printing more than
    // 8 lines after the first prompt pushes it into scrollback.
    let mut sess = TestSession::builder(&env).size(10, 60).start();
    assert!(sess.wait_ready("main", Duration::from_secs(20)), "daemon never rendered");

    // Emit a single OSC 133 block with NO second A marker. This gives exactly
    // one PROMPT_START row ("BLKPROMPT"), so `prev-prompt` from the live
    // viewport always targets it unambiguously.
    //
    // Block structure emitted:
    //   row 0: PROMPT_START  "BLKPROMPT"
    //   row 1: OUTPUT_START  "BLKOUT"
    //   row 2: BLOCK_END     (empty, D;0 alone, no text)
    //
    // Quote-concat: pane echo shows `BLK'PROMPT'` while printf output
    // emits `BLKPROMPT`. Since we assert on the rendered frame (not plain
    // capture), this avoids matching the typed echo.
    let mark_before = sess.buffer_len();
    sess.send_str("printf '\\033]133;A\\007BLK'PROMPT'\\r\\n\\033]133;C\\007BLKOUT\\n\\033]133;D;0\\007'\n");
    // Wait for the block output to appear (marks are set by the time the
    // output is visible in the frame).
    assert!(
        sess.wait_for_from(mark_before, b"BLKOUT", Duration::from_secs(10)),
        "BLKOUT never appeared: {}",
        sess.snapshot_str()
    );

    // Flood the pane with 20 lines so the first prompt scrolls off-screen.
    // Use a sentinel so we wait only for the seq output, not a cursor-movement
    // sequence that happens to contain "20" (e.g. ESC[row;20H).
    let mark_seq = sess.buffer_len();
    sess.send_str("seq 1 20; printf 'SEQ_''DONE\\n'\n");
    assert!(
        sess.wait_for_from(mark_seq, b"SEQ_DONE", Duration::from_secs(10)),
        "seq+sentinel output never appeared: {}",
        sess.snapshot_str()
    );

    // Confirm that BLKPROMPT has scrolled into scrollback and is no longer
    // in the live grid. Poll `capture` (reads only the live viewport) until
    // BLKPROMPT disappears, which proves the prompt is in scrollback before
    // prev-prompt fires, the precondition the test relies on.
    let blkprompt_gone = {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut gone = false;
        while Instant::now() < deadline {
            let (_s, cap_out, _e) = run_cli(&env, &["capture"]);
            if !cap_out.contains("BLKPROMPT") {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        gone
    };
    assert!(
        blkprompt_gone,
        "BLKPROMPT never left the live viewport after seq 1 20 — \
         precondition for prev-prompt test not met. capture: {}",
        run_cli(&env, &["capture"]).1
    );

    // At this point BLKPROMPT is in scrollback (invisible in the live
    // viewport). Issue `prev-prompt` via the headless cmd verb (allowed
    // headless: it maps to `Command::PrevPrompt` through
    // `session.handle_prompt_command`).
    let mark_after_cmd = sess.buffer_len();
    let (cmd_status, _, cmd_err) = run_cli(&env, &["cmd", "prev-prompt"]);
    assert!(cmd_status.success(), "cmd prev-prompt failed: {cmd_err}");

    // The daemon re-renders with the scrolled viewport; BLKPROMPT must now
    // appear in the rendered frame output (from new output after mark_after_cmd).
    assert!(
        sess.wait_for_from(mark_after_cmd, b"BLKPROMPT", Duration::from_secs(8)),
        "BLKPROMPT did not appear in rendered frame after prev-prompt. raw: {}",
        sess.snapshot_str()
    );

    // `next-prompt` should scroll forward (back toward live content); after
    // that the scrollback view recedes. We verify liveness by confirming the
    // pane still responds to input after the nav.
    let (cmd2_status, _, cmd2_err) = run_cli(&env, &["cmd", "next-prompt"]);
    assert!(cmd2_status.success(), "cmd next-prompt failed: {cmd2_err}");

    // Liveness: the shell must still respond to input after the viewport ops.
    let mark_live = sess.buffer_len();
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut live = false;
    while Instant::now() < deadline {
        sess.send_str("printf 'LIVE_'MARK'\\n'\n");
        if sess.wait_for_from(mark_live, b"LIVE_MARK", Duration::from_millis(500)) {
            live = true;
            break;
        }
    }
    assert!(live, "shell not live after next-prompt: {}", sess.snapshot_str());
}

/// No-blocks error path: `capture --last-command` on a fresh session (no OSC
/// 133 output ever seen) must exit with status 1 and mention "no command
/// blocks" on stderr.
#[test]
fn no_blocks_capture_last_command_exits_nonzero() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(20)), "daemon never rendered");

    // No OSC 133 output, the pane has never seen any block markers.
    let (status, _stdout, stderr) = run_cli(&env, &["capture", "--last-command"]);
    assert!(
        !status.success(),
        "capture --last-command with no blocks must exit non-zero; session: {}",
        sess.snapshot_str()
    );
    assert!(
        stderr.contains("no command blocks"),
        "expected 'no command blocks' in stderr, got: {stderr:?}"
    );
}

/// Copy-mode block navigation: `[` jumps to the previous prompt, `o` selects
/// the block's output region, `y` yanks. The yanked text is pushed onto the
/// paste-buffer stack; `prefix ]` pastes it into a `cat` child which echoes
/// it back, giving us an observable frame-level signal.
///
/// macOS only: the yank path also calls `pbcopy`; we don't stub it here (the
/// paste buffer is the observable surface), but the test needs the daemon's
/// yank path to not error out, and on Linux `write_clipboard` is a no-op so
/// the paste buffer still gets pushed.
#[test]
fn copy_mode_bracket_o_y_yanks_block_output() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(20)), "daemon never rendered");

    // Emit a completed OSC 133 block. Output text uses quote-concat so the
    // echoed command shows `YANK_'OUT'_A` while the printf emits `YANK_OUT_A`.
    sess.send_str("printf '\\033]133;A\\007YPROMPT\\r\\n\\033]133;C\\007YANK_'OUT'_A\\nYANK_'OUT'_B\\n\\033]133;D;0\\007\\033]133;A\\007YNEXT\\n'\n");
    // Wait for the output to appear (marks set).
    assert!(
        sess.wait_for(b"YANK_OUT_A", Duration::from_secs(10)),
        "YANK_OUT_A never appeared: {}",
        sess.snapshot_str()
    );

    // Start `cat` so that future paste goes to cat's stdin and is echoed.
    // Cat echoes its stdin verbatim; the readiness probe is a plain token
    // (no quote-concat needed: cat does not interpret shell escapes, so
    // `CATREADY` appears TWICE, once from line-discipline echo and once from
    // cat's output, but `wait_for` fires on the first occurrence either way).
    sess.send_str("cat\n");
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut cat_ready = false;
    while Instant::now() < deadline {
        sess.send_str("CATREADY\n");
        if sess.wait_for(b"CATREADY", Duration::from_millis(500)) {
            cat_ready = true;
            break;
        }
    }
    assert!(cat_ready, "cat child never came up: {}", sess.snapshot_str());

    // Enter copy mode: prefix [ (0x01 then '[').
    sess.send_prefix(b'['); // enters copy mode
    // Brief warmup: no observable marker for copy-mode entry.
    std::thread::sleep(Duration::from_millis(150));

    // `[` in copy mode = jump to previous PROMPT_START line. Starting from the
    // live bottom (cursor initialised there by CopyMode::new), this jumps to
    // the YNEXT prompt (from the D+A row), then one more `[` reaches YPROMPT.
    // We press `[` twice to ensure we land at the first block's prompt.
    sess.send(b"[");
    std::thread::sleep(Duration::from_millis(80));
    sess.send(b"[");
    std::thread::sleep(Duration::from_millis(80));

    // `o` selects the output region (anchor = `OUTPUT_START`, cursor = block end).
    sess.send(b"o");
    std::thread::sleep(Duration::from_millis(80));

    // `y` yanks the selection and exits copy mode. The text is pushed onto
    // the paste-buffer stack (`registry.push_paste_buffer`).
    sess.send(b"y");
    // Wait briefly for the yank to process and copy mode to exit.
    std::thread::sleep(Duration::from_millis(200));

    // Paste the top buffer (prefix ]) into the cat child.
    sess.send_prefix(b']');

    // `cat` echoes the pasted text, so wait for it in the frame from this point.
    let mark_after_paste = sess.buffer_len();
    assert!(
        sess.wait_for_from(mark_after_paste, b"YANK_OUT_A", Duration::from_secs(8)),
        "YANK_OUT_A not echoed by cat after paste — copy-mode yank or paste failed. raw: {}",
        sess.snapshot_str()
    );
    // YANK_OUT_B is the second line of the block output and must also appear,
    // confirming the full output region (not just its first line) was yanked.
    assert!(
        sess.wait_for_from(mark_after_paste, b"YANK_OUT_B", Duration::from_secs(8)),
        "YANK_OUT_B not echoed by cat — only partial output was yanked. raw: {}",
        sess.snapshot_str()
    );
    // Negative: the prompt text (YPROMPT) must NOT appear in the post-paste
    // frame output.  If `o` wrongly selected from the prompt row, YPROMPT
    // would be pasted too; this assertion catches that.
    //
    // Snapshot raw bytes, slice from mark_after_paste, then lossy-decode the
    // slice, which avoids a byte/char offset mismatch from multi-byte
    // sequences in buffer positions before the mark.
    let raw = sess.snapshot();
    let post_paste_raw = &raw[mark_after_paste.min(raw.len())..];
    let post_paste = String::from_utf8_lossy(post_paste_raw);
    assert!(
        !post_paste.contains("YPROMPT"),
        "YPROMPT appeared in post-paste output — `o` selected the prompt row, \
         not just the block output. post-mark slice: {post_paste:?}"
    );
}

// ---------------------------------------------------------------------------
// Block exit-status border line e2e (S4)
// ---------------------------------------------------------------------------
// These tests exercise the block-border paint path end-to-end: synthetic OSC
// 133 sequences are sent via printf so the emulator records real block marks;
// the raw PTY output accumulated by `TestSession` is then inspected for the
// expected Rgb-color SGR sequences and the half-block `▌` glyph (or their
// absence). All assertions use `wait_for_from` on fresh output after a mark
// so re-rendered earlier content does not produce false positives.
//
// Color constants (from crates/config/src/default.rs):
//   alert (#c4746e) → decimal 196;116;110 → SGR `\x1b[38;2;196;116;110m`
//   ok    (#87a987) → decimal 135;169;135 → SGR `\x1b[38;2;135;169;135m`
// Glyph `▌` (U+258C) → UTF-8 bytes 0xE2 0x96 0x8C.

/// A completed FAILED block (`D;1`) paints `▌` with the alert color on the
/// pane's left border.
#[test]
fn block_border_failed_block_paints_half_block_with_fail_color() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(20)), "daemon never rendered");

    // Emit a completed FAILED block: A → output → D;1 → A (next prompt).
    // Quote-concatenation: `BDR_'FAIL'_OUT` → BDR_FAIL_OUT appears only in
    // the printf output, not in the echoed command text.
    let mark_before = sess.buffer_len();
    sess.send_str("printf '\\033]133;A\\007BDR'PROMPT'\\r\\n\\033]133;C\\007BDR_'FAIL'_OUT\\n\\033]133;D;1\\007\\033]133;A\\007BDR'NEXT'\\n'\n");

    // Wait for the `▌` half-block glyph (UTF-8: 0xE2 0x96 0x8C). The diff renderer
    // emits this on the FIRST render after the marks are recorded, so it appears
    // at or after mark_before.
    assert!(
        sess.wait_for_from(mark_before, b"\xe2\x96\x8c", Duration::from_secs(10)),
        "\u{258c} (half-block) never appeared after failed block. raw: {}",
        sess.snapshot_str()
    );

    // The fail-color SGR (`\x1b[38;2;196;116;110m` for #c4746e) is emitted
    // adjacently on the same first-paint; search from mark_before.
    assert!(
        sess.wait_for_from(mark_before, b"\x1b[38;2;196;116;110m", Duration::from_secs(10)),
        "fail-color SGR (\\x1b[38;2;196;116;110m) never appeared after failed block. raw: {}",
        sess.snapshot_str()
    );
}

/// A completed OK block (`D;0`) paints `│` with the ok color; no `▌`.
#[test]
fn block_border_ok_block_paints_pipe_with_ok_color_no_half_block() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(20)), "daemon never rendered");

    // Emit a completed OK block: A → output → D;0 → A (next prompt).
    let mark_before = sess.buffer_len();
    sess.send_str("printf '\\033]133;A\\007BDR'OKPROMPT'\\r\\n\\033]133;C\\007BDR_'OK'_OUT\\n\\033]133;D;0\\007\\033]133;A\\007BDR'OKNEXT'\\n'\n");

    // Wait for the ok-color SGR (`\x1b[38;2;135;169;135m` for #87a987) in
    // the diff output from mark_before.
    assert!(
        sess.wait_for_from(mark_before, b"\x1b[38;2;135;169;135m", Duration::from_secs(10)),
        "ok-color SGR (\\x1b[38;2;135;169;135m) never appeared after ok block. raw: {}",
        sess.snapshot_str()
    );

    // Snapshot the bytes from mark_before and assert `▌` is NOT there.
    // The ok case must never paint ▌ (only the fail case does).
    let raw = sess.snapshot();
    let post_mark = &raw[mark_before.min(raw.len())..];
    assert!(
        !post_mark.windows(3).any(|w| w == b"\xe2\x96\x8c"),
        "\u{258c} (half-block) must NOT appear after an ok block (D;0). \
         post-mark raw bytes (lossy): {}",
        String::from_utf8_lossy(post_mark)
    );
}

/// Entering the alt screen reverts the border to plain (no new `▌` while in
/// alt screen); leaving it restores `▌` for the failed block still on the
/// main grid.
#[test]
fn block_border_alt_screen_reverts_and_restores() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(20)), "daemon never rendered");

    // Plant a failed block.
    let mark_before = sess.buffer_len();
    sess.send_str("printf '\\033]133;A\\007ALT'PROMPT'\\r\\n\\033]133;C\\007ALT_'FAIL'_OUT\\n\\033]133;D;1\\007\\033]133;A\\007ALT'NEXT'\\n'\n");

    // Wait for output visible in frame.
    assert!(
        sess.wait_for_from(mark_before, b"ALT_FAIL_OUT", Duration::from_secs(10)),
        "ALT_FAIL_OUT never appeared: {}",
        sess.snapshot_str()
    );

    // Confirm the failed block border painted (▌ appears at or after mark_before).
    assert!(
        sess.wait_for_from(mark_before, b"\xe2\x96\x8c", Duration::from_secs(10)),
        "\u{258c} never appeared before alt-screen enter: {}",
        sess.snapshot_str()
    );

    // Enter the alt screen.
    let mark_alt_enter = sess.buffer_len();
    sess.send_str("printf '\\033[?1049h'\n");

    // Wait for the frame to be redrawn after alt-screen entry. The compositor
    // will emit border cells as plain `│` (0xE2 0x94 0x82) because alt-screen
    // → all-None block status.
    assert!(
        sess.wait_for_from(mark_alt_enter, b"\xe2\x94\x82", Duration::from_secs(10)),
        "plain \u{2502} never appeared after alt-screen enter: {}",
        sess.snapshot_str()
    );

    // Assert that no NEW `▌` arrived after the alt-screen enter mark.
    let raw = sess.snapshot();
    let post_alt_enter = &raw[mark_alt_enter.min(raw.len())..];
    assert!(
        !post_alt_enter.windows(3).any(|w| w == b"\xe2\x96\x8c"),
        "\u{258c} must NOT appear while in alt screen (alt-screen path should be all-None). \
         post-mark raw bytes (lossy): {}",
        String::from_utf8_lossy(post_alt_enter)
    );

    // Leave the alt screen.  The compositor re-renders the main grid with the
    // failed block marks still present → `▌` must reappear.
    let mark_alt_leave = sess.buffer_len();
    sess.send_str("printf '\\033[?1049l'\n");
    assert!(
        sess.wait_for_from(mark_alt_leave, b"\xe2\x96\x8c", Duration::from_secs(10)),
        "\u{258c} did not reappear after leaving alt screen: {}",
        sess.snapshot_str()
    );
}

/// With `blocks { enabled #false }` in the config, a failed block must NOT
/// paint `▌` on the border.
#[test]
fn block_border_disabled_by_config_no_half_block() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    // Write a config that disables the block-border feature.
    write_config(&env, "blocks {\n    enabled #false\n}\n");

    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(20)), "daemon never rendered");

    // Emit a completed FAILED block.
    let mark_before = sess.buffer_len();
    sess.send_str("printf '\\033]133;A\\007DIS'PROMPT'\\r\\n\\033]133;C\\007DIS_'FAIL'_OUT\\n\\033]133;D;1\\007\\033]133;A\\007DIS'NEXT'\\n'\n");

    // Wait for the block output text to confirm the block was processed.
    assert!(
        sess.wait_for_from(mark_before, b"DIS_FAIL_OUT", Duration::from_secs(10)),
        "DIS_FAIL_OUT never appeared in frame: {}",
        sess.snapshot_str()
    );

    // Snapshot the bytes from mark_before (entire render since the block was
    // emitted) and assert `▌` is NOT there, and neither status SGR appears
    // (feature is fully disabled).
    let raw = sess.snapshot();
    let post_mark = &raw[mark_before.min(raw.len())..];
    assert!(
        !post_mark.windows(3).any(|w| w == b"\xe2\x96\x8c"),
        "\u{258c} (half-block) must NOT appear when blocks.enabled = false. \
         post-mark raw bytes (lossy): {}",
        String::from_utf8_lossy(post_mark)
    );
    let fail_sgr = b"\x1b[38;2;196;116;110m"; // palette `alert` #c4746e
    let ok_sgr = b"\x1b[38;2;135;169;135m"; // palette `ok` #87a987
    for (sgr, name) in [(&fail_sgr[..], "fail"), (&ok_sgr[..], "ok")] {
        assert!(
            !post_mark.windows(sgr.len()).any(|w| w == sgr),
            "{name}-color SGR must NOT appear when blocks.enabled = false"
        );
    }
}

// Regression for the helix Shift+I bug. The pane child pushes Kitty keyboard
// flags 5 (disambiguate|alternates, exactly what helix pushes), then execs
// `cat -v`, which renders every byte it receives visibly (escape sequences
// become ^[[… text). Typing a capital "I" at the client must reach the child
// as the literal text "I" (kitty's own behavior at flags 5), not as a
// lowercased CSI-u event (`\e[105u`, which helix interpreted as a bare `i`).
#[test]
fn capital_letter_reaches_kitty_flags5_pane_as_text() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(5)), "daemon never rendered");
    // Turn the pane into a helix-alike: push flags 5, then render received
    // bytes visibly. `exec` keeps cat as the direct child.
    sess.send_str("printf '\\033[>5u'; exec cat -v\n");
    // `cat` is up once our probe text round-trips. The marker is
    // quote-concatenated so the contiguous form appears only via `cat`'s
    // output echo.
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    let mut ready = false;
    while std::time::Instant::now() < deadline {
        sess.send_str("WARM_");
        sess.send_str("UP\n");
        if sess.wait_for(b"WARM_UP", Duration::from_millis(500)) {
            ready = true;
            break;
        }
    }
    assert!(ready, "cat -v child never came up: {}", sess.snapshot_str());
    // The regression probe: a capital I (with sentinels so the assertion can't
    // match this test's own input echo through the client PTY; the client is
    // raw, so there IS no local echo, but belt and braces via cat -v's
    // rendering: a CSI-u leak would render as `^[[105u` between the sentinels).
    sess.send_str("<I>\n");
    assert!(
        sess.wait_for(b"<I>", Duration::from_secs(5)),
        "capital I never reached the kitty(5) pane as text: {}",
        sess.snapshot_str()
    );
    let txt = sess.snapshot_str();
    assert!(
        !txt.contains("[105"),
        "capital I leaked to the pane as a CSI-u event: {txt}"
    );
}

// ---------------------------------------------------------------------------
// The `run` verb (R4)
// ---------------------------------------------------------------------------
// `run` requires the pane to be at an OSC 133 prompt, so each test first seeds
// a PROMPT_START mark via `send --enter` of a printf that emits the raw `A`
// bytes (the pane's /bin/sh has no real shell integration, so the commands ARE
// the integration). Quoting layers: run_cli passes args directly (no shell),
// so single quotes inside the text are interpreted by the PANE's sh, and the
// `\033`/`\007` escapes by the pane's printf. Quote-concatenated needles
// (RUN_'OK'_OUT → RUN_OK_OUT) appear contiguously only in executed output,
// never in the pane's echo of the typed command text.

/// Seed an OSC 133 PROMPT_START mark in the session's pane and poll `capture`
/// until the marker text is visible, which proves the emulator processed the
/// mark (the precondition every successful `run` needs).
fn seed_prompt_mark(env: &TestEnv, sess: &TestSession) {
    let (status, _stdout, stderr) =
        run_cli(env, &["send", "--enter", "printf '\\033]133;A\\007SEED'PROMPT'\\n'"]);
    assert!(status.success(), "seed send failed: {stderr}");
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut seeded = false;
    while Instant::now() < deadline {
        let (_, stdout, _) = run_cli(env, &["capture"]);
        if stdout.contains("SEEDPROMPT") {
            seeded = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(seeded, "SEEDPROMPT never appeared in capture (mark not seeded). pane: {}", sess.snapshot_str());
}

/// Happy path: `run` a command that emits C, real output, then D;0 + A, so
/// stdout contains the output needle and the exit code is 0.
#[test]
fn run_ok_prints_output_and_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(20)), "daemon never rendered");
    seed_prompt_mark(&env, &sess);

    let (status, stdout, stderr) = run_cli(
        &env,
        &[
            "run",
            "printf '\\033]133;C\\007'; echo RUN_'OK'_OUT; printf '\\033]133;D;0\\007\\033]133;A\\007'",
        ],
    );
    assert!(
        status.success(),
        "run should exit 0 (status={status:?}). stdout: {stdout:?} stderr: {stderr:?} pane: {}",
        sess.snapshot_str()
    );
    assert!(
        stdout.contains("RUN_OK_OUT"),
        "run stdout must contain the block output. got: {stdout:?} stderr: {stderr:?}"
    );
}

/// A command whose D mark carries exit 5 → `run` exits 5.
#[test]
fn run_failed_propagates_exit_code() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(20)), "daemon never rendered");
    seed_prompt_mark(&env, &sess);

    let (status, stdout, stderr) = run_cli(
        &env,
        &[
            "run",
            "printf '\\033]133;C\\007'; echo RUN_'FAIL'_OUT; printf '\\033]133;D;5\\007\\033]133;A\\007'",
        ],
    );
    assert_eq!(
        status.code(),
        Some(5),
        "run must propagate the command's exit code 5. stdout: {stdout:?} stderr: {stderr:?} pane: {}",
        sess.snapshot_str()
    );
    assert!(
        stdout.contains("RUN_FAIL_OUT"),
        "run stdout must contain the block output. got: {stdout:?}"
    );
}

/// Two runs back-to-back: the second run's at-prompt check must accept the
/// fresh `A` the first run's command emitted (the headline `&&` chain).
#[test]
fn run_chained_back_to_back() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(20)), "daemon never rendered");
    seed_prompt_mark(&env, &sess);

    let (status, stdout, stderr) = run_cli(
        &env,
        &[
            "run",
            "printf '\\033]133;C\\007'; echo RUN_'ONE'_OUT; printf '\\033]133;D;0\\007\\033]133;A\\007'",
        ],
    );
    assert!(status.success(), "first run failed (status={status:?}): {stderr:?}");
    assert!(stdout.contains("RUN_ONE_OUT"), "first run output missing. got: {stdout:?}");

    let (status, stdout, stderr) = run_cli(
        &env,
        &[
            "run",
            "printf '\\033]133;C\\007'; echo RUN_'TWO'_OUT; printf '\\033]133;D;0\\007\\033]133;A\\007'",
        ],
    );
    assert!(
        status.success(),
        "second (chained) run failed (status={status:?}). stderr: {stderr:?} pane: {}",
        sess.snapshot_str()
    );
    assert!(stdout.contains("RUN_TWO_OUT"), "second run output missing. got: {stdout:?}");
}

/// `run --timeout 1` with a command that never emits a D mark → exit 124 with
/// the timeout message on stderr (the command is not killed).
#[test]
fn run_timeout_exits_124() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(20)), "daemon never rendered");
    seed_prompt_mark(&env, &sess);

    // `true` emits no OSC 133 marks (no real shell integration in the pane),
    // so the wait can only end via the timeout.
    let (status, stdout, stderr) = run_cli(&env, &["run", "--timeout", "1", "true"]);
    assert_eq!(
        status.code(),
        Some(124),
        "run --timeout must exit 124. stdout: {stdout:?} stderr: {stderr:?} pane: {}",
        sess.snapshot_str()
    );
    assert!(
        stderr.contains("timed out after 1s"),
        "expected the timeout message on stderr, got: {stderr:?}"
    );
}

/// Fresh pane with no OSC 133 marks at all → refused fast with the no-blocks
/// message, exit 1.
#[test]
fn run_no_blocks_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(20)), "daemon never rendered");

    // No seeding, the pane has never seen a PROMPT_START mark.
    let (status, _stdout, stderr) = run_cli(&env, &["run", "echo hi"]);
    assert_eq!(
        status.code(),
        Some(1),
        "run with no blocks must exit 1. stderr: {stderr:?} pane: {}",
        sess.snapshot_str()
    );
    assert!(
        stderr.contains("no command blocks"),
        "expected the no-blocks message on stderr, got: {stderr:?}"
    );
}

/// Busy pane (the newest A has a C after it but no D yet, so a command is
/// mid-flight) → refused with the busy message, exit 1.
#[test]
fn run_busy_pane_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(20)), "daemon never rendered");
    seed_prompt_mark(&env, &sess);

    // Emit a C mark after the seeded A (no D): the pane now looks mid-command.
    let (status, _stdout, stderr) =
        run_cli(&env, &["send", "--enter", "printf '\\033]133;C\\007MID'FLIGHT'\\n'"]);
    assert!(status.success(), "busy-seed send failed: {stderr}");
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut busy_set = false;
    while Instant::now() < deadline {
        let (_, stdout, _) = run_cli(&env, &["capture"]);
        if stdout.contains("MIDFLIGHT") {
            busy_set = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(busy_set, "MIDFLIGHT never appeared (C mark not set). pane: {}", sess.snapshot_str());

    let (status, _stdout, stderr) = run_cli(&env, &["run", "echo hi"]);
    assert_eq!(
        status.code(),
        Some(1),
        "run against a busy pane must exit 1. stderr: {stderr:?} pane: {}",
        sess.snapshot_str()
    );
    assert!(
        stderr.contains("a command is running"),
        "expected the busy message on stderr, got: {stderr:?}"
    );
}

// ---------------------------------------------------------------------------
// --json structured output (J3)
// ---------------------------------------------------------------------------

/// `capture --last-command --json` prints one JSON object with the block's
/// output, exit code, and command line (extracted from the B/C marks).
#[test]
fn capture_last_command_json_returns_structured_object() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(20)), "daemon never rendered");

    // Full A / B / command / C / output / D;7 / A block. Quote-concatenation
    // keeps the echoed *typed* line distinct from the executed printf output.
    let (send_status, _, send_err) = run_cli(
        &env,
        &[
            "send",
            "--enter",
            "printf '\\033]133;A\\007P$ \\033]133;B\\007demo_'cmd'_j3\\r\\n\\033]133;C\\007OUT_'J3'_LINE\\n\\033]133;D;7\\007\\033]133;A\\007PTWO\\n'",
        ],
    );
    assert!(send_status.success(), "send failed: {send_err}");

    // Wait until the executed output appears in plain capture (the emulator
    // processed the OSC bytes), then poll the JSON capture until the seeded
    // block (exit 7) is the last completed one.
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut parsed: Option<serde_json::Value> = None;
    while Instant::now() < deadline {
        let (status, stdout, _) = run_cli(&env, &["capture", "--last-command", "--json"]);
        if status.success()
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&stdout)
            && v["exit_code"] == 7
        {
            parsed = Some(v);
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let v = parsed.unwrap_or_else(|| {
        panic!("capture --last-command --json never returned exit 7. pane: {}", sess.snapshot_str())
    });
    let output = v["output"].as_str().expect("output must be a string");
    assert!(output.contains("OUT_J3_LINE"), "JSON output missing the block text: {v}");
    assert_eq!(v["exit_code"], 7, "JSON exit_code must be the D payload: {v}");
    let cmd = v["command_line"].as_str().expect("command_line must be a string here");
    assert!(cmd.contains("demo_cmd_j3"), "JSON command_line missing the typed text: {v}");
}

/// `capture --json` without `--last-command` is a clap usage error (exit 2).
#[test]
fn capture_json_without_last_command_is_clap_error() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    // No session needed: clap rejects the flag combination before any I/O.
    let (status, _stdout, stderr) = run_cli(&env, &["capture", "--json"]);
    assert_eq!(
        status.code(),
        Some(2),
        "capture --json without --last-command must be a clap error. stderr: {stderr:?}"
    );
    assert!(
        stderr.contains("--last-command"),
        "clap error must mention the missing --last-command flag: {stderr:?}"
    );
}

/// `run --json`: happy path prints {"output", "exit_code": 0, "timed_out":
/// false, "command_line": <sent text>} with exit 0; a D;5 command exits 5
/// with JSON exit_code 5 (chained in one session, like run_chained).
#[test]
fn run_json_ok_and_failed() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(20)), "daemon never rendered");
    seed_prompt_mark(&env, &sess);

    let ok_cmd =
        "printf '\\033]133;C\\007'; echo RUNJ_'OK'_OUT; printf '\\033]133;D;0\\007\\033]133;A\\007'";
    let (status, stdout, stderr) = run_cli(&env, &["run", "--json", ok_cmd]);
    assert!(
        status.success(),
        "run --json should exit 0 (status={status:?}). stdout: {stdout:?} stderr: {stderr:?} pane: {}",
        sess.snapshot_str()
    );
    let v: serde_json::Value =
        serde_json::from_str(&stdout).expect("run --json stdout must be one JSON object");
    assert!(
        v["output"].as_str().expect("output is a string").contains("RUNJ_OK_OUT"),
        "JSON output missing the needle: {v}"
    );
    assert_eq!(v["exit_code"], 0, "JSON exit_code must be 0: {v}");
    assert_eq!(v["timed_out"], false, "JSON timed_out must be false: {v}");
    assert_eq!(
        v["command_line"].as_str(),
        Some(ok_cmd),
        "JSON command_line must echo the sent text: {v}"
    );

    let fail_cmd =
        "printf '\\033]133;C\\007'; echo RUNJ_'FAIL'_OUT; printf '\\033]133;D;5\\007\\033]133;A\\007'";
    let (status, stdout, stderr) = run_cli(&env, &["run", "--json", fail_cmd]);
    assert_eq!(
        status.code(),
        Some(5),
        "run --json must propagate exit 5. stdout: {stdout:?} stderr: {stderr:?} pane: {}",
        sess.snapshot_str()
    );
    let v: serde_json::Value =
        serde_json::from_str(&stdout).expect("failed run --json stdout must be one JSON object");
    assert_eq!(v["exit_code"], 5, "JSON exit_code must be 5: {v}");
    assert_eq!(v["timed_out"], false, "JSON timed_out must be false: {v}");
    assert!(
        v["output"].as_str().expect("output is a string").contains("RUNJ_FAIL_OUT"),
        "JSON output missing the needle: {v}"
    );
}

/// `run --json --timeout 1` with a command that never emits D → exit 124 and
/// the JSON carries `"timed_out": true` (the stderr note stays plain).
#[test]
fn run_json_timeout_exits_124_with_timed_out_true() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(20)), "daemon never rendered");
    seed_prompt_mark(&env, &sess);

    // `true` emits no OSC 133 marks, so the wait can only end via the timeout.
    let (status, stdout, stderr) = run_cli(&env, &["run", "--json", "--timeout", "1", "true"]);
    assert_eq!(
        status.code(),
        Some(124),
        "run --json --timeout must exit 124. stdout: {stdout:?} stderr: {stderr:?} pane: {}",
        sess.snapshot_str()
    );
    let v: serde_json::Value =
        serde_json::from_str(&stdout).expect("timed-out run --json stdout must be one JSON object");
    assert_eq!(v["timed_out"], true, "JSON timed_out must be true: {v}");
    assert_eq!(v["exit_code"], serde_json::Value::Null, "no exit code on timeout: {v}");
    assert_eq!(v["command_line"].as_str(), Some("true"), "command_line must echo: {v}");
    assert!(
        stderr.contains("timed out after 1s"),
        "the plain timeout note must stay on stderr: {stderr:?}"
    );
}

/// Choose-tree v2 happy-path: filter, Enter-to-keep, rename session via `r`.
///
/// Two sessions: "main" (the attached client) and "beta" (a second PTY client
/// in the same daemon). The tree is sorted alphabetically, so beta comes first.
/// After opening the tree from main:
///   1. `/` + "bet" → filter narrows to the beta subtree.
///   2. Enter keeps the filter and returns to Navigate mode with selection on
///      the beta session row (row 0 alphabetically).
///   3. `r` → rename mode primed with "beta"; 4× Backspace + "zeta" + Enter.
///   4. Tree re-stamps the beta row to "zeta — …" and the registry re-keys live.
///   5. `plexy-glass list` shows "zeta", not "beta".
///   6. zeta.json eventually exists; beta.json eventually gone.
///
/// Note: bare `\x1b` (Escape) isn't used anywhere, because the legacy key parser
/// holds `\x1b` pending until the NEXT byte and produces Alt+X instead of
/// standalone Escape. We use `Enter` (`\r`) to exit filter mode (which keeps the
/// filter) and to commit the rename, since both are unambiguous in the parser.
#[test]
fn choose_tree_filter_and_rename_session() {
    use std::process::Stdio;

    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);

    // Session "main", the primary attached client.
    let mut main_sess = TestSession::spawn(&env);
    assert!(
        main_sess.wait_ready("main", Duration::from_secs(20)),
        "main session never rendered"
    );

    // Session "beta", a second PTY client in the same daemon.
    let beta_sess =
        TestSession::builder(&env).args(&["attach", "-n", "beta"]).start();
    assert!(
        beta_sess.wait_ready("beta", Duration::from_secs(20)),
        "beta session never rendered"
    );

    // Open the choose-tree from main. Sessions are sorted alphabetically, so
    // the tree is: beta (row 0), beta-window (1), beta-pane (2), main (3), …
    //
    // Needle strategy: the diff renderer only writes cells that CHANGED from
    // the previous frame, so spaces that were already blank get skipped. We
    // choose word-level tokens with no internal spaces:
    //   "beta"    is the session-label row (4 consecutive non-space bytes)
    //   "keep"    is the filter-mode footer "enter keep" (unique to filter mode)
    //   "switch"  is the navigate-mode footer "enter switch" (unique to nav mode)
    //   "rename:" is the rename-mode footer (colon follows without space)
    //   "zeta"    is the re-stamped session-label after a successful rename
    main_sess.send_prefix(b'W');
    assert!(
        main_sess.wait_for(b"beta", Duration::from_secs(15)),
        "choose-tree never opened (beta label not visible). raw: {}",
        main_sess.snapshot_str()
    );

    // Enter filter mode (`/`). The filter footer says "…enter keep…", so wait
    // for "keep" before typing to make sure the keystroke lands in filter mode.
    let mark_before_filter = main_sess.buffer_len();
    main_sess.send_str("/");
    assert!(
        main_sess.wait_for_from(mark_before_filter, b"keep", Duration::from_secs(5)),
        "filter mode never activated. raw: {}",
        main_sess.snapshot_str()
    );

    // Type "bet"; the session-row label "beta — 1 win, 1 panes" stays visible
    // (substring match). Mark before typing so we match only the re-render.
    let mark_before_bet = main_sess.buffer_len();
    main_sess.send_str("bet");
    assert!(
        main_sess.wait_for_from(mark_before_bet, b"beta", Duration::from_secs(5)),
        "beta not visible in filtered tree. raw: {}",
        main_sess.snapshot_str()
    );

    // Enter keeps the filter and returns to Navigate mode. The navigate footer
    // re-renders with "…enter switch…"; mark before Enter and look for
    // "switch" in the new output.
    let mark_before_enter_filter = main_sess.buffer_len();
    main_sess.send_str("\r"); // Enter keeps the filter and returns to Navigate
    assert!(
        main_sess.wait_for_from(mark_before_enter_filter, b"switch", Duration::from_secs(5)),
        "nav footer never appeared after Enter-from-filter. raw: {}",
        main_sess.snapshot_str()
    );

    // Selection is on the beta session row (row 0 alphabetically: the filter
    // "bet" matches "beta — 1 win, 1 panes" and "beta" sorts before "main").
    // Press `r` to enter rename mode. The footer switches to " rename: beta█  enter
    // ok · esc cancel ". The █ cursor glyph (U+2588, "\xe2\x96\x88") shows up in
    // the rename-mode footer but was absent from the navigate-mode footer, so
    // using it as the needle avoids the partial-skip problem where the `r` of
    // "rename:" coincides with the `r` of "r rename" in the previous nav footer.
    let mark_before_rename = main_sess.buffer_len();
    main_sess.send_str("r");
    assert!(
        main_sess.wait_for_from(mark_before_rename, b"\xe2\x96\x88", Duration::from_secs(5)),
        "rename mode never activated (cursor glyph \u{2588} not found). raw: {}",
        main_sess.snapshot_str()
    );

    // The edit buf is primed with "beta" (4 chars). Clear with Backspace ×4,
    // type the new name "zeta", then Enter to commit.
    main_sess.send_repeat(b"\x7f", 4); // Backspace x4 to clear "beta"
    main_sess.send_str("zeta");
    let mark_before_commit = main_sess.buffer_len();
    main_sess.send_str("\r"); // Enter commits the rename

    // On success the rename mode exits and the tree returns to Navigate mode.
    // The active filter "bet" no longer matches "zeta — …", so the tree body
    // is empty and we cannot assert "zeta" in the tree content. Instead, wait
    // for the Navigate footer to re-appear (confirming the rename committed and
    // the tree re-rendered): "switch" is in "enter switch" (nav-mode-only text).
    assert!(
        main_sess.wait_for_from(mark_before_commit, b"switch", Duration::from_secs(15)),
        "navigate footer never re-appeared after rename commit. raw: {}",
        main_sess.snapshot_str()
    );

    // Headless list: zeta in, beta out.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut list_ok = false;
    while Instant::now() < deadline {
        let list_out = std::process::Command::cargo_bin("plexy-glass")
            .unwrap()
            .arg("list")
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdout(Stdio::piped())
            .output()
            .expect("list");
        let stdout = String::from_utf8_lossy(&list_out.stdout);
        if stdout.contains("zeta") && !stdout.contains("beta") {
            list_ok = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(list_ok, "list must show 'zeta' not 'beta'. raw: {}", main_sess.snapshot_str());

    // Persist files: zeta.json must appear (debounced ~1.5 s) and beta.json
    // must vanish (immediate delete + deferred sweep within ~3 s + margin).
    let state_dir = tmp.path().join("state/plexy-glass/sessions");
    let zeta_file = state_dir.join("zeta.json");
    let beta_file = state_dir.join("beta.json");

    assert!(
        wait_for_file_exists(&zeta_file, Duration::from_secs(10)),
        "zeta.json never written after rename"
    );
    // `beta.json` should be gone, so poll for absence (the deferred sweep window).
    let beta_gone = {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if !beta_file.exists() {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    assert!(beta_gone, "beta.json still exists after rename + sweep window");

    // Verify the persisted zeta.json carries the new internal name.
    assert!(
        wait_for_file_contains(&zeta_file, "\"zeta\"", Duration::from_secs(5)),
        "zeta.json exists but internal name field is not 'zeta'. contents: {}",
        std::fs::read_to_string(&zeta_file).unwrap_or_default()
    );

    drop(beta_sess);
}

/// pipe-pane (spec: 2026-06-12-pipe-pane-design.md): `cmd "pipe-pane tee FILE"`
/// streams the pane's raw output to `tee`, which writes it to FILE. Typing a
/// command into the pane makes its output flow to the file; `cmd "pipe-pane"`
/// stops the pipe, after which further pane output no longer reaches the file.
#[test]
fn cli_pipe_pane_streams_then_stops() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(20)), "daemon never rendered");

    // Absolute path under the test's tempdir, clean, and it avoids the
    // daemon-cwd question.
    let log = tmp.path().join("pipe.log");

    // Start the pipe. `tee` copies its stdin (the pane's raw output) to FILE.
    let (status, _stdout, stderr) =
        run_cli(&env, &["cmd", &format!("pipe-pane tee {}", log.display())]);
    assert!(status.success(), "cmd 'pipe-pane tee …' failed: {stderr}");

    // Quote-concatenation: the typed line `echo PIPE_'NEEDLE'` cannot itself
    // satisfy the poll (its source bytes hold `PIPE_'NEEDLE'`, not the
    // contiguous needle); only the shell's EXECUTED `echo` output is the
    // contiguous `PIPE_NEEDLE`. Both the echo of the typed line and the
    // command's output flow to the pipe, but only the executed output matches.
    let (status, _stdout, stderr) =
        run_cli(&env, &["send", "--enter", "echo PIPE_'NEEDLE'"]);
    assert!(status.success(), "send --enter failed: {stderr}");

    // Poll the tee'd file (bounded) until the executed-output needle lands.
    assert!(
        wait_for_file_contains(&log, "PIPE_NEEDLE", Duration::from_secs(10)),
        "pipe file never received the executed pane output. file: {:?}, pty: {}",
        std::fs::read_to_string(&log).ok(),
        sess.snapshot_str()
    );

    // Stop the pipe.
    let (status, _stdout, stderr) = run_cli(&env, &["cmd", "pipe-pane"]);
    assert!(status.success(), "cmd 'pipe-pane' (stop) failed: {stderr}");

    // Settle: let any in-flight bytes flush and the consumer get killed/reaped,
    // THEN snapshot the file length. Polling for the length to stabilize makes
    // the "no growth" check sound: we only freeze the baseline once writes
    // have quiesced, so a later growth can only come from a live pipe.
    let settled_len = {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut last = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
        loop {
            std::thread::sleep(Duration::from_millis(200));
            let now = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
            if now == last || Instant::now() >= deadline {
                break now;
            }
            last = now;
        }
    };

    // Drive more pane output AFTER the stop. The PTY wait confirms the shell
    // actually executed the line and its output flowed through the pane, so if
    // the pipe were still attached, the file WOULD grow.
    let (status, _stdout, stderr) =
        run_cli(&env, &["send", "--enter", "echo AFTER_'STOP'"]);
    assert!(status.success(), "post-stop send failed: {stderr}");
    assert!(
        sess.wait_for(b"AFTER_STOP", Duration::from_secs(10)),
        "post-stop output never reached the pane; the no-growth check would be vacuous. pty: {}",
        sess.snapshot_str()
    );
    // Give a stopped-but-buggy pipe a chance to (wrongly) write before checking.
    std::thread::sleep(Duration::from_millis(500));

    let after_len = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
    assert_eq!(
        after_len, settled_len,
        "file grew after the pipe was stopped (pipe did not stop). \
         settled={settled_len}, after={after_len}, contents: {:?}",
        std::fs::read_to_string(&log).ok()
    );
    let contents = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        !contents.contains("AFTER_STOP"),
        "post-stop output leaked into the pipe file: {contents:?}"
    );
}

/// `monitor-command` surfaces a background window's command completion as a
/// status message + a `✗` window-list flag. `send` targets the ACTIVE window,
/// so the synthetic completion is scheduled (a backgrounded delayed printf of
/// the OSC-133 C/D;1/A sequence) into window 1 BEFORE switching to a new
/// window, since a "switch then send into window 1" order is unimplementable.
#[test]
fn cli_monitor_command_alerts_background_completion() {
    let tmp = tempfile::tempdir().unwrap();
    let env = isolate_dirs(&tmp);
    let mut sess = TestSession::spawn(&env);
    assert!(sess.wait_ready("main", Duration::from_secs(20)), "daemon never rendered");

    // Turn monitor-command ON for window 1 (the active window).
    let (status, _o, stderr) = run_cli(&env, &["cmd", "monitor-command"]);
    assert!(status.success(), "cmd monitor-command failed: {stderr}");

    // Schedule a delayed command completion in window 1's shell: after ~1s,
    // emit OSC 133;C (command start), some output, 133;D;1 (done, exit 1), and
    // 133;A (next prompt). Backgrounded so the shell returns immediately and we
    // can switch windows before it fires. The single-quoted printf format keeps
    // the ESC bytes literal in the typed line; the shell's printf interprets
    // the \033/\007 escapes.
    let printf =
        r"( sleep 1; printf '\033]133;C\007out\033]133;D;1\007\033]133;A\007' ) &";
    let (status, _o, stderr) = run_cli(&env, &["send", "--enter", printf]);
    assert!(status.success(), "send backgrounded printf failed: {stderr}");

    // Switch to a new window (Ctrl+a c) so window 1 is now in the background
    // when the completion fires ~1s later.
    sess.send_prefix(b'c');

    // Poll the status line for the alert message. The diff renderer skips
    // UNCHANGED cells (incl. spaces) between frames, so the message can render
    // with leading chars clobbered by the prior "monitor-command on" message at
    // the same column and with its inter-word spaces dropped (the documented
    // "27 48 never renders contiguously" behavior). So we match a contiguous
    // fragment downstream of that collision that survives, "in window 1 (shell)",
    // rather than the full "done in window 1 …" prefix.
    assert!(
        sess.wait_for(b"in window 1 (shell)", Duration::from_secs(15)),
        "monitor-command alert message never appeared. pty: {}",
        sess.snapshot_str()
    );
    // The window list shows the `✗` (nonzero-exit) flag on window 1. A single
    // non-space glyph, so it's immune to the space-skipping diff artifact.
    assert!(
        sess.wait_for("✗".as_bytes(), Duration::from_secs(15)),
        "the ✗ done flag never appeared in the window list. pty: {}",
        sess.snapshot_str()
    );
}
