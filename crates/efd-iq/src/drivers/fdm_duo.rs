//! FDM-DUO IQ driver: USB control/bulk transport + capture task.
//!
//! Ported from the EladSpectrum reference. Owns the rusb handle for the
//! 0x1721:0x061a device and streams normalized [I, Q] f32 samples onto a
//! broadcast channel. Also reports the FPGA LO centre frequency via a
//! `watch` channel so the FFT task can label its axis correctly.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rusb::{DeviceHandle, GlobalContext};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::error::IqError;
use crate::source::FdmDuoConfig;
use crate::types::IqBlock;

pub const ELAD_RF_ENDPOINT: u8 = 0x86;
pub const USB_BUFFER_SIZE: usize = 512 * 24; // 12288 bytes
pub const DEFAULT_SAMPLE_RATE: u32 = 192_000;
const S_RATE: i64 = 122_880_000;
const USB_TIMEOUT: Duration = Duration::from_millis(2000);
const CTRL_TIMEOUT: Duration = Duration::from_millis(1000);

/// Device info read during initialization.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub serial: String,
    pub hw_version_major: u8,
    pub hw_version_minor: u8,
    pub sample_rate_correction: i32,
}

/// Low-level handle wrapping the opened FDM-DUO USB device.
pub struct FdmDuo {
    handle: DeviceHandle<GlobalContext>,
    pub info: DeviceInfo,
}

impl FdmDuo {
    /// Open and initialize the FDM-DUO. Detaches kernel driver, claims
    /// interface, reads device info, initializes FIFO.
    pub fn open() -> Result<Self, IqError> {
        Self::open_with_ids(
            crate::source::ELAD_VENDOR_ID,
            crate::source::ELAD_PRODUCT_ID,
        )
    }

    pub fn open_with_ids(vid: u16, pid: u16) -> Result<Self, IqError> {
        let handle = rusb::open_device_with_vid_pid(vid, pid)
            .ok_or(IqError::DeviceNotFound { vid, pid })?;
        info!("FDM-DUO device opened");

        // Detach kernel driver if active
        if handle.kernel_driver_active(0).unwrap_or(false) {
            debug!("detaching kernel driver");
            let _ = handle.detach_kernel_driver(0);
        }

        // Claim interface
        handle.claim_interface(0)?;
        debug!("interface claimed");

        let mut dev = Self {
            handle,
            info: DeviceInfo {
                serial: String::new(),
                hw_version_major: 0,
                hw_version_minor: 0,
                sample_rate_correction: 0,
            },
        };

        dev.read_device_info();
        dev.init_fifo()?;
        dev.read_sample_rate_correction();

        info!(serial = %dev.info.serial,
              hw = format_args!("{}.{}", dev.info.hw_version_major, dev.info.hw_version_minor),
              correction = dev.info.sample_rate_correction,
              "FDM-DUO initialized");
        Ok(dev)
    }

    fn read_device_info(&mut self) {
        // USB driver version
        let mut buf = [0u8; 64];
        if let Ok(2) = self.handle.read_control(0xC0, 0xFF, 0x0000, 0x0000, &mut buf[..2], CTRL_TIMEOUT) {
            debug!(version = format_args!("{}.{}", buf[0], buf[1]), "USB driver version");
        }

        // HW version
        if let Ok(2) = self.handle.read_control(0xC0, 0xA2, 0x404C, 0x0151, &mut buf[..2], CTRL_TIMEOUT) {
            self.info.hw_version_major = buf[0];
            self.info.hw_version_minor = buf[1];
        }

        // Serial number
        if let Ok(32) = self.handle.read_control(0xC0, 0xA2, 0x4000, 0x0151, &mut buf[..32], CTRL_TIMEOUT) {
            self.info.serial = String::from_utf8_lossy(&buf[..32])
                .trim_end_matches('\0')
                .to_string();
        }
    }

    fn read_sample_rate_correction(&mut self) {
        let mut buf = [0u8; 4];
        if let Ok(4) = self.handle.read_control(0xC0, 0xA2, 0x4024, 0x0151, &mut buf, CTRL_TIMEOUT) {
            let correction = i32::from_le_bytes(buf);
            if (-1_000_000..=1_000_000).contains(&correction) {
                self.info.sample_rate_correction = correction;
            } else {
                warn!(correction, "sample rate correction out of range, ignoring");
            }
        }
    }

    /// Initialize FIFO: stop, init (set EP6 to slave mode).
    fn init_fifo(&self) -> Result<(), IqError> {
        let mut buf = [0u8; 1];

        // Stop FIFO
        let res = self.handle.read_control(0xC0, 0xE1, 0x0000, 0xE9 << 8, &mut buf, CTRL_TIMEOUT);
        if res != Ok(1) {
            warn!("stop FIFO returned {:?}", res);
        }

        // Init FIFO (set EP6 FIFO to slave mode)
        let res = self.handle.read_control(0xC0, 0xE1, 0x0000, 0xE8 << 8, &mut buf, CTRL_TIMEOUT);
        if res != Ok(1) {
            warn!("init FIFO returned {:?}", res);
        }

        debug!("FIFO initialized");
        Ok(())
    }

    /// Start FIFO streaming: clear halt, start FIFO.
    pub fn start_streaming(&self) -> Result<(), IqError> {
        let mut buf = [0u8; 1];

        // Re-init FIFO
        self.init_fifo()?;

        // Clear halt on bulk endpoint
        match self.handle.clear_halt(ELAD_RF_ENDPOINT) {
            Ok(()) => debug!("endpoint cleared"),
            Err(rusb::Error::NotFound) => {}
            Err(e) => warn!("clear halt: {e}"),
        }

        // Start FIFO
        let res = self.handle.read_control(0xC0, 0xE1, 0x0001, 0xE9 << 8, &mut buf, CTRL_TIMEOUT);
        match res {
            Ok(1) if buf[0] == 0xE9 => {
                debug!("streaming enabled");
                Ok(())
            }
            Ok(n) => Err(IqError::FifoControl(format!(
                "start FIFO: got {n} bytes, buf[0]=0x{:02X}",
                buf[0]
            ))),
            Err(e) => Err(IqError::Usb(e)),
        }
    }

    /// Stop FIFO streaming.
    pub fn stop_streaming(&self) {
        let mut buf = [0u8; 1];
        let _ = self.handle.read_control(0xC0, 0xE1, 0x0000, 0xE9 << 8, &mut buf, CTRL_TIMEOUT);
        debug!("streaming stopped");
    }

    /// Perform a synchronous bulk read from the RF endpoint.
    /// Returns the number of bytes actually read.
    pub fn bulk_read(&self, buf: &mut [u8]) -> Result<usize, rusb::Error> {
        self.handle.read_bulk(ELAD_RF_ENDPOINT, buf, USB_TIMEOUT)
    }

    /// Read the FPGA tuning frequency (LO center of IQ stream) in Hz.
    /// This is the actual center frequency of the IQ data, set by the radio's FPGA.
    pub fn read_frequency(&self) -> Result<u64, IqError> {
        let mut buf = [0u8; 16];
        let res = self.handle.read_control(0xC0, 0xE1, 0x0000, 0xF5 << 8, &mut buf[..11], CTRL_TIMEOUT);
        match res {
            Ok(11) => {
                // Frequency in bytes 1-4, big-endian
                let freq = ((buf[1] as u64) << 24)
                    | ((buf[2] as u64) << 16)
                    | ((buf[3] as u64) << 8)
                    | (buf[4] as u64);
                Ok(freq)
            }
            Ok(n) => Err(IqError::FifoControl(format!(
                "read_frequency: expected 11 bytes, got {n}"
            ))),
            Err(e) => Err(IqError::Usb(e)),
        }
    }

    /// Effective sample rate accounting for per-device correction.
    pub fn effective_clock(&self) -> i64 {
        S_RATE + self.info.sample_rate_correction as i64
    }
}

impl Drop for FdmDuo {
    fn drop(&mut self) {
        self.stop_streaming();
        let _ = self.handle.release_interface(0);
    }
}

/// Convert a buffer of raw USB data (32-bit signed LE IQ pairs) into
/// normalized f32 samples in [-1.0, 1.0].
///
/// Each IQ sample is 8 bytes: 4 bytes I (little-endian i32) + 4 bytes Q.
/// Returns a Vec of [I, Q] pairs.
pub fn convert_samples(usb_data: &[u8]) -> Vec<[f32; 2]> {
    const BYTES_PER_SAMPLE: usize = 8;
    // `count` floors to the last whole IQ pair, so the loop below can
    // never index past the end. A non-multiple length would mean the
    // device gave us a torn packet — log once and drop the trailing
    // bytes rather than corrupt the stream.
    let leftover = usb_data.len() % BYTES_PER_SAMPLE;
    if leftover != 0 {
        warn!(
            len = usb_data.len(),
            leftover,
            "fdm-duo convert_samples: buffer not a multiple of {BYTES_PER_SAMPLE} bytes, dropping tail"
        );
    }
    let count = usb_data.len() / BYTES_PER_SAMPLE;
    let mut out = Vec::with_capacity(count);

    for i in 0..count {
        let off = i * BYTES_PER_SAMPLE;
        let i_val = i32::from_le_bytes([
            usb_data[off],
            usb_data[off + 1],
            usb_data[off + 2],
            usb_data[off + 3],
        ]);
        let q_val = i32::from_le_bytes([
            usb_data[off + 4],
            usb_data[off + 5],
            usb_data[off + 6],
            usb_data[off + 7],
        ]);
        out.push([
            i_val as f32 / 2_147_483_648.0,
            q_val as f32 / 2_147_483_648.0,
        ]);
    }

    out
}

/// Spawn the FDM-DUO capture task. Runs the rusb blocking loop on a dedicated
/// `spawn_blocking` worker.
pub fn spawn(
    cfg: FdmDuoConfig,
    tx: broadcast::Sender<Arc<IqBlock>>,
    center_freq_tx: watch::Sender<u64>,
    cancel: CancellationToken,
) -> JoinHandle<Result<(), IqError>> {
    tokio::task::spawn_blocking(move || run(cfg, tx, center_freq_tx, cancel))
}

fn run(
    cfg: FdmDuoConfig,
    tx: broadcast::Sender<Arc<IqBlock>>,
    center_freq_tx: watch::Sender<u64>,
    cancel: CancellationToken,
) -> Result<(), IqError> {
    let dev = FdmDuo::open_with_ids(cfg.vendor_id, cfg.product_id)?;
    dev.start_streaming()?;

    // Read initial FPGA center frequency
    match dev.read_frequency() {
        Ok(freq) => {
            info!("IQ center frequency: {freq} Hz");
            let _ = center_freq_tx.send(freq);
        }
        Err(e) => warn!("could not read FPGA frequency: {e}"),
    }

    info!("IQ capture started (clock={})", dev.effective_clock());

    let start = Instant::now();
    let mut buf = vec![0u8; USB_BUFFER_SIZE];
    let mut block_count: u64 = 0;

    loop {
        if cancel.is_cancelled() {
            info!("IQ capture cancelled after {block_count} blocks");
            return Err(IqError::Cancelled);
        }

        match dev.bulk_read(&mut buf) {
            Ok(n) if n > 0 => {
                let samples = convert_samples(&buf[..n]);
                let timestamp_us = start.elapsed().as_micros() as u64;

                let block = Arc::new(IqBlock {
                    samples,
                    timestamp_us,
                });

                // If no receivers, silently drop the block
                let _ = tx.send(block);

                block_count += 1;
                if block_count % 1000 == 0 {
                    debug!(block_count, "IQ blocks captured");
                }
                // Periodically re-read FPGA center frequency (LO may change)
                if block_count % 500 == 0 {
                    if let Ok(freq) = dev.read_frequency() {
                        let _ = center_freq_tx.send(freq);
                    }
                }
            }
            Ok(_) => {
                // Zero-length read — unusual but not fatal
                warn!("zero-length USB read");
            }
            Err(rusb::Error::Timeout) => {
                // Timeout — retry
                continue;
            }
            Err(rusb::Error::NoDevice) => {
                error!("FDM-DUO disconnected");
                return Err(IqError::Usb(rusb::Error::NoDevice));
            }
            Err(e) => {
                error!("USB read error: {e}");
                return Err(IqError::Usb(e));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_samples_basic() {
        // i32 max → ~1.0, i32 min → -1.0, zero → 0.0
        let mut buf = [0u8; 24]; // 3 samples

        // Sample 0: I=0, Q=0
        // (already zero)

        // Sample 1: I = i32::MAX, Q = i32::MIN
        let i_max = i32::MAX.to_le_bytes();
        let q_min = i32::MIN.to_le_bytes();
        buf[8..12].copy_from_slice(&i_max);
        buf[12..16].copy_from_slice(&q_min);

        // Sample 2: I = 1073741824 (0.5), Q = -1073741824 (-0.5)
        let half_pos = 1_073_741_824i32.to_le_bytes();
        let half_neg = (-1_073_741_824i32).to_le_bytes();
        buf[16..20].copy_from_slice(&half_pos);
        buf[20..24].copy_from_slice(&half_neg);

        let samples = convert_samples(&buf);
        assert_eq!(samples.len(), 3);

        assert!((samples[0][0]).abs() < 1e-7);
        assert!((samples[0][1]).abs() < 1e-7);

        assert!((samples[1][0] - 1.0).abs() < 1e-6);
        assert!((samples[1][1] + 1.0).abs() < 1e-6);

        assert!((samples[2][0] - 0.5).abs() < 1e-6);
        assert!((samples[2][1] + 0.5).abs() < 1e-6);
    }

    #[test]
    fn convert_samples_partial_ignored() {
        // 10 bytes → 1 complete sample, 2 leftover bytes ignored
        let buf = [0u8; 10];
        let samples = convert_samples(&buf);
        assert_eq!(samples.len(), 1);
    }
}
