use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use efd_proto::TxAudio;

use crate::error::AudioError;
use crate::opus::OpusDecoder;

/// Configuration for the USB TX audio output.
#[derive(Debug, Clone)]
pub struct UsbTxConfig {
    /// ALSA device name for the FDM-DUO USB audio output.
    pub device: String,
    pub sample_rate: u32,
}

impl Default for UsbTxConfig {
    fn default() -> Self {
        Self {
            device: "default".into(),
            sample_rate: 48_000,
        }
    }
}

/// Spawn the USB TX audio task.
///
/// Consumes Opus-encoded `TxAudio` from the mpsc channel, decodes to PCM,
/// and writes to the FDM-DUO USB audio device via ALSA.
pub fn spawn_usb_tx_task(
    config: UsbTxConfig,
    tx_rx: mpsc::Receiver<TxAudio>,
    cancel: CancellationToken,
) -> JoinHandle<Result<(), AudioError>> {
    tokio::task::spawn_blocking(move || run_usb_tx(config, tx_rx, cancel))
}

fn run_usb_tx(
    config: UsbTxConfig,
    mut tx_rx: mpsc::Receiver<TxAudio>,
    cancel: CancellationToken,
) -> Result<(), AudioError> {
    use alsa::pcm::{Access, Format, HwParams};
    use alsa::{Direction, PCM};

    let pcm = PCM::new(&config.device, Direction::Playback, false)?;

    {
        let hwp = HwParams::any(&pcm)?;
        hwp.set_access(Access::RWInterleaved)?;
        hwp.set_format(Format::FloatLE)?;
        hwp.set_channels(1)?;
        hwp.set_rate(config.sample_rate, alsa::ValueOr::Nearest)?;
        let buffer_frames = (config.sample_rate / 10) as i64; // 100ms buffer
        hwp.set_buffer_size_near(buffer_frames)?;
        pcm.hw_params(&hwp)?;
    }

    pcm.prepare()?;
    let mut decoder = OpusDecoder::new()?;

    info!(device = %config.device, "USB TX audio opened");

    loop {
        if cancel.is_cancelled() {
            break;
        }

        let frame = match tx_rx.blocking_recv() {
            Some(f) => f,
            None => {
                debug!("TX audio channel closed");
                break;
            }
        };

        let pcm_data = match decoder.decode_float(&frame.opus_data) {
            Ok(d) => d,
            Err(e) => {
                warn!(seq = frame.seq, "Opus decode error: {e}");
                continue;
            }
        };

        let io = pcm.io_f32()?;
        let mut offset = 0;
        while offset < pcm_data.len() {
            match io.writei(&pcm_data[offset..]) {
                Ok(n) => offset += n,
                Err(e) => {
                    warn!("USB TX write error: {e}, recovering");
                    if let Err(e2) = pcm.recover(e.errno() as i32, true) {
                        warn!("USB TX recover failed: {e2}");
                        return Err(AudioError::Alsa(e));
                    }
                }
            }
        }
    }

    info!("USB TX audio stopped");
    Ok(())
}
