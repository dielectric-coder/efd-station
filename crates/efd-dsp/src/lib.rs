pub mod demod;
pub mod drm;
pub mod drm_file;
pub mod error;
pub mod fft;
pub mod filter;
pub mod window;

pub use demod::{spawn_demod_task, AudioBlock, DemodConfig, DemodTuning};
pub use drm::{spawn_drm_bridge, DrmConfig};
pub use drm_file::{spawn_drm_file_bridge, DrmFileHandles};
pub use filter::FirDecimator;
pub use error::DspError;
pub use fft::{spawn_fft_task, FftConfig};
pub use window::blackman_harris;

// Re-export IqBlock from efd-iq (single source of truth)
pub use efd_iq::IqBlock;
