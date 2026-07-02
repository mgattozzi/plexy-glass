use crate::error::DaemonError;
use crate::paths::RuntimePaths;
use nix::fcntl::{Flock, FlockArg};
use nix::unistd;
use std::fs;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use tokio::net::UnixListener;
use tracing::{info, warn};

/// Holds the daemon's exclusive `flock` + listening socket. Both are released
/// when this value is dropped.
#[derive(Debug)]
pub struct Listener {
    pub paths: RuntimePaths,
    pub socket: UnixListener,
    _lock: Flock<File>,
}

impl Listener {
    /// Acquire the lockfile, bind (or rebind) the socket, and return a listener.
    /// On stale-socket conditions, this unlinks and re-binds.
    pub fn bind(paths: RuntimePaths) -> Result<Self, DaemonError> {
        paths.create_dirs()?;

        // Open the lockfile (create if needed) and take an exclusive non-blocking flock.
        let lockfile = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&paths.lockfile)?;
        let lock = Flock::lock(lockfile, FlockArg::LockExclusiveNonblock).map_err(|(_f, errno)| {
            DaemonError::LockfileBusy {
                path: paths.lockfile.clone(),
                source: io::Error::from_raw_os_error(errno as i32),
            }
        })?;

        // Refuse to clobber another user's socket.
        if let Ok(meta) = fs::metadata(&paths.socket)
            && meta.uid() != unistd::getuid().as_raw()
        {
            return Err(DaemonError::SocketOwnedByOtherUser {
                path: paths.socket,
            });
        }

        // Unlink any stale socket file and bind.
        match fs::remove_file(&paths.socket) {
            Ok(()) => warn!(path = %paths.socket.display(), "removed stale socket"),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(DaemonError::Io(e)),
        }
        let socket = UnixListener::bind(&paths.socket)?;
        // 0600 socket: only the owner can connect.
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))?;

        // Write our PID down so the user can inspect it.
        let pid = unistd::getpid().as_raw();
        fs::write(&paths.pidfile, format!("{pid}\n"))?;

        info!(socket = %paths.socket.display(), pid, "daemon listening");
        Ok(Self {
            paths,
            socket,
            _lock: lock,
        })
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // Best-effort cleanup. We keep the lockfile around (just release the lock by
        // dropping the `File`) but remove the socket and pid file so a future daemon
        // doesn't see stale artifacts.
        let _ = fs::remove_file(&self.paths.socket);
        let _ = fs::remove_file(&self.paths.pidfile);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, RuntimePaths) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::for_dirs(&tmp.path().join("rt"), &tmp.path().join("log"));
        (tmp, paths)
    }

    #[tokio::test]
    async fn bind_succeeds_when_nothing_else_is_running() {
        let (_tmp, paths) = fixture();
        let l = Listener::bind(paths.clone()).expect("bind");
        assert!(paths.socket.exists());
        assert!(paths.pidfile.exists());
        drop(l);
        assert!(!paths.socket.exists());
    }

    #[tokio::test]
    async fn bind_recovers_from_stale_socket_file() {
        let (_tmp, paths) = fixture();
        paths.create_dirs().unwrap();
        // Plant a stale plain file at the socket path.
        fs::write(&paths.socket, b"stale").unwrap();
        let l = Listener::bind(paths.clone()).expect("bind despite stale file");
        assert!(paths.socket.exists());
        drop(l);
    }

    #[tokio::test]
    async fn second_bind_fails_with_lockfile_busy() {
        let (_tmp, paths) = fixture();
        let _l1 = Listener::bind(paths.clone()).expect("first bind");
        let err = Listener::bind(paths).expect_err("second bind must fail");
        match err {
            DaemonError::LockfileBusy { .. } => {}
            other => panic!("expected LockfileBusy, got {other:?}"),
        }
    }
}
