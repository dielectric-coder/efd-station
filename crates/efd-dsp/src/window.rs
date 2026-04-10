use std::f64::consts::PI;

/// Generate Blackman-Harris window coefficients.
pub fn blackman_harris(size: usize) -> Vec<f64> {
    const A0: f64 = 0.35875;
    const A1: f64 = 0.48829;
    const A2: f64 = 0.14128;
    const A3: f64 = 0.01168;

    let n = (size - 1) as f64;
    (0..size)
        .map(|i| {
            let x = i as f64 / n;
            A0 - A1 * (2.0 * PI * x).cos()
                + A2 * (4.0 * PI * x).cos()
                - A3 * (6.0 * PI * x).cos()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blackman_harris_endpoints() {
        let w = blackman_harris(4096);
        assert_eq!(w.len(), 4096);
        // Endpoints should be small (near zero for BH window)
        assert!(w[0] < 0.01);
        assert!(w[4095] < 0.01);
        // Middle should be near 1.0
        assert!((w[2047] - 1.0).abs() < 0.01);
    }

    #[test]
    fn blackman_harris_symmetry() {
        let w = blackman_harris(1024);
        for i in 0..512 {
            assert!(
                (w[i] - w[1023 - i]).abs() < 1e-12,
                "asymmetric at i={i}"
            );
        }
    }
}
