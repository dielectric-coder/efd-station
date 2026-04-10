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
    let mut agc = Agc::new();
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
                // Each IQ block is ~1536 samples at 192kHz (~8ms)
                warn!(
                    skipped_blocks = n,
                    skipped_ms = n * 8,
                    "demod receiver lagged, audio gap"
                );
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => {
                return Err(DspError::ChannelClosed);
            }
        };

        // Demodulate
        let mut demod_samples = demodulate(&block.samples, config.mode);

        // Apply AGC to bring weak signals to audible level
        agc.process(&mut demod_samples);

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
/// Output is normalized to [-1.0, 1.0] assuming max deviation of 5kHz
/// at 192kHz sample rate.
fn demod_fm(iq: &[[f32; 2]]) -> Vec<f32> {
    if iq.len() < 2 {
        return vec![0.0; iq.len()];
    }

    // Normalize: atan2 returns [-pi, pi] radians per sample.
    // Max expected FM deviation = 5kHz, at 192kHz sample rate:
    //   max_phase_per_sample = 2*pi*5000/192000 ≈ 0.1636 rad
    // We scale so that ±max_deviation maps to ±1.0.
    let max_phase = std::f32::consts::PI * 2.0 * 5000.0 / 192000.0;

    let mut out = Vec::with_capacity(iq.len());
    out.push(0.0);

    for i in 1..iq.len() {
        let [i1, q1] = iq[i - 1];
        let [i2, q2] = iq[i];
        let re = i2 * i1 + q2 * q1;
        let im = q2 * i1 - i2 * q1;
        let phase_diff = im.atan2(re);
        out.push((phase_diff / max_phase).clamp(-1.0, 1.0));
    }

    out
}

/// Simple AGC: measures peak level and applies gain to target -6 dB (0.5 peak).
/// Uses a slow attack / fast release to avoid pumping.
struct Agc {
    gain: f32,
    target: f32,
    attack: f32,  // gain reduction speed (fast)
    release: f32, // gain increase speed (slow)
    max_gain: f32,
}

impl Agc {
    fn new() -> Self {
        Self {
            gain: 1000.0, // start with high gain for weak signals
            target: 0.5,
            attack: 0.1,   // fast attack
            release: 0.001, // slow release
            max_gain: 100_000.0,
        }
    }

    fn process(&mut self, samples: &mut [f32]) {
        for s in samples.iter_mut() {
            *s *= self.gain;
            let level = s.abs();
            if level > self.target {
                // Too loud — reduce gain quickly
                self.gain *= 1.0 - self.attack * (level / self.target - 1.0).min(1.0);
            } else if level < self.target * 0.5 {
                // Too quiet — increase gain slowly
                self.gain *= 1.0 + self.release;
            }
            self.gain = self.gain.clamp(1.0, self.max_gain);
            // Hard clip to prevent distortion
            *s = s.clamp(-1.0, 1.0);
        }
    }
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
        // Constant frequency → constant phase difference → constant normalized output
        let n = 256;
        let freq = 0.01f32; // small normalized freq to stay within deviation range
        let iq: Vec<[f32; 2]> = (0..n)
            .map(|i| {
                let phase = 2.0 * PI * freq * i as f32;
                [phase.cos(), phase.sin()]
            })
            .collect();

        let audio = demod_fm(&iq);
        // After the first sample, all values should be approximately equal
        // (constant frequency → constant phase difference → constant output)
        let first = audio[1];
        for &v in &audio[2..] {
            assert!(
                (v - first).abs() < 0.01,
                "FM demod should be constant, got {v} vs {first}"
            );
        }
        // Should be non-zero (there is a frequency offset)
        assert!(first.abs() > 0.01, "FM demod output should be non-zero");
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
