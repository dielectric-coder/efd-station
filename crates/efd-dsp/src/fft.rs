use std::sync::Arc;

use efd_proto::FftBins;
use rustfft::num_complex::Complex;
use rustfft::FftPlanner;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};

use crate::error::DspError;
use crate::window::blackman_harris;

// Re-export IqBlock from efd-iq would create a dependency.
// Instead, accept any type that provides [f32; 2] samples + timestamp.
// We define a minimal trait-free approach: the pipeline passes us an
// `IqBlock`-shaped struct. To avoid coupling efd-dsp to efd-iq,
// we accept a generic struct here.

/// A block of IQ samples consumed by the FFT task.
/// This mirrors `efd_iq::IqBlock` without creating a crate dependency.
#[derive(Debug, Clone)]
pub struct IqBlock {
    pub samples: Vec<[f32; 2]>,
    pub timestamp_us: u64,
}

/// Configuration for the FFT task.
#[derive(Debug, Clone)]
pub struct FftConfig {
    /// FFT size (default: 4096).
    pub fft_size: usize,
    /// Number of frames to average before publishing (default: 3).
    pub averaging: usize,
    /// Center frequency in Hz (metadata passed through to FftBins).
    pub center_freq_hz: u64,
    /// Span in Hz (metadata passed through to FftBins).
    pub span_hz: u32,
    /// Reference level in dBm (metadata passed through to FftBins).
    pub ref_level_db: f32,
}

impl Default for FftConfig {
    fn default() -> Self {
        Self {
            fft_size: 4096,
            averaging: 3,
            center_freq_hz: 7_100_000,
            span_hz: 192_000,
            ref_level_db: -20.0,
        }
    }
}

/// Spawn the FFT processing task.
///
/// Consumes IQ blocks from `iq_rx`, computes windowed FFT with averaging,
/// and publishes `FftBins` on `fft_tx`.
pub fn spawn_fft_task(
    iq_rx: broadcast::Receiver<Arc<IqBlock>>,
    fft_tx: broadcast::Sender<Arc<FftBins>>,
    config: FftConfig,
    cancel: CancellationToken,
) -> JoinHandle<Result<(), DspError>> {
    tokio::task::spawn_blocking(move || run_fft(iq_rx, fft_tx, config, cancel))
}

fn run_fft(
    mut iq_rx: broadcast::Receiver<Arc<IqBlock>>,
    fft_tx: broadcast::Sender<Arc<FftBins>>,
    config: FftConfig,
    cancel: CancellationToken,
) -> Result<(), DspError> {
    let fft_size = config.fft_size;
    let averaging = config.averaging.max(1);

    // Prepare FFT plan
    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(fft_size);

    // Precompute window
    let window = blackman_harris(fft_size);

    // Working buffers
    let mut sample_buf: Vec<[f32; 2]> = Vec::with_capacity(fft_size);
    let mut fft_buf: Vec<Complex<f32>> = vec![Complex::new(0.0, 0.0); fft_size];
    let mut accum: Vec<f32> = vec![0.0; fft_size];
    let mut avg_count = 0usize;
    let mut last_timestamp: u64;

    debug!(fft_size, averaging, "FFT task started");

    loop {
        if cancel.is_cancelled() {
            return Err(DspError::Cancelled);
        }

        // Block waiting for next IQ block (broadcast::Receiver is not async-only,
        // but we're on a blocking thread so use blocking_recv)
        let block = match iq_rx.blocking_recv() {
            Ok(b) => b,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!(skipped = n, "FFT receiver lagged");
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => {
                return Err(DspError::ChannelClosed);
            }
        };

        last_timestamp = block.timestamp_us;

        // Accumulate samples into the FFT window buffer
        for &[i_s, q_s] in &block.samples {
            sample_buf.push([i_s, q_s]);

            if sample_buf.len() == fft_size {
                // Apply window and fill FFT input
                for (idx, &[i_val, q_val]) in sample_buf.iter().enumerate() {
                    let w = window[idx] as f32;
                    fft_buf[idx] = Complex::new(i_val * w, q_val * w);
                }

                // Execute FFT (in-place)
                fft.process(&mut fft_buf);

                // Compute magnitude in dB with FFT shift, accumulate
                let half = fft_size / 2;
                for j in 0..fft_size {
                    let src = (j + half) % fft_size;
                    let re = fft_buf[src].re;
                    let im = fft_buf[src].im;
                    let mag = (re * re + im * im).sqrt() / fft_size as f32;
                    let mag = mag.max(1e-10);
                    let db = 20.0 * mag.log10();
                    accum[j] += db;
                }
                avg_count += 1;

                // Publish if averaging complete
                if avg_count >= averaging {
                    let bins: Vec<f32> = accum.iter().map(|&v| v / averaging as f32).collect();

                    let fft_bins = Arc::new(FftBins {
                        center_freq_hz: config.center_freq_hz,
                        span_hz: config.span_hz,
                        ref_level_db: config.ref_level_db,
                        bins,
                        timestamp_us: last_timestamp,
                    });

                    if fft_tx.send(fft_bins).is_err() {
                        trace!("no FFT receivers");
                    }

                    // Reset accumulator
                    accum.iter_mut().for_each(|v| *v = 0.0);
                    avg_count = 0;
                }

                sample_buf.clear();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    /// Generate a pure tone IQ block (complex exponential at `freq_bin` bins
    /// offset from center, for a given FFT size and sample rate).
    fn make_tone_block(fft_size: usize, freq_offset_bins: f32, num_samples: usize) -> IqBlock {
        let phase_per_sample = 2.0 * PI * freq_offset_bins / fft_size as f32;
        let samples: Vec<[f32; 2]> = (0..num_samples)
            .map(|n| {
                let phase = phase_per_sample * n as f32;
                [phase.cos(), phase.sin()]
            })
            .collect();
        IqBlock {
            samples,
            timestamp_us: 0,
        }
    }

    #[test]
    fn fft_detects_tone() {
        let fft_size = 1024;
        let tone_bin = 100.0; // tone at bin 100 from center

        // Build FFT processor inline (no async)
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(fft_size);
        let window = blackman_harris(fft_size);

        let block = make_tone_block(fft_size, tone_bin, fft_size);
        let mut buf: Vec<Complex<f32>> = block
            .samples
            .iter()
            .enumerate()
            .map(|(i, &[re, im])| {
                let w = window[i] as f32;
                Complex::new(re * w, im * w)
            })
            .collect();

        fft.process(&mut buf);

        // Compute magnitude spectrum with FFT shift
        let half = fft_size / 2;
        let mags: Vec<f32> = (0..fft_size)
            .map(|j| {
                let src = (j + half) % fft_size;
                let c = buf[src];
                (c.re * c.re + c.im * c.im).sqrt() / fft_size as f32
            })
            .collect();

        // Find peak bin (should be near center + tone_bin)
        let peak_bin = mags
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap()
            .0;

        // The tone is at +100 bins from center, which after FFT shift
        // maps to bin (half + 100)
        let expected_bin = half + tone_bin as usize;
        assert!(
            (peak_bin as i32 - expected_bin as i32).unsigned_abs() <= 1,
            "peak at bin {peak_bin}, expected near {expected_bin}"
        );
    }
}
