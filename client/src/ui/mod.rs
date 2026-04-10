pub mod controls;
pub mod gl_util;
pub mod spectrum;
pub mod waterfall;

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

/// Available zoom levels.
pub const ZOOM_LEVELS: &[i32] = &[1, 2, 4, 6, 8, 10];

/// Shared display range for spectrum and waterfall.
#[derive(Clone)]
pub struct DisplayRange {
    ref_level: Arc<AtomicI32>, // top dBm
    range: Arc<AtomicI32>,     // dB span (positive)
    zoom: Arc<AtomicI32>,      // zoom factor (1, 2, 4, 6, 8, 10)
    pan: Arc<AtomicI32>,       // pan offset in milli-fractions (-500 to 500)
}

impl DisplayRange {
    pub fn new(ref_level: i32, range: i32) -> Self {
        Self {
            ref_level: Arc::new(AtomicI32::new(ref_level)),
            range: Arc::new(AtomicI32::new(range)),
            zoom: Arc::new(AtomicI32::new(1)),
            pan: Arc::new(AtomicI32::new(0)),
        }
    }

    pub fn ref_level(&self) -> f64 {
        self.ref_level.load(Ordering::Relaxed) as f64
    }

    pub fn range(&self) -> f64 {
        self.range.load(Ordering::Relaxed) as f64
    }

    pub fn db_top(&self) -> f64 {
        self.ref_level()
    }

    pub fn db_bottom(&self) -> f64 {
        self.ref_level() - self.range()
    }

    pub fn set_ref_level(&self, v: i32) {
        self.ref_level.store(v, Ordering::Relaxed);
    }

    pub fn set_range(&self, v: i32) {
        self.range.store(v, Ordering::Relaxed);
    }

    pub fn zoom(&self) -> i32 {
        self.zoom.load(Ordering::Relaxed)
    }

    pub fn set_zoom(&self, z: i32) {
        self.zoom.store(z, Ordering::Relaxed);
        self.clamp_pan();
    }

    /// Pan offset as a fraction of full span (-0.5 to 0.5).
    pub fn pan_frac(&self) -> f64 {
        self.pan.load(Ordering::Relaxed) as f64 / 1000.0
    }

    pub fn set_pan_frac(&self, p: f64) {
        self.pan.store((p * 1000.0) as i32, Ordering::Relaxed);
        self.clamp_pan();
    }

    /// Nudge pan by a fraction of the visible width.
    pub fn pan_by(&self, delta_frac: f64) {
        let cur = self.pan_frac();
        self.set_pan_frac(cur + delta_frac);
    }

    /// Visible x-range in [0,1] normalized coordinates.
    /// Returns (start, end) where the full span is [0, 1].
    pub fn visible_range(&self) -> (f64, f64) {
        let z = self.zoom() as f64;
        let half = 0.5 / z;
        let center = 0.5 + self.pan_frac();
        let lo = (center - half).clamp(0.0, 1.0);
        let hi = (center + half).clamp(0.0, 1.0);
        (lo, hi)
    }

    fn clamp_pan(&self) {
        let z = self.zoom() as f64;
        let half = 0.5 / z;
        let max_pan = 0.5 - half;
        let cur = self.pan_frac();
        let clamped = cur.clamp(-max_pan, max_pan);
        if (clamped - cur).abs() > 0.0001 {
            self.pan.store((clamped * 1000.0) as i32, Ordering::Relaxed);
        }
    }

    /// Cycle to next zoom level. Returns the new zoom.
    pub fn zoom_in(&self) -> i32 {
        let cur = self.zoom();
        let next = ZOOM_LEVELS
            .iter()
            .find(|&&z| z > cur)
            .copied()
            .unwrap_or(cur);
        self.set_zoom(next);
        next
    }

    /// Cycle to previous zoom level. Returns the new zoom.
    pub fn zoom_out(&self) -> i32 {
        let cur = self.zoom();
        let prev = ZOOM_LEVELS
            .iter()
            .rev()
            .find(|&&z| z < cur)
            .copied()
            .unwrap_or(cur);
        self.set_zoom(prev);
        prev
    }
}

/// Parse FDM-DUO filter bandwidth string to Hz.
///
/// Handles formats: "2.4k", "500", "100&4", "D300", "D1k",
/// "Narrow" (≈6k), "Wide" (≈15k), "Data" (≈9k).
pub fn parse_filter_bw_hz(bw: &str) -> Option<f64> {
    let s = bw.trim();
    match s {
        "Narrow" => return Some(6_000.0),
        "Wide" => return Some(15_000.0),
        "Data" => return Some(9_000.0),
        _ => {}
    }
    // Strip leading "D" (digital mode filters)
    let s = s.strip_prefix('D').unwrap_or(s);
    // Strip "&N" suffix (CW narrow variants like "100&4")
    let s = s.split('&').next().unwrap_or(s);
    // Parse "2.4k" or "500"
    if let Some(num) = s.strip_suffix('k') {
        num.parse::<f64>().ok().map(|v| v * 1000.0)
    } else {
        s.parse::<f64>().ok()
    }
}
