//! plexy-glass daemon: owns PTYs and serves clients over a Unix socket.

pub mod args;
pub mod error;

pub use args::DaemonArgs;
pub use error::DaemonError;

/// Top-level entry point, wired in later tasks.
pub async fn run(_args: DaemonArgs) -> Result<(), DaemonError> {
    Err(DaemonError::NotYetImplemented)
}
