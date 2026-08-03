#![allow(dead_code)]

use std::os::raw::c_void;
use std::ptr;

use khronos_egl as egl;

use crate::osd::OsdSprite;
use crate::render::RgbaBuffer;
use wayland_client::Proxy;

mod gles2 {
    #![allow(
        non_snake_case,
        non_camel_case_types,
        dead_code,
        unused_variables,
        unused_unsafe,
        unsafe_op_in_unsafe_fn,
        clippy::too_many_arguments,
        clippy::missing_safety_doc,
        clippy::upper_case_acronyms,
        clippy::unreadable_literal,
        clippy::missing_transmute_annotations,
        clippy::manual_c_str_literals
    )]
    include!(concat!(env!("OUT_DIR"), "/gles2.rs"));
}

use gles2::types::{GLchar, GLfloat, GLint, GLsizeiptr, GLuint, GLvoid};

const VERTEX_SHADER: &str = r#"
attribute vec2 a_pos;
uniform vec4 u_src;
varying vec2 v_uv;
void main() {
    vec2 ndc = vec2(a_pos.x * 2.0 - 1.0, a_pos.y * 2.0 - 1.0);
    v_uv = u_src.xy + vec2(a_pos.x, 1.0 - a_pos.y) * u_src.zw;
    gl_Position = vec4(ndc, 0.0, 1.0);
}
"#;

/// Vertex shader for the OSD pass: samples the texture in top-down order. The
/// captured frame is stored flipped (bottom-up, see the screencopy y-invert
/// handling) to compensate the flip in `VERTEX_SHADER`, but the OSD sprite is
/// top-down — reusing the flipped shader would render the legend upside down
/// and out of its box.
const OSD_VERTEX_SHADER: &str = r#"
attribute vec2 a_pos;
uniform vec4 u_src;
varying vec2 v_uv;
void main() {
    vec2 ndc = vec2(a_pos.x * 2.0 - 1.0, a_pos.y * 2.0 - 1.0);
    v_uv = u_src.xy + vec2(a_pos.x, a_pos.y) * u_src.zw;
    gl_Position = vec4(ndc, 0.0, 1.0);
}
"#;

const FRAGMENT_SHADER: &str = r#"
precision mediump float;
varying vec2 v_uv;
uniform sampler2D u_tex;
void main() {
    gl_FragColor = texture2D(u_tex, v_uv);
}
"#;

/// The GPU path renders its buffers at this multiple of the layer's logical
/// size and advertises the same value via `wl_surface.set_buffer_scale`, so the
/// compositor's fractional-scale resampling doesn't soften the magnified
/// pixels. With `GL_NEAREST` sampling this yields the crisp, pixelated
/// magnifier look.
pub const RENDER_SCALE: i32 = 2;

/// GPU-accelerated renderer backed by EGL + OpenGL ES 2.
///
/// Renders the captured frame as a textured quad (nearest-neighbor sampling
/// into a 2x buffer-scale surface for a crisp, pixelated magnifier look) and
/// draws the OSD as a second alpha-blended quad. All scaling, panning and
/// easing happen on the GPU.
pub struct GpuRenderer {
    egl: egl::DynamicInstance<egl::EGL1_4>,
    display: egl::Display,
    surface: egl::Surface,
    context: egl::Context,
    egl_window: wayland_egl::WlEglSurface,
    program: GLuint,
    osd_program: GLuint,
    vao: GLuint,
    vbo: GLuint,
    frame_tex: GLuint,
    osd_tex: GLuint,
    a_pos_loc: GLint,
    u_src_loc: GLint,
    osd_u_src_loc: GLint,
    width: i32,
    height: i32,
}

impl GpuRenderer {
    pub fn init(
        wl_display: *mut c_void,
        wl_surface: &wayland_client::protocol::wl_surface::WlSurface,
        width: i32,
        height: i32,
    ) -> anyhow::Result<Self> {
        let egl = unsafe { egl::DynamicInstance::<egl::EGL1_4>::load_required() }
            .map_err(|e| anyhow::anyhow!("Failed to load libEGL: {e}"))?;

        let display = unsafe { egl.get_display(wl_display) }
            .ok_or_else(|| anyhow::anyhow!("Failed to create EGL display"))?;
        egl.initialize(display)
            .map_err(|e| anyhow::anyhow!("Failed to initialize EGL: {e:?}"))?;
        egl.bind_api(egl::OPENGL_ES_API)
            .map_err(|e| anyhow::anyhow!("Failed to bind GLES API: {e:?}"))?;

        let config = egl
            .choose_first_config(
                display,
                &[
                    egl::SURFACE_TYPE,
                    egl::WINDOW_BIT,
                    egl::RED_SIZE,
                    8,
                    egl::GREEN_SIZE,
                    8,
                    egl::BLUE_SIZE,
                    8,
                    egl::ALPHA_SIZE,
                    8,
                    egl::RENDERABLE_TYPE,
                    egl::OPENGL_ES2_BIT,
                    egl::NONE,
                ],
            )
            .map_err(|e| anyhow::anyhow!("Failed to choose EGL config: {e:?}"))?
            .ok_or_else(|| anyhow::anyhow!("No suitable EGL config"))?;

        let context = egl
            .create_context(
                display,
                config,
                None,
                &[egl::CONTEXT_CLIENT_VERSION, 2, egl::NONE],
            )
            .map_err(|e| anyhow::anyhow!("Failed to create EGL context: {e:?}"))?;

        let egl_window = wayland_egl::WlEglSurface::new(
            wl_surface.id(),
            width * RENDER_SCALE,
            height * RENDER_SCALE,
        )
        .map_err(|e| anyhow::anyhow!("Failed to create wl_egl_window: {e}"))?;
        let surface = unsafe {
            egl.create_window_surface(display, config, egl_window.ptr() as *mut c_void, None)
                .map_err(|e| anyhow::anyhow!("Failed to create EGL window surface: {e:?}"))?
        };

        egl.make_current(display, Some(surface), Some(surface), Some(context))
            .map_err(|e| anyhow::anyhow!("Failed to make EGL context current: {e:?}"))?;

        // Swap interval 0: never block in eglSwapBuffers waiting for buffer
        // release, so the event loop stays responsive (keys keep working)
        // while the panning animation redraws every frame.
        egl.swap_interval(display, 0)
            .map_err(|e| anyhow::anyhow!("Failed to set EGL swap interval: {e:?}"))?;

        gles2::load_with(|name| {
            egl.get_proc_address(name)
                .map(|f| f as *const c_void)
                .unwrap_or(ptr::null())
        });

        let program = Self::build_program(VERTEX_SHADER)?;
        let u_src_loc = get_uniform_location(program, c"u_src".as_ptr());
        let a_pos_loc = get_attrib_location(program, c"a_pos".as_ptr());
        if u_src_loc < 0 || a_pos_loc < 0 {
            anyhow::bail!(
                "Shader locations not found (u_src={}, a_pos={})",
                u_src_loc,
                a_pos_loc
            );
        }
        let u_tex_loc = get_uniform_location(program, c"u_tex".as_ptr());
        if u_tex_loc < 0 {
            anyhow::bail!("Shader uniform u_tex not found");
        }
        let osd_program = Self::build_program(OSD_VERTEX_SHADER)?;
        let osd_u_src_loc = get_uniform_location(osd_program, c"u_src".as_ptr());
        let osd_u_tex_loc = get_uniform_location(osd_program, c"u_tex".as_ptr());
        if osd_u_src_loc < 0 || osd_u_tex_loc < 0 {
            anyhow::bail!(
                "OSD shader locations not found (u_src={}, u_tex={})",
                osd_u_src_loc,
                osd_u_tex_loc
            );
        }

        let mut vao = 0;
        let mut vbo = 0;
        let mut frame_tex = 0;
        let mut osd_tex = 0;
        unsafe {
            gles2::GenVertexArraysOES(1, &mut vao);
            gles2::BindVertexArrayOES(vao);
            gles2::GenBuffers(1, &mut vbo);
            gles2::BindBuffer(gles2::ARRAY_BUFFER, vbo);
            gles2::EnableVertexAttribArray(a_pos_loc as GLuint);
            gles2::VertexAttribPointer(
                a_pos_loc as GLuint,
                2,
                gles2::FLOAT,
                gles2::FALSE,
                0,
                ptr::null(),
            );
            gles2::PixelStorei(gles2::UNPACK_ALIGNMENT, 4);
            gles2::PixelStorei(gles2::PACK_ALIGNMENT, 4);
            gles2::GenTextures(1, &mut frame_tex);
            gles2::BindTexture(gles2::TEXTURE_2D, frame_tex);
            gles2::TexParameteri(
                gles2::TEXTURE_2D,
                gles2::TEXTURE_MIN_FILTER,
                gles2::NEAREST as GLint,
            );
            gles2::TexParameteri(
                gles2::TEXTURE_2D,
                gles2::TEXTURE_MAG_FILTER,
                gles2::NEAREST as GLint,
            );
            gles2::TexParameteri(gles2::TEXTURE_2D, gles2::TEXTURE_WRAP_S, gles2::CLAMP_TO_EDGE as GLint);
            gles2::TexParameteri(gles2::TEXTURE_2D, gles2::TEXTURE_WRAP_T, gles2::CLAMP_TO_EDGE as GLint);
            gles2::GenTextures(1, &mut osd_tex);
            gles2::BindTexture(gles2::TEXTURE_2D, osd_tex);
            gles2::TexParameteri(
                gles2::TEXTURE_2D,
                gles2::TEXTURE_MIN_FILTER,
                gles2::LINEAR as GLint,
            );
            gles2::TexParameteri(
                gles2::TEXTURE_2D,
                gles2::TEXTURE_MAG_FILTER,
                gles2::LINEAR as GLint,
            );
            gles2::TexParameteri(gles2::TEXTURE_2D, gles2::TEXTURE_WRAP_S, gles2::CLAMP_TO_EDGE as GLint);
            gles2::TexParameteri(gles2::TEXTURE_2D, gles2::TEXTURE_WRAP_T, gles2::CLAMP_TO_EDGE as GLint);
            gles2::UseProgram(program);
            gles2::Uniform1i(u_tex_loc, 0);
            gles2::UseProgram(osd_program);
            gles2::Uniform1i(osd_u_tex_loc, 0);
            gles2::ActiveTexture(gles2::TEXTURE0);
            check_gl_error("init");
        }

        let renderer = GpuRenderer {
            egl,
            display,
            surface,
            context,
            egl_window,
            program,
            osd_program,
            vao,
            vbo,
            frame_tex,
            osd_tex,
            a_pos_loc,
            u_src_loc,
            osd_u_src_loc,
            width: width * RENDER_SCALE,
            height: height * RENDER_SCALE,
        };

        tracing::info!(
            "GPU renderer initialized ({}x{} logical, EGL {})",
            width,
            height,
            renderer
                .egl
                .query_string(Some(renderer.display), egl::VERSION)
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "?".into())
        );
        Ok(renderer)
    }

    fn build_program(vertex_shader: &str) -> anyhow::Result<GLuint> {
        unsafe {
            let vs = gles2::CreateShader(gles2::VERTEX_SHADER);
            let vs_src = vertex_shader.as_ptr() as *const GLchar;
            let vs_len = vertex_shader.len() as GLint;
            gles2::ShaderSource(vs, 1, &vs_src, &vs_len);
            gles2::CompileShader(vs);
            let mut ok: GLint = 0;
            gles2::GetShaderiv(vs, gles2::COMPILE_STATUS, &mut ok);
            if ok == gles2::FALSE as GLint {
                let mut len: GLint = 0;
                gles2::GetShaderiv(vs, gles2::INFO_LOG_LENGTH, &mut len);
                let mut log = vec![0u8; len.max(1) as usize];
                gles2::GetShaderInfoLog(vs, len, ptr::null_mut(), log.as_mut_ptr() as *mut GLchar);
                anyhow::bail!("Vertex shader compile error: {}", String::from_utf8_lossy(&log));
            }

            let fs = gles2::CreateShader(gles2::FRAGMENT_SHADER);
            let fs_src = FRAGMENT_SHADER.as_ptr() as *const GLchar;
            let fs_len = FRAGMENT_SHADER.len() as GLint;
            gles2::ShaderSource(fs, 1, &fs_src, &fs_len);
            gles2::CompileShader(fs);
            let mut ok: GLint = 0;
            gles2::GetShaderiv(fs, gles2::COMPILE_STATUS, &mut ok);
            if ok == gles2::FALSE as GLint {
                let mut len: GLint = 0;
                gles2::GetShaderiv(fs, gles2::INFO_LOG_LENGTH, &mut len);
                let mut log = vec![0u8; len.max(1) as usize];
                gles2::GetShaderInfoLog(fs, len, ptr::null_mut(), log.as_mut_ptr() as *mut GLchar);
                anyhow::bail!("Fragment shader compile error: {}", String::from_utf8_lossy(&log));
            }

            let program = gles2::CreateProgram();
            gles2::AttachShader(program, vs);
            gles2::AttachShader(program, fs);
            gles2::LinkProgram(program);
            let mut ok: GLint = 0;
            gles2::GetProgramiv(program, gles2::LINK_STATUS, &mut ok);
            if ok == gles2::FALSE as GLint {
                let mut len: GLint = 0;
                gles2::GetProgramiv(program, gles2::INFO_LOG_LENGTH, &mut len);
                let mut log = vec![0u8; len.max(1) as usize];
                gles2::GetProgramInfoLog(
                    program,
                    len,
                    ptr::null_mut(),
                    log.as_mut_ptr() as *mut GLchar,
                );
                anyhow::bail!("Program link error: {}", String::from_utf8_lossy(&log));
            }
            gles2::DeleteShader(vs);
            gles2::DeleteShader(fs);
            Ok(program)
        }
    }

    pub fn resize(&mut self, width: i32, height: i32) {
        let width = width * RENDER_SCALE;
        let height = height * RENDER_SCALE;
        if width == self.width && height == self.height {
            return;
        }
        self.width = width;
        self.height = height;
        self.egl_window.resize(width, height, 0, 0);
        unsafe {
            gles2::Viewport(0, 0, width, height);
        }
    }

    /// Upload the captured frame as a texture. Should be called whenever a new
    /// capture is ready (typically once).
    pub fn upload_frame(&mut self, frame: &RgbaBuffer) {
        unsafe {
            gles2::BindTexture(gles2::TEXTURE_2D, self.frame_tex);
            gles2::TexImage2D(
                gles2::TEXTURE_2D,
                0,
                gles2::RGBA as GLint,
                frame.width,
                frame.height,
                0,
                gles2::RGBA,
                gles2::UNSIGNED_BYTE,
                frame.data.as_ptr() as *const GLvoid,
            );
        }
    }

    /// Draw one frame: the magnified view (source rect in normalized texture
    /// coordinates) optionally overlaid with an OSD sprite, then present.
    ///
    /// `src` is `(x, y, w, h)` in texture space (0.0..1.0).
    pub fn draw(&mut self, src: Option<(f64, f64, f64, f64)>, osd: Option<&OsdSprite>) {
        unsafe {
            gles2::Viewport(0, 0, self.width, self.height);
            gles2::ClearColor(0.0, 0.0, 0.0, 1.0);
            gles2::Clear(gles2::COLOR_BUFFER_BIT);
            gles2::UseProgram(self.program);
            gles2::BindVertexArrayOES(self.vao);
            gles2::BindBuffer(gles2::ARRAY_BUFFER, self.vbo);
            gles2::EnableVertexAttribArray(self.a_pos_loc as GLuint);
            gles2::VertexAttribPointer(
                self.a_pos_loc as GLuint,
                2,
                gles2::FLOAT,
                gles2::FALSE,
                0,
                ptr::null(),
            );

            if let Some((x, y, w, h)) = src {
                gles2::ActiveTexture(gles2::TEXTURE0);
                gles2::BindTexture(gles2::TEXTURE_2D, self.frame_tex);
                let verts: [GLfloat; 12] = [
                    0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 1.0,
                ];
                gles2::BufferData(
                    gles2::ARRAY_BUFFER,
                    size_of_val(&verts) as GLsizeiptr,
                    verts.as_ptr() as *const GLvoid,
                    gles2::STREAM_DRAW,
                );
                gles2::Uniform4f(
                    self.u_src_loc,
                    x as GLfloat,
                    y as GLfloat,
                    w as GLfloat,
                    h as GLfloat,
                );
                gles2::DrawArrays(gles2::TRIANGLES, 0, 6);
            }

            if let Some(sprite) = osd {
                gles2::Enable(gles2::BLEND);
                gles2::BlendFunc(gles2::SRC_ALPHA, gles2::ONE_MINUS_SRC_ALPHA);
                gles2::ActiveTexture(gles2::TEXTURE0);
                gles2::BindTexture(gles2::TEXTURE_2D, self.osd_tex);
                gles2::UseProgram(self.osd_program);
                gles2::TexImage2D(
                    gles2::TEXTURE_2D,
                    0,
                    gles2::RGBA as GLint,
                    sprite.width,
                    sprite.height,
                    0,
                    gles2::RGBA,
                    gles2::UNSIGNED_BYTE,
                    sprite.buffer.data.as_ptr() as *const GLvoid,
                );
                let x0 = sprite.x as GLfloat / self.width as GLfloat;
                let y0 = sprite.y as GLfloat / self.height as GLfloat;
                let x1 = (sprite.x + sprite.width) as GLfloat / self.width as GLfloat;
                let y1 = (sprite.y + sprite.height) as GLfloat / self.height as GLfloat;
                let verts: [GLfloat; 12] = [
                    x0, y0, x1, y0, x1, y1, x0, y0, x1, y1, x0, y1,
                ];
                gles2::BufferData(
                    gles2::ARRAY_BUFFER,
                    size_of_val(&verts) as GLsizeiptr,
                    verts.as_ptr() as *const GLvoid,
                    gles2::STREAM_DRAW,
                );
                gles2::Uniform4f(self.osd_u_src_loc, 0.0, 0.0, 1.0, 1.0);
                gles2::DrawArrays(gles2::TRIANGLES, 0, 6);
                gles2::Disable(gles2::BLEND);
                gles2::UseProgram(self.program);
            }

            check_gl_error("draw");
        }
        self.egl
            .swap_buffers(self.display, self.surface)
            .map_err(|e| anyhow::anyhow!("eglSwapBuffers failed: {e:?}"))
            .unwrap_or_else(|e| tracing::error!("{e:#}"));
    }
}

impl Drop for GpuRenderer {
    fn drop(&mut self) {
        unsafe {
            gles2::DeleteBuffers(1, &self.vbo);
            gles2::DeleteVertexArraysOES(1, &self.vao);
            gles2::DeleteTextures(1, &self.frame_tex);
            gles2::DeleteTextures(1, &self.osd_tex);
            gles2::DeleteProgram(self.program);
            gles2::DeleteProgram(self.osd_program);
        }
        let _ = self.egl.make_current(self.display, None, None, None);
        let _ = self.egl.destroy_context(self.display, self.context);
        let _ = self.egl.destroy_surface(self.display, self.surface);
        let _ = self.egl.terminate(self.display);
    }
}

fn get_attrib_location(program: GLuint, name: *const GLchar) -> GLint {
    unsafe { gles2::GetAttribLocation(program, name) }
}

fn get_uniform_location(program: GLuint, name: *const GLchar) -> GLint {
    unsafe { gles2::GetUniformLocation(program, name) }
}

fn check_gl_error(stage: &str) {
    loop {
        let err = unsafe { gles2::GetError() };
        if err == gles2::NO_ERROR {
            break;
        }
        tracing::warn!("GL error 0x{err:04x} after {stage}");
    }
}
