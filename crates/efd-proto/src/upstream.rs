use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::radio::RecKind;

/// Raw Kenwood-style CAT command forwarded to the radio over USB serial.
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

/// Begin recording IQ or decoded audio to a file on the server.
///
/// If `path` is `None`, the server picks a timestamped filename under
/// its configured recordings directory. If `Some`, the server still
/// normalises / validates it (rejects traversal, enforces directory).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct StartRecording {
    pub kind: RecKind,
    pub path: Option<String>,
}
