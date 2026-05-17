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

pub async fn run(_args: DaemonArgs) -> Result<(), DaemonError> {
    Err(DaemonError::NotYetImplemented)
}
