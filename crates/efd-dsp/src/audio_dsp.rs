//! Audio-domain DSP block that sits on every path reaching Audio Out
//! (see `docs/CM5-sdr-backend-pipeline.drawio`). Applies DNB / DNR /
//! DNF / APF stages to the audio stream on its way to the Opus encoder
//! and ALSA playback.
//!
//! Phase 1: every stage is a pass-through stub. "All filters off" is
//! the default, and structurally the same as the block not being
//! there. Per-stage filter math lands in later phases; the client-side
//! enable flags likewise land later (no proto change yet).

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
#[derive(Debug, Clone, Default)]
pub struct AudioDsp {
    flags: AudioDspFlags,
}

impl AudioDsp {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn flags(&self) -> AudioDspFlags {
        self.flags
    }

    pub fn set_flags(&mut self, flags: AudioDspFlags) {
        self.flags = flags;
    }

    /// Apply all enabled stages in-place. Phase 1: every stage is a
    /// no-op pass-through regardless of its flag. The flag reads are
    /// kept so a future reader sees exactly where real implementations
    /// plug in.
    pub fn process(&self, _samples: &mut [f32]) {
        if self.flags.dnb { /* TODO: impulse-noise blanker */ }
        if self.flags.dnr { /* TODO: spectral denoise */ }
        if self.flags.dnf { /* TODO: narrow notch */ }
        if self.flags.apf { /* TODO: narrow passband */ }
    }
}
