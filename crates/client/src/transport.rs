use std::fs::OpenOptions;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use std::{env, fmt, io};

use nix::libc;
use tokio::io::{AsyncRead, AsyncWrite, split};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::time;
use tracing::{debug, info};

use crate::error::ClientError;
use crate::install;
use crate::install::remote_sh;

/// An SSH target: an `ssh_config` alias or `user@host`, the routing key for a
/// remote daemon. A newtype over `String` so a host can't be mixed up with any
/// other string (a session name, a binary path) at a call site. `Deref<str>` +
/// `Display` let it stand in wherever a `&str` host was read; `From` converts at
/// the config/CLI boundary (`Config.remotes` / the `-H` flag are plain strings).
/// Client-crate-contained: SSH targets never reach the postcard wire.
///
/// `Ord`/`PartialOrd` are derived (lexicographic, same as the `String` it wraps)
/// so the roster's `assemble` can keep sorting hosts alphabetically.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Host(pub String);

impl Deref for Host {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for Host {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for Host {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// How a connection opens: auto-spawn the daemon (interactive/list) or fail if
/// none is running (scripting verbs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connect {
    Spawn,
    Only,
}

/// Whether `--install` should provision the remote binary before connecting.
/// A named enum so it can't be silently swapped with another `bool` on `Target`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InstallPolicy {
    /// Use whatever `plexy-glass` the remote already has (PATH or the cache path).
    #[default]
    UseExisting,
    /// Provision the remote binary from the nightly release first (`--install`).
    Provision,
}

impl InstallPolicy {
    /// Whether to provision the remote binary before connecting.
    pub const fn provisions(self) -> bool {
        matches!(self, Self::Provision)
    }

    /// Flip the picker's persistent `i` toggle.
    #[must_use]
    pub const fn toggled(self) -> Self {
        match self {
            Self::UseExisting => Self::Provision,
            Self::Provision => Self::UseExisting,
        }
    }
}

/// Where a verb runs: the local daemon, or a remote one over SSH.
#[derive(Debug, Clone, Default)]
pub struct Target {
    /// `Some(ssh-target)` routes over SSH; `None` uses the local socket.
    pub host: Option<Host>,
    /// Explicit remote `plexy-glass` path (`--remote-bin`).
    pub remote_bin: Option<String>,
    /// `--install`: provision the remote binary before connecting.
    pub install: InstallPolicy,
}

/// Build the argv for `ssh` (after the program name) to run `<remote-bin> cmd…`
/// on the host. `-T` disables remote PTY allocation so a framed byte stream
/// (the `bridge`) stays 8-bit clean.
///
/// With `--remote-bin`, we invoke that exact path directly. Otherwise we try
/// `plexy-glass` on the remote's non-interactive PATH first, then fall back to
/// the `--install` cache path, so a manual PATH install and an
/// `--install`-provisioned binary both work with no extra flag. That fallback is
/// a shell conditional, so it runs under `sh -c` (via [`remote_sh`], correct
/// whatever the remote login shell is) and `exec` hands the raw stdio to the
/// chosen binary. If neither exists the final `exec` fails 127, which the client
/// surfaces as [`ClientError::RemoteNotFound`]. `cmd` is the subcommand + flags,
/// e.g. `["bridge"]`, `["bridge", "--no-spawn"]`, or `["kill", "--all"]`.
pub fn ssh_remote_args(host: &Host, target: &Target, cmd: &[&str]) -> Vec<String> {
    let mut args = vec!["-T".to_string(), host.to_string()];
    if let Some(bin) = &target.remote_bin {
        args.push(bin.clone());
        args.extend(cmd.iter().map(|s| (*s).to_string()));
    } else {
        let cache = install::REMOTE_CACHE_BIN;
        let tail = cmd.join(" ");
        let script = format!(
            "command -v plexy-glass >/dev/null 2>&1 && exec plexy-glass {tail} || exec {cache} {tail}"
        );
        args.push(remote_sh(&script));
    }
    args
}

/// The `ssh` argv to run the `bridge` for a connection verb (attach + every
/// request/reply). `Connect::Only` appends `--no-spawn` so a scripting verb
/// never starts a remote daemon.
pub fn ssh_args(host: &Host, target: &Target, connect: Connect) -> Vec<String> {
    let cmd: &[&str] = if connect == Connect::Only {
        &["bridge", "--no-spawn"]
    } else {
        &["bridge"]
    };
    ssh_remote_args(host, target, cmd)
}

#[cfg(test)]
mod ssh_tests {
    use super::*;

    fn target(remote_bin: Option<&str>) -> Target {
        Target {
            host: Some("h".into()),
            remote_bin: remote_bin.map(str::to_string),
            install: InstallPolicy::UseExisting,
        }
    }

    #[test]
    fn ssh_args_explicit_bin_is_invoked_directly() {
        assert_eq!(
            ssh_args(
                &Host::from("prod"),
                &target(Some("/opt/pg")),
                Connect::Spawn
            ),
            vec!["-T", "prod", "/opt/pg", "bridge"]
        );
        assert_eq!(
            ssh_args(&Host::from("u@h"), &target(Some("/opt/pg")), Connect::Only),
            vec!["-T", "u@h", "/opt/pg", "bridge", "--no-spawn"]
        );
    }

    #[test]
    fn ssh_args_default_falls_back_path_then_cache() {
        let cache = install::REMOTE_CACHE_BIN;
        // Spawn: try PATH, then the cache path.
        let a = ssh_args(&Host::from("prod"), &target(None), Connect::Spawn);
        assert_eq!(a[0], "-T");
        assert_eq!(a[1], "prod");
        assert_eq!(a.len(), 3);
        assert_eq!(
            a[2],
            format!(
                "sh -c 'command -v plexy-glass >/dev/null 2>&1 && exec plexy-glass bridge || exec {cache} bridge'"
            )
        );
        // Only: --no-spawn rides both branches.
        let b = ssh_args(&Host::from("prod"), &target(None), Connect::Only);
        assert_eq!(
            b[2],
            format!(
                "sh -c 'command -v plexy-glass >/dev/null 2>&1 && exec plexy-glass bridge --no-spawn || exec {cache} bridge --no-spawn'"
            )
        );
    }

    #[test]
    fn ssh_remote_args_runs_kill_on_the_remote() {
        let cache = install::REMOTE_CACHE_BIN;
        // Explicit bin: `kill --all` as direct argv.
        assert_eq!(
            ssh_remote_args(
                &Host::from("prod"),
                &target(Some("/opt/pg")),
                &["kill", "--all"]
            ),
            vec!["-T", "prod", "/opt/pg", "kill", "--all"]
        );
        // Default: the same PATH-then-cache fallback, running `kill` remotely.
        let k = ssh_remote_args(&Host::from("prod"), &target(None), &["kill"]);
        assert_eq!(
            k[2],
            format!(
                "sh -c 'command -v plexy-glass >/dev/null 2>&1 && exec plexy-glass kill || exec {cache} kill'"
            )
        );
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
            if target.install.provisions() {
                install::install_remote(host).await?;
            }
            let mut child = Command::new("ssh")
                .args(ssh_args(host, target, connect))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit()) // SSH's prompts/errors reach the user
                // A timed-out query (query.rs) drops the Transport without ever
                // reading ssh's exit status; without this the ssh child (and the
                // remote session it holds open) would orphan instead of getting
                // reaped.
                .kill_on_drop(true)
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
