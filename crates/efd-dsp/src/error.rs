use thiserror::Error;

#[derive(Debug, Error)]
pub enum DspError {
    #[error("IQ broadcast channel lagged by {0} messages")]
    Lagged(u64),

    #[error("IQ broadcast channel closed")]
    ChannelClosed,

    #[error("FFT cancelled")]
    Cancelled,
}
