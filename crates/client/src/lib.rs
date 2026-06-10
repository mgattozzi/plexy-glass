//! plexy-glass client.

pub mod args;
pub mod error;
pub mod kill;
pub mod negotiate;
pub mod pump;
pub mod transport;
pub mod tty;

pub use args::ClientArgs;
pub use error::ClientError;
pub use kill::{KillOutcome, kill, kill_all};
pub use pump::{handshake_spawn, pump};
pub use transport::{connect_only, connect_or_spawn, default_socket_path};
pub use tty::{HostTty, current_size};

use plexy_glass_protocol::{
    ClientHello, ClientMsg, Codec, PROTOCOL_VERSION, ServerMsg, SpawnSpec, client_handshake,
    client_handshake_with,
};
use std::os::fd::AsFd;
use tokio::sync::mpsc;
use tracing::info;

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
    let (mut reader, mut writer) = tokio::io::split(stream);

    let stdin = tokio::io::stdin();
    let stdin_fd = stdin.as_fd();
    let mut tty_guard = HostTty::enter_raw(stdin_fd)?;

    // --- Negotiation phase (runs in raw mode, before the dumb pump) ---
    use std::io::Write as _;
    let mut stdout = std::io::stdout();
    // Probe the outer terminal for Kitty / XTVERSION support.
    let _ = stdout.write_all(negotiate::PROBE);
    let _ = stdout.flush();
    // Read whatever the terminal replies within a short window. We read raw from
    // the fd (the async stdin reader is not yet spawned), so a non-answering
    // terminal can't hang us.
    let probe_reply = negotiate::read_probe_reply(stdin_fd, std::time::Duration::from_millis(120));
    let kbd = negotiate::classify(&probe_reply);
    // Keystrokes the user typed during the probe window land after the DA1
    // sentinel in `probe_reply`. The pump reads stdin fresh, so without this
    // they'd be dropped; replay them as initial input once the session attaches.
    let type_ahead = negotiate::type_ahead_after_probe(&probe_reply).to_vec();
    let caps = negotiate::EnabledCaps { kbd, focus_events: true, color_scheme: true };

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

    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string());
    let hello = ClientHello { version: PROTOCOL_VERSION, term, kbd };
    let server_hello = client_handshake_with(&mut reader, &mut writer, hello).await?;
    info!(daemon_pid = server_hello.daemon_pid, ?kbd, "connected to daemon");

    let initial_size = current_size(stdin_fd)?;

    let spec = spawn_cmd.unwrap_or_else(default_spawn_spec);
    handshake_spawn(&mut reader, &mut writer, name, create_if_missing, Some(spec), initial_size)
        .await?;

    // Replay probe-window type-ahead now that a pane exists to receive it. These
    // are plain keystrokes: focus/theme/mouse/paste modes are enabled only after
    // the probe, so the post-DA1 tail can't carry those events. Send it as Input.
    if !type_ahead.is_empty() {
        pump::send_client_msg(&mut writer, &ClientMsg::Input(bytes::Bytes::from(type_ahead)))
            .await?;
    }

    // SIGWINCH plumbing.
    let (resize_tx, resize_rx) = mpsc::channel(4);
    let owned_fd = stdin.as_fd().try_clone_to_owned().map_err(ClientError::Io)?;
    spawn_sigwinch_task(resize_tx, owned_fd);

    let stdout = tokio::io::stdout();
    let stdin_for_pump = tokio::io::stdin();
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
            std::process::exit(1);
        }
    };
    info!(?exit_status, "session ended");
    let _ = tty_guard.restore();
    let code = match exit_status {
        plexy_glass_protocol::ExitStatus::Code(c) => c,
        _ => 0,
    };
    std::process::exit(code);
}

/// Send `ReloadConfig` to the daemon and print the result.
pub async fn client_reload_config() -> Result<(), ClientError> {
    let socket = default_socket_path()?;
    let stream = connect_or_spawn(&socket).await?;
    let (mut reader, mut writer) = tokio::io::split(stream);

    client_handshake(&mut reader, &mut writer).await?;

    let msg = ClientMsg::ReloadConfig;
    let payload = postcard::to_allocvec(&msg)
        .map_err(|e| plexy_glass_protocol::errors::CodecError::Encode(e.to_string()))?;
    Codec::write_frame(&mut writer, &payload).await?;

    let frame = Codec::read_frame(&mut reader)
        .await?
        .ok_or_else(|| ClientError::Io(std::io::Error::other("daemon closed before reply")))?;
    let reply: ServerMsg = postcard::from_bytes(&frame)
        .map_err(|e| plexy_glass_protocol::errors::CodecError::Decode(e.to_string()))?;
    match reply {
        ServerMsg::ConfigReloaded { error: None } => {
            println!("config reloaded");
            Ok(())
        }
        ServerMsg::ConfigReloaded { error: Some(e) } => {
            eprintln!("config reload error: {e}");
            Ok(())
        }
        ServerMsg::Error(e) => Err(ClientError::DaemonError(e)),
        other => Err(ClientError::Io(std::io::Error::other(format!(
            "unexpected reply from daemon: {other:?}"
        )))),
    }
}

/// Send `KillSession { name }` to the daemon and print the result.
pub async fn client_kill_session(name: String) -> Result<(), ClientError> {
    let socket = default_socket_path()?;
    let stream = connect_or_spawn(&socket).await?;
    let (mut reader, mut writer) = tokio::io::split(stream);

    client_handshake(&mut reader, &mut writer).await?;

    let msg = ClientMsg::KillSession { name };
    let payload = postcard::to_allocvec(&msg)
        .map_err(|e| plexy_glass_protocol::errors::CodecError::Encode(e.to_string()))?;
    Codec::write_frame(&mut writer, &payload).await?;

    let frame = Codec::read_frame(&mut reader)
        .await?
        .ok_or_else(|| ClientError::Io(std::io::Error::other("daemon closed before reply")))?;
    let reply: ServerMsg = postcard::from_bytes(&frame)
        .map_err(|e| plexy_glass_protocol::errors::CodecError::Decode(e.to_string()))?;
    match reply {
        ServerMsg::SessionKilled { name: n } => {
            println!("killed session: {n}");
            Ok(())
        }
        ServerMsg::Error(e) => Err(ClientError::DaemonError(e)),
        other => Err(ClientError::Io(std::io::Error::other(format!(
            "unexpected reply from daemon: {other:?}"
        )))),
    }
}

/// List all sessions and print a table to stdout.
pub async fn client_list() -> Result<(), ClientError> {
    let entries = list_sessions_inline().await?;
    print_sessions_table(&entries);
    Ok(())
}

/// Print a formatted table of session entries to stdout.
pub fn print_sessions_table(entries: &[plexy_glass_protocol::SessionEntry]) {
    if entries.is_empty() {
        println!("(no sessions)");
        return;
    }
    println!("{:<20}  {:>7}  {:>5}  {:>7}", "NAME", "WINDOWS", "PANES", "CLIENTS");
    for e in entries {
        println!("{:<20}  {:>7}  {:>5}  {:>7}", e.name, e.windows, e.panes, e.clients);
    }
}

/// Shared helper: open a connection, handshake, send `ListSessions`, return entries.
async fn list_sessions_inline() -> Result<Vec<plexy_glass_protocol::SessionEntry>, ClientError> {
    let socket = default_socket_path()?;
    let stream = connect_or_spawn(&socket).await?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    client_handshake(&mut reader, &mut writer).await?;

    let msg = ClientMsg::ListSessions;
    let payload = postcard::to_allocvec(&msg)
        .map_err(|e| plexy_glass_protocol::errors::CodecError::Encode(e.to_string()))?;
    Codec::write_frame(&mut writer, &payload).await?;

    let frame = Codec::read_frame(&mut reader)
        .await?
        .ok_or_else(|| ClientError::Io(std::io::Error::other("daemon closed before reply")))?;
    let reply: ServerMsg = postcard::from_bytes(&frame)
        .map_err(|e| plexy_glass_protocol::errors::CodecError::Decode(e.to_string()))?;
    match reply {
        ServerMsg::SessionList { entries } => Ok(entries),
        ServerMsg::Error(e) => Err(ClientError::DaemonError(e)),
        other => Err(ClientError::Io(std::io::Error::other(format!(
            "unexpected reply from daemon: {other:?}"
        )))),
    }
}

/// List sessions persisted on disk (running or not) and print a table.
pub async fn client_list_saved() -> Result<(), ClientError> {
    let socket = default_socket_path()?;
    let stream = connect_or_spawn(&socket).await?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    client_handshake(&mut reader, &mut writer).await?;

    let payload = postcard::to_allocvec(&ClientMsg::ListSavedSessions)
        .map_err(|e| plexy_glass_protocol::errors::CodecError::Encode(e.to_string()))?;
    Codec::write_frame(&mut writer, &payload).await?;

    let frame = Codec::read_frame(&mut reader)
        .await?
        .ok_or_else(|| ClientError::Io(std::io::Error::other("daemon closed before reply")))?;
    let reply: ServerMsg = postcard::from_bytes(&frame)
        .map_err(|e| plexy_glass_protocol::errors::CodecError::Decode(e.to_string()))?;
    match reply {
        ServerMsg::SavedSessionList { entries } => {
            if entries.is_empty() {
                println!("(no saved sessions)");
            } else {
                println!("{:<20}  {:>7}  {:>5}", "NAME", "WINDOWS", "PANES");
                for e in &entries {
                    println!("{:<20}  {:>7}  {:>5}", e.name, e.windows, e.panes);
                }
            }
            Ok(())
        }
        ServerMsg::Error(e) => Err(ClientError::DaemonError(e)),
        other => Err(ClientError::Io(std::io::Error::other(format!(
            "unexpected reply from daemon: {other:?}"
        )))),
    }
}

/// Attach to a session, creating it if it doesn't exist.
///
/// - explicit name supplied → attach-or-create that name
/// - 0 sessions → create and attach to "main"
/// - 1 session  → attach to that session
/// - 2+ sessions → print list, exit 1
pub async fn client_attach_smart(explicit_name: Option<String>) -> Result<(), ClientError> {
    match explicit_name {
        Some(n) => run(Some(n), true, Some(default_spawn_spec())).await,
        None => {
            let entries = list_sessions_inline().await?;
            match entries.len() {
                0 => run(Some("main".to_string()), true, Some(default_spawn_spec())).await,
                1 => run(Some(entries[0].name.clone()), false, None).await,
                n => {
                    eprintln!("error: {n} sessions exist; specify with -n NAME");
                    print_sessions_table(&entries);
                    std::process::exit(1);
                }
            }
        }
    }
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
    name: Option<String>,
    lines: Vec<String>,
) -> Result<bool, ClientError> {
    let socket = default_socket_path()?;
    for line in lines {
        let stream = connect_only(&socket).await?;
        let (mut reader, mut writer) = tokio::io::split(stream);
        client_handshake(&mut reader, &mut writer).await?;

        let msg = ClientMsg::RunCommand { session: name.clone(), line };
        let payload = postcard::to_allocvec(&msg)
            .map_err(|e| plexy_glass_protocol::errors::CodecError::Encode(e.to_string()))?;
        Codec::write_frame(&mut writer, &payload).await?;

        let frame = Codec::read_frame(&mut reader)
            .await?
            .ok_or_else(|| ClientError::Io(std::io::Error::other("daemon closed before reply")))?;
        let reply: ServerMsg = postcard::from_bytes(&frame)
            .map_err(|e| plexy_glass_protocol::errors::CodecError::Decode(e.to_string()))?;
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
                return Err(ClientError::Io(std::io::Error::other(format!(
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
    name: Option<String>,
    bytes: Vec<u8>,
) -> Result<bool, ClientError> {
    let socket = default_socket_path()?;
    let stream = connect_only(&socket).await?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    client_handshake(&mut reader, &mut writer).await?;

    let msg = ClientMsg::SendInput {
        session: name,
        bytes: bytes::Bytes::from(bytes),
    };
    let payload = postcard::to_allocvec(&msg)
        .map_err(|e| plexy_glass_protocol::errors::CodecError::Encode(e.to_string()))?;
    Codec::write_frame(&mut writer, &payload).await?;

    let frame = Codec::read_frame(&mut reader)
        .await?
        .ok_or_else(|| ClientError::Io(std::io::Error::other("daemon closed before reply")))?;
    let reply: ServerMsg = postcard::from_bytes(&frame)
        .map_err(|e| plexy_glass_protocol::errors::CodecError::Decode(e.to_string()))?;
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
        other => Err(ClientError::Io(std::io::Error::other(format!(
            "unexpected reply from daemon: {other:?}"
        )))),
    }
}

/// Capture the focused pane's visible screen text (popup-aware) and print to
/// stdout.
///
/// Returns `Ok(true)` on success, `Ok(false)` when the daemon reports an error
/// (message on stderr). No daemon → `Err`.
pub async fn client_capture(name: Option<String>) -> Result<bool, ClientError> {
    let socket = default_socket_path()?;
    let stream = connect_only(&socket).await?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    client_handshake(&mut reader, &mut writer).await?;

    let msg = ClientMsg::CapturePane { session: name };
    let payload = postcard::to_allocvec(&msg)
        .map_err(|e| plexy_glass_protocol::errors::CodecError::Encode(e.to_string()))?;
    Codec::write_frame(&mut writer, &payload).await?;

    let frame = Codec::read_frame(&mut reader)
        .await?
        .ok_or_else(|| ClientError::Io(std::io::Error::other("daemon closed before reply")))?;
    let reply: ServerMsg = postcard::from_bytes(&frame)
        .map_err(|e| plexy_glass_protocol::errors::CodecError::Decode(e.to_string()))?;
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
        other => Err(ClientError::Io(std::io::Error::other(format!(
            "unexpected reply from daemon: {other:?}"
        )))),
    }
}

fn default_spawn_spec() -> SpawnSpec {
    let program = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    SpawnSpec { program, args: vec![], env: vec![], cwd: None }
}

fn spawn_sigwinch_task(tx: mpsc::Sender<plexy_glass_protocol::PtySize>, fd: std::os::fd::OwnedFd) {
    tokio::spawn(async move {
        let mut sig = match tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::window_change(),
        ) {
            Ok(s) => s,
            Err(_) => return,
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
