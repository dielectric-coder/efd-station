use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use efd_proto::{FftBins, RadioState};
use gl::types::*;
use gtk4::prelude::*;
use gtk4::{gdk, GLArea};

use super::gl_util;
use super::{parse_filter_bw_hz, DisplayRange};

const WF_HEIGHT: usize = 512;
const MAX_PENDING: usize = 64;
const MAX_WIDTH: usize = 4096;

// --- GLSL shaders (ES 3.0) ---

const VERT_QUAD_SRC: &str = r#"#version 300 es
precision mediump float;
layout(location = 0) in vec2 a_pos;
out vec2 v_texcoord;
void main() {
    gl_Position = vec4(a_pos * 2.0 - 1.0, 0.0, 1.0);
    v_texcoord = a_pos;
}
"#;

const FRAG_WATERFALL_SRC: &str = r#"#version 300 es
precision mediump float;
in vec2 v_texcoord;
uniform sampler2D u_waterfall;
uniform float u_offset;
uniform float u_x_lo;  // visible range start (0..1)
uniform float u_x_hi;  // visible range end (0..1)
out vec4 fragColor;
void main() {
    // Zoom/pan: remap x from [0,1] screen to [u_x_lo, u_x_hi] in texture
    float tx = mix(u_x_lo, u_x_hi, v_texcoord.x);
    // Ring buffer scroll: offset the y coordinate
    vec2 tc = vec2(tx, fract(v_texcoord.y + u_offset));
    fragColor = texture(u_waterfall, tc);
}
"#;

const VERT_LINE_SRC: &str = r#"#version 300 es
precision mediump float;
layout(location = 0) in vec2 a_pos;
void main() {
    gl_Position = vec4(a_pos * 2.0 - 1.0, 0.0, 1.0);
}
"#;

const FRAG_LINE_SRC: &str = r#"#version 300 es
precision mediump float;
uniform vec4 u_color;
out vec4 fragColor;
void main() {
    fragColor = u_color;
}
"#;

struct WfGlState {
    prog_wf: GLuint,
    prog_line: GLuint,
    u_offset: GLint,
    u_x_lo: GLint,
    u_x_hi: GLint,
    u_line_color: GLint,
    vao_quad: GLuint,
    vbo_quad: GLuint,
    vao_lines: GLuint,
    vbo_lines: GLuint,
    texture: GLuint,
    tex_width: usize,
    write_row: usize,
    // Temporary row buffer (avoid per-frame allocation)
    row_rgba: Vec<u8>,
}

#[derive(Clone)]
pub struct Waterfall {
    gl_area: GLArea,
    pending: Arc<Mutex<Vec<Vec<f32>>>>,
}

impl Waterfall {
    pub fn new(
        display_range: DisplayRange,
        fft_data: Arc<Mutex<Option<FftBins>>>,
        radio_state: Arc<Mutex<Option<RadioState>>>,
    ) -> Self {
        let pending: Arc<Mutex<Vec<Vec<f32>>>> = Arc::new(Mutex::new(Vec::new()));

        let gl_area = GLArea::new();
        gl_area.set_size_request(-1, 300);
        gl_area.set_auto_render(false);
        gl_area.set_allowed_apis(gdk::GLAPI::GLES);

        let state: Rc<RefCell<Option<WfGlState>>> = Rc::new(RefCell::new(None));

        // --- realize ---
        let st = state.clone();
        gl_area.connect_realize(move |area| {
            area.make_current();
            if area.error().is_some() {
                return;
            }
            gl_util::init_gl();

            let prog_wf =
                gl_util::build_program(VERT_QUAD_SRC, FRAG_WATERFALL_SRC).expect("wf shader");
            let prog_line =
                gl_util::build_program(VERT_LINE_SRC, FRAG_LINE_SRC).expect("wf line shader");

            let u_offset = gl_util::uniform_loc(prog_wf, "u_offset");
            let u_x_lo = gl_util::uniform_loc(prog_wf, "u_x_lo");
            let u_x_hi = gl_util::uniform_loc(prog_wf, "u_x_hi");
            let u_line_color = gl_util::uniform_loc(prog_line, "u_color");

            // Fullscreen quad VBO
            let (vao_quad, vbo_quad) = gl_util::create_vao_vbo();
            let quad_verts: [f32; 12] = [
                0.0, 0.0, 1.0, 0.0, 0.0, 1.0, // triangle 1
                1.0, 0.0, 1.0, 1.0, 0.0, 1.0, // triangle 2
            ];
            unsafe {
                gl::BindVertexArray(vao_quad);
                gl::BindBuffer(gl::ARRAY_BUFFER, vbo_quad);
                gl::BufferData(
                    gl::ARRAY_BUFFER,
                    std::mem::size_of_val(&quad_verts) as GLsizeiptr,
                    quad_verts.as_ptr() as *const _,
                    gl::STATIC_DRAW,
                );
                gl::EnableVertexAttribArray(0);
                gl::VertexAttribPointer(0, 2, gl::FLOAT, gl::FALSE, 0, std::ptr::null());
            }

            // Lines VBO for bandwidth overlay
            let (vao_lines, vbo_lines) = gl_util::create_vao_vbo();
            unsafe {
                gl::BindVertexArray(vao_lines);
                gl::BindBuffer(gl::ARRAY_BUFFER, vbo_lines);
                gl::BufferData(
                    gl::ARRAY_BUFFER,
                    (32 * std::mem::size_of::<f32>()) as GLsizeiptr,
                    std::ptr::null(),
                    gl::DYNAMIC_DRAW,
                );
                gl::EnableVertexAttribArray(0);
                gl::VertexAttribPointer(0, 2, gl::FLOAT, gl::FALSE, 0, std::ptr::null());
            }

            // Waterfall texture
            let mut texture: GLuint = 0;
            let tex_width = 1024_usize;
            unsafe {
                gl::GenTextures(1, &mut texture);
                gl::BindTexture(gl::TEXTURE_2D, texture);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::NEAREST as GLint);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::NEAREST as GLint);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as GLint);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::REPEAT as GLint);
                // Allocate empty texture
                gl::TexImage2D(
                    gl::TEXTURE_2D,
                    0,
                    gl::RGBA as GLint,
                    tex_width as GLsizei,
                    WF_HEIGHT as GLsizei,
                    0,
                    gl::RGBA,
                    gl::UNSIGNED_BYTE,
                    std::ptr::null(),
                );
                gl::BindVertexArray(0);
            }

            *st.borrow_mut() = Some(WfGlState {
                prog_wf,
                prog_line,
                u_offset,
                u_x_lo,
                u_x_hi,
                u_line_color,
                vao_quad,
                vbo_quad,
                vao_lines,
                vbo_lines,
                texture,
                tex_width,
                write_row: 0,
                row_rgba: vec![0u8; tex_width * 4],
            });
        });

        // --- unrealize ---
        let st = state.clone();
        gl_area.connect_unrealize(move |area| {
            area.make_current();
            if let Some(s) = st.borrow_mut().take() {
                unsafe {
                    gl::DeleteProgram(s.prog_wf);
                    gl::DeleteProgram(s.prog_line);
                    gl::DeleteTextures(1, &s.texture);
                    gl::DeleteBuffers(1, &s.vbo_quad);
                    gl::DeleteBuffers(1, &s.vbo_lines);
                    gl::DeleteVertexArrays(1, &s.vao_quad);
                    gl::DeleteVertexArrays(1, &s.vao_lines);
                }
            }
        });

        // --- render ---
        let st = state.clone();
        let pend = pending.clone();
        let dr = display_range;
        gl_area.connect_render(move |area, _ctx| {
            let mut st = st.borrow_mut();
            let Some(ref mut gl) = *st else {
                return glib::Propagation::Proceed;
            };

            let db_bottom = dr.db_bottom();
            let db_range = dr.range();

            // Get widget width and resize texture if needed
            let widget_width = area.width() as usize;
            let desired_width = widget_width.clamp(1, MAX_WIDTH);
            if desired_width != gl.tex_width {
                gl.tex_width = desired_width;
                gl.write_row = 0;
                gl.row_rgba.resize(desired_width * 4, 0);
                unsafe {
                    gl::BindTexture(gl::TEXTURE_2D, gl.texture);
                    gl::TexImage2D(
                        gl::TEXTURE_2D,
                        0,
                        gl::RGBA as GLint,
                        desired_width as GLsizei,
                        WF_HEIGHT as GLsizei,
                        0,
                        gl::RGBA,
                        gl::UNSIGNED_BYTE,
                        std::ptr::null(),
                    );
                }
            }

            // Drain pending lines and upload to texture
            let new_lines: Vec<Vec<f32>> = {
                pend.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .drain(..)
                    .collect()
            };

            if !new_lines.is_empty() {
                unsafe {
                    gl::BindTexture(gl::TEXTURE_2D, gl.texture);
                }
                for bins in &new_lines {
                    let n = bins.len().max(1);
                    for x in 0..gl.tex_width {
                        let bin_idx = (x * n / gl.tex_width).min(n - 1);
                        let db = bins.get(bin_idx).copied().unwrap_or(db_bottom as f32);
                        let (r, g, b) = db_to_color_u8(db, db_bottom, db_range);
                        let off = x * 4;
                        gl.row_rgba[off] = r;
                        gl.row_rgba[off + 1] = g;
                        gl.row_rgba[off + 2] = b;
                        gl.row_rgba[off + 3] = 255;
                    }
                    unsafe {
                        gl::TexSubImage2D(
                            gl::TEXTURE_2D,
                            0,
                            0,
                            gl.write_row as GLint,
                            gl.tex_width as GLsizei,
                            1,
                            gl::RGBA,
                            gl::UNSIGNED_BYTE,
                            gl.row_rgba.as_ptr() as *const _,
                        );
                    }
                    gl.write_row = (gl.write_row + 1) % WF_HEIGHT;
                }
            }

            // Zoom/pan: visible range
            let (vis_lo, vis_hi) = dr.visible_range();
            let vis_span = vis_hi - vis_lo;

            // Map full-span [0,1] position to screen [0,1]
            let to_screen = |full_x: f64| -> f32 {
                ((full_x - vis_lo) / vis_span) as f32
            };

            // Draw waterfall with zoom/pan
            unsafe {
                gl::ClearColor(0.0, 0.0, 0.0, 1.0);
                gl::Clear(gl::COLOR_BUFFER_BIT);

                gl::UseProgram(gl.prog_wf);
                gl::ActiveTexture(gl::TEXTURE0);
                gl::BindTexture(gl::TEXTURE_2D, gl.texture);
                gl::Uniform1f(gl.u_offset, gl.write_row as f32 / WF_HEIGHT as f32);
                gl::Uniform1f(gl.u_x_lo, vis_lo as f32);
                gl::Uniform1f(gl.u_x_hi, vis_hi as f32);
                gl::BindVertexArray(gl.vao_quad);
                gl::DrawArrays(gl::TRIANGLES, 0, 6);
            }

            // Bandwidth overlay lines
            let fft_meta = {
                let guard = fft_data.lock().unwrap_or_else(|e| e.into_inner());
                guard.as_ref().filter(|f| !f.bins.is_empty()).map(|f| {
                    (f.center_freq_hz as f64, f.span_hz as f64)
                })
            };

            if let Some((_center_hz, span_hz)) = fft_meta {
                let rs = radio_state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(ref state) = *rs {
                    if let Some(bw) = parse_filter_bw_hz(&state.filter_bw) {
                        use efd_proto::Mode;
                        let (lo_off, hi_off) = match state.mode {
                            Mode::USB | Mode::CW => (0.0, bw),
                            Mode::LSB | Mode::CWR => (-bw, 0.0),
                            _ => (-bw / 2.0, bw / 2.0),
                        };
                        // Map to screen coordinates via zoom/pan
                        let full_lo = 0.5 + lo_off / span_hz;
                        let full_hi = 0.5 + hi_off / span_hz;
                        let x_lo = to_screen(full_lo);
                        let x_hi = to_screen(full_hi);

                        let line_verts: [f32; 8] = [
                            x_lo, 0.0, x_lo, 1.0,
                            x_hi, 0.0, x_hi, 1.0,
                        ];

                        unsafe {
                            gl::Enable(gl::BLEND);
                            gl::BlendFunc(gl::SRC_ALPHA, gl::ONE_MINUS_SRC_ALPHA);
                            gl::UseProgram(gl.prog_line);
                            gl::Uniform4f(gl.u_line_color, 1.0, 1.0, 0.3, 0.6);
                            gl::BindVertexArray(gl.vao_lines);
                            gl::BindBuffer(gl::ARRAY_BUFFER, gl.vbo_lines);
                            gl::BufferSubData(
                                gl::ARRAY_BUFFER,
                                0,
                                std::mem::size_of_val(&line_verts) as GLsizeiptr,
                                line_verts.as_ptr() as *const _,
                            );
                            gl::DrawArrays(gl::LINES, 0, 4);
                            gl::Disable(gl::BLEND);
                        }
                    }
                }
            }

            unsafe {
                gl::BindVertexArray(0);
            }

            glib::Propagation::Proceed
        });

        Self { gl_area, pending }
    }

    pub fn widget(&self) -> &GLArea {
        &self.gl_area
    }

    pub fn push_line(&self, bins: &[f32]) {
        let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let len = p.len();
        if len >= MAX_PENDING {
            p.drain(0..len / 2);
        }
        p.push(bins.to_vec());
    }

    pub fn queue_draw(&self) {
        self.gl_area.queue_render();
    }
}

fn db_to_color_u8(db: f32, db_bottom: f64, db_range: f64) -> (u8, u8, u8) {
    let t = ((db as f64 - db_bottom) / db_range).clamp(0.0, 1.0) as f32;

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
