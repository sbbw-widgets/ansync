//! `ansyncctl` — CLI front-end for the ansync daemon over D-Bus.

use std::path::PathBuf;

use ansync_crypto::IdentityKeypair;
use clap::{Parser, Subcommand};
use directories::BaseDirs;
use tracing_subscriber::EnvFilter;

const IDENTITY_FILENAME: &str = "identity.key";

#[derive(Debug, Parser)]
#[command(name = "ansyncctl", version, about = "ansync CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Manage the long-term Ed25519 identity stored on disk.
    Identity {
        #[command(subcommand)]
        action: IdentityAction,
    },
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

#[derive(Debug, Subcommand)]
enum IdentityAction {
    /// Generate a new Ed25519 identity at the default path (will not
    /// overwrite an existing one).
    Init {
        /// Override the on-disk path (default `$XDG_DATA_HOME/ansync/identity.key`).
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Print the device id derived from the persisted identity.
    Show {
        #[arg(long)]
        path: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let cli = Cli::parse();
    match cli.command {
        Command::Identity { action } => identity(action)?,
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

fn default_identity_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let base = BaseDirs::new().ok_or("$HOME not set; cannot resolve XDG paths")?;
    Ok(base.data_dir().join("ansync").join(IDENTITY_FILENAME))
}

fn identity(action: IdentityAction) -> Result<(), Box<dyn std::error::Error>> {
    match action {
        IdentityAction::Init { path } => {
            let path = match path {
                Some(p) => p,
                None => default_identity_path()?,
            };
            if path.exists() {
                return Err(format!(
                    "identity already exists at {} — refusing to overwrite",
                    path.display()
                )
                .into());
            }
            let kp = IdentityKeypair::generate();
            kp.save(&path)?;
            println!("identity created at {}", path.display());
            println!("device id: {}", kp.device_id());
            Ok(())
        }
        IdentityAction::Show { path } => {
            let path = match path {
                Some(p) => p,
                None => default_identity_path()?,
            };
            let kp = IdentityKeypair::load(&path)?;
            println!("path: {}", path.display());
            println!("device id: {}", kp.device_id());
            Ok(())
        }
    }
}
