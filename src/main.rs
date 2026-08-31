use anyhow::Result;
use clap::Parser;
use tracing_subscriber::prelude::*;

mod capture;
mod config;
mod config_window;
mod cursor;
mod draw_mode;
mod engine;
mod gpu;
mod input;
mod osd;
mod render;

#[derive(Parser, Debug)]
#[command(name = "maggie", version, about = "Native Wayland screen magnifier")]
struct Args {
    #[arg(short, long, value_name = "LEVEL")]
    zoom: Option<f64>,

    #[arg(short, long)]
    debug: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Panic hook: log to stderr so panics are visible.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        eprintln!("PANIC: {info}");
        default_hook(info);
    }));

    // The log filter honours `RUST_LOG` (e.g. `RUST_LOG=maggie=debug maggie`),
    // falling back to `maggie=info` when it is unset/empty/invalid — note
    // that `try_from_default_env()` returns an *empty* (all-silent) filter for
    // an unset variable, so the environment must be checked explicitly.
    // `--debug` forces debug output regardless of the environment.
    let filter = if args.debug {
        tracing_subscriber::EnvFilter::new("maggie=debug")
    } else if std::env::var("RUST_LOG")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
    {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("maggie=info"))
    } else {
        tracing_subscriber::EnvFilter::new("maggie=info")
    };
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(filter)
        .init();

    tracing::info!("Starting maggie");

    engine::run(args.zoom)?;

    Ok(())
}
