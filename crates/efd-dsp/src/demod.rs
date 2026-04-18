use std::f32::consts::PI;
use std::sync::Arc;

use efd_proto::Mode;
use tokio::sync::{broadcast, mpsc, watch};
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

/// Runtime tuning parameters for the demod task.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DemodTuning {
    pub mode: Mode,
    /// VFO offset from IQ center in Hz (positive = above center).
    /// The demod will frequency-shift the IQ stream by this amount.
    pub vfo_offset_hz: f64,
    /// Channel filter bandwidth in Hz.
    pub filter_bw_hz: f64,
}

impl Default for DemodTuning {
    fn default() -> Self {
        Self {
            mode: Mode::USB,
            vfo_offset_hz: 0.0,
            filter_bw_hz: 3000.0,
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
/// Consumes IQ blocks, frequency-shifts to VFO, applies channel filter,
/// demodulates, applies AGC, decimates, and sends `AudioBlock` out.
///
/// `tuning_rx` carries runtime changes to mode, VFO offset, and filter BW.
///
/// `drm_if_tx`, if `Some`, receives wideband-SSB audio-IF samples when
/// the active mode is [`Mode::DRM`] — this is the stream consumed by
/// [`crate::drm`] to feed DREAM. Listenable-audio modes (USB/LSB/AM/CW/
/// FM) continue to write to `audio_tx`; under DRM the normal audio path
/// goes silent (the DRM bridge produces listenable audio via its own
/// `audio_tx` clone once it decodes). If `None`, DRM mode simply emits
/// nothing — useful for test builds or pipelines that haven't wired a
/// DRM bridge.
pub fn spawn_demod_task(
    iq_rx: broadcast::Receiver<Arc<IqBlock>>,
    audio_tx: mpsc::Sender<AudioBlock>,
    drm_if_tx: Option<broadcast::Sender<AudioBlock>>,
    config: DemodConfig,
    tuning_rx: watch::Receiver<DemodTuning>,
    cancel: CancellationToken,
) -> JoinHandle<Result<(), DspError>> {
    tokio::task::spawn_blocking(move || {
        run_demod(iq_rx, audio_tx, drm_if_tx, config, tuning_rx, cancel)
    })
}

/// NCO (Numerically Controlled Oscillator) for frequency shifting.
/// Uses f64 phase accumulation for precision over long runs.
struct Nco {
    phase: f64,
    phase_inc: f64, // radians per sample
}

impl Nco {
    fn new(freq_hz: f64, sample_rate: u32) -> Self {
        Self {
            phase: 0.0,
            phase_inc: 2.0 * std::f64::consts::PI * freq_hz / sample_rate as f64,
        }
    }

    fn set_freq(&mut self, freq_hz: f64, sample_rate: u32) {
        self.phase_inc = 2.0 * std::f64::consts::PI * freq_hz / sample_rate as f64;
    }

    /// Frequency-shift IQ samples by mixing with e^(-j*2*pi*f*t).
    /// This moves the signal at `freq_hz` to DC.
    fn shift(&mut self, iq: &[[f32; 2]], out: &mut Vec<[f32; 2]>) {
        out.clear();
        out.reserve(iq.len());
        for &[i, q] in iq {
            let cos = self.phase.cos() as f32;
            let sin = self.phase.sin() as f32;
            // Complex multiply: (i + jq) * (cos + jsin)
            // With negative freq, this shifts the signal down to DC
            out.push([i * cos - q * sin, q * cos + i * sin]);
            self.phase += self.phase_inc;
            // Keep phase in [-pi, pi] to avoid precision loss
            if self.phase > std::f64::consts::PI {
                self.phase -= 2.0 * std::f64::consts::PI;
            } else if self.phase < -std::f64::consts::PI {
                self.phase += 2.0 * std::f64::consts::PI;
            }
        }
    }
}

/// Complex FIR decimator: low-pass filter + downsample for IQ signals.
/// Applies the same real-valued FIR to both I and Q channels.
struct ComplexDecimator {
    coeffs: Vec<f32>,
    history_i: Vec<f32>,
    history_q: Vec<f32>,
    taps: usize,
    factor: usize,
    pos: usize,
}

impl ComplexDecimator {
    fn new(factor: usize, num_taps: usize) -> Self {
        let taps = num_taps | 1;
        let cutoff = 0.45 / factor as f32;
        let coeffs = design_lowpass(taps, cutoff);
        Self {
            coeffs,
            history_i: vec![0.0; taps],
            history_q: vec![0.0; taps],
            taps,
            factor,
            pos: 0,
        }
    }

    fn reset(&mut self) {
        self.history_i.fill(0.0);
        self.history_q.fill(0.0);
        self.pos = 0;
    }

    fn process(&mut self, iq: &[[f32; 2]], out: &mut Vec<[f32; 2]>) {
        out.clear();
        if self.factor <= 1 {
            out.extend_from_slice(iq);
            return;
        }
        out.reserve(iq.len() / self.factor + 1);
        for &[i, q] in iq {
            self.history_i.copy_within(1.., 0);
            self.history_i[self.taps - 1] = i;
            self.history_q.copy_within(1.., 0);
            self.history_q[self.taps - 1] = q;
            self.pos += 1;

            if self.pos >= self.factor {
                self.pos = 0;
                let mut sum_i = 0.0f32;
                let mut sum_q = 0.0f32;
                for (idx, &c) in self.coeffs.iter().enumerate() {
                    sum_i += self.history_i[idx] * c;
                    sum_q += self.history_q[idx] * c;
                }
                out.push([sum_i, sum_q]);
            }
        }
    }
}

/// Complex FIR channel filter at the decimated sample rate.
/// Much sharper selectivity than filtering at the original rate.
///
/// For SSB modes, uses complex (asymmetric) coefficients to pass only
/// the desired sideband: USB passes [0, +bw], LSB passes [-bw, 0].
/// For AM/FM, uses real (symmetric) coefficients passing [-bw/2, +bw/2].
struct ChannelFilter {
    coeffs_i: Vec<f32>,
    coeffs_q: Vec<f32>,
    history_i: Vec<f32>,
    history_q: Vec<f32>,
    taps: usize,
}

impl ChannelFilter {
    fn new(bandwidth_hz: f64, sample_rate: u32, num_taps: usize, mode: Mode) -> Self {
        let taps = num_taps | 1;
        let (coeffs_i, coeffs_q) = design_channel_filter(taps, bandwidth_hz, sample_rate, mode);
        Self {
            coeffs_i,
            coeffs_q,
            history_i: vec![0.0; taps],
            history_q: vec![0.0; taps],
            taps,
        }
    }

    fn configure(&mut self, bandwidth_hz: f64, sample_rate: u32, mode: Mode) {
        let (ci, cq) = design_channel_filter(self.taps, bandwidth_hz, sample_rate, mode);
        self.coeffs_i = ci;
        self.coeffs_q = cq;
        self.reset();
    }

    fn reset(&mut self) {
        self.history_i.fill(0.0);
        self.history_q.fill(0.0);
    }

    fn process(&mut self, iq: &[[f32; 2]], out: &mut Vec<[f32; 2]>) {
        out.clear();
        out.reserve(iq.len());
        for &[i, q] in iq {
            self.history_i.copy_within(1.., 0);
            self.history_i[self.taps - 1] = i;
            self.history_q.copy_within(1.., 0);
            self.history_q[self.taps - 1] = q;

            // Complex convolution: (coeffs_i + j·coeffs_q) * (in_i + j·in_q)
            let mut sum_i = 0.0f32;
            let mut sum_q = 0.0f32;
            for idx in 0..self.taps {
                let ci = self.coeffs_i[idx];
                let cq = self.coeffs_q[idx];
                let hi = self.history_i[idx];
                let hq = self.history_q[idx];
                sum_i += ci * hi - cq * hq;
                sum_q += ci * hq + cq * hi;
            }
            out.push([sum_i, sum_q]);
        }
    }
}

/// Design channel filter coefficients — complex bandpass for SSB,
/// real symmetric lowpass for AM/FM.
fn design_channel_filter(
    taps: usize,
    bandwidth_hz: f64,
    sample_rate: u32,
    mode: Mode,
) -> (Vec<f32>, Vec<f32>) {
    let cutoff = (bandwidth_hz / 2.0) / sample_rate as f64;
    let h = design_lowpass(taps, cutoff as f32);

    match mode {
        Mode::USB | Mode::CW => {
            // Shift lowpass down by bw/2 to create bandpass [0, +bw]
            let shift = -bandwidth_hz / 2.0 / sample_rate as f64;
            shift_filter(&h, shift)
        }
        Mode::LSB | Mode::CWR => {
            // Shift lowpass up by bw/2 to create bandpass [-bw, 0]
            let shift = bandwidth_hz / 2.0 / sample_rate as f64;
            shift_filter(&h, shift)
        }
        _ => {
            // Symmetric lowpass for AM, FM, etc.
            let coeffs_q = vec![0.0; taps];
            (h, coeffs_q)
        }
    }
}

/// Frequency-shift real FIR coefficients to create a complex bandpass.
/// h_bp[n] = h[n] · e^(j·2π·f_shift·n)
fn shift_filter(h: &[f32], f_shift: f64) -> (Vec<f32>, Vec<f32>) {
    let mut ci = Vec::with_capacity(h.len());
    let mut cq = Vec::with_capacity(h.len());
    for (n, &coeff) in h.iter().enumerate() {
        let phase = 2.0 * std::f64::consts::PI * f_shift * n as f64;
        ci.push(coeff * phase.cos() as f32);
        cq.push(coeff * phase.sin() as f32);
    }
    (ci, cq)
}


/// DC blocking filter — removes DC offset from demodulated audio.
struct DcBlock {
    prev_in: f32,
    prev_out: f32,
    alpha: f32, // pole position, typically 0.995-0.999
}

impl DcBlock {
    fn new() -> Self {
        Self {
            prev_in: 0.0,
            prev_out: 0.0,
            alpha: 0.998,
        }
    }

    fn process(&mut self, samples: &mut [f32]) {
        for s in samples.iter_mut() {
            let inp = *s;
            *s = inp - self.prev_in + self.alpha * self.prev_out;
            self.prev_in = inp;
            self.prev_out = *s;
        }
    }
}

/// DRM wideband audio-IF: 10 kHz symmetric passband centered at DC so
/// both sidebands of the OFDM block are preserved. Same filter slot as
/// the narrow SSB filter but rebuilt on mode transitions into/out of DRM.
const DRM_IF_BW_HZ: f64 = 10_000.0;

fn run_demod(
    mut iq_rx: broadcast::Receiver<Arc<IqBlock>>,
    audio_tx: mpsc::Sender<AudioBlock>,
    drm_if_tx: Option<broadcast::Sender<AudioBlock>>,
    config: DemodConfig,
    mut tuning_rx: watch::Receiver<DemodTuning>,
    cancel: CancellationToken,
) -> Result<(), DspError> {
    let decim_factor = (config.input_rate / config.output_rate) as usize;

    // Complex decimator: 192kHz → 48kHz IQ with anti-alias filter
    let decim_taps = 8 * decim_factor + 1;
    let mut iq_decimator = ComplexDecimator::new(decim_factor, decim_taps);

    // Channel filter at 48kHz — 127 taps gives ~375Hz transition bandwidth
    // (much sharper than 65 taps at 192kHz which had ~3kHz transition)
    // For SSB modes, uses complex bandpass to select only the desired sideband.
    // For DRM, the same slot is configured as a wide (10 kHz) symmetric
    // passband so both OFDM sidebands survive on their way to DREAM.
    let mut chan_filter = ChannelFilter::new(
        DemodTuning::default().filter_bw_hz,
        config.output_rate,
        127,
        config.mode,
    );

    let mut dc_block = DcBlock::new();
    let mut agc = Agc::new();

    let mut tuning = *tuning_rx.borrow_and_update();
    let mut nco = Nco::new(-tuning.vfo_offset_hz, config.input_rate);
    let initial_bw = if tuning.mode == Mode::DRM {
        DRM_IF_BW_HZ
    } else {
        tuning.filter_bw_hz
    };
    // See mode-change branch below for the rationale — DRM reuses the
    // symmetric AM-filter shape.
    let initial_filter_mode = if tuning.mode == Mode::DRM {
        Mode::AM
    } else {
        tuning.mode
    };
    chan_filter.configure(initial_bw, config.output_rate, initial_filter_mode);

    // Reusable buffers
    let mut shifted_buf: Vec<[f32; 2]> = Vec::new();
    let mut decimated_iq: Vec<[f32; 2]> = Vec::new();
    let mut filtered_buf: Vec<[f32; 2]> = Vec::new();

    debug!(
        mode = ?tuning.mode,
        vfo_offset = tuning.vfo_offset_hz,
        filter_bw = tuning.filter_bw_hz,
        input_rate = config.input_rate,
        output_rate = config.output_rate,
        decim_factor,
        chan_filter_taps = 127,
        "demod task started"
    );

    // Running count of audio blocks we had to drop because `audio_tx`
    // was full. See the `try_send` site below for rationale.
    let mut dropped_audio_blocks: u64 = 0;

    loop {
        if cancel.is_cancelled() {
            return Err(DspError::Cancelled);
        }

        // Check for tuning changes (non-blocking)
        if tuning_rx.has_changed().unwrap_or(false) {
            let new_tuning = *tuning_rx.borrow_and_update();
            if new_tuning != tuning {
                if new_tuning.vfo_offset_hz != tuning.vfo_offset_hz {
                    nco.set_freq(-new_tuning.vfo_offset_hz, config.input_rate);
                }
                if new_tuning.filter_bw_hz != tuning.filter_bw_hz
                    || new_tuning.mode != tuning.mode
                {
                    if new_tuning.mode != tuning.mode {
                        debug!(old = ?tuning.mode, new = ?new_tuning.mode, "demod mode changed");
                        iq_decimator.reset();
                    }
                    let effective_bw = if new_tuning.mode == Mode::DRM {
                        DRM_IF_BW_HZ
                    } else {
                        new_tuning.filter_bw_hz
                    };
                    // DRM uses an AM-style symmetric filter shape so both
                    // sidebands of the OFDM block pass. The ChannelFilter's
                    // `AM` arm already does this; reuse it here rather than
                    // add a DRM-specific code path.
                    let filter_mode = if new_tuning.mode == Mode::DRM {
                        Mode::AM
                    } else {
                        new_tuning.mode
                    };
                    chan_filter.configure(effective_bw, config.output_rate, filter_mode);
                }
                tuning = new_tuning;
            }
        }

        let block = match iq_rx.blocking_recv() {
            Ok(b) => b,
            Err(broadcast::error::RecvError::Lagged(n)) => {
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

        // 1. NCO frequency shift at 192kHz (center VFO at DC)
        nco.shift(&block.samples, &mut shifted_buf);

        // 2. Complex decimate 192kHz → 48kHz (anti-alias + downsample IQ)
        iq_decimator.process(&shifted_buf, &mut decimated_iq);

        // 3. Channel filter at 48kHz (sharp selectivity: 127 taps ≈ 375Hz transition)
        chan_filter.process(&decimated_iq, &mut filtered_buf);

        // DRM branch: emit the filtered real part as wideband audio-IF and
        // loop. No demodulation, no DC block, no AGC — DREAM does its own
        // signal processing on the raw IF, and AGC here would corrupt OFDM
        // amplitude statistics.
        if tuning.mode == Mode::DRM {
            if let Some(tx) = &drm_if_tx {
                let samples: Vec<f32> = filtered_buf.iter().map(|&[i, _q]| i).collect();
                if !samples.is_empty() {
                    let block = AudioBlock {
                        samples,
                        sample_rate: config.output_rate,
                        timestamp_us: block.timestamp_us,
                    };
                    // broadcast::send returns Err only when all receivers
                    // have been dropped — treat that as a no-consumer state
                    // (harmless) rather than a fatal error, so the demod
                    // keeps running if the DRM bridge comes and goes.
                    let _ = tx.send(block);
                }
            }
            continue;
        }

        // 4. Demodulate (now at 48kHz)
        let mut audio = demodulate(&filtered_buf, tuning.mode);

        // 5. DC block (removes DC offset from demod)
        dc_block.process(&mut audio);

        // 6. AGC
        agc.process(&mut audio);

        if audio.is_empty() {
            continue;
        }

        let block = AudioBlock {
            samples: audio,
            sample_rate: config.output_rate,
            timestamp_us: block.timestamp_us,
        };

        // Non-blocking send: if the downstream audio consumer (ALSA
        // task, WS audio encoder) falls behind and the mpsc buffer is
        // full, drop this block rather than stall the whole demod.
        // Stalling here would also stall the IQ broadcast subscriber
        // and the NCO/decimator state, producing far worse artifacts
        // than an occasional audio glitch. Log-rate-limit the warn so
        // a sustained stall doesn't flood the journal.
        match audio_tx.try_send(block) {
            Ok(_) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                dropped_audio_blocks = dropped_audio_blocks.saturating_add(1);
                if dropped_audio_blocks.is_power_of_two() {
                    warn!(
                        dropped = dropped_audio_blocks,
                        "audio consumer slow; demod dropped block(s)"
                    );
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                trace!("audio channel closed");
                return Err(DspError::ChannelClosed);
            }
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
        Mode::CW | Mode::CWR => demod_usb(iq),
        // In DRM mode this `demodulate()` path is only reached for the
        // *listenable-audio* output (audio_tx) — which is silent, because
        // the DRM bridge is the producer of listenable audio once DREAM
        // decodes. The wideband audio-IF that feeds DREAM is produced by
        // `demod_drm_audio_if` below and routed through `drm_if_tx`.
        Mode::DRM => vec![0.0; iq.len()],
        Mode::Unknown => demod_usb(iq),
    }
}

/// AM demodulation: envelope detection.
fn demod_am(iq: &[[f32; 2]]) -> Vec<f32> {
    iq.iter()
        .map(|&[i, q]| (i * i + q * q).sqrt())
        .collect()
}

/// USB demodulation: real part of the (now baseband-centered) analytic signal.
fn demod_usb(iq: &[[f32; 2]]) -> Vec<f32> {
    iq.iter().map(|&[i, _q]| i).collect()
}

/// LSB demodulation: real part extraction.
/// Sideband selection is handled by the complex channel filter which
/// passes only [-bw, 0], so taking Re{} recovers the audio correctly.
fn demod_lsb(iq: &[[f32; 2]]) -> Vec<f32> {
    iq.iter().map(|&[i, _q]| i).collect()
}

/// FM demodulation: instantaneous frequency via phase differencing.
fn demod_fm(iq: &[[f32; 2]]) -> Vec<f32> {
    if iq.len() < 2 {
        return vec![0.0; iq.len()];
    }

    let max_phase = PI * 2.0 * 5000.0 / 192000.0;

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

/// Simple AGC.
struct Agc {
    gain: f32,
    target: f32,
    attack: f32,
    release: f32,
    max_gain: f32,
}

impl Agc {
    fn new() -> Self {
        Self {
            gain: 1.0,
            target: 0.5,
            attack: 0.1,
            release: 0.005,
            max_gain: 100_000.0,
        }
    }

    fn process(&mut self, samples: &mut [f32]) {
        for s in samples.iter_mut() {
            *s *= self.gain;
            let level = s.abs();
            if level > self.target {
                self.gain *= 1.0 - self.attack * (level / self.target - 1.0).min(1.0);
            } else if level < self.target * 0.5 {
                self.gain *= 1.0 + self.release;
            }
            self.gain = self.gain.clamp(1.0, self.max_gain);
            *s = s.clamp(-1.0, 1.0);
        }
    }
}

/// Design a low-pass FIR filter using windowed sinc with Blackman window.
fn design_lowpass(num_taps: usize, cutoff: f32) -> Vec<f32> {
    let m = num_taps as f32 - 1.0;
    let half = m / 2.0;

    let mut coeffs: Vec<f32> = (0..num_taps)
        .map(|n| {
            let n = n as f32;
            let sinc = if (n - half).abs() < 1e-6 {
                2.0 * cutoff
            } else {
                let x = 2.0 * PI * cutoff * (n - half);
                x.sin() / (PI * (n - half))
            };
            let w = 0.42 - 0.5 * (2.0 * PI * n / m).cos() + 0.08 * (4.0 * PI * n / m).cos();
            sinc * w
        })
        .collect();

    let sum: f32 = coeffs.iter().sum();
    if sum.abs() > 1e-10 {
        for c in coeffs.iter_mut() {
            *c /= sum;
        }
    }

    coeffs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn am_demod_tone() {
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
        let n = 256;
        let freq = 0.01f32;
        let iq: Vec<[f32; 2]> = (0..n)
            .map(|i| {
                let phase = 2.0 * PI * freq * i as f32;
                [phase.cos(), phase.sin()]
            })
            .collect();

        let audio = demod_fm(&iq);
        let first = audio[1];
        for &v in &audio[2..] {
            assert!(
                (v - first).abs() < 0.01,
                "FM demod should be constant, got {v} vs {first}"
            );
        }
        assert!(first.abs() > 0.01, "FM demod output should be non-zero");
    }

    #[test]
    fn nco_shifts_tone_to_dc() {
        // A tone at +10kHz in IQ, shifted by -10kHz, should appear at DC.
        // After shift, the magnitude should be ~1.0 (energy preserved)
        // and the signal should be approximately constant (no rotation).
        let sample_rate = 192000;
        let tone_freq = 10000.0_f64;
        let n = 1024;

        let iq: Vec<[f32; 2]> = (0..n)
            .map(|i| {
                let phase = 2.0 * PI as f64 * tone_freq * i as f64 / sample_rate as f64;
                [phase.cos() as f32, phase.sin() as f32]
            })
            .collect();

        let mut nco = Nco::new(-tone_freq, sample_rate as u32);
        let mut shifted = Vec::new();
        nco.shift(&iq, &mut shifted);

        // Magnitude should be ~1.0 throughout
        for &[i, q] in &shifted[10..] {
            let mag = (i * i + q * q).sqrt();
            assert!(
                (mag - 1.0).abs() < 0.01,
                "magnitude should be ~1.0, got {mag}"
            );
        }

        // Phase should be approximately constant (DC = no rotation)
        let phases: Vec<f32> = shifted[10..].iter().map(|&[i, q]| q.atan2(i)).collect();
        let phase_ref = phases[0];
        for &p in &phases[1..] {
            let diff = (p - phase_ref + PI).rem_euclid(2.0 * PI) - PI;
            assert!(
                diff.abs() < 0.01,
                "phase should be constant at DC, diff={diff}"
            );
        }
    }
}
