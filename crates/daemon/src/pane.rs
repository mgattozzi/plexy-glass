//! One PTY-backed pane: child process + emulator + input/output channels.
//!
//! Cloning a `Pane` is cheap (shared `Arc<Inner>`); shared state is protected
//! by Mutex/broadcast/mpsc/watch as before.

use crate::error::DaemonError;
use bytes::Bytes;
use plexy_glass_emulator::{Emulator, Screen};
use plexy_glass_mux::PaneId;
use plexy_glass_protocol::{ExitStatus, PtySize, SpawnSpec};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize as PortablePtySize};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, broadcast, mpsc, watch};
use tracing::{debug, error};

#[derive(Clone)]
pub struct Pane {
    inner: Arc<Inner>,
}

struct Inner {
    id: PaneId,
    input_tx: mpsc::Sender<Bytes>,
    output_tx: broadcast::Sender<Bytes>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    exit_rx: watch::Receiver<Option<ExitStatus>>,
    emulator: Arc<Mutex<Emulator>>,
}

impl Pane {
    pub fn spawn(
        id: PaneId,
        spec: SpawnSpec,
        size: PtySize,
        output_notify: Arc<Notify>,
        death_tx: Option<mpsc::Sender<PaneId>>,
    ) -> Result<Self, DaemonError> {
        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system
            .openpty(to_portable(size))
            .map_err(|e| DaemonError::Io(std::io::Error::other(format!("openpty: {e}"))))?;

        let mut cmd = CommandBuilder::new(&spec.program);
        cmd.args(&spec.args);
        if let Some(cwd) = &spec.cwd {
            cmd.cwd(cwd);
        }
        if !spec.env.is_empty() {
            cmd.env_clear();
            for (k, v) in &spec.env {
                cmd.env(k, v);
            }
        }
        cmd.env("PLEXY_GLASS", "1");

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| DaemonError::Io(std::io::Error::other(format!("spawn: {e}"))))?;
        drop(pair.slave);

        let (input_tx, mut input_rx) = mpsc::channel::<Bytes>(64);
        let (output_tx, _) = broadcast::channel::<Bytes>(256);
        let (exit_tx, exit_rx) = watch::channel::<Option<ExitStatus>>(None);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| DaemonError::Io(std::io::Error::other(format!("clone reader: {e}"))))?;
        let mut writer = pair
            .master
            .take_writer()
            .map_err(|e| DaemonError::Io(std::io::Error::other(format!("take writer: {e}"))))?;
        let master = pair.master;

        let emulator = Arc::new(Mutex::new(Emulator::new(size.rows, size.cols)));

        let output_tx_clone = output_tx.clone();
        let emulator_for_reader = Arc::clone(&emulator);
        let notify_for_reader = Arc::clone(&output_notify);
        // Reader also writes emulator-generated replies (DSR cursor reports,
        // DA, …) back through the input mpsc so the writer thread forwards
        // them to the child. Without this, TUI line editors block on `ESC[6n`.
        let reply_tx = input_tx.clone();
        let reader_notify_for_self = Arc::clone(&output_notify);
        let reader_handle = std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        debug!("pane reader EOF");
                        // Final notify so the renderer can pick up any
                        // unprocessed bytes before the connection tears down.
                        reader_notify_for_self.notify_one();
                        return;
                    }
                    Ok(n) => {
                        let replies = {
                            // invariant: emulator mutex held briefly to advance + drain.
                            let mut e = emulator_for_reader
                                .lock()
                                .expect("pane emulator mutex poisoned");
                            e.advance(&buf[..n]);
                            e.take_replies()
                        };
                        let chunk = Bytes::copy_from_slice(&buf[..n]);
                        let _ = output_tx_clone.send(chunk);
                        notify_for_reader.notify_one();
                        for reply in replies {
                            if let Err(err) = reply_tx.blocking_send(Bytes::from(reply)) {
                                debug!(error = %err, "could not forward emulator reply");
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        debug!(error = %e, "pane reader closed");
                        return;
                    }
                }
            }
        });

        std::thread::spawn(move || {
            while let Some(chunk) = input_rx.blocking_recv() {
                if let Err(e) = writer.write_all(&chunk) {
                    error!(error = %e, "pane writer error");
                    return;
                }
                if let Err(e) = writer.flush() {
                    error!(error = %e, "pane flush error");
                    return;
                }
            }
        });

        std::thread::spawn(move || {
            let status = wait_child(&mut child);
            let _ = exit_tx.send(Some(status));
            // Wait for the reader thread to drain any remaining PTY bytes
            // (line-editor cleanup, final prompt erase, etc.) into the
            // emulator before signaling death. Otherwise the connection
            // might tear down the renderer while the host TTY is still in a
            // mid-render state, leaving the user's host shell to need a
            // keystroke before redrawing.
            let _ = reader_handle.join();
            if let Some(tx) = death_tx {
                let _ = tx.blocking_send(id);
            }
        });

        Ok(Self {
            inner: Arc::new(Inner {
                id,
                input_tx,
                output_tx,
                master: Mutex::new(master),
                exit_rx,
                emulator,
            }),
        })
    }

    pub fn id(&self) -> PaneId {
        self.inner.id
    }

    pub async fn send_input(&self, bytes: Bytes) -> Result<(), DaemonError> {
        self.inner
            .input_tx
            .send(bytes)
            .await
            .map_err(|_| DaemonError::Io(std::io::Error::other("pane input channel closed")))
    }

    pub fn subscribe_output(&self) -> broadcast::Receiver<Bytes> {
        self.inner.output_tx.subscribe()
    }

    pub fn resize(&self, size: PtySize) -> Result<(), DaemonError> {
        {
            // invariant: contended only by resize calls; brief hold.
            let master = self
                .inner
                .master
                .lock()
                .expect("pane master mutex poisoned");
            master
                .resize(to_portable(size))
                .map_err(|e| DaemonError::Io(std::io::Error::other(format!("resize: {e}"))))?;
        }
        {
            // invariant: emulator mutex contended only briefly.
            let mut emu = self
                .inner
                .emulator
                .lock()
                .expect("pane emulator mutex poisoned");
            emu.resize(size.rows, size.cols);
        }
        Ok(())
    }

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

    pub fn exit_rx(&self) -> watch::Receiver<Option<ExitStatus>> {
        self.inner.exit_rx.clone()
    }

    pub fn with_screen<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Screen) -> R,
    {
        // invariant: emulator mutex contended only briefly.
        let emu = self
            .inner
            .emulator
            .lock()
            .expect("pane emulator mutex poisoned");
        f(emu.screen())
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
        Ok(s) if s.success() => ExitStatus::Code(0),
        Ok(s) => ExitStatus::Code(s.exit_code() as i32),
        Err(_) => ExitStatus::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexy_glass_mux::PaneId;
    use tokio::sync::Notify;

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
        let session = Pane::spawn(PaneId(0), spec, size(), Arc::new(Notify::new()), None).expect("spawn");
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
        let session = Pane::spawn(PaneId(0), spec, size(), Arc::new(Notify::new()), None).expect("spawn");
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
        let session = Pane::spawn(PaneId(0), spec, size(), Arc::new(Notify::new()), None).expect("spawn");
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
        let session = Pane::spawn(PaneId(0), spec, size(), Arc::new(Notify::new()), None).expect("spawn");

        session
            .resize(PtySize {
                rows: 30,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize");
        let (r, c) = session.with_screen(|s| (s.rows(), s.cols()));
        assert_eq!((r, c), (30, 100));

        // Send EOF so `cat` exits.
        session
            .send_input(Bytes::from_static(&[0x04]))
            .await
            .unwrap();
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
        let session = Pane::spawn(PaneId(0), spec, size(), Arc::new(Notify::new()), None).expect("spawn");
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
