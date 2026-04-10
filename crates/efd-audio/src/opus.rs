use audiopus::coder::{Decoder, Encoder};
use audiopus::packet::Packet;
use audiopus::{Application, Channels, MutSignals, SampleRate};

use crate::error::AudioError;

/// Opus frame size: 20ms at 48kHz = 960 samples.
pub const OPUS_FRAME_SIZE: usize = 960;

/// Maximum encoded Opus frame size in bytes.
const MAX_OPUS_PACKET: usize = 1275;

/// Opus encoder for outbound audio (demod → WS clients).
pub struct OpusEncoder {
    encoder: Encoder,
}

impl OpusEncoder {
    pub fn new() -> Result<Self, AudioError> {
        let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Audio)?;
        Ok(Self { encoder })
    }

    /// Encode a frame of `OPUS_FRAME_SIZE` f32 samples into Opus bytes.
    pub fn encode_float(&mut self, pcm: &[f32]) -> Result<Vec<u8>, AudioError> {
        let mut out = vec![0u8; MAX_OPUS_PACKET];
        let n = self.encoder.encode_float(pcm, &mut out)?;
        out.truncate(n);
        Ok(out)
    }
}

/// Opus decoder for inbound audio (WS TX audio → PCM).
pub struct OpusDecoder {
    decoder: Decoder,
}

impl OpusDecoder {
    pub fn new() -> Result<Self, AudioError> {
        let decoder = Decoder::new(SampleRate::Hz48000, Channels::Mono)?;
        Ok(Self { decoder })
    }

    /// Decode Opus bytes into f32 PCM samples.
    pub fn decode_float(&mut self, opus_data: &[u8]) -> Result<Vec<f32>, AudioError> {
        let mut out = vec![0.0f32; OPUS_FRAME_SIZE];
        let packet: Packet<'_> = opus_data.try_into().map_err(|_| {
            audiopus::Error::EmptyPacket
        })?;
        let signals: MutSignals<'_, f32> = (&mut out).try_into().map_err(|_| {
            audiopus::Error::EmptyPacket
        })?;
        let n = self.decoder.decode_float(Some(packet), signals, false)?;
        out.truncate(n);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    #[test]
    fn opus_round_trip() {
        let mut enc = OpusEncoder::new().unwrap();
        let mut dec = OpusDecoder::new().unwrap();

        // Generate a 1kHz tone at 48kHz, one frame
        let pcm: Vec<f32> = (0..OPUS_FRAME_SIZE)
            .map(|i| (2.0 * PI * 1000.0 * i as f32 / 48000.0).sin() * 0.5)
            .collect();

        // Encode
        let encoded = enc.encode_float(&pcm).unwrap();
        assert!(!encoded.is_empty(), "encoded data should not be empty");
        // Opus should compress: 960 f32 samples = 3840 bytes → typically < 200 bytes
        assert!(encoded.len() < 500, "Opus should compress, got {} bytes", encoded.len());

        // Decode
        let decoded = dec.decode_float(&encoded).unwrap();
        assert_eq!(decoded.len(), OPUS_FRAME_SIZE);

        // Decoded signal should have meaningful energy (not silence).
        // Note: we don't check sample-accurate SNR because Opus introduces
        // codec delay (lookahead), so samples are time-shifted.
        let energy: f32 = decoded.iter().map(|s| s * s).sum::<f32>() / OPUS_FRAME_SIZE as f32;
        assert!(
            energy > 0.01,
            "decoded audio should have energy, got RMS^2 = {energy}"
        );
    }
}
