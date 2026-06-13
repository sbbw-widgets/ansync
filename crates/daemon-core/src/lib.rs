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

use std::collections::HashMap;
use std::sync::Mutex as StdMutex;

use ansync_core::{Capabilities, DeviceId, DeviceName, Permission};
use ansync_crypto::IdentityKeypair;
use ansync_dbus::{DaemonAction, DaemonState, serve};
use ansync_discovery::{Discovery, MdnsDiscovery};
use ansync_files::{
    AutoAcceptPolicy, fs::client::FsClient, fs::fuse_mount::FuseMount, receive_file,
};
use ansync_input::{InputDeviceFactory, InputSession, UinputFactory};
use ansync_pairing::PeerStore;
use ansync_permissions::FilePermissionsStore;
use ansync_proto::InputMessage;
use ansync_transport::pinning::TrustedPeers;
use ansync_transport::{
    Connection, QuicConnection, QuicServer, QuicStream, QuicTransport, Stream as _, StreamKind,
};
use ansync_video::sink_egui::{FrameSlot, new_slot};
use ansync_video::{HostDecoder, VideoCodec, VideoDecoder};
use directories::BaseDirs;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::Mutex;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tracing::{debug, error, info, warn};

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

        let (action_tx, action_rx) = unbounded_channel::<DaemonAction>();
        let state = Arc::new(
            DaemonState::new(
                identity,
                self.config.device_name.clone(),
                peers.clone(),
                permissions.clone(),
            )
            .with_actions(action_tx),
        );

        let dbus_conn = serve(state.clone()).await?;
        info!(service = ansync_dbus::SERVICE_NAME, "D-Bus surface ready");

        let mirrors = Arc::new(MirrorRegistry::default());
        let action_handle = tokio::spawn(action_loop(action_rx, mirrors.clone()));

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
            mirrors: mirrors.clone(),
        }));

        wait_for_shutdown().await?;

        accept_handle.abort();
        action_handle.abort();
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
    mirrors: Arc<MirrorRegistry>,
}

/// Per-peer mirror state: frame slot the decoder populates and the
/// thread handle of the open window (if any).
#[derive(Default)]
pub struct MirrorRegistry {
    entries: StdMutex<HashMap<DeviceId, Arc<MirrorEntry>>>,
}

pub struct MirrorEntry {
    pub slot: FrameSlot,
    pub peer_name: String,
    /// `Some` while a window thread is alive; cleared on HideScreen.
    window: StdMutex<Option<std::thread::JoinHandle<()>>>,
}

impl MirrorRegistry {
    /// Get-or-create the entry. Slot survives multiple peer reconnects
    /// so the window can stay open while video pauses + resumes.
    pub fn ensure(&self, id: &DeviceId, name: &str) -> Arc<MirrorEntry> {
        let mut entries = self.entries.lock().expect("mirror registry poisoned");
        entries
            .entry(id.clone())
            .or_insert_with(|| {
                Arc::new(MirrorEntry {
                    slot: new_slot(),
                    peer_name: name.to_string(),
                    window: StdMutex::new(None),
                })
            })
            .clone()
    }

    pub fn get(&self, id: &DeviceId) -> Option<Arc<MirrorEntry>> {
        self.entries
            .lock()
            .ok()
            .and_then(|e| e.get(id).cloned())
    }
}

async fn action_loop(
    mut rx: UnboundedReceiver<DaemonAction>,
    mirrors: Arc<MirrorRegistry>,
) {
    while let Some(action) = rx.recv().await {
        match action {
            DaemonAction::ShowScreen { device } => {
                let Some(entry) = mirrors.get(&device) else {
                    warn!(%device, "ShowScreen: no mirror entry (peer not connected?)");
                    continue;
                };
                let mut guard = entry.window.lock().expect("window slot poisoned");
                if guard.is_some() {
                    debug!(%device, "ShowScreen: window already up");
                    continue;
                }
                let slot = entry.slot.clone();
                let title = format!("ansync — {}", entry.peer_name);
                let handle = std::thread::Builder::new()
                    .name(format!("ansync-mirror-{device}"))
                    .spawn(move || {
                        if let Err(e) = ansync_video::sink_egui::run(title, slot) {
                            warn!(error = %e, "mirror window exited with error");
                        }
                    })
                    .ok();
                *guard = handle;
                info!(%device, "ShowScreen: window spawned");
            }
            DaemonAction::HideScreen { device } => {
                let Some(entry) = mirrors.get(&device) else {
                    continue;
                };
                let mut guard = entry.window.lock().expect("window slot poisoned");
                // eframe doesn't expose a clean external close API; the
                // user closes the window. Just clear our handle so the
                // next ShowScreen can re-open. The thread terminates
                // when the user closes the window.
                *guard = None;
                info!(%device, "HideScreen: handle cleared");
            }
        }
    }
}

async fn accept_loop(ctx: AcceptCtx) {
    loop {
        match ctx.server.accept().await {
            Ok(conn) => {
                let peers = ctx.peers.clone();
                let permissions = ctx.permissions.clone();
                let factory = ctx.factory.clone();
                let download_dir = ctx.download_dir.clone();
                let mirrors = ctx.mirrors.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        handle_connection(conn, peers, permissions, factory, download_dir, mirrors)
                            .await
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
    mirrors: Arc<MirrorRegistry>,
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

    // Ensure the mirror entry exists for this peer so the Video
    // stream loop can populate it as soon as the companion opens its
    // Video bidi stream.
    let mirror_entry = mirrors.ensure(&peer_id, &peer.name.0);

    // Auto-mount FUSE if the peer's `files_mount` flag is on. The
    // BackgroundSession is held on the stack so dropping it on
    // peer-disconnect umounts cleanly.
    let _fuse_session = maybe_auto_mount(&conn, &peer_id, &peer.name, permissions.as_ref()).await;

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
            StreamKind::Video => {
                let slot = mirror_entry.slot.clone();
                let pid = peer_id.clone();
                tokio::spawn(video_stream_loop(stream, slot, pid));
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

async fn maybe_auto_mount(
    conn: &QuicConnection,
    peer_id: &DeviceId,
    peer_name: &DeviceName,
    permissions: &dyn ansync_permissions::PermissionsStore,
) -> Option<ansync_files::fs::BackgroundSession> {
    match permissions.check(peer_id, Permission::FilesMount).await {
        Ok(true) => {}
        Ok(false) => {
            debug!(%peer_id, "files_mount off; skip auto-mount");
            return None;
        }
        Err(e) => {
            warn!(error = %e, "files_mount perm check failed; skip auto-mount");
            return None;
        }
    }
    let stream = match conn.open(StreamKind::Fs).await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "open Fs stream for auto-mount failed");
            return None;
        }
    };
    let client = FsClient::new(stream);
    let runtime = tokio::runtime::Handle::current();
    let mount_root = match runtime_mount_dir() {
        Some(r) => r,
        None => {
            warn!("$XDG_RUNTIME_DIR not set; skip auto-mount");
            return None;
        }
    };
    let mount_point = mount_root.join(sanitize(&peer_name.0));
    if let Err(e) = std::fs::create_dir_all(&mount_point) {
        warn!(error = %e, path = %mount_point.display(), "create mount dir failed");
        return None;
    }
    let mount = FuseMount::new(client, runtime);
    match mount.spawn(&mount_point) {
        Ok(session) => {
            info!(path = %mount_point.display(), "FUSE mount up");
            Some(session)
        }
        Err(e) => {
            warn!(error = %e, "FUSE mount failed");
            None
        }
    }
}

fn runtime_mount_dir() -> Option<PathBuf> {
    let base = std::env::var("XDG_RUNTIME_DIR").ok()?;
    Some(PathBuf::from(base).join("ansync").join("mounts"))
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

async fn video_stream_loop(mut stream: QuicStream, slot: FrameSlot, peer_id: DeviceId) {
    // Initial dimension hint is rewritten by SPS on the first IDR;
    // 1080p is a safe upper bound for the NVDEC / VA-API surface pools.
    let mut decoder = match HostDecoder::configure(VideoCodec::H264, 1920, 1080) {
        Ok(d) => d,
        Err(e) => {
            warn!(%peer_id, error = %e, "video decoder unavailable");
            return;
        }
    };
    info!(%peer_id, "video stream wired");
    loop {
        let bytes = match stream.recv().await {
            Ok(b) => b,
            Err(ansync_transport::TransportError::Closed) => {
                info!(%peer_id, "video stream closed");
                return;
            }
            Err(e) => {
                warn!(%peer_id, error = %e, "video recv error");
                return;
            }
        };
        if let Err(e) = decoder.feed(bytes).await {
            warn!(%peer_id, error = %e, "decoder feed failed; continuing");
            continue;
        }
        match decoder.take().await {
            Ok(Some(frame)) => {
                if let Ok(mut s) = slot.lock() {
                    *s = Some(frame);
                }
            }
            Ok(None) => {}
            Err(e) => {
                warn!(%peer_id, error = %e, "decoder take failed");
            }
        }
    }
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
