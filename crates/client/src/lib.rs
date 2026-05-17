//! plexy-glass client.

pub mod args;
pub mod error;
pub mod tty;

pub use args::ClientArgs;
pub use error::ClientError;
pub use tty::{HostTty, current_size};

pub async fn run(_args: ClientArgs) -> Result<(), ClientError> {
    Err(ClientError::NotYetImplemented)
}
