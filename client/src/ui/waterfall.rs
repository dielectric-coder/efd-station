use std::sync::{Arc, Mutex};

use gtk4::prelude::*;
use gtk4::DrawingArea;

const WF_HEIGHT: usize = 512;
const MAX_PENDING: usize = 64;

struct WfBuffer {
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

    fn push_line(&mut self, bins: &[f32]) {
        if self.width == 0 {
            return;
        }

        let row_bytes = self.width * 4;
        self.pixels.copy_within(0..(WF_HEIGHT - 1) * row_bytes, row_bytes);

        let n = bins.len().max(1);
        for x in 0..self.width {
            let bin_idx = x * n / self.width;
            let db = bins.get(bin_idx).copied().unwrap_or(-120.0);
            let (r, g, b) = db_to_color_u8(db);
            let off = x * 4;
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
        let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let len = p.len();
        if len >= MAX_PENDING {
            p.drain(0..len / 2); // drop oldest half
        }
        p.push(bins.to_vec());
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
    // Drain pending first, release lock immediately
    let new_lines: Vec<Vec<f32>> = {
        pending.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect()
    };

    let mut buf = buffer.lock().unwrap_or_else(|e| e.into_inner());
    buf.resize(width as usize);

    for bins in &new_lines {
        buf.push_line(bins);
    }

    // Create a safe copy of pixel data for the surface
    let pixel_copy = buf.pixels.clone();
    let buf_width = buf.width;
    drop(buf); // release lock before Cairo operations

    let stride = gtk4::cairo::Format::ARgb32
        .stride_for_width(buf_width as u32)
        .unwrap();

    let surface = gtk4::cairo::ImageSurface::create_for_data(
        pixel_copy,
        gtk4::cairo::Format::ARgb32,
        buf_width as i32,
        WF_HEIGHT as i32,
        stride,
    );

    let Ok(surface) = surface else { return };

    cr.save().unwrap();
    cr.scale(1.0, height as f64 / WF_HEIGHT as f64);
    let _ = cr.set_source_surface(&surface, 0.0, 0.0);
    let _ = cr.paint();
    cr.restore().unwrap();
}

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
