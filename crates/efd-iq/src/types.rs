/// A block of IQ samples produced by an IQ capture driver.
#[derive(Debug, Clone)]
pub struct IqBlock {
    /// Normalized IQ samples, each `[I, Q]` in `[-1.0, 1.0]`.
    pub samples: Vec<[f32; 2]>,
    /// Monotonic timestamp in microseconds since capture start.
    pub timestamp_us: u64,
}
