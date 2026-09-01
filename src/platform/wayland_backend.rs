//! Wayland implementations of the platform traits.
//!
//! Provides `CaptureBackend` (via `zwlr_screencopy`) and `CursorBackend`
//! (via the `xcursor` crate) for the Wayland platform.

use super::{CaptureBackend, CapturedFrame, CursorBackend, OutputInfo};
use crate::render::RgbaBuffer;

// ── CaptureBackend ──────────────────────────────────────────────────────

/// Wayland screen capture via `zwlr_screencopy`.
///
/// This backend captures the primary output using the wlr-screencopy
/// protocol. The capture is asynchronous — the actual frame delivery
/// happens through Wayland events in the engine's event loop.
///
/// For the initial implementation, this backend is a placeholder:
/// the actual screencopy logic remains in `engine.rs` (in the
/// `ScreencastFrameData` event handler) because it is tightly coupled
/// to the Wayland event dispatch model. A full extraction would require
/// the `CaptureBackend` trait to support async capture, which is a
/// future Phase 3 concern.
pub struct WaylandCaptureBackend {
    // Placeholder — actual screencopy state lives in MagnifierWindow.
}

impl WaylandCaptureBackend {
    pub fn new() -> Self {
        Self {}
    }
}

impl CaptureBackend for WaylandCaptureBackend {
    fn capture_primary(&mut self) -> anyhow::Result<CapturedFrame> {
        // The actual capture is handled asynchronously via zwlr_screencopy
        // events in engine.rs. This method is a synchronous interface that
        // will be used by the winit-based cross-platform backend. On Wayland,
        // the capture flow is event-driven and cannot be made synchronous
        // without blocking the event loop.
        anyhow::bail!(
            "Wayland capture is asynchronous — use the event-driven path in engine.rs"
        )
    }

    fn available_outputs(&self) -> Vec<OutputInfo> {
        // Output enumeration is handled by smithay-client-toolkit's
        // OutputState in engine.rs. A full implementation would query
        // the bound outputs from OutputState.
        vec![]
    }
}

// ── CursorBackend ───────────────────────────────────────────────────────

/// Wayland cursor theme loading via the `xcursor` crate.
///
/// Loads cursor bitmaps from the standard XCursor theme search paths
/// (`$XCURSOR_THEME`, `~/.icons/`, etc.). This is the same logic that
/// currently lives in `src/cursor.rs::load_system_cursor()`.
pub struct WaylandCursorBackend;

impl WaylandCursorBackend {
    pub fn new() -> Self {
        Self
    }
}

impl CursorBackend for WaylandCursorBackend {
    fn load_cursor(&mut self, name: &str, size: u32) -> Option<(RgbaBuffer, (f64, f64))> {
        // Use the XCURSOR_THEME env var (or "default") as the theme,
        // falling back to the standard search paths.
        let theme = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".to_string());
        crate::cursor::load_cursor(&theme, name, size)
    }
}
