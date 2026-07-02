//! pipe-pane: stream a pane's raw output bytes to an external consumer
//! command. See docs/superpowers/specs/2026-06-12-pipe-pane-design.md.
//!
//! The pipe rides the pane's EXISTING output broadcast
//! (`Pane::subscribe_output`): one drain task per pipe receives every chunk
//! and writes it to the consumer's stdin. The pane itself is never stalled,
//! the broadcast send never blocks the reader thread, and a drain that falls
//! a full channel behind has irrecoverably lost data so the pipe is CLOSED
//! (honest failure over corrupt logs).
//!
//! Lifecycle: every close path funnels into the drain's SINGLE exit path.
//! Kill the consumer first (a kill is harmless if it already exited), then
//! `wait().await` to reap it (no zombies), clear the pane's pipe slot (only
//! if it still holds this pipe, a replace may have installed a successor),
//! and report asynchronously-discovered close reasons on the status line.
//! External close paths (stop, replace, pane teardown) signal the side-band
//! cancel watch; every await in the drain selects on it, so even a drain
//! parked in a blocked stdin write observes the cancel promptly.

use crate::LockExt;
use crate::error::DaemonError;
use crate::pane::Pane;
use crate::session::Session;
use bytes::Bytes;
use std::io::Error;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{broadcast, watch};

/// The per-pane pipe slot. Lives on `Pane`'s shared inner state; the drain
/// task holds its own `Arc` clone (NOT a `Pane` clone, which would keep the
/// pane's broadcast sender alive and mask the `Closed` arm).
pub type PipeSlot = Arc<StdMutex<Option<PipeHandle>>>;

/// Status-line text when the drain lagged the broadcast (data lost, so closed).
pub const MSG_TOO_SLOW: &str = "pipe-pane: consumer too slow — pipe closed";
/// Status-line text when the consumer exited on its own.
pub const MSG_CONSUMER_EXITED: &str = "pipe-pane: consumer exited";
/// Status-line text for `:pipe-pane` (stop) with a pipe running.
pub const MSG_STOPPED: &str = "pipe-pane stopped";
/// Status-line text for `:pipe-pane` (stop) with no pipe running.
pub const MSG_NO_PIPE: &str = "pipe-pane: no pipe";

/// Why a pipe closed. `Stopped`/`Replaced` are reported synchronously by the
/// prompt arm's return value; `TooSlow`/`ChildExited` are discovered by the
/// drain and surfaced via the session's status line; `PaneClosed` needs no
/// message (the pane is gone).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeCloseReason {
    Stopped,
    Replaced,
    TooSlow,
    ChildExited,
    PaneClosed,
}

/// Monotonic pipe id: lets the drain's exit path clear the pane slot only if
/// the slot still holds *its* pipe (a replace installs a successor).
static NEXT_PIPE_ID: AtomicU64 = AtomicU64::new(0);

/// The pane-slot half of one running pipe. Dropping the handle alone does NOT
/// stop the pipe. Close paths must call [`PipeHandle::cancel`] (the value is
/// recorded in the watch channel's shared state, so the handle may drop
/// immediately after).
pub struct PipeHandle {
    id: u64,
    /// Side-band cancel: close paths send `Some(reason)`; every drain await
    /// selects on the receiver.
    cancel_tx: watch::Sender<Option<PipeCloseReason>>,
    /// Consumer pid at spawn time. Exposed for tests' kill/reap (no-zombie)
    /// assertions.
    pid: Option<u32>,
}

impl PipeHandle {
    /// Signal the drain to close with `reason`. Sync and non-blocking, so it
    /// is callable from `Pane::kill_child` and the reader thread.
    pub fn cancel(&self, reason: PipeCloseReason) {
        let _ = self.cancel_tx.send(Some(reason));
    }

    /// The consumer's pid at spawn time (`None` if it exited before spawn
    /// returned the id).
    pub const fn pid(&self) -> Option<u32> {
        self.pid
    }
}

/// Take any pipe out of `slot` and cancel it with `reason`. Returns whether a
/// pipe was running. Used by the stop verb (`Stopped`) and pane teardown
/// (`PaneClosed`, from `Pane::kill_child` and the reader thread's EOF/Err
/// arms). Idempotent: a second call finds the slot empty.
pub fn cancel_slot(slot: &PipeSlot, reason: PipeCloseReason) -> bool {
    // invariant: pipe slot mutex held briefly; no await, no nested locks.
    let taken = slot.lock_recover().take();
    match taken {
        Some(handle) => {
            handle.cancel(reason);
            true
        }
        None => false,
    }
}

/// Start (or replace) a pipe on `pane`: spawn `shell -c cmd` at `cwd` with
/// stdin piped and stdout/stderr to /dev/null (the consumer is a sink, not a
/// pane), install a new `PipeHandle` in the pane's slot (cancelling any
/// predecessor with reason `Replaced`), and spawn the drain task. A spawn
/// failure returns `Err` and leaves any existing pipe untouched.
pub fn start_pipe(
    pane: &Pane,
    session: Weak<Session>,
    shell: &str,
    cmd: &str,
    cwd: Option<String>,
) -> Result<(), DaemonError> {
    let mut command = Command::new(shell);
    command
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    let mut child = command
        .spawn()
        .map_err(|e| DaemonError::Io(Error::other(format!("pipe-pane spawn: {e}"))))?;
    // invariant: `stdin(Stdio::piped())` above guarantees the handle exists.
    let stdin = child.stdin.take().expect("piped stdin");
    let rx = pane.subscribe_output();
    install_and_drain(pane.pipe_slot(), rx, child, stdin, session);
    Ok(())
}

/// Install a fresh `PipeHandle` in `slot` (cancelling any predecessor as
/// `Replaced`) and spawn the drain task. Split from [`start_pipe`] as the
/// test seam: tests can drive it with their own (tiny-capacity) broadcast
/// channel instead of a real pane's 256-chunk one.
pub(crate) fn install_and_drain(
    slot: PipeSlot,
    rx: broadcast::Receiver<Bytes>,
    child: Child,
    stdin: ChildStdin,
    session: Weak<Session>,
) {
    let id = NEXT_PIPE_ID.fetch_add(1, Ordering::Relaxed);
    let (cancel_tx, cancel_rx) = watch::channel(None);
    let handle = PipeHandle { id, cancel_tx, pid: child.id() };
    let prev = {
        // invariant: pipe slot mutex held briefly; no await, no nested locks.
        slot.lock_recover().replace(handle)
    };
    if let Some(prev) = prev {
        prev.cancel(PipeCloseReason::Replaced);
    }
    tokio::spawn(drain(slot, id, rx, child, stdin, cancel_rx, session));
}

/// Read the reason out of a completed cancel-watch wait. A closed channel
/// (handle dropped without a cancel, which should not happen but is harmless)
/// maps to `PaneClosed`, which reports nothing.
fn cancel_reason(
    r: Result<watch::Ref<'_, Option<PipeCloseReason>>, watch::error::RecvError>,
) -> PipeCloseReason {
    match r {
        Ok(v) => (*v).unwrap_or(PipeCloseReason::PaneClosed),
        Err(_) => PipeCloseReason::PaneClosed,
    }
}

/// The per-pipe drain task: forward broadcast chunks to the consumer's stdin
/// until any close condition fires, then run the single exit path (kill →
/// reap → clear slot → report). Holds no locks across awaits and only a
/// `Weak<Session>`, so it never deadlocks the session or keeps it alive.
async fn drain(
    slot: PipeSlot,
    id: u64,
    mut rx: broadcast::Receiver<Bytes>,
    mut child: Child,
    mut stdin: ChildStdin,
    mut cancel_rx: watch::Receiver<Option<PipeCloseReason>>,
    session: Weak<Session>,
) {
    let reason = loop {
        // Phase 1: wait for the next chunk (or a close condition).
        let chunk = tokio::select! {
            biased;
            r = cancel_rx.wait_for(Option::is_some) => break cancel_reason(r),
            // The consumer exited on its own (its `$SHELL -c` toplevel; a
            // grandchild may keep the pipe's read end open, which is why the
            // exit path below kills explicitly rather than just dropping
            // `stdin`).
            _ = child.wait() => break PipeCloseReason::ChildExited,
            res = rx.recv() => match res {
                Ok(chunk) => chunk,
                // Fell a full channel behind: data is irrecoverably lost, so
                // close rather than write a silently-gapped stream.
                Err(broadcast::error::RecvError::Lagged(_)) => break PipeCloseReason::TooSlow,
                // The pane's broadcast sender dropped (pane fully torn down).
                Err(broadcast::error::RecvError::Closed) => break PipeCloseReason::PaneClosed,
            },
        };
        // Phase 2: write it. The write parks when the consumer stops reading
        // and the OS pipe buffer fills; a parked write cannot observe a
        // channel close, so it must keep the side-band cancel selectable.
        tokio::select! {
            biased;
            r = cancel_rx.wait_for(Option::is_some) => break cancel_reason(r),
            w = stdin.write_all(&chunk) => {
                if w.is_err() {
                    // EPIPE: the consumer closed its stdin or died.
                    break PipeCloseReason::ChildExited;
                }
            }
        }
    };
    // SINGLE exit path. Kill first, and it has to be a kill (not just dropping
    // our stdin): `$SHELL -c` grandchildren can hold the pipe's read end open
    // past the shell's exit. Harmless if the consumer already exited. Then reap.
    let _ = child.start_kill();
    let _ = child.wait().await;
    {
        // invariant: pipe slot mutex held briefly; no await, no nested locks.
        let mut guard = slot.lock_recover();
        if guard.as_ref().is_some_and(|h| h.id == id) {
            *guard = None;
        }
    }
    // Surface the asynchronously-discovered reasons. Stopped/Replaced were
    // already reported synchronously by the prompt arm; PaneClosed has no
    // audience.
    if let Some(session) = session.upgrade() {
        match reason {
            PipeCloseReason::TooSlow => session.set_status_info(MSG_TOO_SLOW.into()).await,
            PipeCloseReason::ChildExited => {
                session.set_status_info(MSG_CONSUMER_EXITED.into()).await;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env;
    use nix::sys::signal;
    use nix::unistd::Pid;
    use std::time::{Duration, Instant};
    use tokio::time;

    /// Whether `pid` names a live (un-reaped) process. `kill -0` semantics:
    /// a zombie still counts as alive until its parent reaps it, which is exactly
    /// the signal the no-zombie assertions need.
    pub fn pid_alive(pid: u32) -> bool {
        signal::kill(Pid::from_raw(pid as i32), None).is_ok()
    }

    fn spawn_consumer(cmd: &str) -> (Child, ChildStdin) {
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(cmd)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn consumer");
        let stdin = child.stdin.take().expect("piped stdin");
        (child, stdin)
    }

    fn slot_empty(slot: &PipeSlot) -> bool {
        slot.lock().unwrap().is_none()
    }

    // The too-slow seam: a tiny (capacity-1) broadcast channel that is ALREADY
    // lagged when the drain first polls it (three sends against capacity 1), so
    // the drain's very first `recv()` returns `Lagged`. The pipe must close
    // (consumer killed AND reaped, slot cleared), the too-slow status must
    // land on the session, and the producer side must remain unaffected
    // (broadcast send never blocks or errors with a live receiver).
    #[tokio::test(flavor = "multi_thread")]
    async fn lagged_drain_closes_pipe_and_reports_too_slow() {
        let _g = test_env::isolate();
        let session = Session::new(
            "t-pipe-lag".into(),
            plexy_glass_protocol::SpawnSpec {
                program: "/bin/cat".into(),
                args: vec![],
                env: vec![],
                cwd: None,
            },
            plexy_glass_protocol::PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
            Arc::new(plexy_glass_config::built_in_default()),
        )
        .expect("session");

        let (tx, rx) = broadcast::channel::<Bytes>(1);
        // Lag the receiver by construction: 3 sends, capacity 1.
        for _ in 0..3 {
            tx.send(Bytes::from_static(b"x")).expect("send with live receiver");
        }
        let (child, stdin) = spawn_consumer("exec sleep 30");
        let pid = child.id().expect("consumer pid");
        let slot: PipeSlot = Arc::new(StdMutex::new(None));
        install_and_drain(
            Arc::clone(&slot),
            rx,
            child,
            stdin,
            Arc::downgrade(&session),
        );

        assert!(
            test_env::poll_until(Duration::from_secs(10), || slot_empty(&slot)).await,
            "lagged pipe never cleared its slot"
        );
        // Killed AND reaped: kill -0 fails only once the zombie is gone.
        assert!(
            test_env::poll_until(Duration::from_secs(10), || !pid_alive(pid)).await,
            "consumer survived (or was left a zombie) after the lagged close"
        );
        // The status line reports the too-slow close.
        assert!(
            test_env::poll_until(Duration::from_secs(10), || {
                let Ok(mut m) = session.window_manager.try_lock() else { return false };
                m.take_active_message() == Some(MSG_TOO_SLOW)
            })
            .await,
            "too-slow status never surfaced"
        );
        // Producer unaffected: the drain's receiver was dropped (count → 0)
        // but the sender itself remains valid. A zero receiver count also
        // confirms the drain task exited cleanly (it wasn't leaked).
        assert_eq!(
            tx.receiver_count(),
            0,
            "drain receiver must be dropped after the pipe closes"
        );
    }

    // A handle dropped from the slot must stop the drain via cancel even when
    // the cancel value lands after the handle (and its sender) are gone: the
    // watch channel records the final value in shared state.
    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_slot_kills_and_reaps_consumer() {
        let (tx, rx) = broadcast::channel::<Bytes>(256);
        let (child, stdin) = spawn_consumer("exec sleep 30");
        let pid = child.id().expect("consumer pid");
        let slot: PipeSlot = Arc::new(StdMutex::new(None));
        install_and_drain(Arc::clone(&slot), rx, child, stdin, Weak::new());
        assert!(!slot_empty(&slot), "handle installed");

        assert!(cancel_slot(&slot, PipeCloseReason::Stopped), "pipe was running");
        let deadline = Instant::now() + Duration::from_secs(10);
        while pid_alive(pid) && Instant::now() < deadline {
            time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!pid_alive(pid), "consumer survived (or zombied) after cancel");
        // Second stop is a no-op.
        assert!(!cancel_slot(&slot, PipeCloseReason::Stopped));
        drop(tx);
    }
}
