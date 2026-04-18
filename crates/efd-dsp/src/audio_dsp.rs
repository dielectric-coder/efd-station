//! Audio-domain DSP block that sits on every path reaching Audio Out
//! (see `docs/CM5-sdr-backend-pipeline.drawio`). Applies DNB / DNR /
//! DNF / APF stages to the audio stream on its way to the Opus encoder
//! and ALSA playback.
//!
//! Phase 3c makes DNR / DNF / APF audibly effective with three RBJ
//! biquads rather than true spectral-domain processing. That's a
//! deliberate trade-off: cheap (~10 ns/sample per biquad at 48 kHz),
//! zero-artifact (no FFT smearing, no noise-profile learning glitch),
//! and behaves the way most ham-radio operators actually expect:
//!
//! - **DNR** — 2-pole lowpass at 2.5 kHz. Voice sits at 300–2800 Hz;
//!   high-frequency hiss lives above. Clip the hiss, keep the voice.
//!   Not Wiener / spectral-subtraction "noise reduction" in the
//!   literature sense, but the effect HF ops ask for under the DNR
//!   label.
//! - **DNF** — narrow biquad notch at 1 kHz, Q=15. Targets the classic
//!   single-tone heterodyne whistle. A future phase wires this to a
//!   user-settable centre (most SDR apps expose a draggable notch);
//!   the fixed 1 kHz is a sensible default for casual HF listening.
//! - **APF** — peaking EQ at 700 Hz, Q=3, +6 dB. Lifts a narrow band
//!   where CW sidetones and voice vowels live so weak signals pop.
//!
//! All three share the `Biquad` struct, copied from
//! [`crate::audio_if`] so this file doesn't create a cross-module
//! dep on a pre-phase-3 private type. If a later phase wants to share
//! one implementation, promote `audio_if::Biquad` to `crate::biquad`
//! and import from both.

use std::f32::consts::PI;

/// EWMA smoothing factor for the audio envelope tracker.
///
/// At 48 kHz, 1/256 ≈ 188 Hz — fast enough to follow voice modulation
/// (which sits above ~300 Hz) while staying much slower than the
/// impulse spikes we want to catch.
const DNB_ENV_ALPHA: f32 = 1.0 / 256.0;

/// Magnitude multiplier above the running envelope at which an audio
/// sample is declared an impulse. Same 5× heuristic as the pre-IF NB.
const DNB_BLANK_THRESHOLD: f32 = 5.0;

/// DNR: 2-pole lowpass cutoff.
const DNR_CUTOFF_HZ: f32 = 2500.0;
/// DNR: quality factor. 0.707 = Butterworth response (maximally flat).
const DNR_Q: f32 = 0.707;

/// DNF: notch centre frequency. Classic heterodyne beat note.
const DNF_CENTRE_HZ: f32 = 1000.0;
/// DNF: notch quality factor — higher Q = narrower notch.
const DNF_Q: f32 = 15.0;

/// APF: peaking-EQ centre frequency. Bang in the middle of a voice /
/// CW sidetone band.
const APF_CENTRE_HZ: f32 = 700.0;
/// APF: quality factor.
const APF_Q: f32 = 3.0;
/// APF: peak gain in dB.
const APF_GAIN_DB: f32 = 6.0;

/// Flags selecting which audio-domain DSP stages are active. Default
/// is all-off, which is indistinguishable from the block being absent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioDspFlags {
    /// Digital Noise Blanker — short-impulse-noise removal.
    pub dnb: bool,
    /// Digital Noise Reduction — spectral denoise.
    pub dnr: bool,
    /// Digital Notch Filter — tunable narrow-band notch.
    pub dnf: bool,
    /// Audio Peak Filter — tunable narrow-band passband.
    pub apf: bool,
}

/// Audio-domain DSP post-processor.
///
/// Placed between the per-source audio mux and the Opus encoder +
/// ALSA out so every listenable path goes through it (IQ→IF demod,
/// USB audio, DRM, file — all converge here per the drawio).
///
/// Carries internal state (envelope tracker for DNB + biquad state
/// for DNR/DNF/APF) across calls so `process` doesn't spend the
/// first few samples of every frame re-converging.
#[derive(Debug, Clone)]
pub struct AudioDsp {
    flags: AudioDspFlags,
    /// EWMA envelope for DNB.
    dnb_env: f32,
    /// Lowpass for DNR.
    dnr_filter: Biquad,
    /// Notch for DNF.
    dnf_filter: Biquad,
    /// Peaking EQ for APF.
    apf_filter: Biquad,
}

impl Default for AudioDsp {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioDsp {
    pub fn new() -> Self {
        Self::with_sample_rate(48_000.0)
    }

    pub fn with_sample_rate(sample_rate: f32) -> Self {
        Self {
            flags: AudioDspFlags::default(),
            dnb_env: 0.0,
            dnr_filter: Biquad::lowpass(sample_rate, DNR_CUTOFF_HZ, DNR_Q),
            dnf_filter: Biquad::notch(sample_rate, DNF_CENTRE_HZ, DNF_Q),
            apf_filter: Biquad::peaking(sample_rate, APF_CENTRE_HZ, APF_Q, APF_GAIN_DB),
        }
    }

    pub fn flags(&self) -> AudioDspFlags {
        self.flags
    }

    pub fn set_flags(&mut self, flags: AudioDspFlags) {
        // If a filter just turned on, reset its internal state so
        // transient samples don't ring out of stale history.
        if flags.dnr && !self.flags.dnr {
            self.dnr_filter.reset();
        }
        if flags.dnf && !self.flags.dnf {
            self.dnf_filter.reset();
        }
        if flags.apf && !self.flags.apf {
            self.apf_filter.reset();
        }
        self.flags = flags;
    }

    /// Apply all enabled stages in-place in the order
    /// DNB → DNR → DNF → APF, matching the block chain in the
    /// drawio.
    pub fn process(&mut self, samples: &mut [f32]) {
        if self.flags.dnb {
            dnb(samples, &mut self.dnb_env);
        }
        if self.flags.dnr {
            self.dnr_filter.process(samples);
        }
        if self.flags.dnf {
            self.dnf_filter.process(samples);
        }
        if self.flags.apf {
            self.apf_filter.process(samples);
        }
    }
}

/// Audio-domain envelope-threshold impulse blanker.
///
/// Runs `|x|` through a slow EWMA and zeroes any sample exceeding
/// `threshold × env`. Check-before-update so an impulse doesn't
/// poison the envelope for the next one (matches the pre-IF NB
/// behaviour).
fn dnb(samples: &mut [f32], env: &mut f32) {
    for s in samples.iter_mut() {
        let mag = s.abs();
        if *env == 0.0 {
            *env = mag;
        }
        if mag > DNB_BLANK_THRESHOLD * *env {
            *s = 0.0;
        } else {
            *env = DNB_ENV_ALPHA * mag + (1.0 - DNB_ENV_ALPHA) * *env;
        }
    }
}

/// Direct-form-I biquad with RBJ cookbook coefficient families.
/// Copied from [`crate::audio_if::Biquad`] rather than shared
/// because `audio_if` keeps its struct private; when a second
/// shared-biquad consumer appears, lift this into a dedicated
/// module. Lowpass / notch / peaking EQ formulations cover the
/// three filter flavours this block needs.
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
    fn lowpass(sample_rate: f32, cutoff_hz: f32, q: f32) -> Self {
        let omega = 2.0 * PI * cutoff_hz / sample_rate;
        let alpha = omega.sin() / (2.0 * q);
        let cos_omega = omega.cos();
        let a0 = 1.0 + alpha;
        let b0 = (1.0 - cos_omega) / 2.0;
        let b1 = 1.0 - cos_omega;
        let b2 = (1.0 - cos_omega) / 2.0;
        let a1 = -2.0 * cos_omega;
        let a2 = 1.0 - alpha;
        Self::from_norm(b0 / a0, b1 / a0, b2 / a0, a1 / a0, a2 / a0)
    }

    fn notch(sample_rate: f32, centre_hz: f32, q: f32) -> Self {
        let omega = 2.0 * PI * centre_hz / sample_rate;
        let alpha = omega.sin() / (2.0 * q);
        let cos_omega = omega.cos();
        let a0 = 1.0 + alpha;
        let b0 = 1.0;
        let b1 = -2.0 * cos_omega;
        let b2 = 1.0;
        let a1 = -2.0 * cos_omega;
        let a2 = 1.0 - alpha;
        Self::from_norm(b0 / a0, b1 / a0, b2 / a0, a1 / a0, a2 / a0)
    }

    fn peaking(sample_rate: f32, centre_hz: f32, q: f32, gain_db: f32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let omega = 2.0 * PI * centre_hz / sample_rate;
        let alpha = omega.sin() / (2.0 * q);
        let cos_omega = omega.cos();
        let a0 = 1.0 + alpha / a;
        let b0 = 1.0 + alpha * a;
        let b1 = -2.0 * cos_omega;
        let b2 = 1.0 - alpha * a;
        let a1 = -2.0 * cos_omega;
        let a2 = 1.0 - alpha / a;
        Self::from_norm(b0 / a0, b1 / a0, b2 / a0, a1 / a0, a2 / a0)
    }

    fn from_norm(b0: f32, b1: f32, b2: f32, a1: f32, a2: f32) -> Self {
        Self {
            b0,
            b1,
            b2,
            a1,
            a2,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn reset(&mut self) {
        self.x1 = 0.0;
        self.x2 = 0.0;
        self.y1 = 0.0;
        self.y2 = 0.0;
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
    fn dnb_off_is_pass_through() {
        let mut dsp = AudioDsp::new();
        let mut samples = vec![0.1f32; 256];
        samples[100] = 5.0; // big spike, but DNB disabled
        let before = samples.clone();
        dsp.process(&mut samples);
        assert_eq!(samples, before);
    }

    #[test]
    fn dnb_blanks_lone_impulse() {
        let mut dsp = AudioDsp::new();
        dsp.set_flags(AudioDspFlags { dnb: true, ..Default::default() });
        let mut samples = vec![0.1f32; 2048];
        samples[1024] = 1.0; // 10× envelope
        dsp.process(&mut samples);
        assert!(samples[1024].abs() < 1e-6, "impulse zeroed");
        assert!((samples[1023] - 0.1).abs() < 1e-6, "neighbour preserved");
        assert!((samples[1025] - 0.1).abs() < 1e-6, "neighbour preserved");
    }

    #[test]
    fn dnb_below_threshold_survives() {
        let mut dsp = AudioDsp::new();
        dsp.set_flags(AudioDspFlags { dnb: true, ..Default::default() });
        let mut samples = vec![0.1f32; 2048];
        samples[1024] = 0.3; // 3× env, under threshold
        dsp.process(&mut samples);
        assert!((samples[1024] - 0.3).abs() < 1e-6);
    }

    #[test]
    fn dnb_envelope_not_biased_by_impulse() {
        let mut dsp = AudioDsp::new();
        dsp.set_flags(AudioDspFlags { dnb: true, ..Default::default() });
        let mut samples = vec![0.1f32; 4096];
        samples[50] = 5.0;
        dsp.process(&mut samples);
        assert!((dsp.dnb_env - 0.1).abs() < 0.02, "env stayed near 0.1, got {}", dsp.dnb_env);
    }

    #[test]
    fn flags_are_mutable_at_runtime() {
        let mut dsp = AudioDsp::new();
        assert_eq!(dsp.flags(), AudioDspFlags::default());
        dsp.set_flags(AudioDspFlags { dnr: true, ..Default::default() });
        assert!(dsp.flags().dnr);
    }

    /// Drive a sinusoid at `freq_hz` through the filter and measure
    /// the RMS ratio in / out after a settling prefix. Coarse but
    /// enough to assert "this filter passes / rejects this band."
    fn filter_gain_db(filter: &mut Biquad, freq_hz: f32, sample_rate: f32) -> f32 {
        let n = 4096;
        let warmup = 1024;
        let mut buf: Vec<f32> = (0..n)
            .map(|i| (2.0 * PI * freq_hz * i as f32 / sample_rate).sin())
            .collect();
        let in_rms: f32 =
            (buf[warmup..].iter().map(|x| x * x).sum::<f32>() / (n - warmup) as f32).sqrt();
        filter.process(&mut buf);
        let out_rms: f32 =
            (buf[warmup..].iter().map(|x| x * x).sum::<f32>() / (n - warmup) as f32).sqrt();
        20.0 * (out_rms / in_rms).log10()
    }

    #[test]
    fn dnr_lowpass_passes_voice_band() {
        let mut f = Biquad::lowpass(48_000.0, DNR_CUTOFF_HZ, DNR_Q);
        // 1 kHz is well inside the voice band — should pass within
        // a dB or so.
        let g = filter_gain_db(&mut f, 1_000.0, 48_000.0);
        assert!(g > -3.0, "1 kHz gain was {g} dB, expected near 0");
    }

    #[test]
    fn dnr_lowpass_rejects_hf_hiss() {
        let mut f = Biquad::lowpass(48_000.0, DNR_CUTOFF_HZ, DNR_Q);
        // 6 kHz is well above cutoff — should be attenuated noticeably.
        let g = filter_gain_db(&mut f, 6_000.0, 48_000.0);
        assert!(g < -10.0, "6 kHz gain was {g} dB, expected well below -10");
    }

    #[test]
    fn dnf_notch_kills_on_centre() {
        let mut f = Biquad::notch(48_000.0, DNF_CENTRE_HZ, DNF_Q);
        let g = filter_gain_db(&mut f, DNF_CENTRE_HZ, 48_000.0);
        assert!(g < -20.0, "notch depth was {g} dB, expected < -20");
    }

    #[test]
    fn dnf_notch_passes_voice_off_centre() {
        let mut f = Biquad::notch(48_000.0, DNF_CENTRE_HZ, DNF_Q);
        // 300 Hz is far outside the notch — should pass essentially unity.
        let g = filter_gain_db(&mut f, 300.0, 48_000.0);
        assert!(g > -1.0, "off-centre gain was {g} dB");
    }

    #[test]
    fn apf_peaking_boosts_centre() {
        let mut f = Biquad::peaking(48_000.0, APF_CENTRE_HZ, APF_Q, APF_GAIN_DB);
        let g = filter_gain_db(&mut f, APF_CENTRE_HZ, 48_000.0);
        // Expect roughly +APF_GAIN_DB ± settling tolerance.
        assert!(
            (g - APF_GAIN_DB).abs() < 1.5,
            "peak gain was {g} dB, expected near {APF_GAIN_DB}"
        );
    }

    #[test]
    fn apf_peaking_flat_off_centre() {
        let mut f = Biquad::peaking(48_000.0, APF_CENTRE_HZ, APF_Q, APF_GAIN_DB);
        // Well away from centre — gain should be near 0 dB.
        let g = filter_gain_db(&mut f, 3_000.0, 48_000.0);
        assert!(g.abs() < 1.5, "off-centre gain was {g} dB");
    }
}
