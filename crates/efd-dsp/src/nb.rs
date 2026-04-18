//! Pre-IF noise blanker (IQ domain).
//!
//! The `NB` box in the pipeline drawio — sits between the IQ source
//! and `IQ → IF`, operating on the complex baseband stream. Distinct
//! from [`AudioDsp`]'s `DNB` stage, which does a similar job in the
//! audio domain after demod.
//!
//! Algorithm (phase 3b): envelope-threshold impulse blanker. Tracks
//! the mean instantaneous magnitude |I+jQ| through a slow EWMA, and
//! replaces any sample whose magnitude exceeds `k` times the running
//! mean with zero. Simple, allocation-free, and targets exactly the
//! class of noise this blanker is meant for — short high-amplitude
//! impulses (ignition noise, arcing, bursty static). Voice / AM
//! modulation rides on a slowly-varying envelope, so setting the
//! EWMA time constant much longer than a modulation period keeps the
//! envelope mean stable; impulses are shorter than that envelope can
//! follow, so they stand out cleanly.
//!
//! Tunables are compile-time constants here; if we expose user
//! controls (threshold slider, aggressiveness preset) in a later
//! phase they slot into `NoiseBlankerConfig`.
//!
//! [`AudioDsp`]: crate::AudioDsp
use std::sync::Arc;

use efd_iq::IqBlock;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};

use crate::error::DspError;

/// EWMA smoothing factor for the envelope tracker.
///
/// At 192 kHz, 1/1024 ≈ 190 Hz bandwidth — fast enough to track the
/// baseband envelope of voice/AM modulation but much slower than
/// impulse spikes.
const ENV_ALPHA: f32 = 1.0 / 1024.0;

/// Magnitude multiplier above the running envelope at which a sample
/// is declared an impulse. Five is a classic ham-radio NB default —
/// empirically rejects ignition and lightning static without touching
/// CW keying or voice peaks.
const BLANK_THRESHOLD: f32 = 5.0;

/// Configuration for [`spawn_noise_blanker`].
#[derive(Debug, Clone)]
pub struct NoiseBlankerConfig {
    /// Initial enable state. `false` means pass-through, equivalent
    /// to the block being absent.
    pub enabled: bool,
}

impl Default for NoiseBlankerConfig {
    fn default() -> Self {
        Self { enabled: false }
    }
}

/// Per-task envelope tracker state. Kept across input blocks so the
/// EWMA doesn't have to re-converge on every packet boundary.
struct NbState {
    /// Running estimate of mean sample magnitude. Seeded to 1.0 on
    /// first use and converges within a few hundred samples.
    env_mean: f32,
}

impl NbState {
    fn new() -> Self {
        Self { env_mean: 0.0 }
    }
}

/// Spawn the noise-blanker task.
///
/// Subscribes to `iq_in` (raw IQ from the capture driver) and
/// publishes to `iq_out` (clean IQ consumed by the IF demod). In
/// pass-through / disabled mode the input `Arc<IqBlock>` is
/// forwarded zero-copy; the enabled path clones the block before
/// mutating so the raw-IQ consumer (FFT) keeps a pristine view.
///
/// The `enabled_rx` watch channel lets the WS upstream toggle the
/// blanker at runtime without tearing the pipeline down.
pub fn spawn_noise_blanker(
    iq_in: broadcast::Receiver<Arc<IqBlock>>,
    iq_out: broadcast::Sender<Arc<IqBlock>>,
    enabled_rx: tokio::sync::watch::Receiver<bool>,
    cancel: CancellationToken,
) -> JoinHandle<Result<(), DspError>> {
    tokio::spawn(async move { run(iq_in, iq_out, enabled_rx, cancel).await })
}

async fn run(
    mut iq_in: broadcast::Receiver<Arc<IqBlock>>,
    iq_out: broadcast::Sender<Arc<IqBlock>>,
    enabled_rx: tokio::sync::watch::Receiver<bool>,
    cancel: CancellationToken,
) -> Result<(), DspError> {
    debug!("NB task started");
    let mut state = NbState::new();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Err(DspError::Cancelled),
            r = iq_in.recv() => {
                let block = match r {
                    Ok(b) => b,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "NB: input broadcast lagged");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => return Err(DspError::ChannelClosed),
                };
                let out = if *enabled_rx.borrow() {
                    let mut cloned: IqBlock = (*block).clone();
                    blank(&mut cloned.samples, &mut state);
                    Arc::new(cloned)
                } else {
                    block
                };
                if iq_out.send(out).is_err() {
                    trace!("NB: iq_out has no subscribers");
                }
            }
        }
    }
}

/// Envelope-threshold impulse blanker on IQ samples.
///
/// For each sample, compute `|I + jQ|`, update the EWMA envelope
/// estimate, and if the sample exceeds `BLANK_THRESHOLD × env_mean`
/// zero it out. The envelope tracker does *not* see impulse samples
/// in its update (we check-before-update), so a strong impulse
/// doesn't bias the mean upward and desensitise the detector for
/// the next block.
fn blank(samples: &mut [[f32; 2]], state: &mut NbState) {
    for s in samples.iter_mut() {
        let mag = (s[0] * s[0] + s[1] * s[1]).sqrt();
        // Seed the envelope on first real sample — avoids a long
        // run of false-positive blanks while env_mean is still 0.
        if state.env_mean == 0.0 {
            state.env_mean = mag;
        }
        if mag > BLANK_THRESHOLD * state.env_mean {
            s[0] = 0.0;
            s[1] = 0.0;
        } else {
            state.env_mean = ENV_ALPHA * mag + (1.0 - ENV_ALPHA) * state.env_mean;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mag(s: [f32; 2]) -> f32 {
        (s[0] * s[0] + s[1] * s[1]).sqrt()
    }

    #[test]
    fn steady_signal_passes_through() {
        // A constant envelope shouldn't trip the blanker.
        let mut s = NbState::new();
        let mut samples: Vec<[f32; 2]> = (0..4096).map(|_| [0.1, 0.0]).collect();
        let before: Vec<[f32; 2]> = samples.clone();
        blank(&mut samples, &mut s);
        assert_eq!(samples, before);
    }

    #[test]
    fn lone_impulse_is_blanked() {
        // Envelope settles at 0.1; inject a 10× spike at sample
        // 2048 and verify it's zeroed while its neighbours survive.
        let mut s = NbState::new();
        let mut samples: Vec<[f32; 2]> = (0..4096).map(|_| [0.1, 0.0]).collect();
        samples[2048] = [1.0, 0.0]; // 10× the envelope
        blank(&mut samples, &mut s);
        assert!(mag(samples[2048]) < 1e-6, "impulse should be zeroed");
        assert!((mag(samples[2047]) - 0.1).abs() < 1e-6, "neighbour preserved");
        assert!((mag(samples[2049]) - 0.1).abs() < 1e-6, "neighbour preserved");
    }

    #[test]
    fn below_threshold_spike_survives() {
        // A 3× spike is under the 5× threshold — should pass.
        let mut s = NbState::new();
        let mut samples: Vec<[f32; 2]> = (0..4096).map(|_| [0.1, 0.0]).collect();
        samples[2048] = [0.3, 0.0];
        blank(&mut samples, &mut s);
        assert!((mag(samples[2048]) - 0.3).abs() < 1e-6);
    }

    #[test]
    fn impulse_does_not_bias_envelope() {
        // After a big impulse the tracker should still be close to
        // the pre-impulse envelope — otherwise a single spike
        // raises the detector's floor and the next few spikes pass.
        let mut s = NbState::new();
        let mut samples: Vec<[f32; 2]> = (0..4096).map(|_| [0.1, 0.0]).collect();
        samples[100] = [5.0, 0.0];
        blank(&mut samples, &mut s);
        assert!((s.env_mean - 0.1).abs() < 0.02, "env stayed near 0.1, got {}", s.env_mean);
    }
}
