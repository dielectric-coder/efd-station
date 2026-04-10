use std::sync::{Arc, Mutex};

use efd_proto::FftBins;
use gtk4::prelude::*;
use gtk4::DrawingArea;

#[derive(Clone)]
pub struct Spectrum {
    area: DrawingArea,
}

impl Spectrum {
    pub fn new(fft_data: Arc<Mutex<Option<FftBins>>>) -> Self {
        let area = DrawingArea::new();
        area.set_content_height(200);

        let data = fft_data.clone();
        area.set_draw_func(move |_area, cr, width, height| {
            draw_spectrum(cr, width, height, &data);
        });

        Self { area }
    }

    pub fn widget(&self) -> &DrawingArea {
        &self.area
    }

    pub fn queue_draw(&self) {
        self.area.queue_draw();
    }
}

fn draw_spectrum(
    cr: &gtk4::cairo::Context,
    width: i32,
    height: i32,
    fft_data: &Arc<Mutex<Option<FftBins>>>,
) {
    let w = width as f64;
    let h = height as f64;

    // Background
    cr.set_source_rgb(0.0, 0.0, 0.05);
    cr.rectangle(0.0, 0.0, w, h);
    let _ = cr.fill();

    // Grid lines
    cr.set_source_rgba(0.2, 0.2, 0.3, 0.5);
    cr.set_line_width(0.5);
    for i in 1..10 {
        let y = h * i as f64 / 10.0;
        cr.move_to(0.0, y);
        cr.line_to(w, y);
        let _ = cr.stroke();
    }
    for i in 1..10 {
        let x = w * i as f64 / 10.0;
        cr.move_to(x, 0.0);
        cr.line_to(x, h);
        let _ = cr.stroke();
    }

    let bins = fft_data.lock().unwrap();
    let Some(ref fft) = *bins else { return };

    if fft.bins.is_empty() {
        return;
    }

    // dB range for display
    let db_top: f64 = -20.0;
    let db_bottom: f64 = -120.0;
    let db_range = db_top - db_bottom;

    // Draw spectrum line
    cr.set_source_rgb(0.2, 1.0, 0.3);
    cr.set_line_width(1.0);

    let n = fft.bins.len();
    for (i, &db) in fft.bins.iter().enumerate() {
        let x = (i as f64 / n as f64) * w;
        let normalized = ((db as f64 - db_bottom) / db_range).clamp(0.0, 1.0);
        let y = h * (1.0 - normalized);

        if i == 0 {
            cr.move_to(x, y);
        } else {
            cr.line_to(x, y);
        }
    }
    let _ = cr.stroke();

    // dB scale labels
    cr.set_source_rgb(0.6, 0.6, 0.6);
    cr.set_font_size(10.0);
    for db in (-120..=-20).step_by(10) {
        let normalized = ((db as f64 - db_bottom) / db_range).clamp(0.0, 1.0);
        let y = h * (1.0 - normalized);
        cr.move_to(2.0, y - 2.0);
        let _ = cr.show_text(&format!("{db}"));
    }
}
