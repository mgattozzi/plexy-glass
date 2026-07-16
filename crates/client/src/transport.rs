use std::fs::OpenOptions;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use std::{env, fmt, io};

use nix::libc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, split};
use tokio::net::UnixStream;
use tokio::process::{Child, ChildStderr, Command};
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
pub struct RemoteName(pub String);

impl Deref for RemoteName {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RemoteName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for RemoteName {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for RemoteName {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// Which daemon a verb talks to: the local socket in this user's runtime dir, or
/// a remote one over SSH.
///
/// An enum rather than `Option<RemoteName>` because **the local daemon is a real,
/// nameable destination, not the absence of one**. Conflating those is what let
/// the session picker read `None` as both "attached to local" and "no daemon
/// attached": `accept()` compares a row's host against the current one, so a
/// standalone picker (no session) claimed every local row as `same_daemon` and
/// answered Enter-on-local with `Cancel` — i.e. quit to the shell — while the
/// caller could not tell that apart from a real Esc. With `Local` spelled out,
/// `Option<Host>` recovers its honest meaning ("attached, or not") and the
/// ambiguity is unrepresentable.
///
/// `Ord` is derived, so `Local` sorts before every `Remote` (declaration order)
/// and remotes sort among themselves lexicographically — which is what the
/// roster wants anyway.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Host {
    /// This machine's daemon, over its unix socket.
    #[default]
    Local,
    /// A daemon on another machine, reached by running `bridge` over SSH.
    Remote(RemoteName),
}

impl Host {
    /// The SSH target, or `None` when this is the local socket. The only way to
    /// get a name out, so a caller that needs to `ssh` somewhere has to handle
    /// `Local` rather than route it to a bogus host.
    #[must_use]
    pub const fn remote(&self) -> Option<&RemoteName> {
        match self {
            Self::Local => None,
            Self::Remote(name) => Some(name),
        }
    }

    /// Whether this routes over SSH. Drives `ClientHello.remote` and the status
    /// bar's `ssh` badge.
    #[must_use]
    pub const fn is_remote(&self) -> bool {
        matches!(self, Self::Remote(_))
    }

    /// Whether this is the local socket. The inverse of [`is_remote`](Self::is_remote),
    /// spelled out so callers branching local-first read as a positive.
    #[must_use]
    pub const fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }
}

impl fmt::Display for Host {
    /// `local` for the local daemon, else the SSH target. This is the picker's
    /// host-anchor label, so the string is user-visible.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local => f.write_str("local"),
            Self::Remote(name) => f.write_str(name),
        }
    }
}

impl From<RemoteName> for Host {
    fn from(name: RemoteName) -> Self {
        Self::Remote(name)
    }
}

/// How a connection opens: auto-spawn the daemon (interactive/list), connect to
/// an existing one (scripting verbs), or a background roster probe that must
/// never touch the terminal or block on interactive auth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connect {
    Spawn,
    Only,
    /// A picker fan-out probe: like [`Connect::Only`] (`--no-spawn`), but the
    /// ssh child runs with `BatchMode=yes` + a short `ConnectTimeout` and its
    /// stderr is CAPTURED, not inherited — so an auth-required host (a Tailscale
    /// check, a passphrase-only key) fails fast and gets classified, instead of
    /// hijacking the terminal the picker is drawn on. Remote-only in effect: a
    /// local `Probe` is just a connect-only unix-socket open.
    Probe,
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
    /// Which daemon to talk to. Defaults to [`Host::Local`].
    pub host: Host,
    /// Explicit remote `plexy-glass` path (`--remote-bin`).
    pub remote_bin: Option<String>,
    /// `--install`: provision the remote binary before connecting.
    pub install: InstallPolicy,
}

/// The exit status our own remote script uses to say "I tried every candidate
/// and none of them is a working `plexy-glass`".
///
/// A sentinel, because the shell's own codes cannot carry this. The obvious read
/// is "missing → 127, present-but-broken → 126", and it is simply not portable:
/// a missing `exec` target is **126** under bash (which is `/bin/sh` on macOS, a
/// first-class remote target here) and **127** under dash. So `== 127` both
/// misses a genuinely absent binary on a Darwin remote and claims "plexy-glass
/// not found" for unrelated 127s — a missing `sh`, an unset `$HOME` making `~`
/// expand to nothing, a binary whose `#!` interpreter is gone. Only the script
/// knows what it actually looked for, so let it say so.
pub const REMOTE_NOT_FOUND_EXIT: i32 = 3;

/// Build the argv for `ssh` (after the program name) to run `<remote-bin> cmd…`
/// on the host. `-T` disables remote PTY allocation so a framed byte stream
/// (the `bridge`) stays 8-bit clean.
///
/// With `--remote-bin`, we invoke that exact path directly: an explicit path is
/// an instruction, not a hint, so it wins outright and a failure there should be
/// loud rather than quietly fall through to something else.
///
/// Otherwise we search, and the search is why this is more than a one-liner.
/// `ssh host cmd` runs the remote user's LOGIN shell **non-interactively**, so
/// none of the rc files that build an interactive PATH are read — and if that
/// login shell is nushell it never reads the POSIX profile chain in any mode, so
/// the `~/.cargo/env` line rustup writes into `~/.profile` is dead code there.
/// The upshot is that a `plexy-glass` the user installed, and can run, and can
/// see on their PATH, is invisible to us. So look in the places those installs
/// actually land, not just on PATH.
///
/// Each candidate is probed by **running** it (`--version`), not by testing that
/// the file exists. `command -v` answers the wrong question: a wrong-architecture
/// binary is present and executable, so `command -v` says yes, `exec` then fails,
/// and POSIX says a failed `exec` **terminates a non-interactive shell** — so the
/// old `||` fallback never ran for the one case a fallback is for. Running it is
/// the only probe that distinguishes present-and-working from present-and-broken,
/// and a failed probe is survivable where a failed `exec` is not.
///
/// Runs under `sh -c` via [`remote_sh`] (correct whatever the login shell is), so
/// the script must contain **no single quote**. `cmd` is the subcommand + flags,
/// e.g. `["bridge"]`, `["bridge", "--no-spawn"]`, or `["kill", "--all"]`.
pub fn ssh_remote_args(host: &RemoteName, target: &Target, cmd: &[&str]) -> Vec<String> {
    let mut args = vec!["-T".to_string(), host.to_string()];
    if let Some(bin) = &target.remote_bin {
        args.push(bin.clone());
        args.extend(cmd.iter().map(|s| (*s).to_string()));
    } else {
        let cache = install::REMOTE_CACHE_BIN;
        let tail = cmd.join(" ");
        // PATH first (a deliberate install wins), then where cargo and pip-style
        // installs land, then the `--install` cache.
        let script = format!(
            "for c in plexy-glass $HOME/.cargo/bin/plexy-glass $HOME/.local/bin/plexy-glass {cache}; \
             do \"$c\" --version >/dev/null 2>&1 && exec \"$c\" {tail}; done; \
             echo plexy-glass: no working binary found on this host >&2; exit {REMOTE_NOT_FOUND_EXIT}"
        );
        args.push(remote_sh(&script));
    }
    args
}

/// The `ssh` argv to run the `bridge` for a connection verb (attach + every
/// request/reply). Anything but `Connect::Spawn` appends `--no-spawn` so a
/// scripting verb or a roster probe never starts a remote daemon.
pub fn ssh_args(host: &RemoteName, target: &Target, connect: Connect) -> Vec<String> {
    let cmd: &[&str] = if connect == Connect::Spawn {
        &["bridge"]
    } else {
        &["bridge", "--no-spawn"]
    };
    ssh_remote_args(host, target, cmd)
}

#[cfg(test)]
mod ssh_tests {
    use std::fs::{self, Permissions};
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command as StdCommand;

    use super::*;

    fn target(remote_bin: Option<&str>) -> Target {
        Target {
            host: Host::Remote(RemoteName::from("h")),
            remote_bin: remote_bin.map(str::to_string),
            install: InstallPolicy::UseExisting,
        }
    }

    #[test]
    fn ssh_args_explicit_bin_is_invoked_directly() {
        assert_eq!(
            ssh_args(
                &RemoteName::from("prod"),
                &target(Some("/opt/pg")),
                Connect::Spawn
            ),
            vec!["-T", "prod", "/opt/pg", "bridge"]
        );
        assert_eq!(
            ssh_args(
                &RemoteName::from("u@h"),
                &target(Some("/opt/pg")),
                Connect::Only
            ),
            vec!["-T", "u@h", "/opt/pg", "bridge", "--no-spawn"]
        );
    }

    /// Pull the script back out of `sh -c '<script>'` so a test can run it.
    fn script_of(args: &[String]) -> String {
        let arg = &args[2];
        let inner = arg
            .strip_prefix("sh -c '")
            .and_then(|r| r.strip_suffix('\''))
            .expect("remote_sh wraps the script in sh -c '...'");
        assert!(
            !inner.contains('\''),
            "the remote script must contain no single quote: {inner}"
        );
        inner.to_string()
    }

    /// A stub `plexy-glass` that answers `--version` and otherwise announces
    /// itself, so a test can see WHICH candidate got exec'd.
    fn working_stub(dir: &Path, says: &str) {
        stub(
            dir,
            &format!("#!/bin/sh\ncase \"$1\" in --version) exit 0;; esac\necho {says}\n"),
        );
    }

    /// A stub that is present and executable but CANNOT EXEC — a wrong-arch
    /// binary, in effect. A bogus interpreter is the portable way to get the real
    /// failure mode: the file exists and has its executable bit, so `command -v`
    /// reports it happily, and only actually trying to run it fails.
    fn broken_stub(dir: &Path) {
        stub(dir, "#!/nonexistent/interpreter\n");
    }

    fn stub(dir: &Path, body: &str) {
        fs::create_dir_all(dir).unwrap();
        let p = dir.join("plexy-glass");
        fs::write(&p, body).unwrap();
        fs::set_permissions(&p, Permissions::from_mode(0o755)).unwrap();
    }

    /// Run the default remote script under a real `sh`, with `HOME`/`PATH`
    /// pointed at fixtures. Returns (exit code, stdout).
    fn run_script(home: &Path, path: &str) -> (i32, String) {
        let script = script_of(&ssh_args(
            &RemoteName::from("h"),
            &target(None),
            Connect::Spawn,
        ));
        let out = StdCommand::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .env("HOME", home)
            .env("PATH", path)
            .output()
            .unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        )
    }

    #[test]
    fn ssh_args_default_is_one_quote_free_sh_script() {
        let a = ssh_args(&RemoteName::from("prod"), &target(None), Connect::Spawn);
        assert_eq!(a[0], "-T");
        assert_eq!(a[1], "prod");
        assert_eq!(a.len(), 3, "the script is ONE ssh argument");
        let s = script_of(&a);
        assert!(
            s.contains("exec \"$c\" bridge"),
            "execs the chosen candidate: {s}"
        );
        assert!(
            s.contains(install::REMOTE_CACHE_BIN),
            "searches the --install cache: {s}"
        );
        // `--no-spawn` must ride the exec, not just one branch of it.
        let b = script_of(&ssh_args(
            &RemoteName::from("prod"),
            &target(None),
            Connect::Only,
        ));
        assert!(b.contains("exec \"$c\" bridge --no-spawn"), "got: {b}");
        // A `Probe` is `--no-spawn` too (BatchMode/ConnectTimeout are added at
        // spawn time in `open_transport`, not in the argv `ssh_args` builds).
        let p = script_of(&ssh_args(
            &RemoteName::from("prod"),
            &target(None),
            Connect::Probe,
        ));
        assert!(p.contains("exec \"$c\" bridge --no-spawn"), "got: {p}");
    }

    /// The bug this whole search exists for: `ssh host cmd` runs the login shell
    /// NON-interactively, so a cargo-installed `plexy-glass` the user can run
    /// interactively is not on the PATH we get. Find it anyway.
    #[test]
    fn remote_script_finds_a_binary_that_is_not_on_path() {
        let home = tempfile::tempdir().unwrap();
        working_stub(&home.path().join(".cargo/bin"), "from-cargo-bin");
        let empty = tempfile::tempdir().unwrap();
        let (code, out) = run_script(home.path(), &empty.path().display().to_string());
        assert_eq!(code, 0, "should have exec'd the cargo-bin candidate");
        assert_eq!(out, "from-cargo-bin");
    }

    /// PATH wins when it works: a deliberate install beats the fallbacks.
    #[test]
    fn remote_script_prefers_path() {
        let home = tempfile::tempdir().unwrap();
        working_stub(&home.path().join(".cargo/bin"), "from-cargo-bin");
        let path_dir = tempfile::tempdir().unwrap();
        working_stub(path_dir.path(), "from-path");
        let (code, out) = run_script(home.path(), &path_dir.path().display().to_string());
        assert_eq!(code, 0);
        assert_eq!(out, "from-path");
    }

    /// The case the old `||` fallback could NOT handle, and the reason the probe
    /// runs the binary instead of testing that the file exists. A wrong-arch
    /// binary is present and executable, so `command -v` reported it, `exec` then
    /// failed, and POSIX terminates a non-interactive shell on a failed `exec` —
    /// so the fallback never ran. Probing by running makes the failure survivable.
    #[test]
    fn remote_script_skips_a_present_but_broken_binary_and_keeps_looking() {
        let home = tempfile::tempdir().unwrap();
        working_stub(&home.path().join(".cargo/bin"), "from-cargo-bin");
        let path_dir = tempfile::tempdir().unwrap();
        broken_stub(path_dir.path());
        let (code, out) = run_script(home.path(), &path_dir.path().display().to_string());
        assert_eq!(code, 0, "a broken PATH binary must not abort the search");
        assert_eq!(out, "from-cargo-bin");
    }

    /// Nothing anywhere: the script raises OUR sentinel, so the client can say
    /// so instead of guessing from the shell's 126-vs-127 (which is not portable
    /// — bash says 126 for a missing exec target, dash says 127).
    #[test]
    fn remote_script_raises_our_sentinel_when_nothing_works() {
        let home = tempfile::tempdir().unwrap();
        let empty = tempfile::tempdir().unwrap();
        let (code, _) = run_script(home.path(), &empty.path().display().to_string());
        assert_eq!(
            code, REMOTE_NOT_FOUND_EXIT,
            "no candidate worked, so the script must report it itself"
        );
    }

    #[test]
    fn ssh_remote_args_runs_kill_on_the_remote() {
        let cache = install::REMOTE_CACHE_BIN;
        // Explicit bin: `kill --all` as direct argv.
        assert_eq!(
            ssh_remote_args(
                &RemoteName::from("prod"),
                &target(Some("/opt/pg")),
                &["kill", "--all"]
            ),
            vec!["-T", "prod", "/opt/pg", "kill", "--all"]
        );
        // Default: the same search the bridge uses, running `kill` remotely.
        let k = ssh_remote_args(&RemoteName::from("prod"), &target(None), &["kill"]);
        let script = script_of(&k);
        assert!(script.contains("exec \"$c\" kill"), "execs kill: {script}");
        assert!(script.contains(cache), "searches the cache: {script}");
    }
}

/// The `ConnectTimeout` (seconds) a `Connect::Probe` ssh runs with, so a dead
/// host fails at the TCP-connect phase inside the picker's per-host budget
/// rather than hanging until the OS default (~75s) and relying on our own kill.
const PROBE_CONNECT_TIMEOUT_SECS: u32 = 2;

/// A daemon connection as a split reader/writer, from the local socket or an
/// SSH `bridge` child. `child` keeps the SSH process (and thus the pipes)
/// alive for the transport's lifetime, and lets `ssh_not_found` inspect its
/// exit status; `None` for local.
pub struct Transport {
    pub reader: Box<dyn AsyncRead + Send + Unpin>,
    pub writer: Box<dyn AsyncWrite + Send + Unpin>,
    child: Option<Child>,
    /// ssh's captured stderr, `Some` only for a `Connect::Probe` transport
    /// (every other path inherits stderr so ssh's prompts reach the user). Read
    /// by [`probe_diagnose`](Self::probe_diagnose) when a probe fails, to tell
    /// "needs auth" from "unreachable".
    stderr: Option<ChildStderr>,
}

impl Transport {
    /// Kill the ssh child and return whatever it wrote to stderr — for a
    /// `Connect::Probe` that failed or timed out. Killing FIRST closes ssh's
    /// stderr so the read reaches EOF instead of blocking on a still-running
    /// session (a Tailscale check holds the connection open); the banner ssh
    /// already wrote sits in the kernel pipe buffer and survives the kill.
    /// Empty for a local transport or any non-probe one (stderr was inherited,
    /// so there's nothing captured to read).
    pub async fn probe_diagnose(&mut self) -> String {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
        let Some(mut err) = self.stderr.take() else {
            return String::new();
        };
        let mut buf = Vec::new();
        // Bounded read: the banner is small and already buffered post-kill, but
        // never wait unboundedly on a pipe that a wedged ssh might hold open.
        let _ = time::timeout(Duration::from_millis(500), err.read_to_end(&mut buf)).await;
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// After a failed handshake/read on an SSH transport, check whether our own
    /// remote script reported that it found no working `plexy-glass`, and if so
    /// return that instead of a bare EOF. `None` for local, or when the child
    /// exited for any other reason.
    ///
    /// Matches [`REMOTE_NOT_FOUND_EXIT`], which the script raises itself, rather
    /// than reading the shell's 126/127 — see that constant for why those cannot
    /// carry this. `ssh` propagates the remote command's status as its own, and
    /// its own failures (auth, unreachable, bad `ProxyJump`) are 255, so they
    /// correctly fall through to the real error.
    pub async fn ssh_not_found(&mut self) -> Option<ClientError> {
        let child = self.child.as_mut()?;
        let status = child.wait().await.ok()?;
        (status.code() == Some(REMOTE_NOT_FOUND_EXIT)).then_some(ClientError::RemoteNotFound)
    }
}

/// Open a connection to the target daemon (local socket or SSH `bridge`).
pub async fn open_transport(target: &Target, connect: Connect) -> Result<Transport, ClientError> {
    match &target.host {
        Host::Local => {
            let socket = default_socket_path()?;
            let stream = match connect {
                Connect::Spawn => connect_or_spawn(&socket).await?,
                // A `Probe` never spawns, same as `Only`: the local daemon rides
                // the unix socket, so BatchMode/captured-stderr don't apply here.
                Connect::Only | Connect::Probe => connect_only(&socket).await?,
            };
            let (r, w) = split(stream);
            Ok(Transport {
                reader: Box::new(r),
                writer: Box::new(w),
                child: None,
                stderr: None,
            })
        }
        Host::Remote(host) => {
            if target.install.provisions() {
                install::install_remote(host).await?;
            }
            let probe = connect == Connect::Probe;
            let mut cmd = Command::new("ssh");
            if probe {
                // Background roster probe: never prompt, never touch the
                // terminal. BatchMode makes ssh fail fast instead of blocking on
                // password/passphrase/keyboard-interactive auth; ConnectTimeout
                // bounds a dead host; stderr is captured below so an auth banner
                // is classified, not painted over the picker.
                cmd.arg("-o")
                    .arg("BatchMode=yes")
                    .arg("-o")
                    .arg(format!("ConnectTimeout={PROBE_CONNECT_TIMEOUT_SECS}"));
            }
            let mut child = cmd
                .args(ssh_args(host, target, connect))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                // Foreground connections inherit stderr so SSH's prompts/errors
                // reach the user; a probe captures it instead (read by
                // `probe_diagnose` to distinguish needs-auth from unreachable).
                .stderr(if probe {
                    Stdio::piped()
                } else {
                    Stdio::inherit()
                })
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
            // invariant: stderr is piped above iff `probe`, so take() is Some there.
            let stderr = probe.then(|| child.stderr.take().expect("ssh stderr piped"));
            Ok(Transport {
                reader: Box::new(reader),
                writer: Box::new(writer),
                child: Some(child),
                stderr,
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
