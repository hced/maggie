#![allow(dead_code)]

use std::ptr;

use crate::osd::OsdSprite;
use crate::render::RgbaBuffer;
use crate::platform::gles2::Gles2Backend;

/// A magnified-cursor sprite ready to draw: its screen position, the sprite
/// buffer, and the hotspot offset inside the sprite (in sprite pixels).
pub type CursorSprite<'a> = &'a ((i32, i32), RgbaBuffer, (f64, f64));

pub(crate) mod gles2 {
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

/// Vertex shader for sprite passes (magnified cursor, OSD legend). The quad
/// covers the sprite rect given by `u_rect` in normalized screen coordinates
/// (y down); `a_pos` maps the whole texture into the quad so the full sprite
/// is sampled inside its rect, upright. The NDC y-flip is applied explicitly
/// here, so this pass is independent of the frame texture's flip handling.
const SPRITE_VERTEX_SHADER: &str = r#"
attribute vec2 a_pos;
uniform vec4 u_rect;
varying vec2 v_uv;
void main() {
    vec2 screen = u_rect.xy + a_pos * u_rect.zw;
    vec2 ndc = vec2(screen.x * 2.0 - 1.0, 1.0 - screen.y * 2.0);
    v_uv = a_pos;
    gl_Position = vec4(ndc, 0.0, 1.0);
}
"#;

/// Samples the texture, optionally painting texels outside the [0,1] UV
/// square black (`u_oob_black`) instead of letting the sampler clamp them
/// (edge-stretch). Used for the hold-to-zoom `Extend` edge mode: when the
/// anchored view reaches past the frozen capture, the region beyond the frame
/// is either black (this branch) or stretched edge pixels (CLAMP_TO_EDGE).
const FRAGMENT_SHADER: &str = r#"
// The OOB comparison below must be exact: at the right/bottom walls the
// beyond-capture boundary lands exactly at the viewport center (under the
// magnified cursor's hotspot), and mediump rounding of the boundary fragment
// to exactly 1.0 used to make the last capture texel bleed one buffer column
// (half a logical pixel) past the boundary — a visible "excess" between the
// end of the magnified pixels and the cursor tip. Use highp where available
// (f32; guaranteed exact here) and treat exactly-1.0 as out-of-bounds so
// precision can never stretch content past the boundary.
#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif
varying vec2 v_uv;
uniform sampler2D u_tex;
uniform float u_oob_black;
void main() {
    if (u_oob_black > 0.5 && (v_uv.x < 0.0 || v_uv.x >= 1.0 || v_uv.y < 0.0 || v_uv.y >= 1.0)) {
        gl_FragColor = vec4(0.0, 0.0, 0.0, 1.0);
    } else {
        gl_FragColor = texture2D(u_tex, v_uv);
    }
}
"#;

/// Fragment shader for the annotation overlay pass. Applies `u_uv_offset`
/// to shift the texture UV, enabling pan-without-rerender: the annotation
/// buffer is rendered once and shifted on the GPU when the view pans.
const OVERLAY_FRAGMENT_SHADER: &str = r#"
#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif
varying vec2 v_uv;
uniform sampler2D u_tex;
uniform vec2 u_uv_offset;
void main() {
    vec2 uv = v_uv + u_uv_offset;
    if (uv.x < 0.0 || uv.x >= 1.0 || uv.y < 0.0 || uv.y >= 1.0) {
        gl_FragColor = vec4(0.0, 0.0, 0.0, 0.0);
    } else {
        gl_FragColor = texture2D(u_tex, uv);
    }
}
"#;

/// Fragment shader for the inverted-color annotation cursor. Outputs
/// luminance (white on crosshair arms, black elsewhere). The blend mode
/// `ONE_MINUS_DST_COLOR / ONE_MINUS_SRC_COLOR` handles the actual
/// inversion: white pixels invert the destination, black pixels pass
/// through unchanged. This mirrors HouseLordPaint's diff-blend crosshair.
const INVERT_CURSOR_FRAGMENT_SHADER: &str = r#"
#extension GL_OES_standard_derivatives : enable
#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif
varying vec2 v_uv;
uniform float u_size;   // crosshair bounding-box size in pixels (N)
void main() {
    // Center origin: 0.5 = center of quad, range is -0.5..0.5.
    vec2 c = v_uv - 0.5;
    float half_sz = u_size * 0.5;
    // Anti-aliasing width: ~1 sub-pixel.
    vec2 dx = dFdx(c);
    vec2 dy = dFdy(c);
    float aa = length(dx) + length(dy);
    // Crosshair arm half-thickness and gap (empty center).
    float hw = u_size * 0.04;   // arm half-thickness (8% of size)
    float gap = u_size * 0.08;  // center gap radius
    // Horizontal arm: |y| near 0, |x| in [gap, 0.5].
    float y_in  = 1.0 - smoothstep(hw - aa, hw + aa, abs(c.y));
    float x_arm = smoothstep(gap - aa, gap + aa, abs(c.x))
                * (1.0 - smoothstep(0.5 - aa, 0.5 + aa, abs(c.x)));
    float horizontal = y_in * x_arm;
    // Vertical arm: |x| near 0, |y| in [gap, 0.5].
    float x_in  = 1.0 - smoothstep(hw - aa, hw + aa, abs(c.x));
    float y_arm = smoothstep(gap - aa, gap + aa, abs(c.y))
                * (1.0 - smoothstep(0.5 - aa, 0.5 + aa, abs(c.y)));
    float vertical = x_in * y_arm;
    float cov = clamp(horizontal + vertical, 0.0, 1.0);
    // Output luminance: white on arms, black elsewhere.
    // Blend func ONE_MINUS_DST_COLOR inverts the destination under arms.
    gl_FragColor = vec4(vec3(cov), 1.0);
}
"#;

/// The GPU path renders its buffers at this multiple of the layer's logical
/// size and advertises the same value via `wl_surface.set_buffer_scale`, so the
/// compositor's fractional-scale resampling doesn't soften the magnified
/// pixels. With `GL_NEAREST` magnification this yields the crisp, pixelated
/// magnifier look when zooming in; the min filter is `GL_LINEAR` so zooming out
/// below 1x downscales the frozen frame smoothly instead of aliasing.
pub const RENDER_SCALE: i32 = 2;

/// GPU-accelerated renderer backed by EGL + OpenGL ES 2.
///
/// Renders the captured frame as a textured quad (nearest-neighbor
/// magnification into a 2x buffer-scale surface for a crisp, pixelated
/// magnifier look when zooming in; a `GL_LINEAR` min filter smooths the
/// downscaled view when zooming out below 1x) and draws the OSD as a second
/// alpha-blended quad. All scaling, panning and easing happen on the GPU.
pub struct GpuRenderer {
    /// `glow` context over the same EGL/GLES2 function pointers, for the
    /// egui-based Configuration window.
    glow: std::sync::Arc<glow::Context>,
    program: GLuint,
    sprite_program: GLuint,
    overlay_program: GLuint,
    invert_program: GLuint,
    u_crosshair_size_loc: GLint,
    vao: GLuint,
    vbo: GLuint,
    frame_tex: GLuint,
    osd_tex: GLuint,
    cursor_tex: GLuint,
    overlay_tex: GLuint,
    minimap_tex: GLuint,
    a_pos_loc: GLint,
    u_src_loc: GLint,
    u_rect_loc: GLint,
    u_oob_black_loc: GLint,
    u_overlay_uv_offset_loc: GLint,
    width: i32,
    height: i32,
}

impl GpuRenderer {
    /// Initialize the GPU renderer using an already-created backend.
    ///
    /// The backend handles EGL/Wayland setup; this method compiles shaders,
    /// creates GL state (VAO, VBO, textures), and returns the renderer.
    pub fn init(
        backend: &Gles2Backend,
        width: i32,
        height: i32,
    ) -> anyhow::Result<Self> {
        // Load GL function pointers through the EGL resolver.
        backend.load_gl();

        // Report which GPU is actually rendering.
        let (vendor, renderer) = backend.gl_info();
        tracing::info!("EGL GPU: {vendor} — {renderer}");

        let glow = backend.glow_context();

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
        let u_oob_black_loc = get_uniform_location(program, c"u_oob_black".as_ptr());
        if u_oob_black_loc < 0 {
            anyhow::bail!("Shader uniform u_oob_black not found");
        }
        let sprite_program = Self::build_program(SPRITE_VERTEX_SHADER)?;
        let u_rect_loc = get_uniform_location(sprite_program, c"u_rect".as_ptr());
        let sprite_u_tex_loc = get_uniform_location(sprite_program, c"u_tex".as_ptr());
        if u_rect_loc < 0 || sprite_u_tex_loc < 0 {
            anyhow::bail!(
                "Sprite shader locations not found (u_rect={}, u_tex={})",
                u_rect_loc,
                sprite_u_tex_loc
            );
        }

        // Overlay program: same vertex shader as sprite, but fragment shader
        // applies u_uv_offset for pan-without-rerender on annotations.
        let overlay_program = Self::build_program_with(
            SPRITE_VERTEX_SHADER,
            OVERLAY_FRAGMENT_SHADER,
        )?;
        let u_overlay_uv_offset_loc = get_uniform_location(
            overlay_program,
            c"u_uv_offset".as_ptr(),
        );
        if u_overlay_uv_offset_loc < 0 {
            anyhow::bail!("Overlay shader uniform u_uv_offset not found");
        }
        let overlay_u_rect_loc = get_uniform_location(
            overlay_program,
            c"u_rect".as_ptr(),
        );
        let overlay_u_tex_loc = get_uniform_location(
            overlay_program,
            c"u_tex".as_ptr(),
        );
        if overlay_u_rect_loc < 0 || overlay_u_tex_loc < 0 {
            anyhow::bail!(
                "Overlay shader locations not found (u_rect={}, u_tex={})",
                overlay_u_rect_loc,
                overlay_u_tex_loc
            );
        }

        // Inverted-color cursor program: draws a crosshair shape as
        // luminance (white on arms, black elsewhere). The blend mode
        // ONE_MINUS_DST_COLOR inverts the destination under the arms.
        let invert_program = Self::build_program_with(
            SPRITE_VERTEX_SHADER,
            INVERT_CURSOR_FRAGMENT_SHADER,
        )?;
        let u_crosshair_size_loc = get_uniform_location(
            invert_program,
            c"u_size".as_ptr(),
        );
        if u_crosshair_size_loc < 0 {
            anyhow::bail!("Crosshair cursor shader uniform u_size not found");
        }

        let mut vao = 0;
        let mut vbo = 0;
        let mut frame_tex = 0;
        let mut osd_tex = 0;
        let mut cursor_tex = 0;
        let mut overlay_tex = 0;
        let mut minimap_tex = 0;
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
            // Min filter is LINEAR so zooming *out* below 1x (e.g. the
            // fully-zoomed-out 0.67x view) downscales smoothly instead of
            // aliasing (NEAREST made text look degraded at small zooms); the
            // mag filter stays NEAREST so zooming *in* keeps the crisp,
            // pixelated magnifier look.
            gles2::TexParameteri(
                gles2::TEXTURE_2D,
                gles2::TEXTURE_MIN_FILTER,
                gles2::LINEAR as GLint,
            );
            gles2::TexParameteri(
                gles2::TEXTURE_2D,
                gles2::TEXTURE_MAG_FILTER,
                gles2::NEAREST as GLint,
            );
            gles2::TexParameteri(
                gles2::TEXTURE_2D,
                gles2::TEXTURE_WRAP_S,
                gles2::CLAMP_TO_EDGE as GLint,
            );
            gles2::TexParameteri(
                gles2::TEXTURE_2D,
                gles2::TEXTURE_WRAP_T,
                gles2::CLAMP_TO_EDGE as GLint,
            );
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
            gles2::TexParameteri(
                gles2::TEXTURE_2D,
                gles2::TEXTURE_WRAP_S,
                gles2::CLAMP_TO_EDGE as GLint,
            );
            gles2::TexParameteri(
                gles2::TEXTURE_2D,
                gles2::TEXTURE_WRAP_T,
                gles2::CLAMP_TO_EDGE as GLint,
            );
            gles2::GenTextures(1, &mut cursor_tex);
            gles2::BindTexture(gles2::TEXTURE_2D, cursor_tex);
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
            gles2::TexParameteri(
                gles2::TEXTURE_2D,
                gles2::TEXTURE_WRAP_S,
                gles2::CLAMP_TO_EDGE as GLint,
            );
            gles2::TexParameteri(
                gles2::TEXTURE_2D,
                gles2::TEXTURE_WRAP_T,
                gles2::CLAMP_TO_EDGE as GLint,
            );
            // Minimap texture: a small dimmed overview of the frozen screen
            // (with a visible-region marker), uploaded per frame while the
            // minimap is visible. LINEAR filtering so the logical-resolution
            // buffer upscales smoothly to the RENDER_SCALE surface.
            gles2::GenTextures(1, &mut minimap_tex);
            gles2::BindTexture(gles2::TEXTURE_2D, minimap_tex);
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
            gles2::TexParameteri(
                gles2::TEXTURE_2D,
                gles2::TEXTURE_WRAP_S,
                gles2::CLAMP_TO_EDGE as GLint,
            );
            gles2::TexParameteri(
                gles2::TEXTURE_2D,
                gles2::TEXTURE_WRAP_T,
                gles2::CLAMP_TO_EDGE as GLint,
            );
            // Screenshot-mode overlay texture (selection scrim + border),
            // uploaded per frame only while Screenshot Mode is active.
            gles2::GenTextures(1, &mut overlay_tex);
            gles2::BindTexture(gles2::TEXTURE_2D, overlay_tex);
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
            gles2::TexParameteri(
                gles2::TEXTURE_2D,
                gles2::TEXTURE_WRAP_S,
                gles2::CLAMP_TO_EDGE as GLint,
            );
            gles2::TexParameteri(
                gles2::TEXTURE_2D,
                gles2::TEXTURE_WRAP_T,
                gles2::CLAMP_TO_EDGE as GLint,
            );
            gles2::UseProgram(program);
            gles2::Uniform1i(u_tex_loc, 0);
            gles2::UseProgram(sprite_program);
            gles2::Uniform1i(sprite_u_tex_loc, 0);
            gles2::ActiveTexture(gles2::TEXTURE0);
            check_gl_error("init");
        }

        let renderer = GpuRenderer {
            glow,
            program,
            sprite_program,
            overlay_program,
            invert_program,
            u_crosshair_size_loc,
            vao,
            vbo,
            frame_tex,
            osd_tex,
            cursor_tex,
            overlay_tex,
            minimap_tex,
            a_pos_loc,
            u_src_loc,
            u_rect_loc,
            u_oob_black_loc,
            u_overlay_uv_offset_loc,
            width: width * RENDER_SCALE,
            height: height * RENDER_SCALE,
        };

        tracing::info!(
            "GPU renderer initialized ({}x{} logical, EGL {})",
            width,
            height,
            backend.egl_version(),
        );
        Ok(renderer)
    }

    fn build_program(vertex_shader: &str) -> anyhow::Result<GLuint> {
        Self::build_program_with(vertex_shader, FRAGMENT_SHADER)
    }

    fn build_program_with(vertex_shader: &str, fragment_shader: &str) -> anyhow::Result<GLuint> {
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
                anyhow::bail!(
                    "Vertex shader compile error: {}",
                    String::from_utf8_lossy(&log)
                );
            }

            let fs = gles2::CreateShader(gles2::FRAGMENT_SHADER);
            let fs_src = fragment_shader.as_ptr() as *const GLchar;
            let fs_len = fragment_shader.len() as GLint;
            gles2::ShaderSource(fs, 1, &fs_src, &fs_len);
            gles2::CompileShader(fs);
            let mut ok: GLint = 0;
            gles2::GetShaderiv(fs, gles2::COMPILE_STATUS, &mut ok);
            if ok == gles2::FALSE as GLint {
                let mut len: GLint = 0;
                gles2::GetShaderiv(fs, gles2::INFO_LOG_LENGTH, &mut len);
                let mut log = vec![0u8; len.max(1) as usize];
                gles2::GetShaderInfoLog(fs, len, ptr::null_mut(), log.as_mut_ptr() as *mut GLchar);
                anyhow::bail!(
                    "Fragment shader compile error: {}",
                    String::from_utf8_lossy(&log)
                );
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

    /// The `glow` context used by the egui Configuration window.
    pub fn glow(&self) -> std::sync::Arc<glow::Context> {
        self.glow.clone()
    }

    /// Present the current framebuffer to the compositor. Delegates to the
    /// backend (which owns the EGL surface).
    pub fn swap_buffers(&self, backend: &Gles2Backend) {
        backend.swap_buffers();
    }

    pub fn resize(&mut self, backend: &mut Gles2Backend, width: i32, height: i32) {
        let width = width * RENDER_SCALE;
        let height = height * RENDER_SCALE;
        if width == self.width && height == self.height {
            return;
        }
        self.width = width;
        self.height = height;
        backend.resize(width, height);
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
    /// coordinates) optionally overlaid with the screenshot selection overlay
    /// (a fullscreen scrim + border sprite), an OSD sprite, the magnified
    /// cursor and the minimap sprite, then present.
    ///
    /// `src` is `(x, y, w, h)` in texture space (0.0..1.0) — it may extend
    /// outside that square when the view reaches past the capture edge (the
    /// magnified cursor always sits at the viewport center, so near the
    /// screen edges the view samples past the frozen frame; those texels are
    /// painted black by the shader). `overlay` is a full-buffer RGBA sprite
    /// drawn (blended) over the frame and under the cursor/OSD; only used in
    /// Screenshot Mode. `upload_overlay` controls whether the overlay pixels
    /// are re-uploaded to the texture this frame: the engine only rebuilds
    /// the overlay when the selection changed and passes `false` otherwise,
    /// so plain pointer motion in Screenshot Mode never re-uploads the
    /// full-screen buffer (which made the mouse laggy). `minimap` is a
    /// sprite drawn last (on top of everything) at its `x, y, width, height`
    /// rect, which are in RENDER_SCALE surface coordinates; the texture
    /// buffer itself stays at logical resolution and is LINEAR-upscaled.
    pub fn draw(
        &mut self,
        src: Option<(f64, f64, f64, f64)>,
        osd: Option<&OsdSprite>,
        hint: Option<&OsdSprite>,
        cursor: Option<CursorSprite>,
        overlay: Option<&RgbaBuffer>,
        upload_overlay: bool,
        upload_cursor: bool,
        minimap: Option<&OsdSprite>,
        toolbar: Option<&OsdSprite>,
        overlay_uv_offset: (f32, f32),
        invert_cursor: bool,
        invert_cursor_center: (f32, f32), // center in RENDER_SCALE surface px
        crosshair_size: f32,               // crosshair bounding-box size in surface px
    ) {
        unsafe {
            gles2::Viewport(0, 0, self.width, self.height);
            // The egui Configuration window (egui-glow) leaves SCISSOR_TEST
            // enabled with a stale rect and its own VAO bound; re-establish a
            // clean base state so the first frame after closing it renders
            // unclipped. The VAO/VBO are re-bound below.
            gles2::Disable(gles2::SCISSOR_TEST);
            gles2::ClearColor(0.0, 0.0, 0.0, 1.0);
            gles2::Clear(gles2::COLOR_BUFFER_BIT);
            gles2::UseProgram(self.program);
            gles2::Uniform1f(self.u_oob_black_loc, 1.0);
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
                let verts: [GLfloat; 12] =
                    [0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 1.0];
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

            // Screenshot-mode overlay: a fullscreen sprite with the selection
            // scrim + colored border, alpha-blended over the frame (below the
            // cursor and OSD legend). The texture is re-uploaded only when
            // the overlay content changed (`upload_overlay`); otherwise the
            // previous upload is reused — cheap per-frame pointer motion.
            if let Some(overlay) = overlay {
                gles2::Enable(gles2::BLEND);
                gles2::BlendFunc(gles2::SRC_ALPHA, gles2::ONE_MINUS_SRC_ALPHA);
                gles2::ActiveTexture(gles2::TEXTURE0);
                gles2::BindTexture(gles2::TEXTURE_2D, self.overlay_tex);
                // Use the overlay program with UV offset for
                // pan-without-rerender on annotation overlays.
                gles2::UseProgram(self.overlay_program);
                gles2::Uniform2f(
                    self.u_overlay_uv_offset_loc,
                    overlay_uv_offset.0,
                    overlay_uv_offset.1,
                );
                if upload_overlay {
                    gles2::TexImage2D(
                        gles2::TEXTURE_2D,
                        0,
                        gles2::RGBA as GLint,
                        overlay.width,
                        overlay.height,
                        0,
                        gles2::RGBA,
                        gles2::UNSIGNED_BYTE,
                        overlay.data.as_ptr() as *const GLvoid,
                    );
                }
                let verts: [GLfloat; 12] =
                    [0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 1.0];
                gles2::BufferData(
                    gles2::ARRAY_BUFFER,
                    size_of_val(&verts) as GLsizeiptr,
                    verts.as_ptr() as *const GLvoid,
                    gles2::STREAM_DRAW,
                );
                // The overlay program uses the same vertex shader as sprite,
                // so u_rect is at the same location — look it up from the
                // overlay program.
                let overlay_u_rect = get_uniform_location(
                    self.overlay_program,
                    c"u_rect".as_ptr(),
                );
                gles2::Uniform4f(overlay_u_rect, 0.0, 0.0, 1.0, 1.0);
                gles2::DrawArrays(gles2::TRIANGLES, 0, 6);
            }

            // Draw Mode toolbar sprite — rendered before cursor so cursor
            // appears on top of the pie menu buttons.
            if let Some(sprite) = toolbar {
                gles2::Enable(gles2::BLEND);
                gles2::BlendFunc(gles2::SRC_ALPHA, gles2::ONE_MINUS_SRC_ALPHA);
                gles2::ActiveTexture(gles2::TEXTURE0);
                gles2::BindTexture(gles2::TEXTURE_2D, self.osd_tex);
                gles2::UseProgram(self.sprite_program);
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
                let verts: [GLfloat; 12] =
                    [0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 1.0];
                gles2::BufferData(
                    gles2::ARRAY_BUFFER,
                    size_of_val(&verts) as GLsizeiptr,
                    verts.as_ptr() as *const GLvoid,
                    gles2::STREAM_DRAW,
                );
                gles2::Uniform4f(self.u_rect_loc, x0, y0, x1 - x0, y1 - y0);
                gles2::DrawArrays(gles2::TRIANGLES, 0, 6);
                // Inversion mask for center label text: white pixels
                // invert the frame underneath (ONE_MINUS_DST_COLOR blend).
                if let Some(outline) = &sprite.outline {
                    gles2::TexImage2D(gles2::TEXTURE_2D, 0, gles2::RGBA as GLint, outline.width, outline.height, 0, gles2::RGBA, gles2::UNSIGNED_BYTE, outline.data.as_ptr() as *const GLvoid);
                    gles2::BlendFuncSeparate(
                        gles2::ONE_MINUS_DST_COLOR, gles2::ONE_MINUS_SRC_ALPHA,
                        gles2::ZERO, gles2::ONE,
                    );
                    gles2::DrawArrays(gles2::TRIANGLES, 0, 6);
                }
                gles2::Disable(gles2::BLEND);
            }

            if let Some((pos, cursor_buf, (_hx, _hy))) = cursor {
                gles2::Enable(gles2::BLEND);
                gles2::BlendFunc(gles2::SRC_ALPHA, gles2::ONE_MINUS_SRC_ALPHA);
                gles2::ActiveTexture(gles2::TEXTURE0);
                gles2::BindTexture(gles2::TEXTURE_2D, self.cursor_tex);
                gles2::UseProgram(self.sprite_program);
                if upload_cursor {
                    gles2::TexImage2D(
                        gles2::TEXTURE_2D,
                        0,
                        gles2::RGBA as GLint,
                        cursor_buf.width,
                        cursor_buf.height,
                        0,
                        gles2::RGBA,
                        gles2::UNSIGNED_BYTE,
                        cursor_buf.data.as_ptr() as *const GLvoid,
                    );
                }
                let x0 = pos.0 as f32 / self.width as f32;
                let y0 = pos.1 as f32 / self.height as f32;
                let x1 = (pos.0 as f32 + cursor_buf.width as f32) / self.width as f32;
                let y1 = (pos.1 as f32 + cursor_buf.height as f32) / self.height as f32;
                let verts: [GLfloat; 12] =
                    [0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 1.0];
                gles2::BufferData(
                    gles2::ARRAY_BUFFER,
                    size_of_val(&verts) as GLsizeiptr,
                    verts.as_ptr() as *const GLvoid,
                    gles2::STREAM_DRAW,
                );
                gles2::Uniform4f(self.u_rect_loc, x0, y0, x1 - x0, y1 - y0);
                gles2::DrawArrays(gles2::TRIANGLES, 0, 6);
                gles2::Disable(gles2::BLEND);
            }

            if let Some(sprite) = osd {
                gles2::Enable(gles2::BLEND);
                gles2::BlendFunc(gles2::SRC_ALPHA, gles2::ONE_MINUS_SRC_ALPHA);
                gles2::ActiveTexture(gles2::TEXTURE0);
                gles2::BindTexture(gles2::TEXTURE_2D, self.osd_tex);
                gles2::UseProgram(self.sprite_program);
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
                let verts: [GLfloat; 12] =
                    [0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 1.0];
                gles2::BufferData(
                    gles2::ARRAY_BUFFER,
                    size_of_val(&verts) as GLsizeiptr,
                    verts.as_ptr() as *const GLvoid,
                    gles2::STREAM_DRAW,
                );
                gles2::Uniform4f(self.u_rect_loc, x0, y0, x1 - x0, y1 - y0);
                gles2::DrawArrays(gles2::TRIANGLES, 0, 6);
                gles2::Disable(gles2::BLEND);
            }

            // Launch hint sprite — a small, dim one-liner shown briefly on
            // startup so the user knows how to open the key legend.
            if let Some(sprite) = hint {
                gles2::Enable(gles2::BLEND);
                gles2::BlendFunc(gles2::SRC_ALPHA, gles2::ONE_MINUS_SRC_ALPHA);
                gles2::ActiveTexture(gles2::TEXTURE0);
                gles2::BindTexture(gles2::TEXTURE_2D, self.osd_tex);
                gles2::UseProgram(self.sprite_program);
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
                let verts: [GLfloat; 12] =
                    [0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 1.0];
                gles2::BufferData(
                    gles2::ARRAY_BUFFER,
                    size_of_val(&verts) as GLsizeiptr,
                    verts.as_ptr() as *const GLvoid,
                    gles2::STREAM_DRAW,
                );
                gles2::Uniform4f(self.u_rect_loc, x0, y0, x1 - x0, y1 - y0);
                gles2::DrawArrays(gles2::TRIANGLES, 0, 6);
                gles2::Disable(gles2::BLEND);
            }

            // Minimap sprite, drawn last so it always sits on top of the
            // frame, overlay and OSD. The texture buffer stays at logical
            // resolution (small, cheap to rebuild per frame); its rect spans
            // the RENDER_SCALE surface and the LINEAR filter upscales it
            // smoothly.
            if let Some(sprite) = minimap {
                gles2::Enable(gles2::BLEND);
                gles2::BlendFunc(gles2::SRC_ALPHA, gles2::ONE_MINUS_SRC_ALPHA);
                gles2::ActiveTexture(gles2::TEXTURE0);
                gles2::BindTexture(gles2::TEXTURE_2D, self.minimap_tex);
                gles2::UseProgram(self.sprite_program);
                gles2::TexImage2D(
                    gles2::TEXTURE_2D,
                    0,
                    gles2::RGBA as GLint,
                    sprite.buffer.width,
                    sprite.buffer.height,
                    0,
                    gles2::RGBA,
                    gles2::UNSIGNED_BYTE,
                    sprite.buffer.data.as_ptr() as *const GLvoid,
                );
                let x0 = sprite.x as GLfloat / self.width as GLfloat;
                let y0 = sprite.y as GLfloat / self.height as GLfloat;
                let x1 = (sprite.x + sprite.width) as GLfloat / self.width as GLfloat;
                let y1 = (sprite.y + sprite.height) as GLfloat / self.height as GLfloat;
                let verts: [GLfloat; 12] =
                    [0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 1.0];
                gles2::BufferData(
                    gles2::ARRAY_BUFFER,
                    size_of_val(&verts) as GLsizeiptr,
                    verts.as_ptr() as *const GLvoid,
                    gles2::STREAM_DRAW,
                );
                gles2::Uniform4f(self.u_rect_loc, x0, y0, x1 - x0, y1 - y0);
                gles2::DrawArrays(gles2::TRIANGLES, 0, 6);
                if let Some(outline) = &sprite.outline {
                    gles2::TexImage2D(gles2::TEXTURE_2D, 0, gles2::RGBA as GLint, outline.width, outline.height, 0, gles2::RGBA, gles2::UNSIGNED_BYTE, outline.data.as_ptr() as *const GLvoid);
                    // The outline texture already contains the animated
                    // gradient color and antialiased alpha. Composite it as a
                    // normal colored overlay; no destination-color inversion.
                    gles2::BlendFunc(gles2::SRC_ALPHA, gles2::ONE_MINUS_SRC_ALPHA);
                    gles2::DrawArrays(gles2::TRIANGLES, 0, 6);
                }
                gles2::Disable(gles2::BLEND);
            }

            // Inverted-color cursor pass: copy the current framebuffer
            // into a texture, then draw a fullscreen quad with the
            // inversion shader that inverts RGB within a circle.
            if invert_cursor {
                // Crosshair-shaped inversion: draw a small quad centered on
                // the cursor with the crosshair luminance shader. The blend
                // mode inverts destination pixels under the arms.
                gles2::Enable(gles2::BLEND);
                gles2::BlendEquation(gles2::FUNC_ADD);
                gles2::BlendFuncSeparate(
                    gles2::ONE_MINUS_DST_COLOR,
                    gles2::ONE_MINUS_SRC_COLOR,
                    gles2::ONE,
                    gles2::ZERO,
                );
                gles2::UseProgram(self.invert_program);
                gles2::Uniform1f(self.u_crosshair_size_loc, 1.0);
                // Center the crosshair quad on invert_cursor_center.
                let half = crosshair_size * 0.5;
                let cx = invert_cursor_center.0;
                let cy = invert_cursor_center.1;
                let x0 = (cx - half) / self.width as f32;
                let y0 = (cy - half) / self.height as f32;
                let w = crosshair_size / self.width as f32;
                let h = crosshair_size / self.height as f32;
                let verts: [GLfloat; 12] =
                    [0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 1.0];
                gles2::BufferData(
                    gles2::ARRAY_BUFFER,
                    size_of_val(&verts) as GLsizeiptr,
                    verts.as_ptr() as *const GLvoid,
                    gles2::STREAM_DRAW,
                );
                gles2::Uniform4f(self.u_rect_loc, x0, y0, w, h);
                gles2::DrawArrays(gles2::TRIANGLES, 0, 6);
                gles2::Disable(gles2::BLEND);
                gles2::UseProgram(self.program);
            }

            check_gl_error("draw");
        }
    }
}

impl Drop for GpuRenderer {
    fn drop(&mut self) {
        unsafe {
            gles2::DeleteBuffers(1, &self.vbo);
            gles2::DeleteVertexArraysOES(1, &self.vao);
            gles2::DeleteTextures(1, &self.frame_tex);
            gles2::DeleteTextures(1, &self.osd_tex);
            gles2::DeleteTextures(1, &self.cursor_tex);
            gles2::DeleteTextures(1, &self.minimap_tex);
            gles2::DeleteProgram(self.program);
            gles2::DeleteProgram(self.sprite_program);
            gles2::DeleteProgram(self.overlay_program);
            gles2::DeleteProgram(self.invert_program);
        }
        // EGL cleanup is handled by Gles2Backend::drop().
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
