//! Audio-domain DSP block that sits on every path reaching Audio Out
//! (see `docs/CM5-sdr-backend-pipeline.drawio`). Applies DNB / DNR /
//! DNF / APF stages to the audio stream on its way to the Opus encoder
//! and ALSA playback.
//!
//! Phase 3b: `DNB` is a real envelope-threshold impulse blanker on
//! the audio sample stream. `DNR` / `DNF` / `APF` remain pass-through
//! stubs — they each need meaningful parameters (noise-floor profile
//! / notch centre / passband centre + width) and a piece of UI we
//! haven't drawn yet, so they're their own follow-up commits.

/// EWMA smoothing factor for the audio envelope tracker.
///
/// At 48 kHz, 1/256 ≈ 188 Hz — fast enough to follow voice modulation
/// (which sits above ~300 Hz) while staying much slower than the
/// impulse spikes we want to catch.
const DNB_ENV_ALPHA: f32 = 1.0 / 256.0;

/// Magnitude multiplier above the running envelope at which an audio
/// sample is declared an impulse. Same 5× heuristic as the pre-IF NB.
const DNB_BLANK_THRESHOLD: f32 = 5.0;

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
/// Carries internal state (envelope tracker for DNB) across calls
/// so `process` doesn't spend the first few samples of every frame
/// re-converging.
#[derive(Debug, Clone)]
pub struct AudioDsp {
    flags: AudioDspFlags,
    /// EWMA envelope for DNB. Seeded to 0 and overwritten on the
    /// first real sample to avoid a false-positive blanking burst
    /// while the tracker converges.
    dnb_env: f32,
}

impl Default for AudioDsp {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioDsp {
    pub fn new() -> Self {
        Self {
            flags: AudioDspFlags::default(),
            dnb_env: 0.0,
        }
    }

    pub fn flags(&self) -> AudioDspFlags {
        self.flags
    }

    pub fn set_flags(&mut self, flags: AudioDspFlags) {
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
            // TODO (phase 3c-DNR): spectral-subtraction denoise.
        }
        if self.flags.dnf {
            // TODO (phase 3c-DNF): adaptive narrow-band notch.
        }
        if self.flags.apf {
            // TODO (phase 3c-APF): narrow passband peaking filter.
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
}
