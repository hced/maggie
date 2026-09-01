//! Maggie (cross-platform) — screen magnifier using winit + wgpu.
//!
//! Build with: `cargo build --release --bin maggie_xp`
//! Run with:   `./target/release/maggie_xp`

fn main() -> anyhow::Result<()> {
    maggie::xp::run()
}
