//! plexy-glass client: owns the host TTY and proxies to the daemon.

pub mod args;
pub mod error;

pub use args::ClientArgs;
pub use error::ClientError;

/// Top-level entry point, wired in later tasks.
pub async fn run(_args: ClientArgs) -> Result<(), ClientError> {
    Err(ClientError::NotYetImplemented)
}
