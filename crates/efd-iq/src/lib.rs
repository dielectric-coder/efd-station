pub mod device;
pub mod error;
pub mod stream;

pub use device::{convert_samples, DeviceInfo, FdmDuo};
pub use error::IqError;
pub use stream::{spawn_iq_capture, IqConfig};

/// A block of IQ samples produced by the USB capture task.
#[derive(Debug, Clone)]
pub struct IqBlock {
    /// Normalized IQ samples, each `[I, Q]` in `[-1.0, 1.0]`.
    pub samples: Vec<[f32; 2]>,
    /// Monotonic timestamp in microseconds since capture start.
    pub timestamp_us: u64,
}
