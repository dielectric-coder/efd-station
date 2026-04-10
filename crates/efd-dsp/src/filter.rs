use std::f32::consts::PI;

/// FIR decimation filter: low-pass + downsample in one step.
///
/// Uses a windowed-sinc FIR filter with Blackman window.
/// Only computes output samples at decimated positions (polyphase optimization).
pub struct FirDecimator {
    coeffs: Vec<f32>,
    history: Vec<f32>,
    taps: usize,
    factor: usize,
    pos: usize, // position within current decimation period
}

impl FirDecimator {
    /// Create a new FIR decimation filter.
    ///
    /// - `factor`: decimation ratio (e.g., 4 for 192kHz → 48kHz)
    /// - `num_taps`: FIR filter length (odd, typically 4*factor+1 to 8*factor+1)
    ///
    /// The cutoff frequency is set to `0.5 / factor` (Nyquist of the output rate)
    /// with a slight rolloff margin (0.45/factor) to ensure stopband rejection.
    pub fn new(factor: usize, num_taps: usize) -> Self {
        assert!(factor >= 1);
        let taps = num_taps | 1; // ensure odd
        let coeffs = design_lowpass(taps, 0.45 / factor as f32);

        Self {
            coeffs,
            history: vec![0.0; taps],
            taps,
            factor,
            pos: 0,
        }
    }

    /// Process a block of input samples, returning decimated output.
    ///
    /// Maintains filter state across calls for seamless block boundaries.
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if self.factor <= 1 {
            return input.to_vec();
        }

        let mut output = Vec::with_capacity(input.len() / self.factor + 1);

        for &sample in input {
            // Shift history and insert new sample
            // Using a circular buffer approach for efficiency
            self.history.copy_within(1.., 0);
            self.history[self.taps - 1] = sample;
            self.pos += 1;

            if self.pos >= self.factor {
                self.pos = 0;
                // Compute FIR output at this decimated position
                let mut sum = 0.0f32;
                for (h, c) in self.history.iter().zip(self.coeffs.iter()) {
                    sum += h * c;
                }
                output.push(sum);
            }
        }

        output
    }

    /// Reset filter state (e.g., after a gap in the input stream).
    pub fn reset(&mut self) {
        self.history.fill(0.0);
        self.pos = 0;
    }
}

/// Design a low-pass FIR filter using windowed sinc method with Blackman window.
///
/// - `num_taps`: filter length (should be odd)
/// - `cutoff`: normalized cutoff frequency (0.0 to 0.5, where 0.5 = Nyquist)
///
/// Returns filter coefficients normalized so they sum to 1.0.
fn design_lowpass(num_taps: usize, cutoff: f32) -> Vec<f32> {
    let m = num_taps as f32 - 1.0;
    let half = m / 2.0;

    let mut coeffs: Vec<f32> = (0..num_taps)
        .map(|n| {
            let n = n as f32;
            // Sinc function
            let sinc = if (n - half).abs() < 1e-6 {
                2.0 * cutoff
            } else {
                let x = 2.0 * PI * cutoff * (n - half);
                x.sin() / (PI * (n - half))
            };
            // Blackman window
            let w = 0.42 - 0.5 * (2.0 * PI * n / m).cos() + 0.08 * (4.0 * PI * n / m).cos();
            sinc * w
        })
        .collect();

    // Normalize to unity gain at DC
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
    fn decimator_factor_4_length() {
        let mut dec = FirDecimator::new(4, 33);
        // 1000 input samples at factor 4 → 250 output samples
        let input: Vec<f32> = vec![1.0; 1000];
        let output = dec.process(&input);
        assert_eq!(output.len(), 250);
    }

    #[test]
    fn decimator_dc_passthrough() {
        // DC signal (all 1.0) should pass through with unity gain
        let mut dec = FirDecimator::new(4, 33);
        let input: Vec<f32> = vec![1.0; 1000];
        let output = dec.process(&input);
        // After filter settles (skip first taps/factor samples), output should be ~1.0
        let settled = &output[10..];
        for &v in settled {
            assert!(
                (v - 1.0).abs() < 0.01,
                "DC should pass through, got {v}"
            );
        }
    }

    #[test]
    fn decimator_rejects_nyquist() {
        // Signal at Nyquist of output rate should be attenuated
        // At factor=4, input Nyquist = 0.5, output Nyquist = 0.125
        // A tone at 0.2 (above output Nyquist) should be rejected
        let mut dec = FirDecimator::new(4, 33);
        let freq = 0.2_f32; // normalized frequency, above output Nyquist
        let input: Vec<f32> = (0..1000)
            .map(|i| (2.0 * PI * freq * i as f32).sin())
            .collect();
        let output = dec.process(&input);
        let settled = &output[10..];
        let peak = settled.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        assert!(
            peak < 0.1,
            "signal above output Nyquist should be rejected, peak={peak}"
        );
    }

    #[test]
    fn decimator_passes_low_freq() {
        // A tone well below the cutoff should pass through
        let mut dec = FirDecimator::new(4, 33);
        let freq = 0.02_f32; // well below cutoff of 0.45/4 ≈ 0.1125
        let input: Vec<f32> = (0..2000)
            .map(|i| (2.0 * PI * freq * i as f32).sin())
            .collect();
        let output = dec.process(&input);
        let settled = &output[20..];
        let peak = settled.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        assert!(
            peak > 0.8,
            "signal below cutoff should pass, peak={peak}"
        );
    }

    #[test]
    fn decimator_factor_1_passthrough() {
        let mut dec = FirDecimator::new(1, 1);
        let input = vec![0.5, -0.3, 0.7];
        let output = dec.process(&input);
        assert_eq!(output, input);
    }

    #[test]
    fn decimator_cross_block_continuity() {
        // Processing in two blocks should give the same result as one block
        let mut dec1 = FirDecimator::new(4, 33);
        let mut dec2 = FirDecimator::new(4, 33);

        let input: Vec<f32> = (0..800)
            .map(|i| (2.0 * PI * 0.05 * i as f32).sin())
            .collect();

        let out_one = dec1.process(&input);

        let out_a = dec2.process(&input[..400]);
        let out_b = dec2.process(&input[400..]);
        let out_two: Vec<f32> = out_a.into_iter().chain(out_b).collect();

        assert_eq!(out_one.len(), out_two.len());
        for (a, b) in out_one.iter().zip(out_two.iter()) {
            assert!(
                (a - b).abs() < 1e-6,
                "cross-block mismatch: {a} vs {b}"
            );
        }
    }
}
