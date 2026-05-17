use thiserror::Error;

#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("not yet implemented")]
    NotYetImplemented,
}
