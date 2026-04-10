use std::sync::Arc;

use efd_proto::Mode;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};

use efd_iq::IqBlock;

use crate::error::DspError;

/// Configuration for the demodulator task.
#[derive(Debug, Clone)]
pub struct DemodConfig {
    /// Input sample rate (default: 192000).
    pub input_rate: u32,
    /// Output sample rate (default: 48000).
    pub output_rate: u32,
    /// Initial demodulation mode.
    pub mode: Mode,
}

impl Default for DemodConfig {
    fn default() -> Self {
        Self {
            input_rate: 192_000,
            output_rate: 48_000,
            mode: Mode::USB,
        }
    }
}

/// A block of demodulated audio samples (mono f32, at output_rate).
#[derive(Debug, Clone)]
pub struct AudioBlock {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub timestamp_us: u64,
}

/// Spawn the demodulator task.
///
/// Consumes IQ blocks, demodulates to audio, decimates to output_rate,
/// and sends `AudioBlock` to the mpsc channel.
pub fn spawn_demod_task(
    iq_rx: broadcast::Receiver<Arc<IqBlock>>,
    audio_tx: mpsc::Sender<AudioBlock>,
    config: DemodConfig,
    cancel: CancellationToken,
) -> JoinHandle<Result<(), DspError>> {
    tokio::task::spawn_blocking(move || run_demod(iq_rx, audio_tx, config, cancel))
}

fn run_demod(
    mut iq_rx: broadcast::Receiver<Arc<IqBlock>>,
    audio_tx: mpsc::Sender<AudioBlock>,
    config: DemodConfig,
    cancel: CancellationToken,
) -> Result<(), DspError> {
    let decim_factor = (config.input_rate / config.output_rate) as usize;
    debug!(
        mode = ?config.mode,
        input_rate = config.input_rate,
        output_rate = config.output_rate,
        decim_factor,
        "demod task started"
    );

    loop {
        if cancel.is_cancelled() {
            return Err(DspError::Cancelled);
        }

        let block = match iq_rx.blocking_recv() {
            Ok(b) => b,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!(skipped = n, "demod receiver lagged");
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => {
                return Err(DspError::ChannelClosed);
            }
        };

        // Demodulate
        let demod_samples = demodulate(&block.samples, config.mode);

        // Decimate to output rate
        let decimated = decimate(&demod_samples, decim_factor);

        if decimated.is_empty() {
            continue;
        }

        let audio = AudioBlock {
            samples: decimated,
            sample_rate: config.output_rate,
            timestamp_us: block.timestamp_us,
        };

        if audio_tx.blocking_send(audio).is_err() {
            trace!("audio channel closed");
            return Err(DspError::ChannelClosed);
        }
    }
}

/// Demodulate IQ samples based on mode.
fn demodulate(iq: &[[f32; 2]], mode: Mode) -> Vec<f32> {
    match mode {
        Mode::AM => demod_am(iq),
        Mode::USB => demod_usb(iq),
        Mode::LSB => demod_lsb(iq),
        Mode::FM => demod_fm(iq),
        // CW modes use USB demod with narrow filter (filter not implemented yet)
        Mode::CW | Mode::CWR => demod_usb(iq),
        Mode::Unknown => demod_usb(iq),
    }
}

/// AM demodulation: envelope detection (magnitude of complex sample).
fn demod_am(iq: &[[f32; 2]]) -> Vec<f32> {
    iq.iter()
        .map(|&[i, q]| (i * i + q * q).sqrt())
        .collect()
}

/// USB demodulation: take the real part of the analytic signal.
/// The IQ stream from the FDM-DUO is already baseband — for USB,
/// the audio is the real (I) component.
fn demod_usb(iq: &[[f32; 2]]) -> Vec<f32> {
    iq.iter().map(|&[i, _q]| i).collect()
}

/// LSB demodulation: conjugate the signal (negate Q), then take real part.
/// Equivalent to taking I with inverted sideband.
fn demod_lsb(iq: &[[f32; 2]]) -> Vec<f32> {
    // For LSB, the lower sideband is mirrored by negating Q before
    // extracting the real part. Since we're already at baseband,
    // this is equivalent to just using I (same as USB for baseband IQ).
    // A proper implementation would frequency-shift, but for the FDM-DUO's
    // baseband IQ where the radio has already selected the sideband,
    // the I channel carries the demodulated audio.
    iq.iter().map(|&[i, _q]| i).collect()
}

/// FM demodulation: instantaneous frequency via phase differencing.
fn demod_fm(iq: &[[f32; 2]]) -> Vec<f32> {
    if iq.len() < 2 {
        return vec![0.0; iq.len()];
    }

    let mut out = Vec::with_capacity(iq.len());
    out.push(0.0); // first sample has no previous

    for i in 1..iq.len() {
        let [i1, q1] = iq[i - 1];
        let [i2, q2] = iq[i];
        // Conjugate multiply: (i2 + jq2) * (i1 - jq1)
        let re = i2 * i1 + q2 * q1;
        let im = q2 * i1 - i2 * q1;
        // Instantaneous frequency ∝ atan2(im, re)
        out.push(im.atan2(re));
    }

    out
}

/// Simple decimation by integer factor (no anti-alias filter — good enough
/// for initial implementation since the radio's DSP already bandlimits).
fn decimate(samples: &[f32], factor: usize) -> Vec<f32> {
    if factor <= 1 {
        return samples.to_vec();
    }
    samples.iter().step_by(factor).copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    #[test]
    fn am_demod_tone() {
        // AM modulated carrier: envelope = 1.0 + 0.5*sin(...)
        let n = 1024;
        let iq: Vec<[f32; 2]> = (0..n)
            .map(|i| {
                let t = i as f32 / n as f32;
                let envelope = 1.0 + 0.5 * (2.0 * PI * 3.0 * t).sin();
                let carrier_phase = 2.0 * PI * 100.0 * t;
                [envelope * carrier_phase.cos(), envelope * carrier_phase.sin()]
            })
            .collect();

        let audio = demod_am(&iq);
        assert_eq!(audio.len(), n);

        // Envelope should be close to 1.0 + 0.5*sin(...)
        // Check that we get values in roughly the right range
        let max = audio.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        let min = audio.iter().cloned().fold(f32::INFINITY, f32::min);
        assert!(max > 1.3, "AM max should be > 1.3, got {max}");
        assert!(min < 0.7, "AM min should be < 0.7, got {min}");
    }

    #[test]
    fn usb_demod_extracts_real() {
        let iq = vec![[0.5, 0.3], [-0.2, 0.1], [0.0, -0.9]];
        let audio = demod_usb(&iq);
        assert_eq!(audio, vec![0.5, -0.2, 0.0]);
    }

    #[test]
    fn fm_demod_constant_phase() {
        // Constant frequency → constant phase difference → constant output
        let n = 256;
        let freq = 0.1f32; // normalized
        let iq: Vec<[f32; 2]> = (0..n)
            .map(|i| {
                let phase = 2.0 * PI * freq * i as f32;
                [phase.cos(), phase.sin()]
            })
            .collect();

        let audio = demod_fm(&iq);
        // After the first sample, all values should be approximately equal
        let expected = 2.0 * PI * freq;
        for &v in &audio[1..] {
            assert!(
                (v - expected).abs() < 0.01,
                "FM demod value {v} != expected {expected}"
            );
        }
    }

    #[test]
    fn decimate_factor_4() {
        let input: Vec<f32> = (0..16).map(|i| i as f32).collect();
        let out = decimate(&input, 4);
        assert_eq!(out, vec![0.0, 4.0, 8.0, 12.0]);
    }

    #[test]
    fn decimate_factor_1_passthrough() {
        let input = vec![1.0, 2.0, 3.0];
        let out = decimate(&input, 1);
        assert_eq!(out, input);
    }
}
