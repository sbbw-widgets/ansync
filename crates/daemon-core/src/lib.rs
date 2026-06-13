//! Orchestrator shared between `ansyncd` and integration tests.
//!
//! Owns the long-term identity, peer store, permission store, mDNS
//! announcer, D-Bus surface, and (Step 7b-2) the QUIC server accept
//! loop. Per-peer connections demux into the appropriate session;
//! today only the `Input` stream is wired all the way through —
//! other stream kinds get accepted and logged so the daemon doesn't
//! drop the connection for them.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use ansync_core::{Capabilities, DeviceId, DeviceName};
use ansync_crypto::IdentityKeypair;
use ansync_dbus::{DaemonState, serve};
use ansync_discovery::{Discovery, MdnsDiscovery};
use ansync_files::{AutoAcceptPolicy, receive_file};
use ansync_input::{InputDeviceFactory, InputSession, UinputFactory};
use ansync_pairing::PeerStore;
use ansync_permissions::FilePermissionsStore;
use ansync_proto::InputMessage;
use ansync_transport::pinning::TrustedPeers;
use ansync_transport::{
    Connection, QuicConnection, QuicServer, QuicStream, QuicTransport, Stream as _, StreamKind,
};
use directories::BaseDirs;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

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
    #[error("transport: {0}")]
    Transport(#[from] ansync_transport::TransportError),
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
    /// Address the QUIC server binds to. `0.0.0.0:0` (default) picks
    /// a random unused port — that port is then announced via mDNS
    /// so peers can connect on the LAN.
    pub listen_addr: SocketAddr,
    /// Root directory where inbound file transfers land. Each peer
    /// gets a `{peer_id}/` subdir to namespace concurrent senders
    /// against each other. Defaults to
    /// `$XDG_DATA_HOME/ansync/incoming/` if unset.
    pub download_dir: Option<PathBuf>,
    /// Capabilities the host can serve to remote peers. Step 7 turns
    /// on `INPUT_FROM_DEV` by default since uinput is the first
    /// capability wired end-to-end. Step 8 adds `FILES`.
    pub capabilities: Capabilities,
}

impl DaemonConfig {
    pub fn new(device_name: String) -> Self {
        Self {
            device_name,
            identity_path: None,
            peers_dir: None,
            permissions_dir: None,
            listen_addr: "0.0.0.0:0".parse().expect("hard-coded addr parses"),
            download_dir: None,
            capabilities: Capabilities::INPUT_FROM_DEV | Capabilities::FILES,
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

    /// Bring the daemon up: claim the D-Bus name, bind QUIC, register
    /// every paired device, start the mDNS announcement, then block
    /// until either SIGINT or SIGTERM is received.
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
        let download_dir = self
            .config
            .download_dir
            .clone()
            .unwrap_or(default_data_dir()?.join("incoming"));

        let identity = IdentityKeypair::load_or_generate(&identity_path)?;
        info!(device_id = %identity.device_id(), "identity loaded");

        let peers = PeerStore::open(peers_dir)?;
        let permissions: Arc<dyn ansync_permissions::PermissionsStore> =
            Arc::new(FilePermissionsStore::open(permissions_dir)?);

        let pubkey = identity.public().as_bytes();
        let mdns = MdnsDiscovery::new(pubkey)?;

        // Bind QUIC before mDNS so we know the real port to announce.
        let transport = QuicTransport::new(identity.clone());
        let trust: Arc<dyn TrustedPeers> = Arc::new(PeerStoreTrust {
            peers: peers.clone(),
        });
        let server = transport.bind_any(self.config.listen_addr, trust)?;
        let listen = server.local_addr()?;
        info!(addr = %listen, "QUIC server bound");

        let state = Arc::new(DaemonState::new(
            identity,
            self.config.device_name.clone(),
            peers.clone(),
            permissions.clone(),
        ));

        let dbus_conn = serve(state.clone()).await?;
        info!(service = ansync_dbus::SERVICE_NAME, "D-Bus surface ready");

        let device_name = DeviceName(self.config.device_name.clone());
        mdns.announce(&device_name, listen.port(), self.config.capabilities)
            .await?;
        info!(name = %device_name, port = listen.port(), "mDNS announce active");

        let factory: Arc<dyn InputDeviceFactory> = Arc::new(UinputFactory);
        let accept_handle = tokio::spawn(accept_loop(AcceptCtx {
            server,
            peers,
            permissions: permissions.clone(),
            factory,
            download_dir,
        }));

        wait_for_shutdown().await?;

        accept_handle.abort();
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

/// `PeerStore`-backed trust predicate for the multi-peer QUIC
/// verifier. Looks up the connecting pubkey against the on-disk
/// store on every handshake — cheap because the store is small and
/// reads are filesystem-cached.
#[derive(Debug)]
struct PeerStoreTrust {
    peers: PeerStore,
}

impl TrustedPeers for PeerStoreTrust {
    fn is_trusted(&self, pubkey: &[u8; 32]) -> bool {
        match self.peers.list() {
            Ok(list) => list.iter().any(|p| &p.pubkey == pubkey),
            Err(e) => {
                warn!(error = %e, "PeerStore::list failed during TLS verify");
                false
            }
        }
    }
}

struct AcceptCtx {
    server: QuicServer,
    peers: PeerStore,
    permissions: Arc<dyn ansync_permissions::PermissionsStore>,
    factory: Arc<dyn InputDeviceFactory>,
    download_dir: PathBuf,
}

async fn accept_loop(ctx: AcceptCtx) {
    loop {
        match ctx.server.accept().await {
            Ok(conn) => {
                let peers = ctx.peers.clone();
                let permissions = ctx.permissions.clone();
                let factory = ctx.factory.clone();
                let download_dir = ctx.download_dir.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        handle_connection(conn, peers, permissions, factory, download_dir).await
                    {
                        warn!(error = %e, "peer connection errored");
                    }
                });
            }
            Err(e) => {
                error!(error = %e, "QUIC accept failed; backing off 100ms");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

async fn handle_connection(
    conn: QuicConnection,
    peers: PeerStore,
    permissions: Arc<dyn ansync_permissions::PermissionsStore>,
    factory: Arc<dyn InputDeviceFactory>,
    download_dir: PathBuf,
) -> Result<(), DaemonError> {
    let pubkey = conn.peer_identity().as_bytes();
    let mut id_bytes = [0u8; 16];
    id_bytes.copy_from_slice(&pubkey[..16]);
    let peer_id = DeviceId(id_bytes);
    let peer = peers.get(&peer_id)?;
    info!(peer = %peer.name, %peer_id, "peer connected");

    // Per-peer InputSession lives behind an Arc<Mutex> so any future
    // input stream re-opened by the peer (e.g. after a brief
    // network blip) can re-attach to the same uinput devices.
    let input_session: Arc<Mutex<InputSession>> = Arc::new(Mutex::new(InputSession::new(
        peer_id.clone(),
        peer.name.clone(),
        permissions.clone(),
        factory.clone(),
    )));

    loop {
        let (kind, stream) = match conn.accept().await {
            Ok(v) => v,
            Err(ansync_transport::TransportError::Closed) => {
                info!(%peer_id, "peer closed connection");
                break;
            }
            Err(e) => return Err(e.into()),
        };
        match kind {
            StreamKind::Input => {
                let session = input_session.clone();
                tokio::spawn(input_stream_loop(stream, session));
            }
            StreamKind::Files => {
                let perms = permissions.clone();
                let peer_id_inbound = peer_id.clone();
                let policy = Arc::new(AutoAcceptPolicy {
                    root: download_dir.clone(),
                });
                tokio::spawn(files_stream_loop(stream, peer_id_inbound, perms, policy));
            }
            other => {
                warn!(kind = ?other, "stream kind accepted but not wired yet — dropping");
                drop(stream);
            }
        }
    }
    input_session.lock().await.shutdown().await;
    Ok(())
}

async fn files_stream_loop(
    mut stream: QuicStream,
    peer_id: DeviceId,
    permissions: Arc<dyn ansync_permissions::PermissionsStore>,
    policy: Arc<AutoAcceptPolicy>,
) {
    match receive_file(&peer_id, permissions.as_ref(), &mut stream, policy.as_ref()).await {
        Ok(path) => info!(%peer_id, dest = %path.display(), "inbound transfer ok"),
        Err(e) => warn!(%peer_id, error = %e, "inbound transfer failed"),
    }
}

async fn input_stream_loop(mut stream: QuicStream, session: Arc<Mutex<InputSession>>) {
    loop {
        let bytes = match stream.recv().await {
            Ok(b) => b,
            Err(ansync_transport::TransportError::Closed) => break,
            Err(e) => {
                warn!(error = %e, "input stream recv failed");
                break;
            }
        };
        let msg: InputMessage = match postcard::from_bytes(&bytes) {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "input postcard decode failed");
                continue;
            }
        };
        let mut s = session.lock().await;
        if let Err(e) = s.dispatch(msg).await {
            warn!(error = %e, "input dispatch failed");
        }
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
