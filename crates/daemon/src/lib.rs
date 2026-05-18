//! plexy-glass daemon.

pub mod args;
pub mod connection;
pub mod error;
pub mod listener;
pub mod pane;
pub mod paths;
pub mod session;

pub use args::DaemonArgs;
pub use connection::Connection;
pub use error::DaemonError;
pub use listener::Listener;
pub use pane::Pane;
pub use paths::RuntimePaths;
pub use session::Session;

use tracing::{error, info};

pub async fn run(args: DaemonArgs) -> Result<(), DaemonError> {
    let paths = RuntimePaths::for_current_user()?;
    paths.create_dirs()?;

    let _log_guard = if args.foreground {
        // Logs already initialized by the top-level binary; nothing to do.
        None
    } else {
        use tracing_subscriber::Layer;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&paths.log_file)?;
        let (writer, guard) = tracing_appender::non_blocking(file);
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .with_ansi(false)
            .with_target(true)
            .with_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            );
        // Best-effort: if a global subscriber is already set (e.g., the top-level
        // binary in tests), keep using it.
        let _ = tracing_subscriber::registry().with(layer).try_init();
        Some(guard)
    };

    let listener = Listener::bind(paths)?;
    let daemon_pid = std::process::id();

    info!(foreground = args.foreground, "daemon ready, entering accept loop");
    loop {
        let (stream, _addr) = match listener.socket.accept().await {
            Ok(p) => p,
            Err(e) => {
                error!(error = %e, "accept failed");
                continue;
            }
        };
        tokio::spawn(async move {
            if let Err(e) = Connection::serve(stream, daemon_pid).await {
                error!(error = %e, "connection ended with error");
            }
        });
    }
}
