//! Maggie — cross-platform screen magnifier library root.
//!
//! This lib crate exposes shared modules so that both the native Wayland
//! binary (`src/main.rs`) and the cross-platform binary
//! (`src/bin/maggie_xp.rs`) can use them.

// Shared modules (always compiled)
pub mod config;
pub mod render;
pub mod osd;
#[cfg(feature = "wayland")]
pub mod cursor;

// Cross-platform engine (always compiled)
pub mod xp;

// Wayland-specific modules (only when wayland feature is enabled)
#[cfg(feature = "wayland")]
pub mod capture;
#[cfg(feature = "wayland")]
pub mod config_window;
#[cfg(feature = "wayland")]
pub mod draw_mode;
#[cfg(feature = "wayland")]
pub mod engine;
#[cfg(feature = "wayland")]
pub mod gpu;
#[cfg(feature = "wayland")]
pub mod input;
#[cfg(feature = "wayland")]
pub mod platform;
#[cfg(feature = "wayland")]
pub mod window;
