//! plexy-glass daemon.

/// Crate-wide serialization for tests that mutate process-global env vars
/// (notably `XDG_STATE_HOME` for the persist/session/registry suites).
///
/// All such tests must lock this single mutex. Per-module locks would not
/// serialize across modules, so concurrent tests could clobber each other's
/// `XDG_STATE_HOME` and read the wrong session directory.
#[cfg(test)]
pub(crate) static STATE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub mod args;
pub mod connection;
pub mod error;
pub mod input_router;
pub mod listener;
pub mod osc_actions;
pub mod pane;
pub mod paste_buffers;
pub mod paths;
pub mod persist;
pub mod registry;
pub mod renderer;
pub mod session;
pub mod window;
pub mod window_manager;

pub use args::DaemonArgs;
pub use connection::Connection;
pub use error::DaemonError;
pub use input_router::{InputEvent, InputRouter};
pub use listener::Listener;
pub use pane::Pane;
pub use paths::RuntimePaths;
pub use registry::SessionRegistry;
pub use renderer::Renderer;
pub use session::{ClientHandle, Session};
pub use window::Window;
pub use window_manager::WindowManager;

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

    let (config, cfg_err) = plexy_glass_config::load_or_default();
    if let Some(e) = cfg_err {
        tracing::warn!(error = %e, "config load error; using built-in default");
    }
    let config = std::sync::Arc::new(config);

    let listener = Listener::bind(paths)?;
    let daemon_pid = std::process::id();
    let registry = std::sync::Arc::new(SessionRegistry::new());

    info!(foreground = args.foreground, "daemon ready, entering accept loop");
    loop {
        let (stream, _addr) = match listener.socket.accept().await {
            Ok(p) => p,
            Err(e) => {
                error!(error = %e, "accept failed");
                continue;
            }
        };
        let registry = std::sync::Arc::clone(&registry);
        let config = std::sync::Arc::clone(&config);
        tokio::spawn(async move {
            if let Err(e) = Connection::serve(stream, daemon_pid, registry, config).await {
                error!(error = %e, "connection ended with error");
            }
        });
    }
}
