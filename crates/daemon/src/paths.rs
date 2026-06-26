use std::path::{Path, PathBuf};

/// Filesystem layout for one running daemon.
#[derive(Debug, Clone)]
pub struct RuntimePaths {
    /// Directory holding the socket, lockfile, and pidfile.
    pub runtime_dir: PathBuf,
    /// Unix socket the daemon listens on.
    pub socket: PathBuf,
    /// Advisory lockfile; an exclusive flock here means "I am the daemon".
    pub lockfile: PathBuf,
    /// File holding the daemon's PID, written after the lock is acquired.
    pub pidfile: PathBuf,
    /// Directory for logs.
    pub log_dir: PathBuf,
    /// Default log file path.
    pub log_file: PathBuf,
}

impl RuntimePaths {
    /// Resolve the canonical paths for the running user.
    /// On Linux uses `$XDG_RUNTIME_DIR` if set, else `$TMPDIR/plexy-glass-$UID`.
    /// On macOS uses `$TMPDIR/plexy-glass-$UID` and `~/Library/Logs/plexy-glass`.
    pub fn for_current_user() -> std::io::Result<Self> {
        let uid = nix::unistd::getuid().as_raw();
        let runtime_dir = Self::resolve_runtime_dir(uid)?;
        let log_dir = Self::resolve_log_dir()?;
        Ok(Self::for_dirs(&runtime_dir, &log_dir))
    }

    /// Build a `RuntimePaths` for explicit directories (used in tests).
    pub fn for_dirs(runtime_dir: &Path, log_dir: &Path) -> Self {
        Self {
            runtime_dir: runtime_dir.to_path_buf(),
            socket: runtime_dir.join("daemon.sock"),
            lockfile: runtime_dir.join("daemon.lock"),
            pidfile: runtime_dir.join("daemon.pid"),
            log_dir: log_dir.to_path_buf(),
            log_file: log_dir.join("daemon.log"),
        }
    }

    fn resolve_runtime_dir(uid: u32) -> std::io::Result<PathBuf> {
        // An explicit instance root wins on every platform. This is the single
        // knob for running a second, fully isolated daemon next to the default
        // one (see also `sessions_dir` / `resolve_log_dir`), and it's the only
        // way to do it on macOS, where XDG_RUNTIME_DIR is ignored and the
        // canonical path is otherwise a fixed per-UID dir, so all invocations
        // share one daemon.
        if let Some(root) = std::env::var_os("PLEXY_GLASS_DIR") {
            return Ok(PathBuf::from(root).join("run"));
        }
        // Per the spec: only honor XDG_RUNTIME_DIR on Linux. On macOS the
        // canonical path is $TMPDIR/plexy-glass-$UID; a stray XDG_RUNTIME_DIR
        // (some users set one for cross-platform consistency) would otherwise
        // point at /run/user/$UID which doesn't exist on macOS.
        #[cfg(target_os = "linux")]
        if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
            return Ok(PathBuf::from(dir).join("plexy-glass"));
        }
        let tmp = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        Ok(tmp.join(format!("plexy-glass-{uid}")))
    }

    fn resolve_log_dir() -> std::io::Result<PathBuf> {
        // Same instance-root override as `resolve_runtime_dir`: keep a test
        // daemon's logs out of the daily driver's log file.
        if let Some(root) = std::env::var_os("PLEXY_GLASS_DIR") {
            return Ok(PathBuf::from(root).join("logs"));
        }
        #[cfg(target_os = "macos")]
        {
            if let Some(home) = std::env::var_os("HOME") {
                return Ok(PathBuf::from(home).join("Library/Logs/plexy-glass"));
            }
        }
        if let Some(state) = std::env::var_os("XDG_STATE_HOME") {
            return Ok(PathBuf::from(state).join("plexy-glass"));
        }
        if let Some(home) = std::env::var_os("HOME") {
            return Ok(PathBuf::from(home).join(".local/state/plexy-glass"));
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "neither $HOME nor $XDG_STATE_HOME is set",
        ))
    }

    /// Ensure `runtime_dir` and `log_dir` exist with restrictive permissions.
    pub fn create_dirs(&self) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        for dir in [&self.runtime_dir, &self.log_dir] {
            std::fs::create_dir_all(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_dirs_assembles_expected_layout() {
        let rt = PathBuf::from("/run/user/1000/plexy-glass");
        let log = PathBuf::from("/home/m/.local/state/plexy-glass");
        let paths = RuntimePaths::for_dirs(&rt, &log);
        assert_eq!(paths.socket, rt.join("daemon.sock"));
        assert_eq!(paths.lockfile, rt.join("daemon.lock"));
        assert_eq!(paths.pidfile, rt.join("daemon.pid"));
        assert_eq!(paths.log_file, log.join("daemon.log"));
    }

    #[test]
    fn instance_dir_override_roots_runtime_log_and_sessions() {
        // PLEXY_GLASS_DIR roots a fully isolated instance on every platform,
        // the knob that lets a second daemon run beside the daily driver.
        let _lock = crate::STATE_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = std::env::var_os("PLEXY_GLASS_DIR");
        // SAFETY: STATE_ENV_LOCK held for the test body; restored below.
        unsafe { std::env::set_var("PLEXY_GLASS_DIR", "/tmp/plexy-instance") };
        // The override returns early before any fallible HOME lookup, so these
        // cannot fail regardless of the test environment.
        let paths = RuntimePaths::for_current_user().unwrap();
        // Restore BEFORE asserting so a failed assert can't leak the var into
        // sibling env-sensitive tests.
        unsafe {
            match &prev {
                Some(v) => std::env::set_var("PLEXY_GLASS_DIR", v),
                None => std::env::remove_var("PLEXY_GLASS_DIR"),
            }
        }
        assert_eq!(paths.socket, PathBuf::from("/tmp/plexy-instance/run/daemon.sock"));
        assert_eq!(paths.pidfile, PathBuf::from("/tmp/plexy-instance/run/daemon.pid"));
        assert_eq!(paths.log_file, PathBuf::from("/tmp/plexy-instance/logs/daemon.log"));
    }

    #[test]
    fn create_dirs_makes_directories_with_0700() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let rt = tmp.path().join("rt");
        let log = tmp.path().join("log");
        let paths = RuntimePaths::for_dirs(&rt, &log);
        paths.create_dirs().unwrap();
        for dir in [&rt, &log] {
            let mode = std::fs::metadata(dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "expected 0o700 on {}", dir.display());
        }
    }
}
