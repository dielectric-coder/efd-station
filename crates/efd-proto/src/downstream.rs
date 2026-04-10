use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::radio::{AgcMode, Mode, Vfo};

/// FFT magnitude bins — server computes, client renders spectrum + waterfall.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct FftBins {
    pub center_freq_hz: u64,
    pub span_hz: u32,
    pub ref_level_db: f32,
    /// Magnitude values in dB, FFT-shifted (DC center).
    pub bins: Vec<f32>,
    /// Monotonic timestamp in microseconds.
    pub timestamp_us: u64,
}

/// Opus-encoded audio chunk for network transport.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct AudioChunk {
    pub opus_data: Vec<u8>,
    /// Sequence number for gap detection.
    pub seq: u32,
}

/// Current radio state polled from rigctld.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct RadioState {
    pub vfo: Vfo,
    pub freq_hz: u64,
    pub mode: Mode,
    pub filter_bw: String,
    pub att: bool,
    pub lp: bool,
    pub agc: AgcMode,
    pub nr: bool,
    pub nb: bool,
    pub s_meter_db: f32,
    pub tx: bool,
}

/// Error reported to client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct ErrorMsg {
    pub code: u16,
    pub message: String,
}
