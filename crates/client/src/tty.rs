use std::io::{self, Write};
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::panic;

use nix::sys::signal::{self, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::sys::termios::{
    self, ControlFlags, InputFlags, LocalFlags, OutputFlags, SetArg, SpecialCharacterIndices,
    Termios,
};

use crate::error::ClientError;
use crate::negotiate::EnabledCaps;

/// RAII handle for the host TTY. Saves termios on construction and restores
/// it on Drop. Also resets cursor + alt-screen state on Drop.
pub struct HostTty {
    fd: RawFd,
    original: Termios,
    restored: bool,
}

impl HostTty {
    /// Put the given fd (typically `stdin`) into raw mode and return a guard.
    pub fn enter_raw(fd: BorrowedFd<'_>) -> Result<Self, ClientError> {
        let original = termios::tcgetattr(fd).map_err(|e| ClientError::Tty(e.to_string()))?;
        let mut raw = original.clone();

        // Standard "cfmakeraw" effects, applied via nix's typed flags.
        raw.input_flags.remove(
            InputFlags::IGNBRK
                | InputFlags::BRKINT
                | InputFlags::PARMRK
                | InputFlags::ISTRIP
                | InputFlags::INLCR
                | InputFlags::IGNCR
                | InputFlags::ICRNL
                | InputFlags::IXON,
        );
        raw.output_flags.remove(OutputFlags::OPOST);
        raw.local_flags.remove(
            LocalFlags::ECHO
                | LocalFlags::ECHONL
                | LocalFlags::ICANON
                | LocalFlags::ISIG
                | LocalFlags::IEXTEN,
        );
        raw.control_flags
            .remove(ControlFlags::CSIZE | ControlFlags::PARENB);
        raw.control_flags.insert(ControlFlags::CS8);
        raw.control_chars[SpecialCharacterIndices::VMIN as usize] = 1;
        raw.control_chars[SpecialCharacterIndices::VTIME as usize] = 0;

        termios::tcsetattr(fd, SetArg::TCSANOW, &raw)
            .map_err(|e| ClientError::Tty(e.to_string()))?;
        Ok(Self {
            fd: fd.as_raw_fd(),
            original,
            restored: false,
        })
    }

    /// Explicitly restore. Safe to call multiple times.
    pub fn restore(&mut self) -> Result<(), ClientError> {
        if self.restored {
            return Ok(());
        }
        // SAFETY: `self.fd` was obtained from a `BorrowedFd` whose lifetime
        // covered enter_raw; restore is the inverse and runs at most once.
        let fd = unsafe { BorrowedFd::borrow_raw(self.fd) };
        termios::tcsetattr(fd, SetArg::TCSANOW, &self.original)
            .map_err(|e| ClientError::Tty(e.to_string()))?;
        // Disable bracketed paste, mouse tracking, then re-enable cursor and
        // exit alternate screen. Reset the cursor style to the terminal's own
        // default (CSI 0 SP q) so a detach doesn't leave the real cursor stuck
        // in whatever shape the focused pane last set via DECSCUSR. The
        // keyboard-protocol / focus / theme inverse (kitty pop, modkeys reset,
        // ?1004l, ?2031l) is whatever negotiation actually enabled, see
        // `negotiated_teardown_bytes`.
        let mut out = io::stdout();
        let _ = out.write_all(b"\x1b[?2004l\x1b[?1003l\x1b[?1002l\x1b[?1006l\x1b[?25h");
        let _ = out.write_all(alt_pop_bytes());
        let _ = out.write_all(b"\x1b[0 q");
        let _ = out.write_all(&negotiated_teardown_bytes());
        let _ = out.flush();
        self.restored = true;
        Ok(())
    }

    /// Borrow the saved original termios snapshot. Used by
    /// `install_emergency_restore` to seed the global restore state.
    pub const fn original_termios(&self) -> &Termios {
        &self.original
    }
}

impl Drop for HostTty {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

/// Read the current TTY size from `fd` using TIOCGWINSZ.
pub fn current_size(fd: BorrowedFd<'_>) -> Result<plexy_glass_protocol::PtySize, ClientError> {
    use nix::libc::{TIOCGWINSZ, ioctl, winsize};
    let mut ws = winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `ws` is a valid out-pointer for TIOCGWINSZ on a borrowed fd we own
    // for the duration of the call.
    let rc = unsafe { ioctl(fd.as_raw_fd(), TIOCGWINSZ, &mut ws) };
    if rc != 0 {
        return Err(ClientError::Io(io::Error::last_os_error()));
    }
    Ok(plexy_glass_protocol::PtySize {
        rows: ws.ws_row,
        cols: ws.ws_col,
        pixel_width: ws.ws_xpixel,
        pixel_height: ws.ws_ypixel,
    })
}

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

static EMERGENCY_INSTALLED: OnceLock<()> = OnceLock::new();
static EMERGENCY_FD: OnceLock<RawFd> = OnceLock::new();
// Termios contains a `RefCell<libc::termios>` (nix 0.31) so it is not `Sync`.
// Wrap it in a `Mutex` so the `OnceLock` is safe to share between threads.
static EMERGENCY_TERMIOS: OnceLock<Mutex<Termios>> = OnceLock::new();
static ARMED: AtomicBool = AtomicBool::new(false);
/// Replaceable, NOT a `OnceLock`: `run`'s attach loop re-probes the terminal and
/// calls `set_enabled_caps` on **every** iteration, so a set-once cell would pin
/// the first attach's caps forever and a reconnect to a differently-capable
/// terminal would tear down the wrong inverse.
static ENABLED_CAPS: Mutex<Option<EnabledCaps>> = Mutex::new(None);
/// Whether we have pushed the alternate screen and owe a pop. See
/// [`set_alt_active`].
static ALT_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Record what the client enabled outward so both teardown paths emit the exact
/// inverse. Called once per attach, and **overwrites**: see `ENABLED_CAPS`.
pub fn set_enabled_caps(caps: EnabledCaps) {
    if let Ok(mut slot) = ENABLED_CAPS.lock() {
        *slot = Some(caps);
    }
}

/// Record that the alternate screen has been entered (`true`) or left (`false`).
///
/// The picker owns the alt screen and writes the `?1049h`/`?1049l` itself, on the
/// same writer it renders with; this is only the bookkeeping that lets the
/// teardown paths below know whether a pop is owed. Without it they emit
/// `?1049l` unconditionally, which is wrong in both directions: on a failure
/// that never opened a picker (a cold `-H badhost`, say) it pops a buffer we
/// never pushed, and `?1049l` is defined to restore the cursor as DECRC — from a
/// slot we never saved — so the cursor jumps somewhere arbitrary and whatever
/// prints next lands there. That was the garbled error-over-the-picker-box.
pub fn set_alt_active(active: bool) {
    ALT_ACTIVE.store(active, Ordering::SeqCst);
}

/// The alt-screen pop, but only when we actually pushed. Consumes the flag so a
/// double teardown (guard `Drop` then the emergency path) pops exactly once.
fn alt_pop_bytes() -> &'static [u8] {
    if ALT_ACTIVE.swap(false, Ordering::SeqCst) {
        b"\x1b[?1049l"
    } else {
        b""
    }
}

/// The kbd/focus/theme teardown bytes (precise inverse of the enable set), or
/// empty if negotiation never recorded caps.
///
/// `try_lock`, not `lock`: this runs from the panic hook and the signal handler,
/// where blocking on a mutex a panicking thread might still hold would wedge the
/// one path whose entire job is to work when everything else is broken. Losing
/// the kbd teardown in that (vanishingly unlikely) race is a far better outcome
/// than hanging, and the termios restore below does not depend on it.
fn negotiated_teardown_bytes() -> Vec<u8> {
    ENABLED_CAPS
        .try_lock()
        .ok()
        .and_then(|slot| slot.as_ref().map(EnabledCaps::teardown_bytes))
        .unwrap_or_default()
}

/// Install a panic hook and SIGINT/SIGTERM/SIGHUP handlers that restore the
/// host TTY before the process dies. Call this once, ideally right after
/// constructing the `HostTty` guard.
///
/// The signal handlers re-raise the signal with the default disposition so
/// the parent shell observes the canonical exit status.
pub fn install_emergency_restore(fd: BorrowedFd<'_>, snapshot: &Termios) {
    // invariant: process-scoped, install exactly once. `run`'s loop calls this
    // every attach, and the set-once guard is load-bearing rather than an
    // oversight: re-running would `take_hook` the hook WE installed last time and
    // wrap it again (one more nested `restore_from_static` per reconnect), and
    // leak another signal task per reconnect, all racing to restore and re-raise.
    // Nothing it captures is per-attach anyway — the fd is always stdin, and the
    // termios snapshot is identical every time because each iteration's guard
    // restores the original before the next `enter_raw` re-reads it. The one
    // genuinely per-attach thing, the enabled caps, is read dynamically by
    // `restore_from_static` via `negotiated_teardown_bytes`, so it stays fresh
    // without reinstalling anything here.
    if EMERGENCY_INSTALLED.set(()).is_err() {
        return;
    }
    let _ = EMERGENCY_FD.set(fd.as_raw_fd());
    let _ = EMERGENCY_TERMIOS.set(Mutex::new(snapshot.clone()));
    ARMED.store(true, Ordering::SeqCst);

    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        restore_from_static();
        default_hook(info);
    }));

    tokio::spawn(async {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT");
        let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM");
        let mut sighup = signal(SignalKind::hangup()).expect("install SIGHUP");
        let (sig, num) = tokio::select! {
            _ = sigint.recv() => ("SIGINT", Signal::SIGINT),
            _ = sigterm.recv() => ("SIGTERM", Signal::SIGTERM),
            _ = sighup.recv() => ("SIGHUP", Signal::SIGHUP),
        };
        tracing::warn!(
            signal = sig,
            "received signal, restoring tty and re-raising"
        );
        restore_from_static();
        // Reset disposition and re-raise so the parent shell sees the
        // canonical exit signal.
        // SAFETY: sigaction is unsafe; we install SIG_DFL with an empty mask
        // and empty flags for a known signal. raise is safe in nix 0.31.
        unsafe {
            let _ = signal::sigaction(
                num,
                &SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty()),
            );
        }
        let _ = signal::raise(num);
    });
}

fn restore_from_static() {
    if !ARMED.swap(false, Ordering::SeqCst) {
        return;
    }
    let Some(fd) = EMERGENCY_FD.get().copied() else {
        return;
    };
    let Some(snap_lock) = EMERGENCY_TERMIOS.get() else {
        return;
    };
    let Ok(snap) = snap_lock.lock() else {
        return;
    };
    // SAFETY: fd remains valid as long as the process holds stdin/stdout.
    let fd = unsafe { BorrowedFd::borrow_raw(fd) };
    let _ = termios::tcsetattr(fd, SetArg::TCSANOW, &snap);
    let mut out = io::stdout();
    let _ = out.write_all(b"\x1b[?2004l\x1b[?1003l\x1b[?1002l\x1b[?1006l\x1b[?25h");
    let _ = out.write_all(alt_pop_bytes());
    let _ = out.write_all(b"\x1b[0 q");
    let _ = out.write_all(&negotiated_teardown_bytes());
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;

    use nix::pty::openpty;

    use super::*;

    #[test]
    fn enter_raw_then_drop_restores_termios() {
        let pair = openpty(None, None).expect("openpty");
        let original_before = termios::tcgetattr(pair.master.as_fd()).expect("tcgetattr");

        {
            let _guard = HostTty::enter_raw(pair.master.as_fd()).expect("enter_raw");
            let raw_now = termios::tcgetattr(pair.master.as_fd()).expect("tcgetattr while raw");
            assert!(
                !raw_now.local_flags.contains(LocalFlags::ICANON),
                "ICANON should be cleared in raw mode"
            );
            assert!(
                !raw_now.local_flags.contains(LocalFlags::ECHO),
                "ECHO should be cleared in raw mode"
            );
        }

        let after_drop = termios::tcgetattr(pair.master.as_fd()).expect("tcgetattr after drop");
        // PENDIN is a kernel-managed transient bit (set when the line discipline has
        // re-input queued from a mode change). Mask it out, it isn't user-controllable
        // termios state and isn't part of what we save/restore.
        let mask = !LocalFlags::PENDIN;
        assert_eq!(
            after_drop.local_flags & mask,
            original_before.local_flags & mask,
            "local_flags must be restored on Drop"
        );
        assert_eq!(
            after_drop.input_flags, original_before.input_flags,
            "input_flags must be restored on Drop"
        );
    }

    #[test]
    fn explicit_restore_is_idempotent() {
        let pair = openpty(None, None).expect("openpty");
        let mut guard = HostTty::enter_raw(pair.master.as_fd()).expect("enter_raw");
        guard.restore().expect("restore");
        guard.restore().expect("restore again");
    }
}
