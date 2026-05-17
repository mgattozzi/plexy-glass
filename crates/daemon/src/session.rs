//! One live PTY-backed child session.
//!
//! [`Session`] owns a [`portable_pty`] master/slave pair plus the spawned
//! child. PTY reads happen on a dedicated OS thread that pushes chunks into a
//! tokio `broadcast` channel; PTY writes are driven from a tokio `mpsc`
//! channel by another OS thread. The child's exit status is published through
//! a `watch` channel by a third thread. All threads exit cleanly when the
//! underlying file descriptors close.

use crate::error::DaemonError;
use bytes::Bytes;
use plexy_glass_protocol::{ExitStatus, PtySize, SpawnSpec};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize as PortablePtySize};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{debug, error};

/// One live PTY-backed child. Cloning this handle is cheap: the underlying
/// resources are held inside `Arc`s.
#[derive(Clone)]
pub struct Session {
    inner: Arc<Inner>,
}

struct Inner {
    input_tx: mpsc::Sender<Bytes>,
    output_tx: broadcast::Sender<Bytes>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    exit_rx: watch::Receiver<Option<ExitStatus>>,
    emulator: Arc<Mutex<plexy_glass_emulator::Emulator>>,
}

impl Session {
    /// Spawn a child with the given spec and initial size. Returns once the
    /// child is running and reader/writer/wait tasks are armed.
    pub fn spawn(spec: SpawnSpec, size: PtySize) -> Result<Self, DaemonError> {
        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system
            .openpty(to_portable(size))
            .map_err(|e| DaemonError::Io(std::io::Error::other(format!("openpty: {e}"))))?;

        let mut cmd = CommandBuilder::new(&spec.program);
        cmd.args(&spec.args);
        if let Some(cwd) = &spec.cwd {
            cmd.cwd(cwd);
        }
        // If the spec provides an env, replace inherited env entirely;
        // otherwise let the child inherit from the daemon's environment.
        if !spec.env.is_empty() {
            cmd.env_clear();
            for (k, v) in &spec.env {
                cmd.env(k, v);
            }
        }
        // Marker so shell rc files (and other tooling) can detect they're
        // running inside plexy-glass, analogous to $TMUX / $ZELLIJ. Set
        // last so a caller can't accidentally override it via spec.env.
        cmd.env("PLEXY_GLASS", "1");

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| DaemonError::Io(std::io::Error::other(format!("spawn: {e}"))))?;
        // The slave fd is now owned by the child; drop our copy so EOF is
        // observable on the master when the child exits.
        drop(pair.slave);

        let (input_tx, mut input_rx) = mpsc::channel::<Bytes>(64);
        let (output_tx, _) = broadcast::channel::<Bytes>(256);
        let (exit_tx, exit_rx) = watch::channel::<Option<ExitStatus>>(None);

        // I/O handles from portable-pty. `take_writer` must be called at most
        // once; we do it here while the master is still uniquely owned.
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| DaemonError::Io(std::io::Error::other(format!("clone reader: {e}"))))?;
        let mut writer = pair
            .master
            .take_writer()
            .map_err(|e| DaemonError::Io(std::io::Error::other(format!("take writer: {e}"))))?;
        let master = pair.master;

        // Terminal emulator: every PTY byte is fed through it before being
        // broadcast, so the daemon can answer screen queries without having to
        // replay the byte stream.
        let emulator = Arc::new(Mutex::new(plexy_glass_emulator::Emulator::new(
            size.rows, size.cols,
        )));

        // PTY -> output broadcast (blocking thread, sends into tokio).
        let output_tx_clone = output_tx.clone();
        let emulator_for_reader = Arc::clone(&emulator);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        debug!("pty reader EOF");
                        return;
                    }
                    Ok(n) => {
                        // Feed the emulator first so any reader that wakes on
                        // the broadcast and then queries the screen sees a
                        // model that already reflects this chunk. Skip on a
                        // poisoned mutex, the broadcast still goes out.
                        if let Ok(mut e) = emulator_for_reader.lock() {
                            e.advance(&buf[..n]);
                        }
                        let chunk = Bytes::copy_from_slice(&buf[..n]);
                        // Best-effort send: if there are no subscribers yet, we drop.
                        let _ = output_tx_clone.send(chunk);
                    }
                    Err(e) => {
                        // EIO on PTY read after slave close is normal on Linux, so we
                        // log at debug rather than error to avoid spam in tests.
                        debug!(error = %e, "pty reader closed");
                        return;
                    }
                }
            }
        });

        // Input mpsc -> PTY (blocking thread).
        std::thread::spawn(move || {
            while let Some(chunk) = input_rx.blocking_recv() {
                if let Err(e) = writer.write_all(&chunk) {
                    error!(error = %e, "pty writer error");
                    return;
                }
                if let Err(e) = writer.flush() {
                    error!(error = %e, "pty flush error");
                    return;
                }
            }
        });

        // Child wait (blocking thread -> watch channel).
        std::thread::spawn(move || {
            let status = wait_child(&mut child);
            let _ = exit_tx.send(Some(status));
        });

        Ok(Self {
            inner: Arc::new(Inner {
                input_tx,
                output_tx,
                master: Mutex::new(master),
                exit_rx,
                emulator,
            }),
        })
    }

    /// Forward client bytes to the child.
    pub async fn send_input(&self, bytes: Bytes) -> Result<(), DaemonError> {
        self.inner
            .input_tx
            .send(bytes)
            .await
            .map_err(|_| DaemonError::Io(std::io::Error::other("session input channel closed")))
    }

    /// Subscribe to the PTY output stream.
    pub fn subscribe_output(&self) -> broadcast::Receiver<Bytes> {
        self.inner.output_tx.subscribe()
    }

    /// Resize the PTY and notify the child via TIOCSWINSZ + SIGWINCH (handled
    /// by the kernel). Also resize the emulator so its grid matches the PTY.
    pub fn resize(&self, size: PtySize) -> Result<(), DaemonError> {
        {
            // invariant: the Mutex is only contended by `resize` calls, which never
            // hold the guard across an await; a panic here would imply a poisoned
            // mutex, which we cannot recover from.
            let master = self.inner.master.lock().expect("session master mutex poisoned");
            master
                .resize(to_portable(size))
                .map_err(|e| DaemonError::Io(std::io::Error::other(format!("resize: {e}"))))?;
        }
        {
            // invariant: the emulator mutex is only ever held briefly by the
            // PTY reader thread and resize; poisoning would mean a panic in
            // the parser, which we cannot recover from.
            let mut emu = self
                .inner
                .emulator
                .lock()
                .expect("emulator mutex poisoned");
            emu.resize(size.rows, size.cols);
        }
        Ok(())
    }

    /// Run `f` with a shared reference to the emulator's screen. Used by the
    /// daemon's screen-query paths; the lock is held only for the duration of
    /// `f`, so callers must not block inside it.
    pub fn with_screen<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&plexy_glass_emulator::Screen) -> R,
    {
        // invariant: see `resize`; poisoning is not recoverable.
        let emu = self
            .inner
            .emulator
            .lock()
            .expect("emulator mutex poisoned");
        f(emu.screen())
    }

    /// Resolves once the child has exited.
    pub async fn wait(&self) -> ExitStatus {
        let mut rx = self.inner.exit_rx.clone();
        loop {
            if let Some(status) = *rx.borrow() {
                return status;
            }
            if rx.changed().await.is_err() {
                return ExitStatus::Unknown;
            }
        }
    }
}

fn to_portable(size: PtySize) -> PortablePtySize {
    PortablePtySize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: size.pixel_width,
        pixel_height: size.pixel_height,
    }
}

fn wait_child(child: &mut Box<dyn Child + Send + Sync>) -> ExitStatus {
    match child.wait() {
        Ok(status) => {
            // portable-pty 0.9 reports signals as a name string; we don't yet
            // map the name back to a numeric signal (the protocol uses i32).
            // Treat signal-killed children as Unknown for now and rely on the
            // exit code path for normal exits. This matches the plan's intent
            // (Code on normal exit, Unknown otherwise).
            if status.signal().is_some() {
                ExitStatus::Unknown
            } else {
                ExitStatus::Code(status.exit_code() as i32)
            }
        }
        Err(_) => ExitStatus::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size() -> PtySize {
        PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    #[tokio::test]
    async fn echo_hello_round_trips() {
        let spec = SpawnSpec {
            program: "/bin/echo".into(),
            args: vec!["hello".into()],
            env: vec![],
            cwd: None,
        };
        let session = Session::spawn(spec, size()).expect("spawn");
        let mut rx = session.subscribe_output();

        let mut got = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            if let Ok(Ok(chunk)) =
                tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await
            {
                got.extend_from_slice(&chunk);
            }
            if got.windows(5).any(|w| w == b"hello") {
                break;
            }
        }
        assert!(
            got.windows(5).any(|w| w == b"hello"),
            "got: {:?}",
            String::from_utf8_lossy(&got)
        );

        let status = tokio::time::timeout(std::time::Duration::from_secs(2), session.wait())
            .await
            .expect("session.wait");
        assert!(matches!(status, ExitStatus::Code(0)), "got {status:?}");
    }

    #[tokio::test]
    async fn cat_echoes_input() {
        let spec = SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };
        let session = Session::spawn(spec, size()).expect("spawn");
        let mut rx = session.subscribe_output();

        session
            .send_input(Bytes::from_static(b"ping\n"))
            .await
            .unwrap();

        let mut got = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            if let Ok(Ok(chunk)) =
                tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await
            {
                got.extend_from_slice(&chunk);
            }
            if got.windows(4).any(|w| w == b"ping") {
                break;
            }
        }
        assert!(
            got.windows(4).any(|w| w == b"ping"),
            "got: {:?}",
            String::from_utf8_lossy(&got)
        );

        // Send Ctrl-D (EOT) so `cat` exits.
        session
            .send_input(Bytes::from_static(&[0x04]))
            .await
            .unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), session.wait()).await;
    }

    #[tokio::test]
    async fn emulator_captures_echo_output() {
        let spec = SpawnSpec {
            program: "/bin/echo".into(),
            args: vec!["hello".into()],
            env: vec![],
            cwd: None,
        };
        let session = Session::spawn(spec, size()).expect("spawn");
        // Wait for the child to exit so the PTY has flushed.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), session.wait()).await;
        // Give the reader thread a beat to drain.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let saw_hello = session.with_screen(|screen| {
            (0..screen.rows()).any(|r| {
                let row_text: String = screen.active.rows[r as usize]
                    .cells
                    .iter()
                    .filter(|c| !c.is_wide_spacer())
                    .map(|c| c.grapheme.as_str())
                    .collect();
                row_text.contains("hello")
            })
        });
        assert!(saw_hello, "emulator did not capture 'hello'");
    }

    #[tokio::test]
    async fn emulator_resizes_with_session() {
        let spec = SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };
        let session = Session::spawn(spec, size()).expect("spawn");

        session.resize(PtySize { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 }).expect("resize");
        let (r, c) = session.with_screen(|s| (s.rows(), s.cols()));
        assert_eq!((r, c), (30, 100));

        // Send EOF so `cat` exits.
        session.send_input(Bytes::from_static(&[0x04])).await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), session.wait()).await;
    }

    #[tokio::test]
    async fn emulator_records_sgr_attributes_from_child() {
        let spec = SpawnSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "printf '\\x1b[1mhi\\x1b[0m'".into()],
            env: vec![],
            cwd: None,
        };
        let session = Session::spawn(spec, size()).expect("spawn");
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), session.wait()).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let bold = session.with_screen(|screen| {
            use plexy_glass_emulator::Attrs;
            screen.active.rows[0]
                .cells
                .iter()
                .take(2)
                .all(|c| c.attrs.contains(Attrs::BOLD))
        });
        assert!(bold, "expected first two cells to be BOLD");
    }
}
