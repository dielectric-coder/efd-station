use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::downstream::{
    AudioChunk, Capabilities, DecodedText, DeviceList, DrmStatus, ErrorMsg, FftBins, RadioState,
    RecordingStatus, StateSnapshot,
};
use crate::radio::{DecoderKind, DeviceId, Mode, SourceClass};
use crate::upstream::{CatCommand, Ptt, StartRecording, TxAudio};

/// Wire-format version. Bump on any breaking change to `ServerMsg` or
/// `ClientMsg` (including reorderings and field additions in the middle of
/// existing variants — bincode is positional).
///
/// Every encoded frame is prefixed with this byte so the receiver can
/// reject mismatched peers cleanly instead of producing garbled state.
///
/// Version 2 — rework phase 1. Drops `SetAudioSource`; adds device
/// enumeration, source/device selection, per-decoder toggles, DSP
/// block toggles (DNB/DNR/DNF/APF), recording control, and
/// state save/load. RadioState gains parsed numeric fields (RIT/XIT
/// /IF shift / SNR). Capabilities gains `supported_decoders`.
///
/// Version 3 — rework phase 3a. Adds `ClientMsg::SetNb(bool)` for
/// the pre-IF noise blanker (distinct from the audio-domain DNB)
/// and the matching `StateSnapshot.nb_on` field.
pub const PROTO_VERSION: u8 = 3;

/// Envelope for all server → client WebSocket messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub enum ServerMsg {
    FftBins(FftBins),
    Audio(AudioChunk),
    RadioState(RadioState),
    Capabilities(Capabilities),
    DrmStatus(DrmStatus),
    Error(ErrorMsg),

    /// Response to `EnumerateDevices`; also pushed unprompted on
    /// hotplug / device-state change.
    DeviceList(DeviceList),
    /// Output from an audio-domain decoder (CW / RTTY / PSK / …).
    DecodedText(DecodedText),
    /// Status of the recording subsystem.
    RecordingStatus(RecordingStatus),
    /// Server-side persisted state, sent in reply to `SaveState` /
    /// `LoadState` and on startup.
    StateSnapshot(StateSnapshot),
}

/// Envelope for all client → server WebSocket messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub enum ClientMsg {
    CatCommand(CatCommand),
    TxAudio(TxAudio),
    Ptt(Ptt),

    /// Set or clear the demod mode override. `Some(mode)` overrides the
    /// radio (SDR demod), `None` clears the override so demod follows
    /// the radio's reported mode (MON-style).
    SetDemodMode(Option<Mode>),

    /// Runtime toggle for DREAM's `-p` (spectrum flip) flag.
    SetDrmFlipSpectrum(bool),

    /// Ask the server to publish its current `DeviceList`.
    EnumerateDevices,
    /// Switch between the `Audio` and `Iq` UI source classes.
    SelectSource(SourceClass),
    /// Select a specific device within the active source class.
    SelectDevice(DeviceId),

    /// Enable or disable a single audio-domain decoder.
    SetDecoder { decoder: DecoderKind, enabled: bool },

    /// Pre-IF noise blanker on the IQ stream (`NB` button in the
    /// client's `ctrl1-left`). Distinct from `SetDnb` — DNB is the
    /// audio-domain impulse blanker on the Audio-Out path, while
    /// this one runs on raw IQ before the IF demod, per the
    /// pipeline drawio.
    SetNb(bool),

    /// Toggle a DSP-block filter on the audio-out path. Each flag is
    /// independent; the block chain is DNB → DNR → DNF → APF.
    SetDnb(bool),
    SetDnr(bool),
    SetDnf(bool),
    SetApf(bool),

    /// Start recording IQ or decoded audio to a file.
    StartRecording(StartRecording),
    StopRecording,

    /// Explicitly save the current session snapshot to persistent
    /// storage. (The server also auto-saves on clean shutdown.)
    SaveState,
    /// Restore from the last saved snapshot.
    LoadState,
}

/// Decode error returned by [`decode_msg`]. Distinct from a bincode error
/// so the caller can log "version skew" once and disconnect rather than
/// flapping on every frame.
#[derive(Debug)]
pub enum WireError {
    /// The frame was empty (no version byte).
    Empty,
    /// The first byte didn't match [`PROTO_VERSION`].
    VersionMismatch { got: u8, want: u8 },
    /// Bincode failed to decode the payload after the version byte.
    Decode(bincode::error::DecodeError),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::Empty => write!(f, "empty wire frame (no version byte)"),
            WireError::VersionMismatch { got, want } => write!(
                f,
                "wire-format version mismatch: got {got}, want {want}"
            ),
            WireError::Decode(e) => write!(f, "bincode decode: {e}"),
        }
    }
}

impl std::error::Error for WireError {}

/// Encode a message: one [`PROTO_VERSION`] byte followed by the bincode
/// payload. Use this everywhere on the wire so version skew is detectable.
pub fn encode_msg<M>(msg: &M) -> Result<Vec<u8>, bincode::error::EncodeError>
where
    M: bincode::Encode,
{
    let cfg = bincode::config::standard();
    let mut out = Vec::with_capacity(64);
    out.push(PROTO_VERSION);
    let payload = bincode::encode_to_vec(msg, cfg)?;
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Decode a message: validate the leading version byte, then bincode the
/// rest. Returns [`WireError::VersionMismatch`] if the peer is on a
/// different wire version so callers can drop the connection cleanly.
pub fn decode_msg<M>(data: &[u8]) -> Result<M, WireError>
where
    M: bincode::Decode<()>,
{
    let (&first, rest) = data.split_first().ok_or(WireError::Empty)?;
    if first != PROTO_VERSION {
        return Err(WireError::VersionMismatch {
            got: first,
            want: PROTO_VERSION,
        });
    }
    let cfg = bincode::config::standard();
    let (msg, _): (M, _) = bincode::decode_from_slice(rest, cfg).map_err(WireError::Decode)?;
    Ok(msg)
}

#[cfg(test)]
mod wire_tests {
    use super::*;
    use crate::upstream::CatCommand;

    #[test]
    fn round_trip_carries_version() {
        let msg = ClientMsg::CatCommand(CatCommand {
            raw: "IF;".into(),
        });
        let bytes = encode_msg(&msg).unwrap();
        assert_eq!(bytes[0], PROTO_VERSION);
        let decoded: ClientMsg = decode_msg(&bytes).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn version_mismatch_is_detected() {
        let mut bytes = encode_msg(&ClientMsg::Ptt(crate::upstream::Ptt { on: true })).unwrap();
        bytes[0] = 99;
        let err = decode_msg::<ClientMsg>(&bytes).unwrap_err();
        match err {
            WireError::VersionMismatch { got: 99, want } => assert_eq!(want, PROTO_VERSION),
            other => panic!("expected version mismatch, got {other:?}"),
        }
    }

    #[test]
    fn empty_input_is_rejected() {
        let err = decode_msg::<ClientMsg>(&[]).unwrap_err();
        assert!(matches!(err, WireError::Empty));
    }
}
