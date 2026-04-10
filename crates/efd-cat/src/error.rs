use thiserror::Error;

#[derive(Debug, Error)]
pub enum CatError {
    #[error("TCP I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("connection to rigctld lost")]
    Disconnected,

    #[error("invalid response from rigctld: {0}")]
    BadResponse(String),

    #[error("CAT task cancelled")]
    Cancelled,
}
