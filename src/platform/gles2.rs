//! GLES2/EGL backend for Wayland.
//!
//! Handles EGL display/context/surface creation and buffer presentation.
//! This is the platform-specific piece that `gpu.rs` delegates to — the
//! actual GL rendering code (shaders, textures, draw calls) stays in
//! `gpu.rs` because it maps 1:1 to wgpu for cross-platform rendering.

use std::os::raw::c_void;

use khronos_egl as egl;

use super::wayland;
use super::wayland::Proxy;

/// GLES2 rendering backend backed by EGL on Wayland.
///
/// Manages the EGL lifecycle: display → config → context → surface → swap.
/// Also owns the `glow::Context` shared with the egui Configuration window.
pub struct Gles2Backend {
    pub(crate) egl: egl::DynamicInstance<egl::EGL1_4>,
    pub(crate) display: egl::Display,
    pub(crate) surface: egl::Surface,
    pub(crate) context: egl::Context,
    pub(crate) egl_window: wayland::WlEglSurface,
    glow: std::sync::Arc<glow::Context>,
}

impl Gles2Backend {
    /// Create a new GLES2 backend from a Wayland display and surface.
    ///
    /// # Safety
    /// `wl_display` must be a valid `wl_display*` pointer from the Wayland
    /// connection. `wl_surface` must be a live `wl_surface` object.
    pub unsafe fn new(
        wl_display: *mut c_void,
        wl_surface: &wayland::wl_surface::WlSurface,
        width: i32,
        height: i32,
        render_scale: i32,
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

        let egl_window = wayland::WlEglSurface::new(
            wl_surface.id(),
            width * render_scale,
            height * render_scale,
        )
        .map_err(|e| anyhow::anyhow!("Failed to create wl_egl_window: {e}"))?;
        let surface = unsafe {
            egl.create_window_surface(display, config, egl_window.ptr() as *mut c_void, None)
                .map_err(|e| anyhow::anyhow!("Failed to create EGL window surface: {e:?}"))?
        };

        egl.make_current(display, Some(surface), Some(surface), Some(context))
            .map_err(|e| anyhow::anyhow!("Failed to make EGL context current: {e:?}"))?;

        // Swap interval 0: never block in eglSwapBuffers waiting for buffer
        // release, so the event loop stays responsive while panning redraws.
        egl.swap_interval(display, 0)
            .map_err(|e| anyhow::anyhow!("Failed to set EGL swap interval: {e:?}"))?;

        // A glow context bound to the same EGL/GLES2 function pointers,
        // used by the egui-based Configuration window.
        let glow = unsafe {
            glow::Context::from_loader_function(|name: &str| {
                egl.get_proc_address(name)
                    .map(|f| f as *const c_void)
                    .unwrap_or(std::ptr::null())
            })
        };

        let backend = Self {
            egl,
            display,
            surface,
            context,
            egl_window,
            glow: std::sync::Arc::new(glow),
        };

        Ok(backend)
    }

    /// Query the EGL version string for logging.
    pub fn egl_version(&self) -> String {
        self.egl
            .query_string(Some(self.display), egl::VERSION)
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "?".into())
    }

    /// Load GL function pointers through the EGL proc address resolver.
    /// Must be called after context creation and before any GL calls.
    #[allow(dead_code)]
    pub fn load_gl_functions(&self, loader: impl FnMut(&str) -> *const c_void) {
        unsafe { crate::gpu::gles2::load_with(loader); }
    }

    /// Load GL function pointers through the EGL proc address resolver.
    /// Convenience method that uses the internal EGL resolver.
    pub fn load_gl(&self) {
        let egl = &self.egl;
        unsafe {
            crate::gpu::gles2::load_with(|name| {
                egl.get_proc_address(name)
                    .map(|f| f as *const c_void)
                    .unwrap_or(std::ptr::null())
            });
        }
    }

    /// Report the GL vendor and renderer strings.
    pub fn gl_info(&self) -> (String, String) {
        let gl_string = |name: u32| unsafe {
            let ptr = crate::gpu::gles2::GetString(name);
            if ptr.is_null() {
                return "(unknown)".to_string();
            }
            std::ffi::CStr::from_ptr(ptr as *const std::os::raw::c_char)
                .to_string_lossy()
                .into_owned()
        };
        (
            gl_string(crate::gpu::gles2::VENDOR),
            gl_string(crate::gpu::gles2::RENDERER),
        )
    }

    /// Present the current framebuffer to the compositor.
    pub fn swap_buffers(&self) {
        self.egl
            .swap_buffers(self.display, self.surface)
            .map_err(|e| anyhow::anyhow!("eglSwapBuffers failed: {e:?}"))
            .unwrap_or_else(|e| tracing::error!("{e:#}"));
    }

    /// Resize the EGL surface to match new dimensions (in RENDER_SCALE units).
    pub fn resize(&mut self, width: i32, height: i32) {
        self.egl_window.resize(width, height, 0, 0);
    }

    /// The `glow` context used by the egui Configuration window.
    pub fn glow_context(&self) -> std::sync::Arc<glow::Context> {
        self.glow.clone()
    }
}

impl Drop for Gles2Backend {
    fn drop(&mut self) {
        let _ = self.egl.make_current(self.display, None, None, None);
        let _ = self.egl.destroy_context(self.display, self.context);
        let _ = self.egl.destroy_surface(self.display, self.surface);
        let _ = self.egl.terminate(self.display);
    }
}
