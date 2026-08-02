use anyhow::Result;
use clap::Parser;
use tracing_subscriber::prelude::*;

mod config;
mod input;
mod render;
mod capture;
mod engine;
mod osd;

#[derive(Parser, Debug)]
#[command(name = "maggie", version, about = "Native Wayland screen magnifier")]
struct Args {
    #[arg(short, long, value_name = "LEVEL")]
    zoom: Option<f64>,

    #[arg(short, long)]
    debug: bool,

    #[arg(long)]
    version: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.debug {
        tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer())
            .with(tracing_subscriber::EnvFilter::new("maggie=debug"))
            .init();
    } else {
        tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer())
            .with(tracing_subscriber::EnvFilter::new("maggie=info"))
            .init();
    }

    if args.version {
        println!("maggie {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    tracing::info!("Starting maggie");

    engine::run(args.zoom)?;

    Ok(())
}
