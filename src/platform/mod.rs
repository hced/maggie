//! Platform abstraction layer.
//!
//! This module isolates all OS-specific code behind trait-based interfaces.
//! The core engine (`engine.rs`) remains platform-agnostic — it operates on
//! [`MagnifierState`] and delegates platform concerns to the active backend.
//!
//! Currently only the Wayland backend is implemented. Future backends
//! (winit + wgpu for Windows/macOS/X11) will implement the same traits.

pub mod wayland;
pub mod wayland_backend;
pub mod gles2;

use crate::render::RgbaBuffer;

/// A captured screen frame (RGBA, top-left origin).
pub struct CapturedFrame {
    pub buffer: RgbaBuffer,
}

/// Platform-agnostic screen capture interface.
///
/// Each platform provides its own implementation:
/// - Wayland: `zwlr_screencopy` via SHM
/// - Windows: DXGI Desktop Duplication
/// - macOS: ScreenCaptureKit
pub trait CaptureBackend {
    /// Capture the primary output (full screen).
    fn capture_primary(&mut self) -> anyhow::Result<CapturedFrame>;

    /// Capture a specific window by its platform-native ID.
    fn capture_window(&mut self, _window_id: u64) -> anyhow::Result<CapturedFrame> {
        anyhow::bail!("Window capture not supported on this platform")
    }

    /// Available display outputs.
    fn available_outputs(&self) -> Vec<OutputInfo> {
        vec![]
    }
}

/// Information about a display output.
#[derive(Debug, Clone)]
pub struct OutputInfo {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f64,
}

/// Platform-agnostic cursor theme loading.
pub trait CursorBackend {
    /// Load a cursor by theme name and size. Returns the RGBA bitmap and
    /// hotspot offset in pixel coordinates, or `None` if unavailable.
    fn load_cursor(&mut self, name: &str, size: u32) -> Option<(RgbaBuffer, (f64, f64))>;
}
