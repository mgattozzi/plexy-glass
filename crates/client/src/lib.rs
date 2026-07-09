//! plexy-glass client.

pub mod bridge;
pub mod error;
pub mod kill;
pub mod negotiate;
pub mod pump;
pub mod shell_integration;
pub mod transport;
pub mod tty;

use std::os::fd::{AsFd, OwnedFd};
use std::time::Duration;
use std::{env, io, process};

pub use bridge::run_bridge;
pub use error::ClientError;
pub use kill::{KillOutcome, kill, kill_all};
use plexy_glass_protocol::errors::CodecError;
use plexy_glass_protocol::{
    ClientHello, ClientMsg, Codec, PROTOCOL_VERSION, ServerMsg, SpawnSpec, client_handshake,
    client_handshake_with,
};
pub use pump::{handshake_spawn, pump};
pub use shell_integration::shell_integration_snippet;
use tokio::io as tokio_io;
use tokio::signal::unix;
use tokio::sync::mpsc;
use tracing::info;
pub use transport::{
    Connect, Target, Transport, connect_only, connect_or_spawn, default_socket_path,
    open_transport, resolve_remote_bin, ssh_args,
};
pub use tty::{HostTty, current_size};

/// Attach to (or create) a session and drive the terminal interactively.
///
/// - `name`: which session to target; `None` lets the daemon pick.
/// - `create_if_missing`: when `true` the daemon creates the session if it
///   does not yet exist; when `false` it returns `SessionNotFound`.
/// - `spawn_cmd`: override the program spawned in new sessions; `None` uses
///   the current `$SHELL`.
pub async fn run(
    name: Option<String>,
    create_if_missing: bool,
    spawn_cmd: Option<SpawnSpec>,
) -> Result<(), ClientError> {
    let socket = default_socket_path()?;
    let stream = connect_or_spawn(&socket).await?;
    let (mut reader, mut writer) = tokio_io::split(stream);

    let stdin = tokio_io::stdin();
    let stdin_fd = stdin.as_fd();
    let mut tty_guard = HostTty::enter_raw(stdin_fd)?;

    // --- Negotiation phase (runs in raw mode, before the dumb pump) ---
    use std::io::Write as _;
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
        plexy_glass_protocol::GraphicsCaps {
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

    // Enable SGR-encoded mouse coords (?1006h), button-event tracking (?1002h,
    // motion only while a button is held; ?1003h would flood with hover), and
    // bracketed paste (?2004h). These are kept OUT of `EnabledCaps` (and so out of
    // its teardown inverse) because HostTty disables ?1006/?1002/?2004
    // unconditionally on restore, and order relative to the kbd enables doesn't
    // matter since DEC private modes are independent.
    let _ = stdout.write_all(b"\x1b[?1006h\x1b[?1002h\x1b[?2004h");
    // Enable exactly the keyboard/focus/theme caps we classified.
    let _ = stdout.write_all(&caps.enable_bytes());
    let _ = stdout.flush();

    // Record the enabled set for both the normal and emergency teardown paths,
    // then install the emergency restore (which reads the recorded caps).
    tty::set_enabled_caps(caps);
    tty::install_emergency_restore(stdin_fd, tty_guard.original_termios());

    let term = env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string());
    let hello = ClientHello {
        version: PROTOCOL_VERSION,
        term,
        kbd,
        graphics,
    };
    let server_hello = client_handshake_with(&mut reader, &mut writer, hello).await?;
    info!(
        daemon_pid = server_hello.daemon_pid,
        ?kbd,
        "connected to daemon"
    );

    let initial_size = current_size(stdin_fd)?;

    let spec = spawn_cmd.unwrap_or_else(default_spawn_spec);
    handshake_spawn(
        &mut reader,
        &mut writer,
        name,
        create_if_missing,
        Some(spec),
        initial_size,
    )
    .await?;

    // Replay probe-window type-ahead now that a pane exists to receive it. These
    // are plain keystrokes: focus/theme/mouse/paste modes are enabled only after
    // the probe, so the post-DA1 tail can't carry those events. Send it as Input.
    if !type_ahead.is_empty() {
        pump::send_client_msg(
            &mut writer,
            &ClientMsg::Input(bytes::Bytes::from(type_ahead)),
        )
        .await?;
    }

    // SIGWINCH plumbing.
    let (resize_tx, resize_rx) = mpsc::channel(4);
    let owned_fd = stdin
        .as_fd()
        .try_clone_to_owned()
        .map_err(ClientError::Io)?;
    spawn_sigwinch_task(resize_tx, owned_fd);

    let stdout = tokio_io::stdout();
    let stdin_for_pump = tokio_io::stdin();
    // Once `pump` has read stdin, `tokio::io::stdin()` has spawned an internal
    // blocking read thread that never finishes (the PTY stdin has no further
    // input and is not closed), so dropping the runtime would HANG waiting for
    // it. We must therefore `std::process::exit` on BOTH the success and the
    // error path rather than returning, and restore the TTY first, since
    // `process::exit` skips destructors. (Errors *before* pump can still use
    // `?`: the blocking reader hasn't been spawned yet, so the runtime drops
    // cleanly there.)
    let exit_status = match pump(reader, writer, stdin_for_pump, stdout, resize_rx).await {
        Ok(s) => s,
        Err(e) => {
            info!(error = %e, "session ended with error");
            let _ = tty_guard.restore();
            eprintln!("plexy-glass: {e}");
            process::exit(1);
        }
    };
    info!(?exit_status, "session ended");
    let _ = tty_guard.restore();
    let code = match exit_status {
        plexy_glass_protocol::ExitStatus::Code(c) => c,
        _ => 0,
    };
    process::exit(code);
}

/// Shared request/reply scaffold: open one connection to `target` (spawning
/// the daemon or not, per `connect`; local socket or SSH `bridge`), handshake,
/// encode + write `msg`, then read and decode exactly one reply frame. Callers
/// do their own per-message reply branching.
async fn request_reply(
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
        other => Err(ClientError::Io(io::Error::other(format!(
            "unexpected reply from daemon: {other:?}"
        )))),
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
        other => Err(ClientError::Io(io::Error::other(format!(
            "unexpected reply from daemon: {other:?}"
        )))),
    }
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
        other => Err(ClientError::Io(io::Error::other(format!(
            "unexpected reply from daemon: {other:?}"
        )))),
    }
}

/// Attach to a session, creating it if it doesn't exist.
///
/// No name means the default session "main", deterministic regardless of what
/// other sessions (declared or otherwise) happen to be running. The old
/// sole-session fallback silently attached plain `attach` to a config-declared
/// session.
pub async fn client_attach_smart(explicit_name: Option<String>) -> Result<(), ClientError> {
    let name = explicit_name.unwrap_or_else(|| "main".to_string());
    run(Some(name), true, Some(default_spawn_spec())).await
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
            other => {
                return Err(ClientError::Io(io::Error::other(format!(
                    "unexpected reply from daemon: {other:?}"
                ))));
            }
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
        other => Err(ClientError::Io(io::Error::other(format!(
            "unexpected reply from daemon: {other:?}"
        )))),
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
        other => Err(ClientError::Io(io::Error::other(format!(
            "unexpected reply from daemon: {other:?}"
        )))),
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
        other => Err(ClientError::Io(io::Error::other(format!(
            "unexpected reply from daemon: {other:?}"
        )))),
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
    let msg = ClientMsg::ExecCommand {
        session: name,
        text,
        timeout_ms: exec_timeout_ms(timeout_secs),
    };
    let reply = request_reply(target, Connect::Only, msg).await?;
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
        other => Err(ClientError::Io(io::Error::other(format!(
            "unexpected reply from daemon: {other:?}"
        )))),
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
    use super::*;

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
}
