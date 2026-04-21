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

    // The FDM-DUO USB audio is stereo S16_LE or S24_3LE natively.
    // Capture stereo S16_LE and downmix to mono f32 ourselves — more
    // reliable than relying on plughw channel conversion.
    let channels: u32 = 2;
    {
        let hwp = HwParams::any(&pcm)?;
        hwp.set_access(Access::RWInterleaved)?;
        hwp.set_format(Format::s16())?;
        hwp.set_channels(channels)?;
        hwp.set_rate(config.sample_rate, alsa::ValueOr::Nearest)?;
        let buffer_frames = (config.sample_rate / 10) as i64; // 100ms buffer
        hwp.set_buffer_size_near(buffer_frames)?;
        pcm.hw_params(&hwp)?;
    }

    pcm.prepare()?;

    // Read in 960-frame chunks (20ms at 48kHz) — matches Opus frame size.
    // Each frame = 2 samples (stereo interleaved).
    let frame_size = 960;
    let mut buf = vec![0i16; frame_size * channels as usize];
    let mut mono = Vec::with_capacity(frame_size);

    info!(
        device = %config.device,
        channels = channels,
        "USB RX audio capture opened"
    );

    loop {
        if cancel.is_cancelled() {
            break;
        }

        let io = pcm.io_i16()?;
        let mut offset = 0;
        while offset < frame_size {
            match io.readi(&mut buf[offset * channels as usize..]) {
                Ok(n) => offset += n,
                Err(e) => {
                    warn!("USB RX read error: {e}, recovering");
                    if let Err(e2) = pcm.recover(e.errno(), true) {
                        warn!("USB RX recover failed: {e2}");
                        return Err(AudioError::Alsa(e));
                    }
                }
            }
        }

        // Downmix stereo interleaved S16_LE to mono f32.
        mono.clear();
        let mut peak_i16: i16 = 0;
        for frame in buf[..frame_size * channels as usize].chunks_exact(channels as usize) {
            let sum = frame.iter().map(|&s| s as f32).sum::<f32>();
            for &s in frame {
                if s.abs() > peak_i16.abs() {
                    peak_i16 = s;
                }
            }
            // Divide by 32767 (not 32768) so encode/decode round-trip is
            // exact: f32 1.0 ↔ s16 32767, f32 -1.0 ↔ s16 -32767. The
            // corresponding encoder (usb_tx.rs) also uses 32767.
            mono.push(sum / (channels as f32 * 32767.0));
        }

        let peak_f32 = mono.iter().copied().map(f32::abs).fold(0.0f32, f32::max);
        static BLOCK_COUNT: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let n = BLOCK_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n < 5 || n % 250 == 0 {
            info!(
                block = n,
                peak_s16 = peak_i16,
                peak_f32 = peak_f32,
                "USB RX ALSA read"
            );
        }

        let block = PcmBlock { samples: mono.clone() };

        if audio_tx.blocking_send(block).is_err() {
            debug!("USB RX audio channel closed");
            break;
        }
    }

    info!("USB RX audio capture stopped");
    Ok(())
}
