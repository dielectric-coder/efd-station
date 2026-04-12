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
