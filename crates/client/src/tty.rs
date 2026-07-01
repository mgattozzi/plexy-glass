use crate::error::ClientError;
use nix::sys::termios::{
    self, ControlFlags, InputFlags, LocalFlags, OutputFlags, SetArg, SpecialCharacterIndices,
    Termios,
};
use std::io::Write;
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};

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
        // exit alternate screen. The keyboard-protocol / focus / theme inverse
        // (kitty pop, modkeys reset, ?1004l, ?2031l) is whatever negotiation
        // actually enabled, see `negotiated_teardown_bytes`.
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[?2004l\x1b[?1003l\x1b[?1002l\x1b[?1006l\x1b[?25h\x1b[?1049l");
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
        return Err(ClientError::Io(std::io::Error::last_os_error()));
    }
    Ok(plexy_glass_protocol::PtySize {
        rows: ws.ws_row,
        cols: ws.ws_col,
        pixel_width: ws.ws_xpixel,
        pixel_height: ws.ws_ypixel,
    })
}

use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};

static EMERGENCY_INSTALLED: OnceLock<()> = OnceLock::new();
static EMERGENCY_FD: OnceLock<RawFd> = OnceLock::new();
// Termios contains a `RefCell<libc::termios>` (nix 0.31) so it is not `Sync`.
// Wrap it in a `Mutex` so the `OnceLock` is safe to share between threads.
static EMERGENCY_TERMIOS: OnceLock<Mutex<Termios>> = OnceLock::new();
static ARMED: AtomicBool = AtomicBool::new(false);
static ENABLED_CAPS: OnceLock<crate::negotiate::EnabledCaps> = OnceLock::new();

/// Record what the client enabled outward so both teardown paths emit the exact
/// inverse. Call once, right after the negotiation phase.
pub fn set_enabled_caps(caps: crate::negotiate::EnabledCaps) {
    let _ = ENABLED_CAPS.set(caps);
}

/// The kbd/focus/theme teardown bytes (precise inverse of the enable set), or
/// empty if negotiation never recorded caps.
fn negotiated_teardown_bytes() -> Vec<u8> {
    ENABLED_CAPS
        .get()
        .map(super::negotiate::EnabledCaps::teardown_bytes)
        .unwrap_or_default()
}

/// Install a panic hook and SIGINT/SIGTERM/SIGHUP handlers that restore the
/// host TTY before the process dies. Call this once, ideally right after
/// constructing the `HostTty` guard.
///
/// The signal handlers re-raise the signal with the default disposition so
/// the parent shell observes the canonical exit status.
pub fn install_emergency_restore(fd: BorrowedFd<'_>, snapshot: &Termios) {
    if EMERGENCY_INSTALLED.set(()).is_err() {
        return;
    }
    let _ = EMERGENCY_FD.set(fd.as_raw_fd());
    let _ = EMERGENCY_TERMIOS.set(Mutex::new(snapshot.clone()));
    ARMED.store(true, Ordering::SeqCst);

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_from_static();
        default_hook(info);
    }));

    tokio::spawn(async {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT");
        let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM");
        let mut sighup = signal(SignalKind::hangup()).expect("install SIGHUP");
        let (sig, num) = tokio::select! {
            _ = sigint.recv() => ("SIGINT", nix::sys::signal::Signal::SIGINT),
            _ = sigterm.recv() => ("SIGTERM", nix::sys::signal::Signal::SIGTERM),
            _ = sighup.recv() => ("SIGHUP", nix::sys::signal::Signal::SIGHUP),
        };
        tracing::warn!(signal = sig, "received signal, restoring tty and re-raising");
        restore_from_static();
        // Reset disposition and re-raise so the parent shell sees the
        // canonical exit signal.
        // SAFETY: sigaction is unsafe; we install SIG_DFL with an empty mask
        // and empty flags for a known signal. raise is safe in nix 0.31.
        unsafe {
            let _ = nix::sys::signal::sigaction(
                num,
                &nix::sys::signal::SigAction::new(
                    nix::sys::signal::SigHandler::SigDfl,
                    nix::sys::signal::SaFlags::empty(),
                    nix::sys::signal::SigSet::empty(),
                ),
            );
        }
        let _ = nix::sys::signal::raise(num);
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
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x1b[?2004l\x1b[?1003l\x1b[?1002l\x1b[?1006l\x1b[?25h\x1b[?1049l");
    let _ = out.write_all(&negotiated_teardown_bytes());
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::pty::openpty;
    use std::os::fd::AsFd;

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
