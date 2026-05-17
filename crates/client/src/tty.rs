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
        // Belt-and-suspenders: re-enable cursor, exit alternate screen.
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[?25h\x1b[?1049l");
        let _ = out.flush();
        self.restored = true;
        Ok(())
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
