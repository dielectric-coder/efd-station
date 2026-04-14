use thiserror::Error;

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("ALSA error: {0}")]
    Alsa(#[from] alsa::Error),

    #[error("Opus error: {0}")]
    Opus(#[from] audiopus::Error),

    #[error("audio channel closed")]
    ChannelClosed,

    #[error("audio task cancelled")]
    Cancelled,

    #[error("file source I/O: {0}")]
    FileIo(#[from] std::io::Error),

    #[error("file source decode: {0}")]
    Decode(#[from] symphonia::core::errors::Error),

    #[error("file source config: {0}")]
    FileConfig(String),
}
