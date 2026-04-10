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
}
