use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use gtk4::prelude::*;
use gtk4::DrawingArea;

const MAX_LINES: usize = 256;

#[derive(Clone)]
pub struct Waterfall {
    area: DrawingArea,
    lines: Arc<Mutex<VecDeque<Vec<f32>>>>,
}

impl Waterfall {
    pub fn new() -> Self {
        let lines: Arc<Mutex<VecDeque<Vec<f32>>>> =
            Arc::new(Mutex::new(VecDeque::with_capacity(MAX_LINES)));
        let area = DrawingArea::new();
        area.set_content_height(256);

        let lines2 = lines.clone();
        area.set_draw_func(move |_area, cr, width, height| {
            draw_waterfall(cr, width, height, &lines2);
        });

        Self { area, lines }
    }

    pub fn widget(&self) -> &DrawingArea {
        &self.area
    }

    pub fn push_line(&self, bins: &[f32]) {
        let mut lines = self.lines.lock().unwrap();
        if lines.len() >= MAX_LINES {
            lines.pop_back();
        }
        lines.push_front(bins.to_vec());
    }

    pub fn queue_draw(&self) {
        self.area.queue_draw();
    }
}

fn draw_waterfall(
    cr: &gtk4::cairo::Context,
    width: i32,
    height: i32,
    lines: &Arc<Mutex<VecDeque<Vec<f32>>>>,
) {
    let w = width as f64;
    let h = height as f64;

    // Background
    cr.set_source_rgb(0.0, 0.0, 0.0);
    cr.rectangle(0.0, 0.0, w, h);
    let _ = cr.fill();

    let lines = lines.lock().unwrap();
    if lines.is_empty() {
        return;
    }

    let _num_lines = lines.len();
    let line_height = (h / MAX_LINES as f64).max(1.0);

    for (row, bins) in lines.iter().enumerate() {
        if bins.is_empty() {
            continue;
        }
        let y = row as f64 * line_height;
        let n = bins.len();

        // Draw each bin as a colored rectangle
        let bin_width = w / n as f64;
        for (col, &db) in bins.iter().enumerate() {
            let x = col as f64 * bin_width;
            let (r, g, b) = db_to_color(db);
            cr.set_source_rgb(r, g, b);
            cr.rectangle(x, y, bin_width.ceil(), line_height.ceil());
            let _ = cr.fill();
        }
    }
}

/// Map dB value to a color (blue→cyan→green→yellow→red→white).
fn db_to_color(db: f32) -> (f64, f64, f64) {
    // Normalize: -120 dB = 0.0, -20 dB = 1.0
    let t = ((db + 120.0) / 100.0).clamp(0.0, 1.0) as f64;

    if t < 0.2 {
        // Black → blue
        let s = t / 0.2;
        (0.0, 0.0, s)
    } else if t < 0.4 {
        // Blue → cyan
        let s = (t - 0.2) / 0.2;
        (0.0, s, 1.0)
    } else if t < 0.6 {
        // Cyan → green
        let s = (t - 0.4) / 0.2;
        (0.0, 1.0, 1.0 - s)
    } else if t < 0.8 {
        // Green → yellow
        let s = (t - 0.6) / 0.2;
        (s, 1.0, 0.0)
    } else {
        // Yellow → red → white
        let s = (t - 0.8) / 0.2;
        (1.0, 1.0 - s * 0.5, s)
    }
}
