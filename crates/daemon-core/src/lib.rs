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

use ansync_audio::{AudioBackend, AudioFormat, AudioSink, AudioSource, CpalBackend, SampleFormat};
use ansync_camera::{CameraFormat, CameraPixelFormat, VirtualCameraSink};
use ansync_clipboard::{ClipboardBackend, ClipboardContent, WaylandClipboard};
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
use ansync_proto::{
    AudioDirection, AudioStreamInit, CameraConfig, ClipboardMessage, ControlMessage, Envelope,
    InputMessage, Message, PROTOCOL_VERSION, VideoCodec as ProtoVideoCodec,
};
use ansync_transport::pinning::TrustedPeers;
use ansync_transport::{
    Connection, QuicConnection, QuicServer, QuicStream, QuicTransport, Stream as _, StreamKind,
};
use ansync_video::sink_egui::{FrameSlot, new_slot};
use ansync_video::{HostDecoder, PixelFormat, VideoCodec, VideoDecoder};
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
            capabilities: Capabilities::INPUT_FROM_DEV
                | Capabilities::FILES
                | Capabilities::CAMERA_VIDEO
                | Capabilities::AUDIO_IN
                | Capabilities::AUDIO_OUT
                | Capabilities::MIC
                | Capabilities::CLIPBOARD,
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
        let cameras = Arc::new(CameraRegistry::default());
        let audios = Arc::new(AudioRegistry::default());
        let action_handle = tokio::spawn(action_loop(
            action_rx,
            mirrors.clone(),
            cameras.clone(),
            audios.clone(),
            permissions.clone(),
        ));

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
            cameras: cameras.clone(),
            audios: audios.clone(),
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
    cameras: Arc<CameraRegistry>,
    audios: Arc<AudioRegistry>,
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
    /// `Some` while the peer is connected. `action_loop` reads this
    /// to open an outbound Input stream on ShowScreen. Cleared by
    /// `handle_connection` on disconnect.
    pub conn: StdMutex<Option<Arc<QuicConnection>>>,
}

/// Per-peer audio pipeline state. Like `CameraRegistry`, but with
/// separate sink + source slots because the route directions are
/// independent — the user may want speakers on the host but no
/// microphone share, for example.
#[derive(Default)]
pub struct AudioRegistry {
    entries: StdMutex<HashMap<DeviceId, Arc<AudioEntry>>>,
}

pub struct AudioEntry {
    pub peer_name: String,
    /// Plays *into* the host (peer → host direction).
    sink: tokio::sync::Mutex<Option<Arc<tokio::sync::Mutex<ansync_audio::CpalSink>>>>,
    /// Captures *from* the host (host → peer direction). The
    /// background pump task moves bytes out of this source onto the
    /// outbound stream.
    pump_handle: StdMutex<Option<tokio::task::JoinHandle<()>>>,
    /// Channel the inbound `StreamKind::Audio` accept loop writes
    /// raw S16LE PCM to. Drained by `audio_render_loop` into `sink`.
    inbound_tx: StdMutex<Option<tokio::sync::mpsc::UnboundedSender<bytes::Bytes>>>,
    /// Active inbound render task; aborted on Stop.
    inbound_handle: StdMutex<Option<tokio::task::JoinHandle<()>>>,
}

impl AudioRegistry {
    pub fn ensure(&self, id: &DeviceId, name: &str) -> Arc<AudioEntry> {
        let mut entries = self.entries.lock().expect("audio registry poisoned");
        entries
            .entry(id.clone())
            .or_insert_with(|| {
                Arc::new(AudioEntry {
                    peer_name: name.to_string(),
                    sink: tokio::sync::Mutex::new(None),
                    pump_handle: StdMutex::new(None),
                    inbound_tx: StdMutex::new(None),
                    inbound_handle: StdMutex::new(None),
                })
            })
            .clone()
    }
    pub fn get(&self, id: &DeviceId) -> Option<Arc<AudioEntry>> {
        self.entries
            .lock()
            .ok()
            .and_then(|e| e.get(id).cloned())
    }
}

/// Per-peer camera pipeline state. Lives across connect / disconnect
/// cycles so a `StopCamera` from D-Bus after a brief drop still finds
/// the sink it owns.
#[derive(Default)]
pub struct CameraRegistry {
    entries: StdMutex<HashMap<DeviceId, Arc<CameraEntry>>>,
}

pub struct CameraEntry {
    pub peer_name: String,
    /// The v4l2loopback sink. Lazily created on the first StartCamera
    /// and reused across reconfigures within the same peer.
    sink: tokio::sync::Mutex<Option<Arc<dyn VirtualCameraSink>>>,
    /// `Some` while a camera stream is alive. Dropping it stops the
    /// decoder feed loop.
    handle: StdMutex<Option<tokio::task::JoinHandle<()>>>,
    /// Bridges the per-peer `StreamKind::Camera` accept handler (which
    /// owns the QuicStream) to the decode → sink loop. Set when
    /// StartCamera arrives, cleared on StopCamera. The accept handler
    /// pushes raw encoded packets here.
    frame_tx: StdMutex<Option<tokio::sync::mpsc::UnboundedSender<bytes::Bytes>>>,
}

impl CameraRegistry {
    pub fn ensure(&self, id: &DeviceId, name: &str) -> Arc<CameraEntry> {
        let mut entries = self.entries.lock().expect("camera registry poisoned");
        entries
            .entry(id.clone())
            .or_insert_with(|| {
                Arc::new(CameraEntry {
                    peer_name: name.to_string(),
                    sink: tokio::sync::Mutex::new(None),
                    handle: StdMutex::new(None),
                    frame_tx: StdMutex::new(None),
                })
            })
            .clone()
    }

    pub fn get(&self, id: &DeviceId) -> Option<Arc<CameraEntry>> {
        self.entries
            .lock()
            .ok()
            .and_then(|e| e.get(id).cloned())
    }
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
                    conn: StdMutex::new(None),
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
    cameras: Arc<CameraRegistry>,
    audios: Arc<AudioRegistry>,
    permissions: Arc<dyn ansync_permissions::PermissionsStore>,
) {
    while let Some(action) = rx.recv().await {
        match action {
            DaemonAction::StartAudioRoute { device, direction } => {
                if let Err(e) =
                    handle_start_audio(&mirrors, &audios, &permissions, &device, direction).await
                {
                    warn!(%device, error = %e, "StartAudioRoute failed");
                }
            }
            DaemonAction::StopAudioRoute { device } => {
                handle_stop_audio(&audios, &device).await;
            }
            DaemonAction::StartMicrophone { device } => {
                if let Err(e) = handle_start_audio(
                    &mirrors,
                    &audios,
                    &permissions,
                    &device,
                    AudioDirection::DeviceToHost,
                )
                .await
                {
                    warn!(%device, error = %e, "StartMicrophone failed");
                }
            }
            DaemonAction::StopMicrophone { device } => {
                handle_stop_audio(&audios, &device).await;
            }
            DaemonAction::SyncClipboard { device } => {
                if let Err(e) = push_clipboard_to_peer(&mirrors, &permissions, &device).await {
                    warn!(%device, error = %e, "SyncClipboard failed");
                }
            }
            DaemonAction::StartCamera { device, config } => {
                if let Err(e) =
                    handle_start_camera(&mirrors, &cameras, &permissions, &device, config).await
                {
                    warn!(%device, error = %e, "StartCamera failed");
                }
            }
            DaemonAction::StopCamera { device } => {
                if let Err(e) = handle_stop_camera(&cameras, &device).await {
                    warn!(%device, error = %e, "StopCamera failed");
                }
            }
            DaemonAction::ShowScreen { device } => {
                let Some(entry) = mirrors.get(&device) else {
                    warn!(%device, "ShowScreen: no mirror entry (peer not connected?)");
                    continue;
                };
                {
                    let guard = entry.window.lock().expect("window slot poisoned");
                    if guard.is_some() {
                        debug!(%device, "ShowScreen: window already up");
                        continue;
                    }
                }
                // Open the outbound Input stream so pointer events
                // from the host window land on the peer.
                let conn = entry.conn.lock().expect("conn slot poisoned").clone();
                let input_tx = if let Some(conn) = conn {
                    match conn.open(StreamKind::Input).await {
                        Ok(stream) => {
                            let (tx, rx) = unbounded_channel::<InputMessage>();
                            tokio::spawn(input_writer_loop(stream, rx, device.clone()));
                            Some(tx)
                        }
                        Err(e) => {
                            warn!(%device, error = %e, "open outbound Input stream failed; window will be view-only");
                            None
                        }
                    }
                } else {
                    warn!(%device, "ShowScreen: no live connection; window will be view-only");
                    None
                };
                let slot = entry.slot.clone();
                let title = format!("ansync — {}", entry.peer_name);
                let handle = std::thread::Builder::new()
                    .name(format!("ansync-mirror-{device}"))
                    .spawn(move || {
                        if let Err(e) = ansync_video::sink_egui::run(title, slot, input_tx) {
                            warn!(error = %e, "mirror window exited with error");
                        }
                    })
                    .ok();
                *entry.window.lock().expect("window slot poisoned") = handle;
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
                let cameras = ctx.cameras.clone();
                let audios = ctx.audios.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(
                        conn,
                        peers,
                        permissions,
                        factory,
                        download_dir,
                        mirrors,
                        cameras,
                        audios,
                    )
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
    cameras: Arc<CameraRegistry>,
    audios: Arc<AudioRegistry>,
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

    let conn_arc = Arc::new(conn);

    // Ensure the mirror entry exists for this peer so the Video
    // stream loop can populate it as soon as the companion opens its
    // Video bidi stream.
    let mirror_entry = mirrors.ensure(&peer_id, &peer.name.0);
    *mirror_entry.conn.lock().expect("conn slot poisoned") = Some(conn_arc.clone());
    let camera_entry = cameras.ensure(&peer_id, &peer.name.0);
    let audio_entry = audios.ensure(&peer_id, &peer.name.0);

    // Auto-mount FUSE if the peer's `files_mount` flag is on. The
    // BackgroundSession is held on the stack so dropping it on
    // peer-disconnect umounts cleanly.
    let _fuse_session =
        maybe_auto_mount(&conn_arc, &peer_id, &peer.name, permissions.as_ref()).await;

    loop {
        let (kind, stream) = match conn_arc.accept().await {
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
            StreamKind::Camera => {
                let entry = camera_entry.clone();
                let pid = peer_id.clone();
                tokio::spawn(camera_stream_loop(stream, entry, pid));
            }
            StreamKind::Audio => {
                let entry = audio_entry.clone();
                let pid = peer_id.clone();
                let perms = permissions.clone();
                tokio::spawn(audio_inbound_loop(stream, entry, pid, perms));
            }
            StreamKind::Clipboard => {
                let pid = peer_id.clone();
                let perms = permissions.clone();
                tokio::spawn(clipboard_inbound_loop(stream, pid, perms));
            }
            other => {
                warn!(kind = ?other, "stream kind accepted but not wired yet — dropping");
                drop(stream);
            }
        }
    }
    input_session.lock().await.shutdown().await;
    // Clear the conn slot so ShowScreen won't try to open streams on
    // a closed connection until the peer reconnects.
    *mirror_entry.conn.lock().expect("conn slot poisoned") = None;
    // Camera pipeline is per-action; if it was running, kill its
    // task and unregister the sink so the v4l2 device is free.
    if let Some(handle) = camera_entry
        .handle
        .lock()
        .expect("handle slot poisoned")
        .take()
    {
        handle.abort();
    }
    *camera_entry.frame_tx.lock().expect("frame tx slot poisoned") = None;
    if let Some(sink) = camera_entry.sink.lock().await.take() {
        if let Err(e) = sink.unregister().await {
            warn!(%peer_id, error = %e, "camera sink unregister on disconnect failed");
        }
    }
    // Audio: tear down both directions if the peer drops mid-stream.
    if let Some(h) = audio_entry
        .pump_handle
        .lock()
        .expect("audio pump slot poisoned")
        .take()
    {
        h.abort();
    }
    if let Some(h) = audio_entry
        .inbound_handle
        .lock()
        .expect("audio inbound slot poisoned")
        .take()
    {
        h.abort();
    }
    *audio_entry
        .inbound_tx
        .lock()
        .expect("audio inbound tx poisoned") = None;
    *audio_entry.sink.lock().await = None;
    Ok(())
}

async fn camera_stream_loop(
    mut stream: QuicStream,
    entry: Arc<CameraEntry>,
    peer_id: DeviceId,
) {
    info!(%peer_id, "camera stream wired");
    loop {
        let bytes = match stream.recv().await {
            Ok(b) => b,
            Err(ansync_transport::TransportError::Closed) => {
                info!(%peer_id, "camera stream closed");
                return;
            }
            Err(e) => {
                warn!(%peer_id, error = %e, "camera stream recv error");
                return;
            }
        };
        let tx = match entry
            .frame_tx
            .lock()
            .expect("frame tx slot poisoned")
            .clone()
        {
            Some(tx) => tx,
            None => {
                // StartCamera hasn't fired yet (companion opened its
                // stream first) or StopCamera already cleared the
                // sender. Drop frames silently — when StartCamera
                // arrives it spawns a fresh decoder loop that picks
                // up subsequent frames.
                continue;
            }
        };
        if tx.send(bytes).is_err() {
            info!(%peer_id, "camera frame receiver dropped; exiting");
            return;
        }
    }
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

/// Read one `ClipboardMessage` per frame from the inbound stream and
/// stamp it into the host Wayland clipboard, gated by
/// `Permission::ClipboardIn`.
async fn clipboard_inbound_loop(
    mut stream: QuicStream,
    peer_id: DeviceId,
    permissions: Arc<dyn ansync_permissions::PermissionsStore>,
) {
    let backend = WaylandClipboard::new();
    loop {
        let bytes = match stream.recv().await {
            Ok(b) => b,
            Err(ansync_transport::TransportError::Closed) => return,
            Err(e) => {
                warn!(%peer_id, error = %e, "clipboard recv error");
                return;
            }
        };
        let msg: ClipboardMessage = match postcard::from_bytes(&bytes) {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "clipboard decode failed");
                continue;
            }
        };
        if !permissions
            .check(&peer_id, Permission::ClipboardIn)
            .await
            .unwrap_or(false)
        {
            warn!(%peer_id, "clipboard_in permission off; dropping inbound clipboard");
            continue;
        }
        let content = match msg {
            ClipboardMessage::Text { content } => ClipboardContent::Text(content),
            ClipboardMessage::Blob { mime, data } => ClipboardContent::Blob { mime, data },
        };
        if let Err(e) = backend.write(content).await {
            warn!(%peer_id, error = %e, "WaylandClipboard write failed");
        }
    }
}

/// Push the current host Wayland clipboard to `peer_id`, gated by
/// `Permission::ClipboardOut`. Exposed via the D-Bus
/// `Device.SyncClipboard` method.
async fn push_clipboard_to_peer(
    mirrors: &MirrorRegistry,
    permissions: &Arc<dyn ansync_permissions::PermissionsStore>,
    device: &DeviceId,
) -> Result<(), DaemonError> {
    if !permissions
        .check(device, Permission::ClipboardOut)
        .await
        .unwrap_or(false)
    {
        warn!(%device, "clipboard_out permission off; refusing SyncClipboard");
        return Ok(());
    }
    let mirror = mirrors
        .get(device)
        .ok_or_else(|| DaemonError::Startup(format!("no mirror entry for {device}")))?;
    let conn = mirror
        .conn
        .lock()
        .expect("conn slot poisoned")
        .clone()
        .ok_or_else(|| DaemonError::Startup(format!("peer {device} not connected")))?;
    let backend = WaylandClipboard::new();
    let content = match backend.read().await {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "WaylandClipboard read failed");
            return Ok(());
        }
    };
    let msg = match content {
        ClipboardContent::Text(s) => ClipboardMessage::Text { content: s },
        ClipboardContent::Blob { mime, data } => ClipboardMessage::Blob { mime, data },
    };
    let mut stream = conn.open(StreamKind::Clipboard).await?;
    let bytes = postcard::to_allocvec(&msg)
        .map_err(|e| DaemonError::Startup(format!("encode ClipboardMessage: {e}")))?;
    stream.send(bytes::Bytes::from(bytes)).await?;
    info!(%device, "host clipboard pushed");
    Ok(())
}

async fn handle_start_audio(
    mirrors: &MirrorRegistry,
    audios: &AudioRegistry,
    permissions: &Arc<dyn ansync_permissions::PermissionsStore>,
    device: &DeviceId,
    direction: AudioDirection,
) -> Result<(), DaemonError> {
    // Permission gates per direction. AudioIn = peer→host (mic
    // forwarding into host PipeWire), AudioOut = host→peer (host
    // capture going to the peer's speaker).
    let need_in = matches!(direction, AudioDirection::DeviceToHost | AudioDirection::Both);
    let need_out = matches!(direction, AudioDirection::HostToDevice | AudioDirection::Both);
    if need_in
        && !permissions
            .check(device, Permission::AudioIn)
            .await
            .unwrap_or(false)
    {
        warn!(%device, "audio_in permission off; refusing StartAudioRoute(DeviceToHost)");
        return Ok(());
    }
    if need_out
        && !permissions
            .check(device, Permission::AudioOut)
            .await
            .unwrap_or(false)
    {
        warn!(%device, "audio_out permission off; refusing StartAudioRoute(HostToDevice)");
        return Ok(());
    }
    let mirror = mirrors
        .get(device)
        .ok_or_else(|| DaemonError::Startup(format!("no mirror entry for {device}")))?;
    let conn = mirror
        .conn
        .lock()
        .expect("conn slot poisoned")
        .clone()
        .ok_or_else(|| DaemonError::Startup(format!("peer {device} not connected")))?;
    let entry = audios.ensure(device, &mirror.peer_name);

    // Send the control message so the companion knows which
    // direction to bring up on its side.
    let mut ctrl = conn.open(StreamKind::Control).await?;
    let env = Envelope {
        version: PROTOCOL_VERSION,
        message: Message::Control(ControlMessage::StartAudioRoute { direction }),
    };
    let bytes = postcard::to_allocvec(&env)
        .map_err(|e| DaemonError::Startup(format!("encode StartAudioRoute: {e}")))?;
    ctrl.send(bytes::Bytes::from(bytes)).await?;
    info!(%device, ?direction, "StartAudioRoute control sent");

    // Inbound: wait for the companion to open a StreamKind::Audio
    // back at us. `audio_inbound_loop` consumes it and pushes into
    // the entry's mpsc; `audio_render_loop` drains into the CpalSink.
    if need_in {
        let (tx, rx) = unbounded_channel::<bytes::Bytes>();
        *entry.inbound_tx.lock().expect("audio inbound tx poisoned") = Some(tx);
        let label = format!("ansync-in-{}", entry.peer_name);
        let format = AudioFormat {
            sample_rate: 48_000,
            channels: 2,
            format: SampleFormat::S16Le,
        };
        let sink = match CpalBackend::new().create_sink(&label, format).await {
            Ok(s) => Arc::new(tokio::sync::Mutex::new(s)),
            Err(e) => {
                warn!(%device, error = %e, "open CpalSink failed");
                return Ok(());
            }
        };
        *entry.sink.lock().await = Some(sink.clone());
        let handle = tokio::spawn(audio_render_loop(rx, sink));
        *entry
            .inbound_handle
            .lock()
            .expect("audio inbound slot poisoned") = Some(handle);
    }

    if need_out {
        let mut stream = conn.open(StreamKind::Audio).await?;
        let init = AudioStreamInit {
            sample_rate: 48_000,
            channels: 2,
            direction: AudioDirection::HostToDevice,
        };
        let header = postcard::to_allocvec(&init)
            .map_err(|e| DaemonError::Startup(format!("encode AudioStreamInit: {e}")))?;
        stream.send(bytes::Bytes::from(header)).await?;
        let label = format!("ansync-out-{}", entry.peer_name);
        let format = AudioFormat {
            sample_rate: 48_000,
            channels: 2,
            format: SampleFormat::S16Le,
        };
        let source = match CpalBackend::new().create_source(&label, format).await {
            Ok(s) => s,
            Err(e) => {
                warn!(%device, error = %e, "open CpalSource failed");
                return Ok(());
            }
        };
        let handle = tokio::spawn(audio_pump_loop(stream, source));
        *entry
            .pump_handle
            .lock()
            .expect("audio pump slot poisoned") = Some(handle);
    }
    Ok(())
}

async fn handle_stop_audio(audios: &AudioRegistry, device: &DeviceId) {
    let entry = match audios.get(device) {
        Some(e) => e,
        None => return,
    };
    if let Some(h) = entry
        .pump_handle
        .lock()
        .expect("audio pump slot poisoned")
        .take()
    {
        h.abort();
    }
    if let Some(h) = entry
        .inbound_handle
        .lock()
        .expect("audio inbound slot poisoned")
        .take()
    {
        h.abort();
    }
    *entry.inbound_tx.lock().expect("audio inbound tx poisoned") = None;
    *entry.sink.lock().await = None;
    info!(%device, "StopAudioRoute done");
}

async fn audio_render_loop(
    mut rx: UnboundedReceiver<bytes::Bytes>,
    sink: Arc<tokio::sync::Mutex<ansync_audio::CpalSink>>,
) {
    while let Some(bytes) = rx.recv().await {
        let mut guard = sink.lock().await;
        if let Err(e) = guard.write(bytes).await {
            warn!(error = %e, "audio_render_loop: sink write failed");
            return;
        }
    }
}

async fn audio_pump_loop(mut stream: QuicStream, mut source: ansync_audio::CpalSource) {
    loop {
        match source.read().await {
            Ok(bytes) => {
                if let Err(e) = stream.send(bytes).await {
                    warn!(error = %e, "audio_pump_loop: stream send failed");
                    return;
                }
            }
            Err(e) => {
                warn!(error = %e, "audio_pump_loop: source read failed");
                return;
            }
        }
    }
}

async fn audio_inbound_loop(
    mut stream: QuicStream,
    entry: Arc<AudioEntry>,
    peer_id: DeviceId,
    _permissions: Arc<dyn ansync_permissions::PermissionsStore>,
) {
    // First frame: the AudioStreamInit header. We log it but use the
    // host-side sink format the action handler already provisioned.
    let _header = match stream.recv().await {
        Ok(b) => b,
        Err(_) => {
            info!(%peer_id, "audio_inbound_loop: stream closed before header");
            return;
        }
    };
    info!(%peer_id, "audio inbound stream wired");
    loop {
        let bytes = match stream.recv().await {
            Ok(b) => b,
            Err(ansync_transport::TransportError::Closed) => {
                info!(%peer_id, "audio inbound stream closed");
                return;
            }
            Err(e) => {
                warn!(%peer_id, error = %e, "audio inbound recv error");
                return;
            }
        };
        let tx = match entry
            .inbound_tx
            .lock()
            .expect("audio inbound tx poisoned")
            .clone()
        {
            Some(tx) => tx,
            None => continue,
        };
        if tx.send(bytes).is_err() {
            info!(%peer_id, "audio inbound receiver dropped; exiting");
            return;
        }
    }
}

async fn handle_start_camera(
    mirrors: &MirrorRegistry,
    cameras: &CameraRegistry,
    permissions: &Arc<dyn ansync_permissions::PermissionsStore>,
    device: &DeviceId,
    config: CameraConfig,
) -> Result<(), DaemonError> {
    if !permissions
        .check(device, Permission::CameraVideo)
        .await
        .unwrap_or(false)
    {
        warn!(%device, "camera_video permission off; refusing StartCamera");
        return Ok(());
    }
    let mirror = mirrors
        .get(device)
        .ok_or_else(|| DaemonError::Startup(format!("no mirror entry for {device}")))?;
    let conn = mirror
        .conn
        .lock()
        .expect("conn slot poisoned")
        .clone()
        .ok_or_else(|| DaemonError::Startup(format!("peer {device} not connected")))?;
    let entry = cameras.ensure(device, &mirror.peer_name);

    // Tear down any previous pipeline before re-bootstrapping so a
    // second StartCamera with a different config doesn't leak a task.
    if let Some(handle) = entry.handle.lock().expect("handle slot poisoned").take() {
        handle.abort();
    }
    {
        let mut sink_guard = entry.sink.lock().await;
        if sink_guard.is_none() {
            let sink: Arc<dyn VirtualCameraSink> = build_camera_sink()?;
            *sink_guard = Some(sink);
        }
    }

    // Push the StartCamera control message to the companion. The
    // Control stream is opener-writes, so the host opens it for this
    // outbound message.
    let mut ctrl = conn.open(StreamKind::Control).await?;
    let env = Envelope {
        version: PROTOCOL_VERSION,
        message: Message::Control(ControlMessage::StartCamera(config.clone())),
    };
    let bytes = postcard::to_allocvec(&env)
        .map_err(|e| DaemonError::Startup(format!("encode StartCamera: {e}")))?;
    ctrl.send(bytes::Bytes::from(bytes)).await?;
    info!(%device, camera = %config.camera_id, "StartCamera control sent");

    // Spawn the decode → sink loop. The companion will open
    // `StreamKind::Camera` in response; we wait for it on the
    // per-peer accept loop and dispatch into a temporary channel.
    let entry_clone = entry.clone();
    let codec = match config.codec {
        ProtoVideoCodec::H264 => VideoCodec::H264,
        ProtoVideoCodec::H265 => VideoCodec::H265,
    };
    let width = config.width;
    let height = config.height;
    let (frame_tx, frame_rx) = unbounded_channel::<bytes::Bytes>();
    *entry.frame_tx.lock().expect("frame tx slot poisoned") = Some(frame_tx);
    let handle = tokio::spawn(camera_decode_loop(
        entry_clone,
        codec,
        width,
        height,
        frame_rx,
    ));
    *entry.handle.lock().expect("handle slot poisoned") = Some(handle);
    Ok(())
}

async fn handle_stop_camera(
    cameras: &CameraRegistry,
    device: &DeviceId,
) -> Result<(), DaemonError> {
    let entry = match cameras.get(device) {
        Some(e) => e,
        None => return Ok(()),
    };
    if let Some(handle) = entry.handle.lock().expect("handle slot poisoned").take() {
        handle.abort();
    }
    *entry.frame_tx.lock().expect("frame tx slot poisoned") = None;
    let mut sink_guard = entry.sink.lock().await;
    if let Some(sink) = sink_guard.take() {
        if let Err(e) = sink.unregister().await {
            warn!(%device, error = %e, "camera sink unregister failed");
        }
    }
    info!(%device, "StopCamera done");
    Ok(())
}

fn build_camera_sink() -> Result<Arc<dyn VirtualCameraSink>, DaemonError> {
    Ok(Arc::new(ansync_camera::V4l2LoopbackSink::new()))
}

async fn camera_decode_loop(
    entry: Arc<CameraEntry>,
    codec: VideoCodec,
    width: u32,
    height: u32,
    mut frame_rx: UnboundedReceiver<bytes::Bytes>,
) {
    let mut decoder = match HostDecoder::configure(codec, width, height) {
        Ok(d) => d,
        Err(e) => {
            warn!(error = %e, "camera decoder unavailable; aborting");
            return;
        }
    };
    // Lazy-register the sink on the first decoded frame so we know the
    // actual frame dimensions (decoder may re-derive from SPS).
    let mut sink_registered = false;
    while let Some(bytes) = frame_rx.recv().await {
        if let Err(e) = decoder.feed(bytes).await {
            warn!(error = %e, "camera decoder feed failed; continuing");
            continue;
        }
        let frame = match decoder.take().await {
            Ok(Some(f)) => f,
            Ok(None) => continue,
            Err(e) => {
                warn!(error = %e, "camera decoder take failed");
                continue;
            }
        };
        let pixel = match frame.format {
            PixelFormat::Nv12 => CameraPixelFormat::Nv12,
            PixelFormat::I420 => CameraPixelFormat::Nv12,
            // BGRA / RGBA decoders should be exceedingly rare for
            // companion camera streams (Android encodes NV12 from the
            // HW pipeline); if it does happen we drop the frame
            // rather than do a CPU repack — v4l2loopback wouldn't
            // present BGRA correctly to most consumers anyway.
            PixelFormat::Bgra8 | PixelFormat::Rgba8 => {
                warn!("camera decoder emitted packed RGB; dropping (sink wants NV12)");
                continue;
            }
        };
        let sink = match entry.sink.lock().await.clone() {
            Some(s) => s,
            None => {
                warn!("camera sink missing while decoder running; bailing");
                return;
            }
        };
        if !sink_registered {
            let fmt = CameraFormat {
                width: frame.width,
                height: frame.height,
                fps: 30,
                pixel_format: pixel,
            };
            if let Err(e) = sink.register(&entry.peer_name, fmt).await {
                warn!(error = %e, "camera sink register failed");
                return;
            }
            sink_registered = true;
        }
        if let Err(e) = sink.write_frame(frame.data).await {
            warn!(error = %e, "camera sink write_frame failed");
        }
    }
    info!(name = %entry.peer_name, "camera_decode_loop: channel closed");
}

async fn input_writer_loop(
    mut stream: QuicStream,
    mut rx: UnboundedReceiver<InputMessage>,
    peer_id: DeviceId,
) {
    while let Some(msg) = rx.recv().await {
        let bytes = match postcard::to_allocvec(&msg) {
            Ok(b) => b,
            Err(e) => {
                warn!(%peer_id, error = %e, "input postcard encode failed");
                continue;
            }
        };
        if let Err(e) = stream.send(bytes::Bytes::from(bytes)).await {
            warn!(%peer_id, error = %e, "input writer stream send failed; exiting");
            return;
        }
    }
    info!(%peer_id, "input writer channel closed");
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
