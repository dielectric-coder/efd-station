use std::sync::{Arc, Mutex};

use gtk4::prelude::*;
use gtk4::DrawingArea;

const WF_HEIGHT: usize = 512;

/// Waterfall pixel buffer — each new FFT line is rendered into a raw pixel
/// array. No per-pixel Cairo calls. The buffer scrolls by shifting rows.
struct WfBuffer {
    /// RGBA pixel data, width × WF_HEIGHT × 4 bytes.
    pixels: Vec<u8>,
    width: usize,
}

impl WfBuffer {
    fn new(width: usize) -> Self {
        Self {
            pixels: vec![0u8; width * WF_HEIGHT * 4],
            width,
        }
    }

    /// Scroll all rows down by 1 and render new FFT bins into row 0.
    fn push_line(&mut self, bins: &[f32]) {
        if self.width == 0 {
            return;
        }

        // Shift all rows down by 1 (memmove)
        let row_bytes = self.width * 4;
        self.pixels.copy_within(0..(WF_HEIGHT - 1) * row_bytes, row_bytes);

        // Render new line into row 0
        let n = bins.len().max(1);
        for x in 0..self.width {
            let bin_idx = x * n / self.width;
            let db = bins.get(bin_idx).copied().unwrap_or(-120.0);
            let (r, g, b) = db_to_color_u8(db);
            let off = x * 4;
            // Cairo ImageSurface ARGB32 is stored as B, G, R, A (little-endian)
            self.pixels[off] = b;
            self.pixels[off + 1] = g;
            self.pixels[off + 2] = r;
            self.pixels[off + 3] = 255;
        }
    }

    fn resize(&mut self, new_width: usize) {
        if new_width != self.width {
            *self = WfBuffer::new(new_width);
        }
    }
}

#[derive(Clone)]
pub struct Waterfall {
    area: DrawingArea,
    #[allow(dead_code)]
    buffer: Arc<Mutex<WfBuffer>>,
    pending: Arc<Mutex<Vec<Vec<f32>>>>,
}

impl Waterfall {
    pub fn new() -> Self {
        let buffer = Arc::new(Mutex::new(WfBuffer::new(1024)));
        let pending = Arc::new(Mutex::new(Vec::new()));
        let area = DrawingArea::new();
        area.set_content_height(300);

        let buffer2 = buffer.clone();
        let pending2 = pending.clone();
        area.set_draw_func(move |_area, cr, width, height| {
            draw(cr, width, height, &buffer2, &pending2);
        });

        Self {
            area,
            buffer,
            pending,
        }
    }

    pub fn widget(&self) -> &DrawingArea {
        &self.area
    }

    pub fn push_line(&self, bins: &[f32]) {
        self.pending.lock().unwrap().push(bins.to_vec());
    }

    pub fn queue_draw(&self) {
        self.area.queue_draw();
    }
}

fn draw(
    cr: &gtk4::cairo::Context,
    width: i32,
    height: i32,
    buffer: &Arc<Mutex<WfBuffer>>,
    pending: &Arc<Mutex<Vec<Vec<f32>>>>,
) {
    let new_lines: Vec<Vec<f32>> = pending.lock().unwrap().drain(..).collect();
    let mut buf = buffer.lock().unwrap();

    buf.resize(width as usize);

    for bins in &new_lines {
        buf.push_line(bins);
    }

    // Create an ImageSurface from the pixel buffer
    let stride = gtk4::cairo::Format::ARgb32
        .stride_for_width(buf.width as u32)
        .unwrap();

    let surface = unsafe {
        gtk4::cairo::ImageSurface::create_for_data_unsafe(
            buf.pixels.as_mut_ptr(),
            gtk4::cairo::Format::ARgb32,
            buf.width as i32,
            WF_HEIGHT as i32,
            stride,
        )
        .unwrap()
    };

    // Scale and blit to screen in one operation
    cr.save().unwrap();
    cr.scale(1.0, height as f64 / WF_HEIGHT as f64);
    cr.set_source_surface(&surface, 0.0, 0.0).unwrap();
    cr.paint().unwrap();
    cr.restore().unwrap();
}

/// Map dB to RGB color (u8).
fn db_to_color_u8(db: f32) -> (u8, u8, u8) {
    let t = ((db + 120.0) / 100.0).clamp(0.0, 1.0);

    let (r, g, b) = if t < 0.2 {
        let s = t / 0.2;
        (0.0, 0.0, s)
    } else if t < 0.4 {
        let s = (t - 0.2) / 0.2;
        (0.0, s, 1.0)
    } else if t < 0.6 {
        let s = (t - 0.4) / 0.2;
        (0.0, 1.0, 1.0 - s)
    } else if t < 0.8 {
        let s = (t - 0.6) / 0.2;
        (s, 1.0, 0.0)
    } else {
        let s = (t - 0.8) / 0.2;
        (1.0, 1.0 - s * 0.5, s)
    };

    ((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
}
