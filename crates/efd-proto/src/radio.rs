use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

/// Receive demodulation mode.
///
/// Wire numbering is positional (bincode); do not reorder variants on
/// the wire without bumping `PROTO_VERSION`.
///
/// `CWR` is CW-lower; `CW` is CW-upper. `DSB` is double-sideband
/// synchronous AM (carrier-present full bandwidth). `SAM` is
/// synchronous AM (PLL-locked envelope); `SAMU` / `SAML` are the
/// upper / lower single-sideband SAM variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub enum Mode {
    AM,
    SAM,
    SAMU,
    SAML,
    DSB,
    LSB,
    USB,
    CW,
    CWR,
    FM,
    /// Digital Radio Mondiale — decoded via the DREAM subprocess bridge
    /// (see `efd-dsp::drm` and `third_party/dream/`). Requires IQ input,
    /// so only available when `Capabilities::has_iq` is true.
    DRM,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub enum Vfo {
    A,
    B,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub enum AgcMode {
    Off,
    Slow,
    Medium,
    Fast,
}

/// Top-level source class selected in the UI (`AUD / IQ`). Separate
/// from [`SourceKind`], which identifies the device family within a
/// class. A single class can contain multiple device kinds
/// (e.g. `Audio` covers `PortableRadio`, FDM-DUO's USB audio
/// passthrough, and `AudioFile` replay).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub enum SourceClass {
    Audio,
    Iq,
}

/// Identifier for the active RF source backing this connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub enum SourceKind {
    FdmDuo,
    HackRf,
    RspDx,
    RtlSdr,
    PortableRadio,
    /// Recorded audio file (WAV / FLAC) replayed through the audio
    /// pipeline. Drives the FLAC-DRM and audio-replay topologies.
    AudioFile,
    /// Recorded IQ file replayed through the IQ pipeline. Drives
    /// IQ-replay topology (live analog / DRM decoding of captured IQ).
    IqFile,
}

impl SourceKind {
    /// The UI source class (`AUD` / `IQ`) that owns this device kind.
    pub fn class(self) -> SourceClass {
        match self {
            SourceKind::PortableRadio | SourceKind::AudioFile => SourceClass::Audio,
            SourceKind::FdmDuo
            | SourceKind::HackRf
            | SourceKind::RspDx
            | SourceKind::RtlSdr
            | SourceKind::IqFile => SourceClass::Iq,
        }
    }
}

/// Audio-domain digital decoder kind. Multiple decoders can be
/// enabled concurrently; enabling is independent of the active demod
/// [`Mode`]. `DecodedText` messages carry the `DecoderKind` that
/// produced them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum DecoderKind {
    Cw,
    Rtty,
    Psk,
    /// Multi-FSK (MFSK4/8/16/32 family).
    Mfsk,
    /// Weather fax.
    Fax,
    /// AFSK 1200 packet radio (AX.25).
    Pckt,
    Wspr,
    Ft8,
    Aprs,
}

/// Stable identifier for a discovered device. Two parts: the
/// `SourceKind` family and an opaque, backend-specific `id` string
/// (USB serial number, sysfs path, file path, etc.). Equality is
/// structural, so clients can round-trip the ID to select or save
/// state without interpreting `id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct DeviceId {
    pub kind: SourceKind,
    pub id: String,
}

/// Recording medium — IQ samples or decoded audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub enum RecKind {
    Iq,
    Audio,
}
