use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

/// Raw Kenwood-style CAT command forwarded to rigctld.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct CatCommand {
    pub raw: String,
}

/// Opus-encoded TX audio from client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct TxAudio {
    pub opus_data: Vec<u8>,
    /// Sequence number for gap detection.
    pub seq: u32,
}

/// Push-to-talk control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct Ptt {
    pub on: bool,
}
