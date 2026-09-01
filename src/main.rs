//! Maggie — Native Wayland screen magnifier.
//!
//! This binary is only available when compiled with the `wayland` feature
//! (enabled by default). For cross-platform builds, use `maggie_xp`.

#![cfg_attr(not(feature = "wayland"), allow(dead_code, unused_imports, unused_variables))]

#[cfg(feature = "wayland")]
use anyhow::Result;
#[cfg(feature = "wayland")]
use clap::Parser;
#[cfg(feature = "wayland")]
use tracing_subscriber::prelude::*;

// When wayland feature is enabled, use modules from the lib crate.
#[cfg(feature = "wayland")]
use maggie::engine;

#[cfg(feature = "wayland")]
#[derive(Parser, Debug)]
#[command(name = "maggie", version, about = "Native Wayland screen magnifier")]
struct Args {
    /// Log filter (e.g. "debug", "maggie=trace")
    #[arg(short, long, env = "RUST_LOG", default_value = "info")]
    log_filter: String,
}

#[cfg(feature = "wayland")]
fn main() -> Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| args.log_filter.into()),
        )
        .init();

    tracing::info!("Starting maggie");
    engine::run(None)
}

// When wayland feature is not enabled, this binary is a no-op.
#[cfg(not(feature = "wayland"))]
fn main() {
    eprintln!("This binary requires the 'wayland' feature. Use 'maggie_xp' for cross-platform.");
}
