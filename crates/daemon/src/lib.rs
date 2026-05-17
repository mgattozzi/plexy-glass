//! plexy-glass daemon.

pub mod args;
pub mod connection;
pub mod error;
pub mod listener;
pub mod paths;
pub mod session;

pub use args::DaemonArgs;
pub use connection::Connection;
pub use error::DaemonError;
pub use listener::Listener;
pub use paths::RuntimePaths;
pub use session::Session;

use tracing::{error, info};

pub async fn run(args: DaemonArgs) -> Result<(), DaemonError> {
    let paths = RuntimePaths::for_current_user()?;
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
