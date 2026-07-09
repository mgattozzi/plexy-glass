use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use std::{env, io};

use nix::libc;
use tokio::io::{AsyncRead, AsyncWrite, split};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::time;
use tracing::{debug, info};

use crate::error::ClientError;

/// How a connection opens: auto-spawn the daemon (interactive/list) or fail if
/// none is running (scripting verbs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connect {
    Spawn,
    Only,
}

/// Where a verb runs: the local daemon, or a remote one over SSH.
#[derive(Debug, Clone, Default)]
pub struct Target {
    /// `Some(ssh-target)` routes over SSH; `None` uses the local socket.
    pub host: Option<String>,
    /// Explicit remote `plexy-glass` path (`--remote-bin`).
    pub remote_bin: Option<String>,
    /// `--install`: provision the remote binary before connecting.
    pub install: bool,
}

/// The remote path to invoke over SSH. `--remote-bin` wins; else the
/// `--install` cache path if installing; else bare `plexy-glass` (found only on
/// the remote's non-interactive PATH). NOTE: Task 4 inserts the cache tier.
pub fn resolve_remote_bin(target: &Target) -> String {
    if let Some(bin) = &target.remote_bin {
        return bin.clone();
    }
    "plexy-glass".to_string()
}

/// Build the argv for `ssh` (after the program name). `-T` disables remote PTY
/// allocation so the framed byte stream stays 8-bit clean.
pub fn ssh_args(host: &str, remote_bin: &str, connect: Connect) -> Vec<String> {
    let mut args = vec![
        "-T".to_string(),
        host.to_string(),
        remote_bin.to_string(),
        "bridge".to_string(),
    ];
    if connect == Connect::Only {
        args.push("--no-spawn".to_string());
    }
    args
}

#[cfg(test)]
mod ssh_tests {
    use super::*;

    #[test]
    fn ssh_args_spawn_has_no_no_spawn_flag() {
        assert_eq!(
            ssh_args("prod", "plexy-glass", Connect::Spawn),
            vec!["-T", "prod", "plexy-glass", "bridge"]
        );
    }

    #[test]
    fn ssh_args_only_appends_no_spawn() {
        assert_eq!(
            ssh_args("u@h", "/opt/pg", Connect::Only),
            vec!["-T", "u@h", "/opt/pg", "bridge", "--no-spawn"]
        );
    }

    #[test]
    fn resolve_remote_bin_prefers_explicit() {
        let t = Target {
            host: Some("h".into()),
            remote_bin: Some("/x/pg".into()),
            install: false,
        };
        assert_eq!(resolve_remote_bin(&t), "/x/pg");
        let t2 = Target {
            host: Some("h".into()),
            remote_bin: None,
            install: false,
        };
        assert_eq!(resolve_remote_bin(&t2), "plexy-glass");
    }
}

/// A daemon connection as a split reader/writer, from the local socket or an
/// SSH `bridge` child. `child` keeps the SSH process (and thus the pipes)
/// alive for the transport's lifetime, and lets `ssh_not_found` inspect its
/// exit status; `None` for local.
pub struct Transport {
    pub reader: Box<dyn AsyncRead + Send + Unpin>,
    pub writer: Box<dyn AsyncWrite + Send + Unpin>,
    child: Option<Child>,
}

impl Transport {
    /// After a failed handshake/read on an SSH transport, check whether the ssh
    /// child exited 127 (remote command not found) and, if so, return the
    /// clearer error to surface instead of a bare EOF. `None` for local, or when
    /// the child exited for another reason.
    pub async fn ssh_not_found(&mut self) -> Option<ClientError> {
        let child = self.child.as_mut()?;
        let status = child.wait().await.ok()?;
        (status.code() == Some(127)).then_some(ClientError::RemoteNotFound)
    }
}

/// Open a connection to the target daemon (local socket or SSH `bridge`).
pub async fn open_transport(target: &Target, connect: Connect) -> Result<Transport, ClientError> {
    match &target.host {
        None => {
            let socket = default_socket_path()?;
            let stream = match connect {
                Connect::Spawn => connect_or_spawn(&socket).await?,
                Connect::Only => connect_only(&socket).await?,
            };
            let (r, w) = split(stream);
            Ok(Transport {
                reader: Box::new(r),
                writer: Box::new(w),
                child: None,
            })
        }
        Some(host) => {
            // Task 4 inserts `if target.install { install_remote(...).await? }` here.
            let remote_bin = resolve_remote_bin(target);
            let mut child = Command::new("ssh")
                .args(ssh_args(host, &remote_bin, connect))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit()) // SSH's prompts/errors reach the user
                .spawn()
                .map_err(ClientError::Io)?;
            // invariant: stdin/stdout are piped above, so take() is Some.
            let writer = child.stdin.take().expect("ssh stdin piped");
            let reader = child.stdout.take().expect("ssh stdout piped");
            Ok(Transport {
                reader: Box::new(reader),
                writer: Box::new(writer),
                child: Some(child),
            })
        }
    }
}

/// Connect to the daemon socket without spawning one if absent.
///
/// Returns `Err(ClientError::Connect { … })` when no daemon is reachable.
/// Scripting verbs (`cmd`, `send`, `capture`) use this so they never
/// accidentally start a daemon, since a missing daemon is a user error there.
pub async fn connect_only(socket: &Path) -> Result<UnixStream, ClientError> {
    UnixStream::connect(socket)
        .await
        .map_err(|source| ClientError::Connect {
            path: socket.to_path_buf(),
            source,
        })
}

/// Connect to the daemon socket, spawning a new daemon if one is not running.
/// Returns the connected stream.
pub async fn connect_or_spawn(socket: &Path) -> Result<UnixStream, ClientError> {
    match UnixStream::connect(socket).await {
        Ok(s) => return Ok(s),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            debug!(error = %e, path = %socket.display(), "daemon not reachable, spawning");
        }
        Err(e) => {
            return Err(ClientError::Connect {
                path: socket.to_path_buf(),
                source: e,
            });
        }
    }

    spawn_daemon()?;

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut delay = Duration::from_millis(20);
    loop {
        match UnixStream::connect(socket).await {
            Ok(s) => {
                info!("connected to spawned daemon");
                return Ok(s);
            }
            Err(e) if Instant::now() >= deadline => {
                return Err(ClientError::Connect {
                    path: socket.to_path_buf(),
                    source: e,
                });
            }
            Err(_) => {
                time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_millis(200));
            }
        }
    }
}

fn spawn_daemon() -> Result<(), ClientError> {
    let exe = env::current_exe().map_err(ClientError::Io)?;
    let mut cmd = Command::new(exe);
    let stderr = plexy_glass_daemon::RuntimePaths::for_current_user()
        .ok()
        .and_then(|p| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&p.log_file)
                .ok()
        })
        .map_or_else(Stdio::null, Stdio::from);
    cmd.arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);
    // SAFETY: setsid is async-signal-safe and called only in the child between
    // fork and exec. We do not touch any shared state from the closure.
    unsafe {
        cmd.pre_exec(|| {
            // Detach: new session so we are not in the client's pgrp.
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    use super::*;

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
