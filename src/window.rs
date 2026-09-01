//! Shared window types and core application logic.
//!
//! This module re-exports the key types from `engine.rs` so that other
//! modules can import them as `crate::window::MagnifierWindow` instead of
//! depending directly on `crate::engine`. This is the first step toward a
//! shared window module: the types live here, and in a future phase the
//! actual struct definitions and impl blocks will be moved from `engine.rs`
//! into this file.
//!
//! Currently this is a thin re-export layer. The full extraction (moving
//! ~5000 lines of code) is documented in SPEC.md §12.11.

// Re-export all public types from engine.rs.
pub use crate::engine::*;
