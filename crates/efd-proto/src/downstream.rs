use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::radio::{AgcMode, DecoderKind, DeviceId, Mode, RecKind, SourceKind, Vfo};

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

/// Current radio state polled from the active source.
///
/// Carries both display-friendly strings (`filter_bw`) and the
/// parsed numeric fields the client needs to draw overlays
/// (`filter_bw_hz`). Server parses once, client consumes directly —
/// no second parser on the client side.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct RadioState {
    pub vfo: Vfo,
    pub freq_hz: u64,
    pub mode: Mode,
    /// Human-readable bandwidth string from the radio (e.g. `"2.4k"`,
    /// `"500"`, `"D300"`). Kept for display.
    pub filter_bw: String,
    /// Parsed bandwidth in Hz, if `filter_bw` could be interpreted as
    /// a numeric width. `None` for labels the server could not parse.
    pub filter_bw_hz: Option<f64>,
    /// Raw `P2` filter index from the radio's `RF<P1><P2P2>;` answer.
    /// Lets the client preselect the BW dropdown without having to
    /// back-parse `filter_bw`. `None` when the RF poll failed or the
    /// current source has no radio behind it.
    pub filter_idx: Option<u8>,
    pub att: bool,
    pub lp: bool,
    pub agc: AgcMode,
    pub agc_threshold: u8,
    pub nr: bool,
    pub nb: bool,
    pub s_meter_db: f32,
    pub tx: bool,
    /// Receiver Incremental Tuning offset in Hz (±). `0` when RIT is
    /// off or cleared.
    pub rit_hz: i32,
    pub rit_on: bool,
    /// Transmit Incremental Tuning offset in Hz.
    pub xit_hz: i32,
    pub xit_on: bool,
    /// IF shift in Hz, from the radio or the software demod.
    pub if_offset_hz: i32,
    /// Estimated signal-to-noise ratio in dB, when the active source
    /// reports one. Typically only populated in IQ / SDR mode where
    /// the demod computes it; MON mode may leave it `None`.
    pub snr_db: Option<f32>,
}

/// Error reported to client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct ErrorMsg {
    pub code: u16,
    pub message: String,
}

/// Where client CAT-style controls are routed.
///
/// Computed server-side from the active source + audio routing so the
/// client never has to replicate the decision. Client greys CAT widgets
/// when `None`; server routes `CatCommand`s accordingly.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub enum ControlTarget {
    /// No CAT surface. Audio-only source with no hardware CAT (USB
    /// dongle, portable radio). Client greys all CAT controls.
    None,
    /// Native hardware CAT (FDM-DUO serial). All controls go to the
    /// radio. Used in AUD + FDM-DUO.
    Radio,
    /// Software demod. All controls go to the demod. Used in IQ with
    /// non-FDM-DUO sources (HackRF, RSPdx, RTL).
    Demod,
    /// Software demod with frequency mirrored to the radio. Mode / BW /
    /// filters / AGC go to the demod only; frequency changes move both
    /// radio and demod. Used in IQ + FDM-DUO.
    DemodMirrorFreq,
}

/// What the active source supports. Sent on WS connect and any time the
/// active source changes. Clients gate UI based on these flags.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct Capabilities {
    pub source: SourceKind,
    pub has_iq: bool,
    pub has_tx: bool,
    pub has_hardware_cat: bool,
    /// USB audio passthrough (FDM-DUO) or USB-dongle line-in (portable
    /// radio). Independent of `has_hardware_cat`.
    pub has_usb_audio: bool,
    pub supported_demod_modes: Vec<Mode>,
    /// Audio-domain decoders the server can run against the current
    /// source's audio stream. Clients enable individual decoders via
    /// `ClientMsg::SetDecoder`.
    pub supported_decoders: Vec<DecoderKind>,
    /// Initial state of DREAM's `-p` flag as the server will use it on
    /// the next DRM bridge spawn. Client uses this to sync its Flip
    /// toggle on connect.
    pub drm_flip_spectrum: bool,
    /// Where client CAT controls are routed. Single source of truth for
    /// both client-side greying and server-side routing.
    pub control_target: ControlTarget,
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

/// Response to `ClientMsg::EnumerateDevices`. Also pushed unprompted
/// when the server's view of available devices changes (e.g. hotplug).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct DeviceList {
    /// Devices in the `Audio` class (portable radios, USB dongles,
    /// recorded WAV/FLAC files offered for replay).
    pub audio_devices: Vec<DeviceId>,
    /// Devices in the `Iq` class (SDRs and IQ-file replays).
    pub iq_devices: Vec<DeviceId>,
    /// Currently selected device, if any.
    pub active: Option<DeviceId>,
}

/// Output from an audio-domain decoder (Tier 3 in the pipeline
/// taxonomy). Emitted as the decoder produces text — rate varies by
/// mode and signal. Client routes to the `disp2-center` cell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct DecodedText {
    pub decoder: DecoderKind,
    pub text: String,
    pub timestamp_us: u64,
}

/// Recording subsystem status. Sent in response to start/stop
/// commands, and periodically while a recording is active.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct RecordingStatus {
    pub active: bool,
    pub kind: Option<RecKind>,
    /// Absolute path of the output file, chosen by the server.
    pub path: Option<String>,
    pub bytes_written: u64,
    /// Seconds of content written so far, computed from the source
    /// sample rate. `None` until the first block lands.
    pub duration_s: Option<f64>,
}

/// Snapshot of persisted state — device selection plus tuning plus
/// DSP toggles. Sent in response to `ClientMsg::SaveState` /
/// `ClientMsg::LoadState`, and on startup so the client can pre-fill
/// UI before any poll arrives.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct StateSnapshot {
    pub active_device: Option<DeviceId>,
    pub freq_hz: u64,
    pub mode: Mode,
    pub filter_bw_hz: Option<f64>,
    pub rit_hz: i32,
    pub xit_hz: i32,
    pub if_offset_hz: i32,
    pub enabled_decoders: Vec<DecoderKind>,
    /// Pre-IF noise blanker (IQ domain), the `NB` UI toggle.
    pub nb_on: bool,
    pub dnb_on: bool,
    pub dnr_on: bool,
    pub dnf_on: bool,
    pub apf_on: bool,
}
