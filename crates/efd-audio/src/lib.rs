pub mod alsa_out;
pub mod discover;
pub mod error;
pub mod file_source;
pub mod opus;
pub mod usb_rx;
pub mod usb_tx;

pub use alsa_out::{spawn_alsa_task, AlsaConfig, PcmBlock};
pub use discover::{
    discover_alsa_devices, ensure_fdmduo_capture_state, probe_capture, resolve_device, FdmDuoAlsa,
};
pub use error::AudioError;
pub use file_source::{spawn_file_source_task, FileSourceConfig};
pub use opus::{OpusDecoder, OpusEncoder, OPUS_FRAME_SIZE};
pub use usb_rx::{spawn_usb_rx_task, UsbRxConfig};
pub use usb_tx::{spawn_usb_tx_task, UsbTxConfig};
