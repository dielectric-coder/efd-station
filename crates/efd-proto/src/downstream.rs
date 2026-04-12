use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::radio::{AgcMode, Mode, SourceKind, Vfo};

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
    pub agc_threshold: u8,
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

/// What the active source supports. Sent on WS connect and any time the
/// active source changes. Clients gate UI based on these flags.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct Capabilities {
    pub source: SourceKind,
    pub has_iq: bool,
    pub has_tx: bool,
    pub has_hardware_cat: bool,
    pub supported_demod_modes: Vec<Mode>,
}

/// Live DRM decoder status parsed from DREAM's TUI output.
///
/// Sent at ~1 Hz while DRM decoding is active; not sent at all when
/// the current demod mode is something else. Unknown fields are `None`
/// so the client can format "---" placeholders matching the TUI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct DrmStatus {
    /// Individual sync/decode lock indicators (one-hot per TUI column).
    pub io_ok: bool,
    pub time_ok: bool,
    pub frame_ok: bool,
    pub fac_ok: bool,
    pub sdc_ok: bool,
    pub msc_ok: bool,

    pub if_level_db: Option<f32>,
    pub snr_db: Option<f32>,
    pub wmer_db: Option<f32>,
    pub mer_db: Option<f32>,

    pub dc_freq_hz: Option<f32>,
    pub sample_offset_hz: Option<f32>,
    pub doppler_hz: Option<f32>,
    pub delay_ms: Option<f32>,

    /// DRM robustness mode: "A" / "B" / "C" / "D".
    pub robustness_mode: Option<String>,
    pub bandwidth_khz: Option<u32>,
    /// SDC modulation scheme, e.g. "16-QAM".
    pub sdc_mode: Option<String>,
    /// MSC modulation scheme, e.g. "SM 64-QAM".
    pub msc_mode: Option<String>,
    pub interleaver_s: Option<u32>,

    pub num_audio_services: u8,
    pub num_data_services: u8,

    /// Monotonic timestamp in microseconds since the DRM bridge started.
    pub timestamp_us: u64,
}
