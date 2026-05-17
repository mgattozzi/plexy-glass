//! `plexy-glass kill`: stop the running daemon, if any.
//!
//! Reads the daemon pidfile, sends SIGTERM, and waits briefly for graceful
//! exit. Falls back to SIGKILL after the deadline. Removes the socket and
//! pidfile (the daemon's own Drop guard only runs on normal exit, not on
//! signal termination).

use crate::error::ClientError;
use plexy_glass_daemon::RuntimePaths;
use std::io;
use std::time::{Duration, Instant};
use tracing::info;

const GRACE_PERIOD: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Outcome of a kill attempt, surfaced to the user.
#[derive(Debug)]
pub enum KillOutcome {
    NoDaemon,
    Stopped,
    ForceKilled,
}

pub async fn kill() -> Result<KillOutcome, ClientError> {
    let paths = RuntimePaths::for_current_user().map_err(ClientError::Io)?;

    let pid = match std::fs::read_to_string(&paths.pidfile) {
        Ok(s) => s
            .trim()
            .parse::<i32>()
            .map_err(|e| ClientError::Io(io::Error::other(format!("invalid pidfile: {e}"))))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            cleanup_socket_only(&paths);
            return Ok(KillOutcome::NoDaemon);
        }
        Err(e) => return Err(ClientError::Io(e)),
    };

    let nix_pid = nix::unistd::Pid::from_raw(pid);
    match nix::sys::signal::kill(nix_pid, nix::sys::signal::Signal::SIGTERM) {
        Ok(()) => info!(pid, "sent SIGTERM to daemon"),
        Err(nix::errno::Errno::ESRCH) => {
            cleanup(&paths);
            return Ok(KillOutcome::NoDaemon);
        }
        Err(e) => {
            return Err(ClientError::Io(io::Error::other(format!(
                "kill({pid}, SIGTERM): {e}"
            ))));
        }
    }

    let deadline = Instant::now() + GRACE_PERIOD;
    while Instant::now() < deadline {
        if matches!(
            nix::sys::signal::kill(nix_pid, None),
            Err(nix::errno::Errno::ESRCH)
        ) {
            cleanup(&paths);
            return Ok(KillOutcome::Stopped);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    let _ = nix::sys::signal::kill(nix_pid, nix::sys::signal::Signal::SIGKILL);
    // Briefly wait for the process to actually disappear so socket cleanup is safe.
    let kill_deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < kill_deadline {
        if matches!(
            nix::sys::signal::kill(nix_pid, None),
            Err(nix::errno::Errno::ESRCH)
        ) {
            break;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    cleanup(&paths);
    Ok(KillOutcome::ForceKilled)
}

fn cleanup(paths: &RuntimePaths) {
    let _ = std::fs::remove_file(&paths.socket);
    let _ = std::fs::remove_file(&paths.pidfile);
    // Leave daemon.lock alone, a future daemon will reuse it. Removing it
    // would race with a starting daemon that's about to flock it.
}

fn cleanup_socket_only(paths: &RuntimePaths) {
    // pidfile absent → daemon never started or was already cleaned. Still try
    // to remove an orphaned socket file just in case.
    let _ = std::fs::remove_file(&paths.socket);
}
