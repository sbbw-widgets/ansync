//! Orchestrator shared between `ansyncd` and integration tests.
//!
//! Owns the long-term identity, peer store, permission store, mDNS
//! announcer, and D-Bus surface. Runtime sessions over QUIC land in a
//! later step — for Step 4 the orchestrator stands the surface up and
//! keeps it alive until SIGINT / SIGTERM.

use std::path::PathBuf;
use std::sync::Arc;

use ansync_core::{Capabilities, DeviceName};
use ansync_crypto::IdentityKeypair;
use ansync_dbus::{DaemonState, serve};
use ansync_discovery::{Discovery, MdnsDiscovery};
use ansync_pairing::PeerStore;
use ansync_permissions::FilePermissionsStore;
use directories::BaseDirs;
use tokio::signal::unix::{SignalKind, signal};
use tracing::{info, warn};

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("core: {0}")]
    Core(#[from] ansync_core::Error),
    #[error("crypto: {0}")]
    Crypto(#[from] ansync_crypto::CryptoError),
    #[error("peer store: {0}")]
    PeerStore(#[from] ansync_pairing::PeerStoreError),
    #[error("permissions: {0}")]
    Permissions(#[from] ansync_permissions::PermissionsError),
    #[error("discovery: {0}")]
    Discovery(#[from] ansync_discovery::DiscoveryError),
    #[error("dbus: {0}")]
    Dbus(#[from] ansync_dbus::DbusError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("startup: {0}")]
    Startup(String),
}

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub device_name: String,
    /// Override the on-disk identity path. Defaults to
    /// `$XDG_DATA_HOME/ansync/identity.key`.
    pub identity_path: Option<PathBuf>,
    /// Override the peers directory. Defaults to
    /// `$XDG_DATA_HOME/ansync/peers/`.
    pub peers_dir: Option<PathBuf>,
    /// Override the per-device permissions directory. Defaults to
    /// `$XDG_CONFIG_HOME/ansync/devices/`.
    pub permissions_dir: Option<PathBuf>,
    /// Capabilities the host can serve to remote peers. Step 4 ships an
    /// empty default — feature crates will OR their flags in as they
    /// land.
    pub capabilities: Capabilities,
}

impl DaemonConfig {
    pub fn new(device_name: String) -> Self {
        Self {
            device_name,
            identity_path: None,
            peers_dir: None,
            permissions_dir: None,
            capabilities: Capabilities::empty(),
        }
    }
}

pub struct Daemon {
    config: DaemonConfig,
}

impl Daemon {
    pub fn new(config: DaemonConfig) -> Self {
        Self { config }
    }

    /// Bring the daemon up: claim the D-Bus name, register every paired
    /// device, start the mDNS announcement, then block until either
    /// SIGINT or SIGTERM is received.
    pub async fn run(self: Arc<Self>) -> Result<(), DaemonError> {
        let identity_path = self
            .config
            .identity_path
            .clone()
            .unwrap_or(default_data_dir()?.join("identity.key"));
        let peers_dir = self
            .config
            .peers_dir
            .clone()
            .unwrap_or(default_data_dir()?.join("peers"));
        let permissions_dir = self
            .config
            .permissions_dir
            .clone()
            .unwrap_or(default_config_dir()?.join("devices"));

        let identity = IdentityKeypair::load_or_generate(&identity_path)?;
        info!(device_id = %identity.device_id(), "identity loaded");

        let peers = PeerStore::open(peers_dir)?;
        let permissions: Arc<dyn ansync_permissions::PermissionsStore> =
            Arc::new(FilePermissionsStore::open(permissions_dir)?);

        let pubkey = identity.public().as_bytes();
        let mdns = MdnsDiscovery::new(pubkey)?;

        let state = Arc::new(DaemonState::new(
            identity,
            self.config.device_name.clone(),
            peers,
            permissions,
        ));

        let dbus_conn = serve(state.clone()).await?;
        info!(service = ansync_dbus::SERVICE_NAME, "D-Bus surface ready");

        let device_name = DeviceName(self.config.device_name.clone());
        // mDNS port 0: we do not yet expose a control endpoint to
        // advertise. Real port replaces this once the daemon starts
        // accepting QUIC connections.
        mdns.announce(&device_name, 0, self.config.capabilities)
            .await?;
        info!(name = %device_name, "mDNS announce active");

        wait_for_shutdown().await?;

        if let Err(e) = mdns.stop_announce().await {
            warn!(error = %e, "mDNS stop_announce failed");
        }
        drop(dbus_conn);
        info!("daemon shut down");
        Ok(())
    }

    pub fn config(&self) -> &DaemonConfig {
        &self.config
    }
}

fn default_data_dir() -> Result<PathBuf, DaemonError> {
    BaseDirs::new()
        .map(|b| b.data_dir().join("ansync"))
        .ok_or_else(|| DaemonError::Startup("$HOME not set; cannot resolve XDG paths".into()))
}

fn default_config_dir() -> Result<PathBuf, DaemonError> {
    BaseDirs::new()
        .map(|b| b.config_dir().join("ansync"))
        .ok_or_else(|| DaemonError::Startup("$HOME not set; cannot resolve XDG paths".into()))
}

async fn wait_for_shutdown() -> Result<(), DaemonError> {
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = term.recv() => info!("SIGTERM"),
        _ = int.recv() => info!("SIGINT"),
    }
    Ok(())
}
