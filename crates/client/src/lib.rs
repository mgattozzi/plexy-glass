//! plexy-glass client.

pub mod bridge;
pub mod error;
pub mod install;
pub mod kill;
pub mod negotiate;
pub mod picker;
pub mod pump;
pub mod query;
pub mod roster;
pub mod shell_integration;
pub mod transport;
pub mod tty;

use std::future::Future;
use std::io::Write;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::time::Duration;
use std::{env, io, process};

pub use bridge::run_bridge;
pub use error::ClientError;
pub use kill::{KillOutcome, kill, kill_all};
use plexy_glass_protocol::errors::CodecError;
use plexy_glass_protocol::{
    ClientHello, ClientMsg, Codec, CreatePolicy, ExitStatus, GraphicsCaps, NegotiatedKbd,
    PROTOCOL_VERSION, ServerMsg, SessionName, SpawnSpec, client_handshake, client_handshake_with,
};
pub use pump::{PumpExit, handshake_spawn, pump};
pub use shell_integration::shell_integration_snippet;
use tokio::process::Command;
use tokio::signal::unix;
use tokio::sync::mpsc;
use tokio::{io as tokio_io, time};
use tracing::info;
pub use transport::{
    Connect, Host, InstallPolicy, Target, Transport, connect_only, connect_or_spawn,
    default_socket_path, open_transport, ssh_args,
};
pub use tty::{HostTty, current_size};

/// The outer terminal's negotiated keyboard/graphics capabilities and any
/// type-ahead sent during the probe window. Captured by [`run_probe`] so the
/// SSH path can probe (raw), drop back to cooked for auth, then carry the
/// result across `open_transport`.
struct ProbeOutcome {
    kbd: NegotiatedKbd,
    graphics: GraphicsCaps,
    caps: negotiate::EnabledCaps,
    type_ahead: Vec<u8>,
}

/// Probe the LOCAL outer terminal for Kitty / XTVERSION / graphics support and
/// capture any type-ahead sent during the probe window. The caller must
/// already hold a raw-mode guard (`negotiate::read_probe_reply` reads the fd
/// directly); this touches only the outer tty, never the daemon connection.
fn run_probe(stdin_fd: BorrowedFd<'_>) -> ProbeOutcome {
    let mut stdout = io::stdout();
    // Probe the outer terminal for Kitty / XTVERSION support.
    let _ = stdout.write_all(negotiate::PROBE);
    let _ = stdout.flush();
    // Read whatever the terminal replies within a short window. We read raw from
    // the fd (the async stdin reader is not yet spawned), so a non-answering
    // terminal can't hang us.
    let probe_reply = negotiate::read_probe_reply(stdin_fd, Duration::from_millis(120));
    let kbd = negotiate::classify(&probe_reply);
    // `PLEXY_FORCE_KITTY` forces Kitty graphics caps on regardless of the probe,
    // a test hook so the e2e harness (whose PTY can't answer the graphics
    // query) can exercise the full image render path. No effect unless set.
    let mut graphics = if env::var_os("PLEXY_FORCE_KITTY").is_some() {
        GraphicsCaps {
            kitty: true,
            sixel: false,
            iterm2: false,
        }
    } else {
        negotiate::classify_graphics(&probe_reply)
    };
    // `PLEXY_FORCE_SIXEL` forces Sixel caps on, the Sixel sibling of
    // `PLEXY_FORCE_KITTY`/`PLEXY_FORCE_ITERM2`. OR'd in (not an override) so a
    // test can force Sixel-only by unsetting `PLEXY_FORCE_KITTY` and setting
    // this: the else-branch `classify_graphics` yields all-false under the
    // harness PTY (which answers no probe), then this sets `sixel = true`.
    if env::var_os("PLEXY_FORCE_SIXEL").is_some() {
        graphics.sixel = true;
    }
    // iTerm2 isn't probeable via escapes, so detect it from the environment (or a
    // `PLEXY_FORCE_ITERM2` test hook) and OR it into the negotiated caps.
    if env::var_os("PLEXY_FORCE_ITERM2").is_some()
        || negotiate::term_program_supports_iterm2(env::var("TERM_PROGRAM").ok().as_deref())
    {
        graphics.iterm2 = true;
    }
    // Keystrokes the user typed during the probe window land after the DA1
    // sentinel in `probe_reply`. The pump reads stdin fresh, so without this
    // they'd be dropped; replay them as initial input once the session attaches.
    let type_ahead = negotiate::type_ahead_after_probe(&probe_reply).to_vec();
    let caps = negotiate::EnabledCaps {
        kbd,
        focus_events: true,
        color_scheme: true,
    };
    ProbeOutcome {
        kbd,
        graphics,
        caps,
        type_ahead,
    }
}

/// Attach to (or create) a session and drive the terminal interactively.
///
/// - `target`: local daemon or a remote one over SSH (`-H`).
/// - `name`: which session to target; `None` lets the daemon pick.
/// - `create_if_missing`: `CreateIfMissing` creates the session if it does not
///   yet exist; `RequireExisting` returns `SessionNotFound`.
/// - `spawn_cmd`: override the program spawned in new sessions; `None` uses
///   the current `$SHELL`.
pub async fn run(
    target: &Target,
    name: Option<String>,
    create_if_missing: CreatePolicy,
    spawn_cmd: Option<SpawnSpec>,
) -> Result<(), ClientError> {
    // ONE async stdin reader + one async stdout for the whole client life. A
    // second `tokio::io::stdin()` would race the first on fd 0 (each spawns its
    // own blocking read thread), so the outer loop owns a single reader that
    // every attach borrows. That blocking read thread never joins once it has
    // read (the PTY stdin has no further input and is not closed), so dropping
    // the runtime would HANG on it — which is why the loop ends in a single
    // `process::exit` (skipping destructors) rather than returning, and we
    // restore the tty ourselves first.
    let mut stdin = tokio_io::stdin();
    let mut stdout = tokio_io::stdout();

    // The session to attach to. From Milestone B on, `pump` can ask to
    // re-attach to a different daemon via `PumpExit::ReconnectTo`; the loop
    // threads that back through `next`. In Milestone A `pump` only ever returns
    // `Ended`, so this loop runs exactly once and single-attach behavior is
    // byte-identical to before.
    let mut next: (Target, Option<String>) = (target.clone(), name);

    let outcome: Result<ExitStatus, ClientError> = loop {
        let target = &next.0;
        let name = next.1.clone();
        let stdin_fd = stdin.as_fd();

        // The local terminal probe needs raw mode; SSH interactive auth
        // (password/passphrase/host-key, read from /dev/tty by ssh) needs cooked
        // mode; the pump needs raw mode again. LOCAL keeps the pre-SSH ordering
        // exactly (connect, THEN one raw guard spanning probe/enable/handshake/
        // pump) so local attach is behaviorally unchanged. SSH probes in a brief
        // raw window, drops back to cooked before the `ssh` child spawns (so auth
        // prompts land normally), then opens the transport and re-enters raw for
        // the handshake + session.
        let (mut t, guard, probe) = if target.host.is_none() {
            let t = open_transport(target, Connect::Spawn).await?;
            let guard = HostTty::enter_raw(stdin_fd)?;
            let probe = run_probe(stdin_fd);
            (t, guard, probe)
        } else {
            let probe = {
                let _raw = HostTty::enter_raw(stdin_fd)?;
                run_probe(stdin_fd)
            }; // _raw drops here, restoring cooked mode for SSH auth
            let t = open_transport(target, Connect::Spawn).await?;
            let guard = HostTty::enter_raw(stdin_fd)?;
            (t, guard, probe)
        };

        // --- Enable escapes + emergency restore. Host-terminal-local (never goes
        // over the wire), so this runs once here for both paths, inside the raw
        // guard that now covers the rest of the session. ---
        let mut out = io::stdout();
        // Enable SGR-encoded mouse coords (?1006h), button-event tracking (?1002h,
        // motion only while a button is held; ?1003h would flood with hover), and
        // bracketed paste (?2004h). These are kept OUT of `EnabledCaps` (and so out of
        // its teardown inverse) because HostTty disables ?1006/?1002/?2004
        // unconditionally on restore, and order relative to the kbd enables doesn't
        // matter since DEC private modes are independent.
        let _ = out.write_all(b"\x1b[?1006h\x1b[?1002h\x1b[?2004h");
        // Enable exactly the keyboard/focus/theme caps we classified.
        let _ = out.write_all(&probe.caps.enable_bytes());
        let _ = out.flush();

        // Record the enabled set for both the normal and emergency teardown paths,
        // then install the emergency restore (which reads the recorded caps). The
        // `guard` local lives to the end of this iteration, so its `Drop` restores
        // the tty on `break` (session end) and on any early `?` return before
        // `pump` — exactly as the old per-attach guard did.
        tty::set_enabled_caps(probe.caps);
        tty::install_emergency_restore(stdin_fd, guard.original_termios());

        let term = env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string());
        let hello = ClientHello {
            version: PROTOCOL_VERSION,
            term,
            kbd: probe.kbd,
            graphics: probe.graphics,
            remote: target.host.is_some(),
        };
        let server_hello = match client_handshake_with(&mut t.reader, &mut t.writer, hello).await {
            Ok(h) => h,
            Err(e) => {
                // On SSH, a remote-binary-not-found (exit 127) shows up as a bare
                // EOF; surface the actionable hint instead (mirrors `request_reply`).
                return Err(t.ssh_not_found().await.unwrap_or_else(|| e.into()));
            }
        };
        info!(
            daemon_pid = server_hello.daemon_pid,
            kbd = ?probe.kbd,
            "connected to daemon"
        );

        let initial_size = current_size(stdin_fd)?;

        let cmd = resolve_attach_spec(target.host.is_some(), spawn_cmd.clone());
        handshake_spawn(
            &mut t.reader,
            &mut t.writer,
            name,
            create_if_missing,
            cmd,
            initial_size,
        )
        .await?;

        // Remember this host in the ad-hoc roster so the session picker can
        // list it next time, whether this is the initial `-H` attach or a
        // picker-driven reconnect (`ReconnectTo` loops back through here). Only
        // fires for a remote; best-effort (`add_adhoc` already swallows its own
        // write errors). No double-listing: this host becomes `current_target`
        // for the NEXT picker open, which excludes it from the query set.
        if let Some(host) = &target.host {
            roster::add_adhoc(host);
        }

        // Replay probe-window type-ahead now that a pane exists to receive it. These
        // are plain keystrokes: focus/theme/mouse/paste modes are enabled only after
        // the probe, so the post-DA1 tail can't carry those events. Send it as Input.
        if !probe.type_ahead.is_empty() {
            pump::send_client_msg(
                &mut t.writer,
                &ClientMsg::Input(bytes::Bytes::from(probe.type_ahead)),
            )
            .await?;
        }

        // SIGWINCH plumbing.
        let (resize_tx, mut resize_rx) = mpsc::channel(4);
        let owned_fd = stdin
            .as_fd()
            .try_clone_to_owned()
            .map_err(ClientError::Io)?;
        spawn_sigwinch_task(resize_tx, owned_fd);

        // `t.child` (the SSH process, if any) rides along unused from here on.
        // `Transport`'s ssh `Command` sets `kill_on_drop(true)` (so a timed-out
        // `query::spawn_query` reaps its child instead of orphaning it), which
        // means dropping `t` here — on `break` or on the next loop iteration's
        // `ReconnectTo` — signals the ssh child to exit. That's fine: by the time
        // we get here the session has already ended or we're moving to a
        // different daemon, so tearing down the old ssh connection promptly is
        // the behavior we want, not a regression.
        match pump(
            &mut t.reader,
            &mut t.writer,
            &mut stdin,
            &mut stdout,
            initial_size,
            &mut resize_rx,
            target,
        )
        .await
        {
            Ok(PumpExit::Ended(status)) => break Ok(status),
            Ok(PumpExit::ReconnectTo {
                target: reconnect_target,
                name: reconnect_name,
            }) => {
                next = (reconnect_target, Some(reconnect_name));
            }
            Err(e) => break Err(e),
        }
    };

    // The tty is already restored: the loop-body `guard` dropped as we broke
    // out. All that's left is to `process::exit` once (see the stdin comment
    // above: the parked blocking reader can't be joined, so returning would
    // hang the runtime drop).
    match outcome {
        Ok(exit_status) => {
            info!(?exit_status, "session ended");
            let code = match exit_status {
                ExitStatus::Code(c) => c,
                _ => 0,
            };
            process::exit(code);
        }
        Err(e) => {
            info!(error = %e, "session ended with error");
            eprintln!("plexy-glass: {e}");
            process::exit(1);
        }
    }
}

/// Shared request/reply scaffold: open one connection to `target` (spawning
/// the daemon or not, per `connect`; local socket or SSH `bridge`), handshake,
/// encode + write `msg`, then read and decode exactly one reply frame. Callers
/// do their own per-message reply branching.
pub(crate) async fn request_reply(
    target: &Target,
    connect: Connect,
    msg: ClientMsg,
) -> Result<ServerMsg, ClientError> {
    let mut t = open_transport(target, connect).await?;
    if let Err(e) = client_handshake(&mut t.reader, &mut t.writer).await {
        // On SSH, a remote-binary-not-found (exit 127) shows up as a bare EOF;
        // surface the actionable hint instead.
        return Err(t.ssh_not_found().await.unwrap_or_else(|| e.into()));
    }

    let payload = postcard::to_allocvec(&msg).map_err(|e| CodecError::Encode(e.to_string()))?;
    Codec::write_frame(&mut t.writer, &payload).await?;

    let frame = Codec::read_frame(&mut t.reader)
        .await?
        .ok_or_else(|| ClientError::Io(io::Error::other("daemon closed before reply")))?;
    postcard::from_bytes(&frame).map_err(|e| CodecError::Decode(e.to_string()).into())
}

/// How long `client_exec` waits, with no `--timeout`, before printing the
/// stall hint below.
const NO_TIMEOUT_HINT_SECS: u64 = 30;

/// Race `fut` against `hint_after`; if `fut` hasn't resolved by then, call
/// `on_stall` once and then await `fut` to completion (never abandons or
/// re-sends the request, purely a diagnostic side effect). Factored out of
/// `request_reply_with_stall_hint` so the timing logic is unit-testable
/// without a real daemon or a real 30-second wait.
async fn race_with_stall_hint<F: Future>(
    fut: F,
    hint_after: Duration,
    on_stall: impl FnOnce(),
) -> F::Output {
    tokio::pin!(fut);
    if let Ok(result) = time::timeout(hint_after, &mut fut).await {
        result
    } else {
        on_stall();
        fut.await
    }
}

/// `request_reply`, but for `run` specifically: when `wire_timeout_ms` is
/// `None` (no `--timeout`, or `--timeout 0` — see `exec_timeout_ms`), the
/// daemon waits indefinitely for the OSC 133 completion mark, which silently
/// hangs an unattended script on a stuck command. If the daemon hasn't
/// answered after `NO_TIMEOUT_HINT_SECS`, print a one-line stderr nudge
/// (once) toward `--timeout`, then keep waiting on the SAME request — this is
/// purely diagnostic and changes neither the reply nor how long `run` waits.
async fn request_reply_with_stall_hint(
    target: &Target,
    msg: ClientMsg,
    wire_timeout_ms: Option<u64>,
) -> Result<ServerMsg, ClientError> {
    if wire_timeout_ms.is_some() {
        return request_reply(target, Connect::Only, msg).await;
    }
    race_with_stall_hint(
        request_reply(target, Connect::Only, msg),
        Duration::from_secs(NO_TIMEOUT_HINT_SECS),
        || {
            eprintln!(
                "plexy-glass: run is still waiting for a completion mark; pass --timeout in scripts"
            );
        },
    )
    .await
}

/// Send `ReloadConfig` to the daemon and print the result.
pub async fn client_reload_config(target: &Target) -> Result<(), ClientError> {
    let reply = request_reply(target, Connect::Spawn, ClientMsg::ReloadConfig).await?;
    match reply {
        ServerMsg::ConfigReloaded { error: None } => {
            println!("config reloaded");
            Ok(())
        }
        // A parse failure is a real failure: return Err so the process exits
        // non-zero (honest-exit-code contract, matching cmd/send/run). The
        // message is carried, not swallowed to stderr-with-exit-0.
        ServerMsg::ConfigReloaded { error: Some(e) } => Err(ClientError::Reload(e)),
        ServerMsg::Error(e) => Err(ClientError::DaemonError(e)),
        _ => Err(ClientError::UnexpectedReply),
    }
}

/// Send `KillSession { name }` to the daemon and print the result.
///
/// Connect-only: killing a session must never *start* a daemon. With none
/// running there is nothing to kill, so a connect failure prints the same
/// "no daemon running" note as the bare `kill` path and exits 0.
pub async fn client_kill_session(target: &Target, name: String) -> Result<(), ClientError> {
    let reply = match request_reply(target, Connect::Only, ClientMsg::KillSession { name }).await {
        Ok(r) => r,
        Err(ClientError::Connect { .. }) => {
            println!("no daemon running");
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    match reply {
        ServerMsg::SessionKilled { name: n } => {
            println!("killed session: {n}");
            Ok(())
        }
        ServerMsg::Error(e) => Err(ClientError::DaemonError(e)),
        _ => Err(ClientError::UnexpectedReply),
    }
}

/// Stop the daemon on the REMOTE host over SSH (`-H … kill [--all]`).
///
/// Unlike the connection verbs, `kill` signals a process rather than speaking
/// the daemon protocol, so for `-H` it must execute ON the remote: the local
/// [`kill`] targets THIS machine's runtime dir, so running it for a remote host
/// would stop the wrong daemon (the user's local one). We run the remote binary's
/// own `kill` over SSH, reusing the bridge's PATH-then-cache resolution; it prints
/// its own outcome (inherited stdio) and we propagate its exit status.
pub async fn client_kill_remote(target: &Target, all: bool) -> Result<(), ClientError> {
    // invariant: only called for a remote target (main.rs guards on host).
    let host = target.host.as_ref().expect("remote kill requires a host");
    let cmd: &[&str] = if all { &["kill", "--all"] } else { &["kill"] };
    let status = Command::new("ssh")
        .args(transport::ssh_remote_args(host, target, cmd))
        .status()
        .await
        .map_err(ClientError::Io)?;
    if !status.success() {
        return Err(ClientError::Io(io::Error::other(format!(
            "remote kill on {host} failed"
        ))));
    }
    Ok(())
}

/// List all sessions and print a table to stdout.
pub async fn client_list(target: &Target) -> Result<(), ClientError> {
    let entries = list_sessions_inline(target).await?;
    print_sessions_table(&entries);
    Ok(())
}

/// Print a formatted table of session entries to stdout.
pub fn print_sessions_table(entries: &[plexy_glass_protocol::SessionEntry]) {
    if entries.is_empty() {
        println!("(no sessions)");
        return;
    }
    println!(
        "{:<20}  {:>7}  {:>5}  {:>7}",
        "NAME", "WINDOWS", "PANES", "CLIENTS"
    );
    for e in entries {
        println!(
            "{:<20}  {:>7}  {:>5}  {:>7}",
            e.name, e.windows, e.panes, e.clients
        );
    }
}

/// Shared helper: open a connection, handshake, send `ListSessions`, return entries.
async fn list_sessions_inline(
    target: &Target,
) -> Result<Vec<plexy_glass_protocol::SessionEntry>, ClientError> {
    let reply = request_reply(target, Connect::Spawn, ClientMsg::ListSessions).await?;
    match reply {
        ServerMsg::SessionList { entries } => Ok(entries),
        ServerMsg::Error(e) => Err(ClientError::DaemonError(e)),
        _ => Err(ClientError::UnexpectedReply),
    }
}

/// Attach to a session, creating it if it doesn't exist.
///
/// No name means the default session "main", deterministic regardless of what
/// other sessions (declared or otherwise) happen to be running. The old
/// sole-session fallback silently attached plain `attach` to a config-declared
/// session.
pub async fn client_attach_smart(
    target: &Target,
    explicit_name: Option<String>,
) -> Result<(), ClientError> {
    // Parse the name (explicit, or the "main" default) into a `SessionName`
    // BEFORE it hits the wire, so our own client can never send an empty or
    // invalid name — a bad `-n ""`/`-n "has space"` fails here locally with the
    // same `EmptyName`/`InvalidName` it would have bounced back off the daemon,
    // instead of a wire round-trip that ejects the connection.
    let name = SessionName::parse(explicit_name.as_deref().unwrap_or("main"))
        .map_err(ClientError::DaemonError)?;
    // `None` lets `run` pick the shell: the local default for a local daemon, or
    // the remote's own `$SHELL` for a remote one (see the spec resolution there).
    run(
        target,
        Some(name.to_string()),
        CreatePolicy::CreateIfMissing,
        None,
    )
    .await
}

/// Run one or more command-prompt lines against a session.
///
/// Each line uses its own connection (one frame per connection, matching the
/// daemon's pre-attach dispatch contract). Stops at the first failure and
/// returns `Ok(false)`; all lines ok → `Ok(true)`. A connect or handshake
/// error propagates as `Err(ClientError)`.
///
/// "No daemon running" surfaces as `ClientError::Connect` (the scripting verbs
/// use `connect_only`, never the auto-spawning path, per spec).
pub async fn client_run_commands(
    target: &Target,
    name: Option<String>,
    lines: Vec<String>,
) -> Result<bool, ClientError> {
    for line in lines {
        let msg = ClientMsg::RunCommand {
            session: name.clone(),
            line,
        };
        let reply = request_reply(target, Connect::Only, msg).await?;
        match reply {
            ServerMsg::CommandResult { ok: true, message } => {
                if let Some(m) = message {
                    println!("{m}");
                }
            }
            ServerMsg::CommandResult { ok: false, message } => {
                eprintln!(
                    "plexy-glass cmd: {}",
                    message.as_deref().unwrap_or("command failed")
                );
                return Ok(false);
            }
            ServerMsg::Error(e) => return Err(ClientError::DaemonError(e)),
            _ => return Err(ClientError::UnexpectedReply),
        }
    }
    Ok(true)
}

/// Write raw bytes into a session's focused pane (popup-aware).
///
/// Single round-trip. Returns `Ok(true)` on success, `Ok(false)` when the
/// daemon reports an error (message printed to stderr). No daemon → `Err`.
pub async fn client_send_input(
    target: &Target,
    name: Option<String>,
    bytes: Vec<u8>,
) -> Result<bool, ClientError> {
    let msg = ClientMsg::SendInput {
        session: name,
        bytes: bytes::Bytes::from(bytes),
    };
    let reply = request_reply(target, Connect::Only, msg).await?;
    match reply {
        ServerMsg::CommandResult { ok: true, message } => {
            if let Some(m) = message {
                println!("{m}");
            }
            Ok(true)
        }
        ServerMsg::CommandResult { ok: false, message } => {
            eprintln!(
                "plexy-glass send: {}",
                message.as_deref().unwrap_or("send failed")
            );
            Ok(false)
        }
        ServerMsg::Error(e) => Err(ClientError::DaemonError(e)),
        _ => Err(ClientError::UnexpectedReply),
    }
}

/// Capture the focused pane's screen text and print to stdout.
///
/// - `last_command = false`: captures the full visible screen (popup-aware).
/// - `last_command = true`: captures the last completed OSC 133 command
///   block's output text (scrollback-inclusive). Exits 1 when no completed
///   block exists (shell integration not active).
///
/// Returns `Ok(true)` on success, `Ok(false)` when the daemon reports an error
/// (message on stderr). No daemon → `Err`.
pub async fn client_capture(
    target: &Target,
    name: Option<String>,
    last_command: bool,
) -> Result<bool, ClientError> {
    let msg = if last_command {
        ClientMsg::CaptureLastCommand { session: name }
    } else {
        ClientMsg::CapturePane { session: name }
    };
    let reply = request_reply(target, Connect::Only, msg).await?;
    match reply {
        ServerMsg::PaneCapture { text } => {
            println!("{text}");
            Ok(true)
        }
        ServerMsg::CommandResult { ok: false, message } => {
            eprintln!(
                "plexy-glass capture: {}",
                message.as_deref().unwrap_or("capture failed")
            );
            Ok(false)
        }
        ServerMsg::Error(e) => Err(ClientError::DaemonError(e)),
        _ => Err(ClientError::UnexpectedReply),
    }
}

/// Capture the last completed OSC 133 command block as structured JSON
/// (`capture --last-command --json`).
///
/// Prints exactly one compact JSON object + newline to stdout:
/// `{"output": <block output text>, "exit_code": <number|null>,
/// "command_line": <string|null>}`. Popup-aware by the same
/// input-target-pane path as plain capture.
///
/// Returns `Ok(true)` on success, `Ok(false)` when the daemon reports an error
/// (plain message on stderr, since errors are not results they are never JSON).
/// No daemon → `Err`.
pub async fn client_capture_block(
    target: &Target,
    name: Option<String>,
) -> Result<bool, ClientError> {
    let reply = request_reply(
        target,
        Connect::Only,
        ClientMsg::CaptureLastBlock { session: name },
    )
    .await?;
    match reply {
        ServerMsg::BlockCapture {
            text,
            exit,
            command_line,
        } => {
            // The user-facing JSON key is `output` (unified with `run --json`);
            // the wire field name `text` is internal.
            let obj = serde_json::json!({
                "output": text,
                "exit_code": exit,
                "command_line": command_line,
            });
            println!("{obj}");
            Ok(true)
        }
        ServerMsg::CommandResult { ok: false, message } => {
            eprintln!(
                "plexy-glass capture: {}",
                message.as_deref().unwrap_or("capture failed")
            );
            Ok(false)
        }
        ServerMsg::Error(e) => Err(ClientError::DaemonError(e)),
        _ => Err(ClientError::UnexpectedReply),
    }
}

/// Run a command in a session's input target pane and wait for its OSC 133
/// completion mark (CLI `run`).
///
/// Prints the completed block's output to stdout (trailing newline iff
/// non-empty) and returns the process exit code for `main` to apply:
/// - the command's recorded exit code (i32 passthrough; the OS truncates);
/// - 0 with a stderr note when the `D` mark carried no exit payload;
/// - 124 on a structural timeout (`ExecDone { timed_out: true }`), with the
///   GNU-`timeout`-style message on stderr (the command keeps running);
/// - 1 on any daemon refusal (`CommandResult { ok: false }`, message on
///   stderr: no session, no blocks, busy pane, alt screen, child exit, …).
///
/// With `json` set, an `ExecDone` prints one compact JSON object + newline to
/// stdout, `{"output": …, "exit_code": <number|null>, "timed_out": <bool>,
/// "command_line": <the text sent>}`, instead of the plain output. The exit
/// codes and stderr notes are EXACTLY as above (the JSON carries data, not
/// diagnostics); refusals stay plain stderr + 1 (errors are not results).
///
/// No daemon → `Err` (run never auto-spawns, like the other scripting verbs).
pub async fn client_exec(
    target: &Target,
    name: Option<String>,
    text: String,
    timeout_secs: Option<u64>,
    json: bool,
) -> Result<i32, ClientError> {
    let command_line = text.clone();
    let timeout_ms = exec_timeout_ms(timeout_secs);
    let msg = ClientMsg::ExecCommand {
        session: name,
        text,
        timeout_ms,
    };
    let reply = request_reply_with_stall_hint(target, msg, timeout_ms).await?;
    match reply {
        ServerMsg::ExecDone {
            exit,
            output,
            timed_out,
        } => {
            if json {
                let obj = serde_json::json!({
                    "output": output,
                    "exit_code": exit,
                    "timed_out": timed_out,
                    "command_line": command_line,
                });
                println!("{obj}");
            }
            if timed_out {
                // The daemon only times out when the client supplied a timeout,
                // so `timeout_secs` is Some here; 0 is an unreachable fallback.
                let secs = timeout_secs.unwrap_or(0);
                eprintln!("run: timed out after {secs}s — the command may still be running");
                return Ok(124);
            }
            if !json && !output.is_empty() {
                println!("{output}");
            }
            if let Some(n) = exit {
                Ok(n)
            } else {
                eprintln!("run: shell integration reported no exit code");
                Ok(0)
            }
        }
        ServerMsg::CommandResult { ok: false, message } => {
            eprintln!(
                "plexy-glass run: {}",
                message.as_deref().unwrap_or("run failed")
            );
            Ok(1)
        }
        ServerMsg::Error(e) => Err(ClientError::DaemonError(e)),
        _ => Err(ClientError::UnexpectedReply),
    }
}

/// Map `run --timeout SECS` to the wire `timeout_ms`. GNU-`timeout` semantics:
/// `--timeout 0` means *no* limit (→ `None`), not "time out instantly". Absent
/// `--timeout` is likewise `None`.
fn exec_timeout_ms(timeout_secs: Option<u64>) -> Option<u64> {
    timeout_secs
        .filter(|&s| s != 0)
        .map(|s| s.saturating_mul(1000))
}

fn default_spawn_spec() -> SpawnSpec {
    let program = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    SpawnSpec {
        program,
        args: vec![],
        env: vec![],
        cwd: None,
    }
}

/// The spawn spec to send in `AttachOrCreate` when creating a session.
///
/// A REMOTE daemon must spawn the REMOTE's shell, so we send `None` and let it
/// resolve its own `$SHELL`; the local client's `$SHELL` (e.g. `/bin/zsh` on
/// macOS) often doesn't exist on the remote host, and sending it fails the
/// session spawn and drops the connection with a bare "daemon closed before
/// Attached". A LOCAL daemon resolves the shell here (identical to the daemon's
/// own default, so behavior is unchanged). An explicit command is honored on
/// both.
fn resolve_attach_spec(is_remote: bool, explicit: Option<SpawnSpec>) -> Option<SpawnSpec> {
    match (is_remote, explicit) {
        (_, Some(cmd)) => Some(cmd),
        (true, None) => None,
        (false, None) => Some(default_spawn_spec()),
    }
}

fn spawn_sigwinch_task(tx: mpsc::Sender<plexy_glass_protocol::PtySize>, fd: OwnedFd) {
    tokio::spawn(async move {
        let Ok(mut sig) = unix::signal(unix::SignalKind::window_change()) else {
            return;
        };
        while sig.recv().await.is_some() {
            if let Ok(size) = current_size(fd.as_fd())
                && tx.send(size).await.is_err()
            {
                break;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[tokio::test]
    async fn race_with_stall_hint_fires_once_then_resolves_the_real_future() {
        let fired = AtomicBool::new(false);
        let result = race_with_stall_hint(
            async {
                time::sleep(Duration::from_millis(30)).await;
                42
            },
            Duration::from_millis(5),
            || fired.store(true, Ordering::SeqCst),
        )
        .await;
        assert_eq!(
            result, 42,
            "the real future's result must still come through"
        );
        assert!(
            fired.load(Ordering::SeqCst),
            "the hint should fire once the future outlasts hint_after"
        );
    }

    #[tokio::test]
    async fn race_with_stall_hint_does_not_fire_when_future_is_fast() {
        let fired = AtomicBool::new(false);
        let result = race_with_stall_hint(async { 7 }, Duration::from_secs(30), || {
            fired.store(true, Ordering::SeqCst);
        })
        .await;
        assert_eq!(result, 7);
        assert!(
            !fired.load(Ordering::SeqCst),
            "the hint must not fire when the future resolves before hint_after"
        );
    }

    #[test]
    fn exec_timeout_zero_means_no_limit() {
        // No `--timeout` → no limit.
        assert_eq!(exec_timeout_ms(None), None);
        // `--timeout 0` → no limit (GNU-timeout semantics), NOT 0ms/instant.
        assert_eq!(exec_timeout_ms(Some(0)), None);
        // A real timeout converts seconds → millis.
        assert_eq!(exec_timeout_ms(Some(5)), Some(5000));
        // Saturates instead of overflowing.
        assert_eq!(exec_timeout_ms(Some(u64::MAX)), Some(u64::MAX));
    }

    #[test]
    fn remote_attach_sends_no_spec_so_the_daemon_picks_its_own_shell() {
        // The regression: a remote create must NOT ship the local $SHELL (which
        // may be /bin/zsh, absent on the remote) — it sends None so the daemon
        // spawns the remote's own $SHELL.
        assert_eq!(resolve_attach_spec(true, None), None);
        // Local still resolves a concrete spec here.
        assert!(resolve_attach_spec(false, None).is_some());
        // An explicit command is honored regardless of remoteness.
        let explicit = SpawnSpec {
            program: "/bin/dash".to_string(),
            args: vec![],
            env: vec![],
            cwd: None,
        };
        assert_eq!(
            resolve_attach_spec(true, Some(explicit.clone()))
                .unwrap()
                .program,
            "/bin/dash"
        );
        assert_eq!(
            resolve_attach_spec(false, Some(explicit)).unwrap().program,
            "/bin/dash"
        );
    }
}
