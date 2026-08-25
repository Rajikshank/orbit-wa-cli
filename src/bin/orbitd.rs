//! Orbit daemon executable. Users normally launch it through `orbit daemon start`.

use anyhow::Result;
use clap::Parser;
use orbit_core::{
    config::{Config, OrbitPaths},
    daemon,
};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
struct Args {
    /// Override the data root; primarily useful for isolated profiles and tests.
    #[arg(long)]
    home: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("orbit_core=info")),
        )
        .with_target(false)
        .init();
    let args = Args::parse();
    let paths = match args.home {
        Some(root) => OrbitPaths::for_root(root),
        None => OrbitPaths::discover()?,
    };
    let config = Config::load(&paths)?;
    daemon::run(paths, config).await
}
