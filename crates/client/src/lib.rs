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

use plexy_glass_protocol::{SpawnSpec, client_handshake};
use std::os::fd::AsFd;
use tokio::sync::mpsc;
use tracing::info;

pub async fn run(_args: ClientArgs) -> Result<(), ClientError> {
    let socket = default_socket_path()?;
    let stream = connect_or_spawn(&socket).await?;
    let (mut reader, mut writer) = tokio::io::split(stream);

    let server_hello = client_handshake(&mut reader, &mut writer).await?;
    info!(daemon_pid = server_hello.daemon_pid, "connected to daemon");

    let stdin = tokio::io::stdin();
    let stdin_fd = stdin.as_fd();
    let _tty_guard = HostTty::enter_raw(stdin_fd)?;
    tty::install_emergency_restore(stdin_fd, _tty_guard.original_termios());
    let initial_size = current_size(stdin_fd)?;

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let spec = SpawnSpec {
        program: shell,
        args: vec![],
        env: vec![], // inherit from daemon for Phase 1
        cwd: None,
    };
    handshake_spawn(&mut reader, &mut writer, spec, initial_size).await?;

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
