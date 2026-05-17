//! plexy-glass daemon.

pub mod args;
pub mod error;
pub mod paths;

pub use args::DaemonArgs;
pub use error::DaemonError;
pub use paths::RuntimePaths;

pub async fn run(_args: DaemonArgs) -> Result<(), DaemonError> {
    Err(DaemonError::NotYetImplemented)
}
