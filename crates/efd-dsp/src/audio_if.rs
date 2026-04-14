//! Audio-rate IF demod filter.
//!
//! In the unified pipeline (see `docs/CM5-sdr-backend-pipeline.drawio`)
//! the "IF demod" block sits between the audio source and both the
//! digital decoders and the DSP→Audio Out path. When the source is
//! already at audio rate (USB audio, file, portable-radio line-in),
//! "IF demod" reduces to a bandpass filter that carves out the
//! narrow slice of the incoming audio where the decoder or listener
//! cares about content.
//!
//! Phase 3 scope: RBJ biquad bandpass with mode-driven center/BW
//! defaults, in-place process on f32 audio. Pass-through for wideband
//! modes (AM/FM) and when mode is unset. Real mode-specific envelope
//! detection, and client-configurable BW, come later.
//!
//! Intentionally independent of the `demod` module — that one operates
//! on IQ and does all of frequency-shift / decimate / filter / detect;
//! this one is a pure audio-domain bandpass.

use std::f32::consts::PI;

use efd_proto::Mode;

/// Audio-rate IF bandpass filter. A single biquad.
///
/// `set_mode` picks reasonable defaults for each mode; `process`
/// applies the filter in place. When the current mode is `None` or a
/// wideband mode (AM/FM/DRM), `process` is a no-op.
#[derive(Debug, Clone)]
pub struct AudioIfFilter {
    sample_rate: f32,
    coeffs: Option<Biquad>,
}

impl AudioIfFilter {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            sample_rate,
            coeffs: None,
        }
    }

    /// Configure the filter for a given mode. A default BW is chosen
    /// per mode; client-configurable BW is a later phase. Modes that
    /// don't benefit from a narrow audio bandpass (AM/FM/DRM/None)
    /// put the filter in pass-through.
    pub fn set_mode(&mut self, mode: Option<Mode>) {
        self.coeffs = mode
            .and_then(mode_to_bandpass)
            .map(|(center, bw)| Biquad::bandpass(self.sample_rate, center, bw));
    }

    /// Apply the filter in place.
    pub fn process(&mut self, samples: &mut [f32]) {
        if let Some(bq) = self.coeffs.as_mut() {
            bq.process(samples);
        }
    }
}

/// Mode → (center_hz, bandwidth_hz) defaults for the audio-rate IF
/// bandpass. Returns `None` for modes that should not be filtered
/// here — either too wide to meaningfully bandpass at audio rate
/// (AM, FM) or handled by a dedicated decoder path (DRM).
fn mode_to_bandpass(mode: Mode) -> Option<(f32, f32)> {
    match mode {
        // Narrow CW tone, ~500 Hz BW centered at the standard 800 Hz sidetone.
        Mode::CW | Mode::CWR => Some((800.0, 500.0)),
        // SSB voice: ~2.4 kHz BW centered at mid-band.
        Mode::USB | Mode::LSB => Some((1500.0, 2400.0)),
        // Pass-through.
        Mode::AM | Mode::FM | Mode::DRM | Mode::Unknown => None,
    }
}

/// RBJ cookbook biquad bandpass filter (constant-0-dB-peak form).
#[derive(Debug, Clone)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    fn bandpass(sample_rate: f32, center_hz: f32, bandwidth_hz: f32) -> Self {
        let omega = 2.0 * PI * center_hz / sample_rate;
        let q = center_hz / bandwidth_hz;
        let alpha = omega.sin() / (2.0 * q);
        let cos_omega = omega.cos();
        let a0 = 1.0 + alpha;
        Self {
            b0: alpha / a0,
            b1: 0.0,
            b2: -alpha / a0,
            a1: -2.0 * cos_omega / a0,
            a2: (1.0 - alpha) / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn process(&mut self, samples: &mut [f32]) {
        for s in samples.iter_mut() {
            let x = *s;
            let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
                - self.a1 * self.y1
                - self.a2 * self.y2;
            self.x2 = self.x1;
            self.x1 = x;
            self.y2 = self.y1;
            self.y1 = y;
            *s = y;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_when_mode_unset() {
        let mut f = AudioIfFilter::new(48_000.0);
        let mut samples = vec![0.5_f32; 10];
        let expected = samples.clone();
        f.process(&mut samples);
        assert_eq!(samples, expected);
    }

    #[test]
    fn passthrough_for_wideband_modes() {
        let mut f = AudioIfFilter::new(48_000.0);
        f.set_mode(Some(Mode::AM));
        let mut samples = vec![0.5_f32; 10];
        let expected = samples.clone();
        f.process(&mut samples);
        assert_eq!(samples, expected);
    }

    #[test]
    fn cw_mode_attenuates_dc() {
        let mut f = AudioIfFilter::new(48_000.0);
        f.set_mode(Some(Mode::CW));
        // Feed DC, should be attenuated (bandpass at 800 Hz rejects DC).
        let mut samples = vec![1.0_f32; 4096];
        f.process(&mut samples);
        // After settling, samples should be near zero.
        let settled = &samples[2048..];
        let max = settled.iter().map(|s| s.abs()).fold(0.0_f32, f32::max);
        assert!(max < 0.01, "DC should be attenuated; got max {max}");
    }
}
