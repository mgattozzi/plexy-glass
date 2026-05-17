//! plexy-glass client.

pub mod args;
pub mod error;
pub mod pump;
pub mod transport;
pub mod tty;

pub use args::ClientArgs;
pub use error::ClientError;
pub use pump::{handshake_spawn, pump};
pub use transport::{connect_or_spawn, default_socket_path};
pub use tty::{HostTty, current_size};

pub async fn run(_args: ClientArgs) -> Result<(), ClientError> {
    Err(ClientError::NotYetImplemented)
}
