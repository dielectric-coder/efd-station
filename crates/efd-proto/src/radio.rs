use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub enum Mode {
    AM,
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

/// Identifier for the active RF source backing this connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub enum SourceKind {
    FdmDuo,
    HackRf,
    RspDx,
    RtlSdr,
    PortableRadio,
}
