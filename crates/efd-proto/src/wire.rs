use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::downstream::{AudioChunk, ErrorMsg, FftBins, RadioState};
use crate::upstream::{CatCommand, Ptt, TxAudio};

/// Envelope for all server → client WebSocket messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub enum ServerMsg {
    FftBins(FftBins),
    Audio(AudioChunk),
    RadioState(RadioState),
    Error(ErrorMsg),
}

/// Envelope for all client → server WebSocket messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub enum ClientMsg {
    CatCommand(CatCommand),
    TxAudio(TxAudio),
    Ptt(Ptt),
}
