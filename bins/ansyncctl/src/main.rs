//! `ansyncctl` — CLI front-end for the ansync daemon over D-Bus.

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "ansyncctl", version, about = "ansync CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List paired devices known to the daemon.
    Devices,
    /// Initiate pairing with a discovered device.
    Pair {
        /// Optional device name or id; if omitted the CLI walks discovery.
        device: Option<String>,
    },
    /// Forget a previously paired device.
    Forget { id: String },
    /// Open the mirror screen for a device.
    Show { id: String },
    /// Push a file to a device.
    Push { id: String, path: String },
    /// Mount the remote filesystem.
    Mount { id: String, mountpoint: String },
    /// Unmount the remote filesystem.
    Unmount { id: String },
    /// Get or set a per-device permission flag.
    Perm {
        id: String,
        flag: String,
        value: Option<bool>,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let cli = Cli::parse();
    match cli.command {
        Command::Devices => println!("(skeleton) list devices"),
        Command::Pair { device } => println!("(skeleton) pair {device:?}"),
        Command::Forget { id } => println!("(skeleton) forget {id}"),
        Command::Show { id } => println!("(skeleton) show {id}"),
        Command::Push { id, path } => println!("(skeleton) push {path} -> {id}"),
        Command::Mount { id, mountpoint } => println!("(skeleton) mount {id} at {mountpoint}"),
        Command::Unmount { id } => println!("(skeleton) unmount {id}"),
        Command::Perm { id, flag, value } => println!("(skeleton) perm {id} {flag} {value:?}"),
    }

    Ok(())
}
