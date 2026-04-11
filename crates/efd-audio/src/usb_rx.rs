use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::error::AudioError;
use crate::PcmBlock;

/// Configuration for USB RX audio capture from the FDM-DUO.
#[derive(Debug, Clone)]
pub struct UsbRxConfig {
    /// ALSA device name for the FDM-DUO USB audio capture.
    pub device: String,
    pub sample_rate: u32,
}

impl Default for UsbRxConfig {
    fn default() -> Self {
        Self {
            device: "default".into(),
            sample_rate: 48_000,
        }
    }
}

/// Spawn the USB RX audio capture task.
///
/// Captures audio from the FDM-DUO USB audio interface (the radio's
/// hardware demodulator output) and sends PcmBlock to the mpsc channel.
pub fn spawn_usb_rx_task(
    config: UsbRxConfig,
    audio_tx: mpsc::Sender<PcmBlock>,
    cancel: CancellationToken,
) -> JoinHandle<Result<(), AudioError>> {
    tokio::task::spawn_blocking(move || run_usb_rx(config, audio_tx, cancel))
}

fn run_usb_rx(
    config: UsbRxConfig,
    audio_tx: mpsc::Sender<PcmBlock>,
    cancel: CancellationToken,
) -> Result<(), AudioError> {
    use alsa::pcm::{Access, Format, HwParams};
    use alsa::{Direction, PCM};

    let pcm = PCM::new(&config.device, Direction::Capture, false)?;

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

    // Read in 960-sample chunks (20ms at 48kHz) — matches Opus frame size.
    let frame_size = 960;
    let mut buf = vec![0.0f32; frame_size];

    info!(device = %config.device, "USB RX audio capture opened");

    loop {
        if cancel.is_cancelled() {
            break;
        }

        let io = pcm.io_f32()?;
        let mut offset = 0;
        while offset < frame_size {
            match io.readi(&mut buf[offset..]) {
                Ok(n) => offset += n,
                Err(e) => {
                    warn!("USB RX read error: {e}, recovering");
                    if let Err(e2) = pcm.recover(e.errno() as i32, true) {
                        warn!("USB RX recover failed: {e2}");
                        return Err(AudioError::Alsa(e));
                    }
                }
            }
        }

        let block = PcmBlock {
            samples: buf.clone(),
        };

        if audio_tx.blocking_send(block).is_err() {
            debug!("USB RX audio channel closed");
            break;
        }
    }

    info!("USB RX audio capture stopped");
    Ok(())
}
