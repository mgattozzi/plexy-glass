//! `plexy-glass kill`: stop the daemon for the current runtime dir, or (with
//! `--all`) every plexy-glass daemon belonging to the current user.
//!
//! The default [`kill`] targets only the daemon whose pidfile lives in *this*
//! runtime dir (`RuntimePaths::for_current_user`), so it never disturbs a
//! second daemon the same user is running under a different `XDG_RUNTIME_DIR`
//! / `TMPDIR`. [`kill_all`] keeps the old sweep (`pgrep -u $UID -f
//! 'plexy-glass daemon'`) for cleaning up orphans left by a stale build, a
//! crashed kill, or an aborted `plexy-glass daemon --foreground`.
//!
//! Both send SIGTERM, poll briefly for graceful exit, then SIGKILL anything
//! still alive. Cleanup scrubs only the *current* runtime dir's socket/pidfile:
//! a SIGKILL'd daemon under a different runtime dir leaves its socket/pidfile
//! behind, but they are harmless, a new daemon unlinks a stale socket on bind
//! and rewrites the pidfile.

use std::path::Path;
use std::process::{self, Command};
use std::time::{Duration, Instant};
use std::{fs, io};

use nix::errno::Errno;
use nix::sys::signal::{self, Signal};
use nix::unistd::{self, Pid};
use plexy_glass_daemon::RuntimePaths;
use tokio::time;
use tracing::info;

use crate::error::ClientError;

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

/// Stop the daemon for the *current* runtime dir only. Identified by the
/// pidfile in this runtime dir, then confirmed against the live set of
/// plexy-glass daemons so a stale pidfile whose PID has been reused by an
/// unrelated process is ignored. Does not touch daemons under a different
/// `XDG_RUNTIME_DIR` / `TMPDIR`.
pub async fn kill() -> Result<KillOutcome, ClientError> {
    let paths = RuntimePaths::for_current_user().map_err(ClientError::Io)?;

    // Scope: only the PID recorded in this runtime dir's pidfile, and only if
    // it is actually one of our live daemons (guards against PID reuse).
    let target = read_pidfile(&paths.pidfile);
    let live = find_all_daemons()?;
    let pids = select_pids(target, &live);

    terminate(pids, &paths).await
}

/// Choose which PIDs the scoped `kill` terminates: the pidfile's PID, but only
/// if it is actually one of our live daemons, so a stale pidfile whose PID was
/// reused by an unrelated process is ignored (the central safety property).
/// Pure, for testability.
fn select_pids(target: Option<i32>, live: &[i32]) -> Vec<i32> {
    match target {
        Some(p) if live.contains(&p) => vec![p],
        _ => Vec::new(),
    }
}

/// Stop *every* plexy-glass daemon owned by the current user, regardless of
/// runtime dir. The pre-scoping behavior, kept for orphan cleanup (`kill
/// --all`).
pub async fn kill_all() -> Result<KillOutcome, ClientError> {
    let paths = RuntimePaths::for_current_user().map_err(ClientError::Io)?;
    let pids = find_all_daemons()?;
    terminate(pids, &paths).await
}

/// SIGTERM the given PIDs, poll for graceful exit, SIGKILL stragglers, then
/// clean up this runtime dir's socket/pidfile. An empty `pids` means nothing
/// to stop (still scrubs a stray socket).
async fn terminate(pids: Vec<i32>, paths: &RuntimePaths) -> Result<KillOutcome, ClientError> {
    if pids.is_empty() {
        cleanup_socket_only(paths);
        return Ok(KillOutcome::NoDaemon);
    }

    let total = pids.len();
    info!(
        count = total,
        "sending SIGTERM to plexy-glass daemon process(es)"
    );
    for pid in &pids {
        let nix_pid = Pid::from_raw(*pid);
        let _ = signal::kill(nix_pid, Signal::SIGTERM);
    }

    let mut alive: Vec<i32> = pids;
    let deadline = Instant::now() + GRACE_PERIOD;
    while Instant::now() < deadline && !alive.is_empty() {
        alive.retain(|&p| is_alive(p));
        if !alive.is_empty() {
            time::sleep(POLL_INTERVAL).await;
        }
    }

    let force_killed = !alive.is_empty();
    if force_killed {
        info!(
            stragglers = alive.len(),
            "sending SIGKILL to remaining daemons"
        );
        for pid in &alive {
            let _ = signal::kill(Pid::from_raw(*pid), Signal::SIGKILL);
        }
        let kill_deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < kill_deadline && !alive.is_empty() {
            alive.retain(|&p| is_alive(p));
            if !alive.is_empty() {
                time::sleep(POLL_INTERVAL).await;
            }
        }
    }

    cleanup(paths);
    if force_killed {
        Ok(KillOutcome::ForceKilled { count: total })
    } else {
        Ok(KillOutcome::Stopped { count: total })
    }
}

/// Read a daemon PID from `pidfile`. Returns `None` if the file is missing or
/// unparseable (the daemon writes `"{pid}\n"` after binding).
fn read_pidfile(pidfile: &Path) -> Option<i32> {
    let me = process::id() as i32;
    fs::read_to_string(pidfile)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .filter(|&p| p != me)
}

/// Return the PIDs of every plexy-glass daemon process owned by the current
/// UID, excluding our own process.
fn find_all_daemons() -> Result<Vec<i32>, ClientError> {
    let uid = unistd::getuid().as_raw();
    let me = process::id() as i32;
    let output = Command::new("pgrep")
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
    // ESRCH = no such process. EPERM = the PID exists but isn't ours (the
    // original daemon died and the PID was reused by another user). In both
    // cases it is not our daemon, so stop waiting on it, otherwise an EPERM
    // straggler would spin out the full grace/SIGKILL windows and mislabel the
    // outcome as ForceKilled for a process we never signalled.
    !matches!(
        signal::kill(Pid::from_raw(pid), None),
        Err(Errno::ESRCH | Errno::EPERM)
    )
}

fn cleanup(paths: &RuntimePaths) {
    let _ = fs::remove_file(&paths.socket);
    let _ = fs::remove_file(&paths.pidfile);
    // Leave daemon.lock alone, a future daemon will reuse it. Removing it
    // would race with a starting daemon that's about to flock it.
}

fn cleanup_socket_only(paths: &RuntimePaths) {
    // No matching processes; still scrub any orphaned socket file.
    let _ = fs::remove_file(&paths.socket);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_pids_ignores_a_reused_stale_pid() {
        // pidfile PID is one of our live daemons → target it.
        assert_eq!(select_pids(Some(42), &[10, 42, 99]), vec![42]);
        // pidfile PID is NOT live (stale, possibly reused) → ignore it.
        assert_eq!(select_pids(Some(42), &[10, 99]), Vec::<i32>::new());
        // No pidfile → nothing.
        assert_eq!(select_pids(None, &[10, 42]), Vec::<i32>::new());
        // No live daemons → nothing, even with a pidfile.
        assert_eq!(select_pids(Some(42), &[]), Vec::<i32>::new());
    }

    #[test]
    fn read_pidfile_trims_self_excludes_and_handles_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("daemon.pid");
        // Foreign PID with trailing newline → Some after trim.
        fs::write(&f, "12345\n").unwrap();
        assert_eq!(read_pidfile(&f), Some(12345));
        // Our own PID → None (self-exclusion guard).
        fs::write(&f, format!("{}\n", process::id())).unwrap();
        assert_eq!(read_pidfile(&f), None);
        // Garbage → None.
        fs::write(&f, "not-a-pid").unwrap();
        assert_eq!(read_pidfile(&f), None);
        // Missing → None.
        assert_eq!(read_pidfile(&dir.path().join("nope.pid")), None);
    }

    #[test]
    fn is_alive_classifies_self_and_impossible_pid() {
        assert!(is_alive(process::id() as i32), "our own process is alive");
        assert!(
            !is_alive(i32::MAX),
            "an impossible PID is not alive (ESRCH)"
        );
    }
}
