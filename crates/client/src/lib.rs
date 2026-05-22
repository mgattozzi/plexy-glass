//! plexy-glass client.

pub mod args;
pub mod error;
pub mod kill;
pub mod pump;
pub mod transport;
pub mod tty;

pub use args::ClientArgs;
pub use error::ClientError;
pub use kill::{KillOutcome, kill};
pub use pump::{handshake_spawn, pump};
pub use transport::{connect_or_spawn, default_socket_path};
pub use tty::{HostTty, current_size};

use plexy_glass_protocol::{ClientMsg, Codec, ServerMsg, SpawnSpec, client_handshake};
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

    let server_hello = client_handshake(&mut reader, &mut writer).await?;
    info!(daemon_pid = server_hello.daemon_pid, "connected to daemon");

    let stdin = tokio::io::stdin();
    let stdin_fd = stdin.as_fd();
    let _tty_guard = HostTty::enter_raw(stdin_fd)?;
    tty::install_emergency_restore(stdin_fd, _tty_guard.original_termios());
    // Enable SGR-encoded mouse coords (?1006h) and any-event tracking (?1003h).
    use std::io::Write as _;
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(b"\x1b[?1006h\x1b[?1003h");
    let _ = stdout.flush();
    // Enable the kitty keyboard protocol so the daemon receives
    // unambiguous modifier info. Terminals that don't support it
    // silently ignore. Disabled in HostTty::restore.
    let _ = stdout.write_all(b"\x1b[>1u");
    let _ = stdout.flush();
    // Enable bracketed paste mode so the host TTY wraps pasted bytes in
    // \x1b[200~...\x1b[201~. The daemon's PasteParser recognizes the
    // wrapper and forwards pastes to the active pane wrapped or stripped
    // depending on whether that pane has its own bracketed-paste mode on.
    let _ = stdout.write_all(b"\x1b[?2004h");
    let _ = stdout.flush();
    let initial_size = current_size(stdin_fd)?;

    let spec = spawn_cmd.unwrap_or_else(default_spawn_spec);
    handshake_spawn(&mut reader, &mut writer, name, create_if_missing, Some(spec), initial_size)
        .await?;

    // SIGWINCH plumbing.
    let (resize_tx, resize_rx) = mpsc::channel(4);
    let owned_fd = stdin.as_fd().try_clone_to_owned().map_err(ClientError::Io)?;
    spawn_sigwinch_task(resize_tx, owned_fd);

    let stdout = tokio::io::stdout();
    let stdin_for_pump = tokio::io::stdin();
    let exit_status = pump(reader, writer, stdin_for_pump, stdout, resize_rx).await?;
    info!(?exit_status, "session ended");
    if let plexy_glass_protocol::ExitStatus::Code(c) = exit_status
        && c != 0
    {
        std::process::exit(c);
    }
    Ok(())
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
