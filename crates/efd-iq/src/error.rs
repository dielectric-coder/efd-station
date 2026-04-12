use efd_proto::SourceKind;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum IqError {
    #[error("USB error: {0}")]
    Usb(#[from] rusb::Error),

    #[error("device not found (VID=0x{vid:04X}, PID=0x{pid:04X})")]
    DeviceNotFound { vid: u16, pid: u16 },

    #[error("FIFO control failed: {0}")]
    FifoControl(String),

    #[error("streaming cancelled")]
    Cancelled,

    #[error("broadcast channel closed")]
    ChannelClosed,

    #[error("backend not implemented: {0:?}")]
    BackendNotImplemented(SourceKind),

    #[error("source {0:?} does not provide IQ")]
    SourceHasNoIq(SourceKind),
}
