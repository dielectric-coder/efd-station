use gl::types::*;
use std::ffi::CString;
use std::ptr;
use std::str;

/// Initialize OpenGL function pointers via eglGetProcAddress.
/// Must be called inside a GLArea `connect_realize` handler after `make_current()`.
pub fn init_gl() {
    // Resolve eglGetProcAddress from libEGL (loaded by GTK4/GDK for GLES contexts).
    let egl_get_proc: unsafe extern "C" fn(*const std::ffi::c_char) -> *const std::ffi::c_void = unsafe {
        let sym = libc::dlsym(libc::RTLD_DEFAULT, b"eglGetProcAddress\0".as_ptr() as *const _);
        assert!(!sym.is_null(), "eglGetProcAddress not found — is libEGL loaded?");
        std::mem::transmute(sym)
    };

    gl::load_with(|name| {
        let cname = CString::new(name).unwrap();
        unsafe { egl_get_proc(cname.as_ptr()) as *const _ }
    });
}

/// Compile a GLSL shader. Returns the shader object ID.
pub fn compile_shader(src: &str, shader_type: GLenum) -> Result<GLuint, String> {
    unsafe {
        let shader = gl::CreateShader(shader_type);
        let c_src = CString::new(src).unwrap();
        gl::ShaderSource(shader, 1, &c_src.as_ptr(), ptr::null());
        gl::CompileShader(shader);

        let mut success: GLint = 0;
        gl::GetShaderiv(shader, gl::COMPILE_STATUS, &mut success);
        if success == 0 {
            let mut len: GLint = 0;
            gl::GetShaderiv(shader, gl::INFO_LOG_LENGTH, &mut len);
            let mut buf = vec![0u8; len as usize];
            gl::GetShaderInfoLog(shader, len, ptr::null_mut(), buf.as_mut_ptr() as *mut _);
            buf.truncate(buf.iter().position(|&c| c == 0).unwrap_or(buf.len()));
            let msg = str::from_utf8(&buf).unwrap_or("(invalid UTF-8)").to_string();
            gl::DeleteShader(shader);
            Err(msg)
        } else {
            Ok(shader)
        }
    }
}

/// Link vertex + fragment shaders into a program. Returns the program ID.
pub fn link_program(vert: GLuint, frag: GLuint) -> Result<GLuint, String> {
    unsafe {
        let program = gl::CreateProgram();
        gl::AttachShader(program, vert);
        gl::AttachShader(program, frag);
        gl::LinkProgram(program);

        let mut success: GLint = 0;
        gl::GetProgramiv(program, gl::LINK_STATUS, &mut success);
        if success == 0 {
            let mut len: GLint = 0;
            gl::GetProgramiv(program, gl::INFO_LOG_LENGTH, &mut len);
            let mut buf = vec![0u8; len as usize];
            gl::GetProgramInfoLog(program, len, ptr::null_mut(), buf.as_mut_ptr() as *mut _);
            buf.truncate(buf.iter().position(|&c| c == 0).unwrap_or(buf.len()));
            let msg = str::from_utf8(&buf).unwrap_or("(invalid UTF-8)").to_string();
            gl::DeleteProgram(program);
            Err(msg)
        } else {
            // Shaders can be detached after linking
            gl::DetachShader(program, vert);
            gl::DetachShader(program, frag);
            Ok(program)
        }
    }
}

/// Build a program from vertex + fragment shader source.
pub fn build_program(vert_src: &str, frag_src: &str) -> Result<GLuint, String> {
    let vert = compile_shader(vert_src, gl::VERTEX_SHADER)?;
    let frag = match compile_shader(frag_src, gl::FRAGMENT_SHADER) {
        Ok(f) => f,
        Err(e) => {
            unsafe { gl::DeleteShader(vert) };
            return Err(e);
        }
    };
    let prog = link_program(vert, frag);
    unsafe {
        gl::DeleteShader(vert);
        gl::DeleteShader(frag);
    }
    prog
}

/// Get a uniform location, returning -1 if not found.
pub fn uniform_loc(program: GLuint, name: &str) -> GLint {
    let cname = CString::new(name).unwrap();
    unsafe { gl::GetUniformLocation(program, cname.as_ptr()) }
}

/// Create a VAO + VBO pair. Returns (vao, vbo).
pub fn create_vao_vbo() -> (GLuint, GLuint) {
    unsafe {
        let mut vao: GLuint = 0;
        let mut vbo: GLuint = 0;
        gl::GenVertexArrays(1, &mut vao);
        gl::GenBuffers(1, &mut vbo);
        (vao, vbo)
    }
}
