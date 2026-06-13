//! `ansyncd` — the ansync daemon.
//!
//! Hosts the D-Bus surface, the QUIC transport, and the screen mirror GUI
//! window (eframe + wgpu) when a client invokes `ShowScreen`.
//!
//! The `dev-playback` feature additionally exposes a `--play-file
//! PATH` flag that drives the decoder from a local Annex-B recording.
//! This is a Step-6 test affordance only; release builds must be
//! compiled without the feature.

use std::path::PathBuf;
use std::sync::Arc;

use ansync_daemon_core::{Daemon, DaemonConfig};
use clap::Parser;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[cfg(feature = "dev-playback")]
mod mirror_window;

#[derive(Debug, Parser)]
#[command(name = "ansyncd", version, about = "ansync daemon")]
struct Args {
    /// Override the device name advertised on the LAN.
    #[arg(long)]
    device_name: Option<String>,
    /// Override the identity key path.
    #[arg(long)]
    identity: Option<PathBuf>,
    /// Override the peers directory.
    #[arg(long)]
    peers_dir: Option<PathBuf>,
    /// Override the per-device permissions directory.
    #[arg(long)]
    permissions_dir: Option<PathBuf>,
    /// Bring up the mirror window and feed it from a local Annex-B
    /// recording (`.h264` / `.h265`). Dev-only — only present when
    /// compiled with `--features dev-playback`.
    #[cfg(feature = "dev-playback")]
    #[arg(long, value_name = "PATH")]
    play_file: Option<PathBuf>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    install_logging()?;
    let args = Args::parse();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    #[cfg(feature = "dev-playback")]
    if let Some(path) = args.play_file.clone() {
        return run_play_file(runtime, path);
    }

    runtime.block_on(run_daemon(args))
}

#[cfg(feature = "dev-playback")]
fn run_play_file(
    runtime: tokio::runtime::Runtime,
    path: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::Mutex;

    let shared: mirror_window::LatestFrame = Arc::new(Mutex::new(None));
    mirror_window::spawn_play_file(&runtime, path, shared.clone());
    // `run` blocks the calling thread on the eframe event loop. When
    // the window closes, drop the tokio runtime to abort the decoder
    // loop.
    let result = mirror_window::run(shared);
    drop(runtime);
    result
}

async fn run_daemon(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let device_name = args
        .device_name
        .or_else(hostname)
        .unwrap_or_else(|| "ansync-host".to_string());

    let mut config = DaemonConfig::new(device_name);
    config.identity_path = args.identity;
    config.peers_dir = args.peers_dir;
    config.permissions_dir = args.permissions_dir;

    let daemon = Arc::new(Daemon::new(config));
    daemon.run().await?;
    Ok(())
}

fn install_logging() -> Result<(), Box<dyn std::error::Error>> {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .with(tracing_journald::layer().ok())
        .try_init()?;
    Ok(())
}

fn hostname() -> Option<String> {
    std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty())
}
