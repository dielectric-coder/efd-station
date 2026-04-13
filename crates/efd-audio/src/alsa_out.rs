use alsa::pcm::{Access, Format, HwParams, State};
use alsa::{Direction, PCM};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::error::AudioError;

/// Configuration for ALSA playback.
#[derive(Debug, Clone)]
pub struct AlsaConfig {
    pub device: String,
    pub sample_rate: u32,
    /// Target latency in milliseconds.
    pub latency_ms: u32,
}

impl Default for AlsaConfig {
    fn default() -> Self {
        Self {
            device: "default".into(),
            sample_rate: 48_000,
            latency_ms: 50,
        }
    }
}

/// A block of PCM audio to play.
#[derive(Debug, Clone)]
pub struct PcmBlock {
    /// Mono f32 samples normalized to [-1.0, 1.0].
    pub samples: Vec<f32>,
}

/// Spawn the ALSA playback task.
///
/// Consumes `PcmBlock` from the mpsc channel and writes to the ALSA device.
pub fn spawn_alsa_task(
    config: AlsaConfig,
    audio_rx: mpsc::Receiver<PcmBlock>,
    cancel: CancellationToken,
) -> JoinHandle<Result<(), AudioError>> {
    tokio::task::spawn_blocking(move || run_alsa(config, audio_rx, cancel))
}

fn run_alsa(
    config: AlsaConfig,
    mut audio_rx: mpsc::Receiver<PcmBlock>,
    cancel: CancellationToken,
) -> Result<(), AudioError> {
    let pcm = PCM::new(&config.device, Direction::Playback, false)?;

    // Configure hardware params
    {
        let hwp = HwParams::any(&pcm)?;
        hwp.set_access(Access::RWInterleaved)?;
        hwp.set_format(Format::FloatLE)?;
        hwp.set_channels(1)?;
        hwp.set_rate(config.sample_rate, alsa::ValueOr::Nearest)?;

        // Buffer/period sizing for target latency
        let buffer_frames = (config.sample_rate * config.latency_ms / 1000) as i64;
        let period_frames = buffer_frames / 4;
        hwp.set_buffer_size_near(buffer_frames)?;
        hwp.set_period_size_near(period_frames, alsa::ValueOr::Nearest)?;

        pcm.hw_params(&hwp)?;
    }

    let actual_rate = pcm.hw_params_current()?.get_rate()?;
    let actual_buffer = pcm.hw_params_current()?.get_buffer_size()?;
    let actual_period = pcm.hw_params_current()?.get_period_size()?;

    info!(
        device = %config.device,
        rate = actual_rate,
        buffer = actual_buffer,
        period = actual_period,
        "ALSA playback opened"
    );

    pcm.prepare()?;

    loop {
        if cancel.is_cancelled() {
            break;
        }

        // Use blocking_recv with a timeout check
        let block = match audio_rx.blocking_recv() {
            Some(b) => b,
            None => {
                debug!("audio channel closed");
                break;
            }
        };

        // Write f32 samples to ALSA
        let io = pcm.io_f32()?;
        let mut offset = 0;
        while offset < block.samples.len() {
            match io.writei(&block.samples[offset..]) {
                Ok(n) => offset += n,
                Err(e) => {
                    warn!("ALSA write error: {e}, recovering");
                    if let Err(e2) = pcm.recover(e.errno(), true) {
                        error!("ALSA recover failed: {e2}");
                        return Err(AudioError::Alsa(e));
                    }
                }
            }
        }
    }

    // Drain remaining audio
    if pcm.state() == State::Running {
        let _ = pcm.drain();
    }

    info!("ALSA playback stopped");
    Ok(())
}
