use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::downstream::{AudioChunk, Capabilities, DrmStatus, ErrorMsg, FftBins, RadioState};
use crate::radio::Mode;
use crate::upstream::{AudioSource, CatCommand, Ptt, TxAudio};

/// Envelope for all server → client WebSocket messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub enum ServerMsg {
    FftBins(FftBins),
    Audio(AudioChunk),
    RadioState(RadioState),
    Capabilities(Capabilities),
    DrmStatus(DrmStatus),
    Error(ErrorMsg),
}

/// Envelope for all client → server WebSocket messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub enum ClientMsg {
    CatCommand(CatCommand),
    TxAudio(TxAudio),
    Ptt(Ptt),
    SetAudioSource(AudioSource),
    /// Set or clear the demod mode override. `Some(mode)` overrides (SDR),
    /// `None` clears the override so demod follows the radio's mode (MON).
    SetDemodMode(Option<Mode>),
}
