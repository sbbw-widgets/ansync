//! `ansyncd` — the ansync daemon.
//!
//! Hosts the D-Bus surface, the QUIC transport, and the screen mirror GUI
//! window (eframe + wgpu) when a client invokes `ShowScreen`.

use std::path::PathBuf;
use std::sync::Arc;

use ansync_daemon_core::{Daemon, DaemonConfig};
use clap::Parser;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

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
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    install_logging()?;

    let args = Args::parse();
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
