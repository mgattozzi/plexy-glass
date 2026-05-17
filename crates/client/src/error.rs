use thiserror::Error;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("not yet implemented")]
    NotYetImplemented,
}
