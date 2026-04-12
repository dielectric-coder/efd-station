use thiserror::Error;

#[derive(Debug, Error)]
pub enum CatError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("CAT connection lost")]
    Disconnected,

    #[error("invalid CAT response: {0}")]
    BadResponse(String),

    #[error("CAT task cancelled")]
    Cancelled,
}
