use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use efd_proto::{FftBins, RadioState};
use gl::types::*;
use gtk4::prelude::*;
use gtk4::{
    Adjustment, Align, Box as GtkBox, Button, DrawingArea, GLArea, Label, Orientation, Overlay,
    SpinButton, gdk,
};

use super::gl_util;
use super::DisplayRange;

const AXIS_HEIGHT: i32 = 30;
const MAX_BINS: usize = 8192;

// --- GLSL shaders (ES 3.0) ---

const VERT_SRC: &str = r#"#version 300 es
precision mediump float;
layout(location = 0) in vec2 a_pos;
out float v_y;
void main() {
    gl_Position = vec4(a_pos * 2.0 - 1.0, 0.0, 1.0);
    v_y = a_pos.y;
}
"#;

// Fill uses a separate vertex shader that passes a [0,1] ratio within the fill
const VERT_FILL_SRC: &str = r#"#version 300 es
precision mediump float;
layout(location = 0) in vec2 a_pos;   // x, y in [0,1]
layout(location = 1) in float a_flag; // 1.0 = top (spectrum line), 0.0 = bottom
out float v_ratio;
void main() {
    gl_Position = vec4(a_pos * 2.0 - 1.0, 0.0, 1.0);
    v_ratio = a_flag;
}
"#;

const FRAG_LINE_SRC: &str = r#"#version 300 es
precision mediump float;
out vec4 fragColor;
void main() {
    fragColor = vec4(0.2, 1.0, 0.3, 1.0);
}
"#;

const FRAG_FILL_SRC: &str = r#"#version 300 es
precision mediump float;
in float v_ratio; // 1.0 at spectrum line, 0.0 at bottom
out vec4 fragColor;
void main() {
    // Bright at spectrum line, fades to transparent at bottom
    vec3 color = mix(vec3(0.0, 0.15, 0.05), vec3(0.15, 0.9, 0.25), v_ratio);
    float alpha = mix(0.02, 0.65, v_ratio * v_ratio);
    fragColor = vec4(color, alpha);
}
"#;

const FRAG_SOLID_SRC: &str = r#"#version 300 es
precision mediump float;
uniform vec4 u_color;
out vec4 fragColor;
void main() {
    fragColor = u_color;
}
"#;

struct GlState {
    prog_line: GLuint,
    prog_fill: GLuint,
    prog_solid: GLuint,
    u_color: GLint,
    vao_spectrum: GLuint,
    vbo_spectrum: GLuint, // line vertices
    vao_fill: GLuint,
    vbo_fill: GLuint, // triangle strip for gradient
    vao_grid: GLuint,
    vbo_grid: GLuint, // grid + center lines
    num_line_verts: i32,
    num_fill_verts: i32,
    num_grid_verts: i32,
}

#[derive(Clone)]
pub struct Spectrum {
    container: GtkBox,
    gl_area: GLArea,
    axis: DrawingArea,
}

impl Spectrum {
    pub fn new(
        fft_data: Arc<Mutex<Option<FftBins>>>,
        radio_state: Arc<Mutex<Option<RadioState>>>,
    ) -> (Self, DisplayRange) {
        let display_range = DisplayRange::new(-40, 120);

        let gl_area = GLArea::new();
        gl_area.set_size_request(-1, 200);
        gl_area.set_auto_render(false);
        gl_area.set_allowed_apis(gdk::GLAPI::GLES);

        let state: Rc<RefCell<Option<GlState>>> = Rc::new(RefCell::new(None));

        // --- realize: compile shaders, create buffers ---
        let st = state.clone();
        gl_area.connect_realize(move |area| {
            area.make_current();
            if area.error().is_some() {
                return;
            }
            gl_util::init_gl();

            let prog_line = gl_util::build_program(VERT_SRC, FRAG_LINE_SRC)
                .expect("spectrum line shader");
            let prog_fill = gl_util::build_program(VERT_FILL_SRC, FRAG_FILL_SRC)
                .expect("spectrum fill shader");
            let prog_solid = gl_util::build_program(VERT_SRC, FRAG_SOLID_SRC)
                .expect("spectrum solid shader");
            let u_color = gl_util::uniform_loc(prog_solid, "u_color");

            let (vao_spectrum, vbo_spectrum) = gl_util::create_vao_vbo();
            let (vao_fill, vbo_fill) = gl_util::create_vao_vbo();
            let (vao_grid, vbo_grid) = gl_util::create_vao_vbo();

            // Allocate VBOs
            unsafe {
                // Spectrum line: MAX_BINS * 2 floats (x,y)
                gl::BindVertexArray(vao_spectrum);
                gl::BindBuffer(gl::ARRAY_BUFFER, vbo_spectrum);
                gl::BufferData(
                    gl::ARRAY_BUFFER,
                    (MAX_BINS * 2 * std::mem::size_of::<f32>()) as GLsizeiptr,
                    std::ptr::null(),
                    gl::DYNAMIC_DRAW,
                );
                gl::EnableVertexAttribArray(0);
                gl::VertexAttribPointer(0, 2, gl::FLOAT, gl::FALSE, 0, std::ptr::null());

                // Fill: MAX_BINS * 2 vertices * 3 floats each (x, y, flag)
                let stride = (3 * std::mem::size_of::<f32>()) as GLsizei;
                gl::BindVertexArray(vao_fill);
                gl::BindBuffer(gl::ARRAY_BUFFER, vbo_fill);
                gl::BufferData(
                    gl::ARRAY_BUFFER,
                    (MAX_BINS * 2 * 3 * std::mem::size_of::<f32>()) as GLsizeiptr,
                    std::ptr::null(),
                    gl::DYNAMIC_DRAW,
                );
                gl::EnableVertexAttribArray(0);
                gl::VertexAttribPointer(0, 2, gl::FLOAT, gl::FALSE, stride, std::ptr::null());
                gl::EnableVertexAttribArray(1);
                gl::VertexAttribPointer(
                    1, 1, gl::FLOAT, gl::FALSE, stride,
                    (2 * std::mem::size_of::<f32>()) as *const _,
                );

                // Grid: generous allocation for lines
                gl::BindVertexArray(vao_grid);
                gl::BindBuffer(gl::ARRAY_BUFFER, vbo_grid);
                gl::BufferData(
                    gl::ARRAY_BUFFER,
                    (512 * 2 * std::mem::size_of::<f32>()) as GLsizeiptr,
                    std::ptr::null(),
                    gl::DYNAMIC_DRAW,
                );
                gl::EnableVertexAttribArray(0);
                gl::VertexAttribPointer(0, 2, gl::FLOAT, gl::FALSE, 0, std::ptr::null());

                gl::BindVertexArray(0);
            }

            *st.borrow_mut() = Some(GlState {
                prog_line,
                prog_fill,
                prog_solid,
                u_color,
                vao_spectrum,
                vbo_spectrum,
                vao_fill,
                vbo_fill,
                vao_grid,
                vbo_grid,
                num_line_verts: 0,
                num_fill_verts: 0,
                num_grid_verts: 0,
            });
        });

        // --- unrealize: clean up GL resources ---
        let st = state.clone();
        gl_area.connect_unrealize(move |area| {
            area.make_current();
            if let Some(s) = st.borrow_mut().take() {
                unsafe {
                    gl::DeleteProgram(s.prog_line);
                    gl::DeleteProgram(s.prog_fill);
                    gl::DeleteProgram(s.prog_solid);
                    gl::DeleteBuffers(1, &s.vbo_spectrum);
                    gl::DeleteBuffers(1, &s.vbo_fill);
                    gl::DeleteBuffers(1, &s.vbo_grid);
                    gl::DeleteVertexArrays(1, &s.vao_spectrum);
                    gl::DeleteVertexArrays(1, &s.vao_fill);
                    gl::DeleteVertexArrays(1, &s.vao_grid);
                }
            }
        });

        // --- render ---
        let st = state.clone();
        let data = fft_data.clone();
        let dr = display_range.clone();
        gl_area.connect_render(move |_area, _ctx| {
            let mut st = st.borrow_mut();
            let Some(ref mut gl) = *st else {
                return glib::Propagation::Proceed;
            };

            let db_top = dr.db_top();
            let db_bottom = dr.db_bottom();
            let db_range = db_top - db_bottom;

            unsafe {
                gl::ClearColor(0.0, 0.0, 0.05, 1.0);
                gl::Clear(gl::COLOR_BUFFER_BIT);
                gl::Enable(gl::BLEND);
                gl::BlendFunc(gl::SRC_ALPHA, gl::ONE_MINUS_SRC_ALPHA);
            }

            // Lock FFT data
            let bins = data.lock().unwrap_or_else(|e| e.into_inner());
            let Some(ref fft) = *bins else {
                return glib::Propagation::Proceed;
            };
            if fft.bins.is_empty() {
                return glib::Propagation::Proceed;
            }

            let center_hz = fft.center_freq_hz as f64;
            let span_hz = fft.span_hz as f64;
            let n = fft.bins.len().min(MAX_BINS);

            // Zoom/pan: visible range in [0,1] of full span
            let (vis_lo, vis_hi) = dr.visible_range();
            let vis_span = vis_hi - vis_lo;
            let visible_span_hz = span_hz * vis_span;

            // Map a full-span normalized position to screen [0,1]
            let to_screen = |full_x: f64| -> f32 {
                ((full_x - vis_lo) / vis_span) as f32
            };

            // Build line + fill vertices for visible bins only
            let bin_lo = ((vis_lo * n as f64) as usize).saturating_sub(1).min(n);
            let bin_hi = ((vis_hi * n as f64) as usize + 2).min(n);

            let mut line_verts: Vec<f32> = Vec::with_capacity((bin_hi - bin_lo) * 2);
            let mut fill_verts: Vec<f32> = Vec::with_capacity((bin_hi - bin_lo) * 6);
            for i in bin_lo..bin_hi {
                let full_x = i as f64 / n as f64;
                let x = to_screen(full_x);
                let db = fft.bins[i];
                let y = ((db as f64 - db_bottom) / db_range).clamp(0.0, 1.0) as f32;
                line_verts.push(x);
                line_verts.push(y);
                fill_verts.extend_from_slice(&[x, y, 1.0]);
                fill_verts.extend_from_slice(&[x, 0.0, 0.0]);
            }

            // Build grid vertices
            let mut grid_verts: Vec<f32> = Vec::new();

            // Horizontal dB grid
            let db_start = (db_bottom / 10.0).ceil() as i32 * 10;
            let db_end = (db_top / 10.0).floor() as i32 * 10;
            for db in (db_start..=db_end).step_by(10) {
                let y = ((db as f64 - db_bottom) / db_range).clamp(0.0, 1.0) as f32;
                grid_verts.extend_from_slice(&[0.0, y, 1.0, y]);
            }

            // Vertical frequency grid (use visible span for step calculation)
            if visible_span_hz > 0.0 {
                let step = nice_freq_step(visible_span_hz);
                let vis_left_hz = center_hz + (vis_lo - 0.5) * span_hz;
                let vis_right_hz = center_hz + (vis_hi - 0.5) * span_hz;
                let first = (vis_left_hz / step).ceil() as i64;
                let last = (vis_right_hz / step).floor() as i64;
                for i in first..=last {
                    let fhz = i as f64 * step;
                    let full_x = (fhz - (center_hz - span_hz / 2.0)) / span_hz;
                    let x = to_screen(full_x);
                    grid_verts.extend_from_slice(&[x, 0.0, x, 1.0]);
                }
            }

            // Center line (at the VFO center frequency)
            let center_screen = to_screen(0.5);
            let mut center_verts = vec![center_screen, 0.0, center_screen, 1.0];

            drop(bins);

            // Upload line data
            gl.num_line_verts = (line_verts.len() / 2) as i32;
            unsafe {
                gl::BindBuffer(gl::ARRAY_BUFFER, gl.vbo_spectrum);
                gl::BufferSubData(
                    gl::ARRAY_BUFFER,
                    0,
                    (line_verts.len() * std::mem::size_of::<f32>()) as GLsizeiptr,
                    line_verts.as_ptr() as *const _,
                );
            }

            // Upload fill data (3 floats per vertex: x, y, flag)
            gl.num_fill_verts = (fill_verts.len() / 3) as i32;
            unsafe {
                gl::BindBuffer(gl::ARRAY_BUFFER, gl.vbo_fill);
                gl::BufferSubData(
                    gl::ARRAY_BUFFER,
                    0,
                    (fill_verts.len() * std::mem::size_of::<f32>()) as GLsizeiptr,
                    fill_verts.as_ptr() as *const _,
                );
            }

            // Upload grid data (grid lines + center line appended)
            let grid_count = (grid_verts.len() / 2) as i32;
            let _center_offset = grid_verts.len();
            grid_verts.append(&mut center_verts);
            gl.num_grid_verts = grid_count;
            unsafe {
                gl::BindBuffer(gl::ARRAY_BUFFER, gl.vbo_grid);
                gl::BufferSubData(
                    gl::ARRAY_BUFFER,
                    0,
                    (grid_verts.len() * std::mem::size_of::<f32>()) as GLsizeiptr,
                    grid_verts.as_ptr() as *const _,
                );
            }

            // Draw grid lines
            unsafe {
                gl::UseProgram(gl.prog_solid);
                gl::Uniform4f(gl.u_color, 0.6, 0.6, 0.6, 0.7);
                gl::LineWidth(1.0);
                gl::BindVertexArray(gl.vao_grid);
                gl::DrawArrays(gl::LINES, 0, gl.num_grid_verts);
            }

            // Draw gradient fill
            unsafe {
                gl::UseProgram(gl.prog_fill);
                gl::BindVertexArray(gl.vao_fill);
                gl::DrawArrays(gl::TRIANGLE_STRIP, 0, gl.num_fill_verts);
            }

            // Draw spectrum line
            unsafe {
                gl::UseProgram(gl.prog_line);
                gl::BindVertexArray(gl.vao_spectrum);
                gl::DrawArrays(gl::LINE_STRIP, 0, gl.num_line_verts);
            }

            // Draw center line (red)
            unsafe {
                gl::UseProgram(gl.prog_solid);
                gl::Uniform4f(gl.u_color, 1.0, 0.3, 0.3, 0.7);
                gl::BindVertexArray(gl.vao_grid);
                gl::DrawArrays(gl::LINES, grid_count, 2);
            }

            unsafe {
                gl::BindVertexArray(0);
                gl::Disable(gl::BLEND);
            }

            glib::Propagation::Proceed
        });

        // Overlay: GLArea + controls
        let overlay = Overlay::new();
        overlay.set_child(Some(&gl_area));

        // Controls box — top-right corner, semi-transparent
        let ctrl_box = GtkBox::new(Orientation::Horizontal, 8);
        ctrl_box.add_css_class("spectrum-controls");
        ctrl_box.set_halign(Align::End);
        ctrl_box.set_valign(Align::Start);
        ctrl_box.set_margin_top(4);
        ctrl_box.set_margin_end(4);

        let ref_label = Label::new(Some("Ref:"));
        ref_label.add_css_class("monospace");
        ctrl_box.append(&ref_label);
        let ref_adj = Adjustment::new(-40.0, -60.0, 0.0, 5.0, 10.0, 0.0);
        let ref_spin = SpinButton::new(Some(&ref_adj), 5.0, 0);
        ref_spin.set_width_chars(4);
        let dr = display_range.clone();
        ref_spin.connect_value_changed(move |spin| {
            dr.set_ref_level(spin.value() as i32);
        });
        ctrl_box.append(&ref_spin);
        let ref_unit = Label::new(Some("dBm"));
        ref_unit.add_css_class("monospace");
        ctrl_box.append(&ref_unit);

        let range_label = Label::new(Some("Range:"));
        range_label.add_css_class("monospace");
        ctrl_box.append(&range_label);
        let range_adj = Adjustment::new(120.0, 40.0, 160.0, 10.0, 20.0, 0.0);
        let range_spin = SpinButton::new(Some(&range_adj), 10.0, 0);
        range_spin.set_width_chars(4);
        let dr = display_range.clone();
        range_spin.connect_value_changed(move |spin| {
            dr.set_range(spin.value() as i32);
        });
        ctrl_box.append(&range_spin);
        let range_unit = Label::new(Some("dB"));
        range_unit.add_css_class("monospace");
        ctrl_box.append(&range_unit);

        // Zoom controls
        let zoom_label_widget = Label::new(Some("Zoom:"));
        zoom_label_widget.add_css_class("monospace");
        ctrl_box.append(&zoom_label_widget);

        let zoom_value = Label::new(Some("1x"));
        zoom_value.add_css_class("monospace");
        zoom_value.set_width_chars(3);

        let zoom_out_btn = Button::with_label("\u{2212}"); // minus sign
        zoom_out_btn.set_valign(Align::Center);
        let dr = display_range.clone();
        let zv = zoom_value.clone();
        zoom_out_btn.connect_clicked(move |_| {
            let z = dr.zoom_out();
            zv.set_text(&format!("{z}x"));
        });
        ctrl_box.append(&zoom_out_btn);

        ctrl_box.append(&zoom_value);

        let zoom_in_btn = Button::with_label("+");
        zoom_in_btn.set_valign(Align::Center);
        let dr = display_range.clone();
        let zv = zoom_value.clone();
        zoom_in_btn.connect_clicked(move |_| {
            let z = dr.zoom_in();
            zv.set_text(&format!("{z}x"));
        });
        ctrl_box.append(&zoom_in_btn);

        // Pan: left/right buttons
        let pan_left_btn = Button::with_label("\u{25C0}"); // left arrow
        pan_left_btn.set_valign(Align::Center);
        let dr = display_range.clone();
        pan_left_btn.connect_clicked(move |_| {
            let z = dr.zoom() as f64;
            dr.pan_by(-0.1 / z);
        });
        ctrl_box.append(&pan_left_btn);

        let pan_right_btn = Button::with_label("\u{25B6}"); // right arrow
        pan_right_btn.set_valign(Align::Center);
        let dr = display_range.clone();
        pan_right_btn.connect_clicked(move |_| {
            let z = dr.zoom() as f64;
            dr.pan_by(0.1 / z);
        });
        ctrl_box.append(&pan_right_btn);

        overlay.add_overlay(&ctrl_box);

        // Frequency axis strip (Cairo — text-heavy, low perf impact)
        let axis = DrawingArea::new();
        axis.set_content_height(AXIS_HEIGHT);

        let data2 = fft_data;
        let rs2 = radio_state;
        let dr2 = display_range.clone();
        axis.set_draw_func(move |_area, cr, width, _height| {
            draw_freq_axis(cr, width, &data2, &rs2, &dr2);
        });

        let container = GtkBox::new(Orientation::Vertical, 0);
        overlay.set_vexpand(true);
        container.append(&overlay);
        container.append(&axis);

        (
            Self {
                container,
                gl_area,
                axis,
            },
            display_range,
        )
    }

    pub fn widget(&self) -> &GtkBox {
        &self.container
    }

    pub fn queue_draw(&self) {
        self.gl_area.queue_render();
        self.axis.queue_draw();
    }
}

// --- Frequency axis (Cairo, unchanged) ---

fn freq_label(hz: f64) -> String {
    let abs = hz.abs();
    if abs >= 1_000_000.0 {
        format!("{:.3} MHz", hz / 1_000_000.0)
    } else if abs >= 1_000.0 {
        format!("{:.1} kHz", hz / 1_000.0)
    } else {
        format!("{:.0} Hz", hz)
    }
}

fn draw_freq_axis(
    cr: &gtk4::cairo::Context,
    width: i32,
    fft_data: &Arc<Mutex<Option<FftBins>>>,
    radio_state: &Arc<Mutex<Option<RadioState>>>,
    display_range: &DisplayRange,
) {
    let w = width as f64;
    let h = AXIS_HEIGHT as f64;

    cr.set_source_rgb(0.05, 0.05, 0.08);
    cr.rectangle(0.0, 0.0, w, h);
    let _ = cr.fill();

    let bins = fft_data.lock().unwrap_or_else(|e| e.into_inner());
    let Some(ref fft) = *bins else { return };
    if fft.bins.is_empty() {
        return;
    }

    let center_hz = fft.center_freq_hz as f64;
    let span_hz = fft.span_hz as f64;
    drop(bins);

    let vfo_hz = {
        let rs = radio_state.lock().unwrap_or_else(|e| e.into_inner());
        rs.as_ref().map(|s| s.freq_hz as f64).unwrap_or(0.0)
    };

    // Zoom/pan: visible range
    let (vis_lo, vis_hi) = display_range.visible_range();
    let vis_span = vis_hi - vis_lo;
    let visible_span_hz = span_hz * vis_span;

    // Map a full-span normalized position to screen pixel
    let to_screen_x = |full_x: f64| -> f64 {
        ((full_x - vis_lo) / vis_span) * w
    };

    cr.select_font_face(
        "Hack Nerd Font Mono",
        gtk4::cairo::FontSlant::Normal,
        gtk4::cairo::FontWeight::Normal,
    );
    cr.set_font_size(10.0);
    let step = nice_freq_step(visible_span_hz);
    let vis_left_hz = center_hz + (vis_lo - 0.5) * span_hz;
    let vis_right_hz = center_hz + (vis_hi - 0.5) * span_hz;
    let first = (vis_left_hz / step).ceil() as i64;
    let last = (vis_right_hz / step).floor() as i64;

    for i in first..=last {
        let fhz = i as f64 * step;
        let full_x = (fhz - (center_hz - span_hz / 2.0)) / span_hz;
        let x = to_screen_x(full_x);
        if x < 30.0 || x > w - 10.0 {
            continue;
        }

        // Tick mark
        cr.set_source_rgba(0.6, 0.6, 0.6, 0.7);
        cr.set_line_width(1.0);
        cr.move_to(x, 0.0);
        cr.line_to(x, 4.0);
        let _ = cr.stroke();

        // Relative offset from center
        cr.set_source_rgb(0.7, 0.7, 0.7);
        let rel = freq_label(fhz - center_hz);
        cr.move_to(x - 20.0, 14.0);
        let _ = cr.show_text(&rel);

        // Absolute frequency
        cr.set_source_rgba(0.8, 0.8, 0.5, 0.8);
        let abs_f = freq_label(vfo_hz + fhz - center_hz);
        cr.move_to(x - 20.0, 26.0);
        let _ = cr.show_text(&abs_f);
    }
}

fn nice_freq_step(span_hz: f64) -> f64 {
    let raw = span_hz / 10.0;
    let mag = 10.0_f64.powf(raw.log10().floor());
    let norm = raw / mag;
    let step = if norm < 1.5 {
        1.0
    } else if norm < 3.5 {
        2.0
    } else if norm < 7.5 {
        5.0
    } else {
        10.0
    };
    step * mag
}
