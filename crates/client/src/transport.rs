use crate::error::ClientError;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::UnixStream;
use tracing::{debug, info};

/// Connect to the daemon socket without spawning one if absent.
///
/// Returns `Err(ClientError::Connect { … })` when no daemon is reachable.
/// Scripting verbs (`cmd`, `send`, `capture`) use this so they never
/// accidentally start a daemon, since a missing daemon is a user error there.
pub async fn connect_only(socket: &Path) -> Result<UnixStream, ClientError> {
    UnixStream::connect(socket)
        .await
        .map_err(|source| ClientError::Connect { path: socket.to_path_buf(), source })
}

/// Connect to the daemon socket, spawning a new daemon if one is not running.
/// Returns the connected stream.
pub async fn connect_or_spawn(socket: &Path) -> Result<UnixStream, ClientError> {
    match UnixStream::connect(socket).await {
        Ok(s) => return Ok(s),
        Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) => {
            debug!(error = %e, path = %socket.display(), "daemon not reachable, spawning");
        }
        Err(e) => {
            return Err(ClientError::Connect { path: socket.to_path_buf(), source: e });
        }
    }

    spawn_daemon()?;

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut delay = Duration::from_millis(20);
    loop {
        match UnixStream::connect(socket).await {
            Ok(s) => {
                info!("connected to spawned daemon");
                return Ok(s);
            }
            Err(e) if std::time::Instant::now() >= deadline => {
                return Err(ClientError::Connect { path: socket.to_path_buf(), source: e });
            }
            Err(_) => {
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_millis(200));
            }
        }
    }
}

fn spawn_daemon() -> Result<(), ClientError> {
    let exe = std::env::current_exe().map_err(ClientError::Io)?;
    let mut cmd = std::process::Command::new(exe);
    let stderr = plexy_glass_daemon::RuntimePaths::for_current_user()
        .ok()
        .and_then(|p| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&p.log_file)
                .ok()
        })
        .map(std::process::Stdio::from)
        .unwrap_or_else(std::process::Stdio::null);
    cmd.arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(stderr);
    // SAFETY: setsid is async-signal-safe and called only in the child between
    // fork and exec. We do not touch any shared state from the closure.
    unsafe {
        cmd.pre_exec(|| {
            // Detach: new session so we are not in the client's pgrp.
            if nix::libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let _child = cmd.spawn().map_err(ClientError::Io)?;
    // We deliberately do not wait on the child; it will live on as a daemon.
    Ok(())
}

/// Compute the canonical socket path for this user.
pub fn default_socket_path() -> Result<PathBuf, ClientError> {
    let paths = plexy_glass_daemon::RuntimePaths::for_current_user().map_err(ClientError::Io)?;
    Ok(paths.socket)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn connects_when_a_listener_is_already_running() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sock");
        let listener = UnixListener::bind(&path).unwrap();

        let accept = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4];
            s.read_exact(&mut buf).await.unwrap();
            s.write_all(b"pong").await.unwrap();
        });

        let mut stream = connect_or_spawn(&path).await.expect("connect");
        stream.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pong");

        let _ = accept.await;
    }
}
