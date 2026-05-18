//! `plexy-glass kill`: stop every plexy-glass daemon process belonging to
//! the current user.
//!
//! Sweeps `pgrep -u $UID -f 'plexy-glass daemon'` so orphaned daemons left
//! behind by a stale build, a crashed kill, or an aborted `plexy-glass
//! daemon --foreground` all get cleaned up in one shot. Sends SIGTERM,
//! polls briefly for graceful exit, then SIGKILLs anything still alive.
//! Finally removes the socket and pidfile.

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
    /// No matching daemon process was running. Any stray socket/pidfile was
    /// cleaned up anyway.
    NoDaemon,
    /// `n` daemon(s) were terminated via SIGTERM within the grace period.
    Stopped { count: usize },
    /// `n` daemon(s) were terminated, but at least one needed SIGKILL.
    ForceKilled { count: usize },
}

pub async fn kill() -> Result<KillOutcome, ClientError> {
    let paths = RuntimePaths::for_current_user().map_err(ClientError::Io)?;

    let pids = find_all_daemons()?;

    if pids.is_empty() {
        cleanup_socket_only(&paths);
        return Ok(KillOutcome::NoDaemon);
    }

    let total = pids.len();
    info!(count = total, "sending SIGTERM to plexy-glass daemon process(es)");
    for pid in &pids {
        let nix_pid = nix::unistd::Pid::from_raw(*pid);
        let _ = nix::sys::signal::kill(nix_pid, nix::sys::signal::Signal::SIGTERM);
    }

    let mut alive: Vec<i32> = pids;
    let deadline = Instant::now() + GRACE_PERIOD;
    while Instant::now() < deadline && !alive.is_empty() {
        alive.retain(|&p| is_alive(p));
        if !alive.is_empty() {
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    let force_killed = !alive.is_empty();
    if force_killed {
        info!(stragglers = alive.len(), "sending SIGKILL to remaining daemons");
        for pid in &alive {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(*pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let kill_deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < kill_deadline && !alive.is_empty() {
            alive.retain(|&p| is_alive(p));
            if !alive.is_empty() {
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        }
    }

    cleanup(&paths);
    if force_killed {
        Ok(KillOutcome::ForceKilled { count: total })
    } else {
        Ok(KillOutcome::Stopped { count: total })
    }
}

/// Return the PIDs of every plexy-glass daemon process owned by the current
/// UID, excluding our own process.
fn find_all_daemons() -> Result<Vec<i32>, ClientError> {
    let uid = nix::unistd::getuid().as_raw();
    let me = std::process::id() as i32;
    let output = std::process::Command::new("pgrep")
        .arg("-u")
        .arg(uid.to_string())
        .arg("-f")
        .arg("plexy-glass daemon")
        .output()
        .map_err(|e| ClientError::Io(io::Error::other(format!("pgrep: {e}"))))?;

    // pgrep exits non-zero when no processes match, and that's not an error.
    let pids: Vec<i32> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<i32>().ok())
        .filter(|&pid| pid != me)
        .collect();
    Ok(pids)
}

fn is_alive(pid: i32) -> bool {
    !matches!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None),
        Err(nix::errno::Errno::ESRCH)
    )
}

fn cleanup(paths: &RuntimePaths) {
    let _ = std::fs::remove_file(&paths.socket);
    let _ = std::fs::remove_file(&paths.pidfile);
    // Leave daemon.lock alone, a future daemon will reuse it. Removing it
    // would race with a starting daemon that's about to flock it.
}

fn cleanup_socket_only(paths: &RuntimePaths) {
    // No matching processes; still scrub any orphaned socket file.
    let _ = std::fs::remove_file(&paths.socket);
}
