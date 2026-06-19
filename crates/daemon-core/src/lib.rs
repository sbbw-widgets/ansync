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
use ansync_dbus::{ConnState, DaemonAction, DaemonState, Device, serve};
use ansync_discovery::{Discovery, MdnsDiscovery};
use ansync_files::{AutoAcceptPolicy, receive_file};
use ansync_input::{InputDeviceFactory, InputSession, UinputFactory};
use ansync_pairing::PeerStore;
use ansync_permissions::FilePermissionsStore;
use ansync_proto::{
    AudioDirection, AudioStreamInit, CameraConfig, ClipboardMessage, ControlMessage, Envelope,
    Hello, InputMessage, Message, NotificationMessage, PROTOCOL_VERSION,
    VideoCodec as ProtoVideoCodec,
};
use ansync_transport::pinning::TrustedPeers;
use ansync_transport::{
    Connection, QuicConnection, QuicServer, QuicStream, QuicTransport, Stream as _, StreamKind,
};
use ansync_video::{DecodedFrame, HostDecoder, PixelFormat, VideoCodec, VideoDecoder};

mod mirror_subprocess;
use mirror_subprocess::spawn_mirror_subprocess;
use directories::BaseDirs;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::Mutex;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputBackend {
    /// Local kernel uinput devices. The default; works on every
    /// Linux host with the `uinput` module loaded + a udev rule that
    /// lets the daemon's user write `/dev/uinput`.
    Uinput,
    /// Bluetooth HID Device. Turns the host into a BT-HID emitter
    /// the peer (or any other paired host) consumes. Requires BlueZ
    /// running + the adapter powered. SDP profile registration is
    /// best-effort — see `ansync_input::bt_hid` for caveats.
    BtHid,
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
    /// Which `InputDeviceFactory` to plug into the per-peer input
    /// session. Defaults to `Uinput`.
    pub input_backend: InputBackend,
}

impl DaemonConfig {
    pub fn new(device_name: String) -> Self {
        Self {
            device_name,
            identity_path: None,
            peers_dir: None,
            permissions_dir: None,
            // Fixed default port so cached companion endpoints
            // (PREF_HOST_ADDR) survive daemon restarts. Override
            // with `DaemonConfig.listen_addr` for tests / multi-host.
            // 47215 picked from the IANA "user-assignable" range,
            // unlikely to clash with anything common.
            listen_addr: "0.0.0.0:47215".parse().expect("hard-coded addr parses"),
            download_dir: None,
            capabilities: Capabilities::INPUT_FROM_DEV
                | Capabilities::FILES
                | Capabilities::CAMERA_VIDEO
                | Capabilities::AUDIO_IN
                | Capabilities::AUDIO_OUT
                | Capabilities::MIC
                | Capabilities::CLIPBOARD,
            input_backend: InputBackend::Uinput,
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

        let local_endpoints: Vec<(String, u16)> = enumerate_lan_ipv4()
            .into_iter()
            .map(|ip| (ip, listen.port()))
            .collect();
        info!(?local_endpoints, "LAN endpoints for direct-dial fallback");

        let (action_tx, action_rx) = unbounded_channel::<DaemonAction>();
        let state = Arc::new(
            DaemonState::new(
                identity.clone(),
                self.config.device_name.clone(),
                peers.clone(),
                permissions.clone(),
            )
            .with_actions(action_tx.clone()),
        );
        if let Ok(mut g) = state.listen_endpoints.lock() {
            *g = local_endpoints;
        }

        let dbus_conn = serve(state.clone()).await?;
        info!(service = ansync_dbus::SERVICE_NAME, "D-Bus surface ready");
        let dbus_conn_arc = Arc::new(dbus_conn);

        let mirrors = Arc::new(MirrorRegistry::default());
        let cameras = Arc::new(CameraRegistry::default());
        let audios = Arc::new(AudioRegistry::default());
        let clipboard_sync = ClipboardSync::default();
        let action_handle = tokio::spawn(action_loop(
            action_rx,
            action_tx.clone(),
            mirrors.clone(),
            cameras.clone(),
            audios.clone(),
            permissions.clone(),
            clipboard_sync.clone(),
            dbus_conn_arc.clone(),
        ));

        let device_name = DeviceName(self.config.device_name.clone());
        mdns.announce(&device_name, listen.port(), self.config.capabilities)
            .await?;
        info!(name = %device_name, port = listen.port(), "mDNS announce active");

        let factory: Arc<dyn InputDeviceFactory> = match self.config.input_backend {
            InputBackend::Uinput => Arc::new(UinputFactory),
            InputBackend::BtHid => {
                #[cfg(feature = "bt-hid")]
                {
                    Arc::new(ansync_input::BtHidFactory::new())
                }
                #[cfg(not(feature = "bt-hid"))]
                {
                    return Err(DaemonError::Startup(
                        "input_backend = BtHid requires the `bt-hid` feature".into(),
                    ));
                }
            }
        };
        // RAM diagnostic: print VmRSS every 30 s under
        // `RUST_LOG=ansync_daemon_core=debug`. Useful for telling
        // allocator-side fragmentation ("RSS climbs then plateaus")
        // apart from a real leak ("RSS climbs without bound"). The
        // task is fire-and-forget — there's no shutdown handshake
        // because reading `/proc/self/status` has no resource to
        // release on drop.
        let mem_stats_handle = tokio::spawn(mem_stats_loop());
        let watcher_handle = tokio::spawn(companion_watcher(
            state.clone(),
            dbus_conn_arc.clone(),
        ));
        let clip_watcher_handle = tokio::spawn(host_clipboard_watcher(
            mirrors.clone(),
            permissions.clone(),
            clipboard_sync.clone(),
        ));
        let accept_handle = tokio::spawn(accept_loop(AcceptCtx {
            server,
            peers,
            permissions: permissions.clone(),
            factory,
            download_dir,
            mirrors: mirrors.clone(),
            cameras: cameras.clone(),
            audios: audios.clone(),
            dbus_conn: dbus_conn_arc.clone(),
            device_name: self.config.device_name.clone(),
            capabilities: self.config.capabilities,
            identity: identity.clone(),
            dbus_state: state.clone(),
            clipboard_sync: clipboard_sync.clone(),
        }));

        wait_for_shutdown().await?;

        accept_handle.abort();
        action_handle.abort();
        mem_stats_handle.abort();
        watcher_handle.abort();
        clip_watcher_handle.abort();
        if let Err(e) = mdns.stop_announce().await {
            warn!(error = %e, "mDNS stop_announce failed");
        }
        drop(dbus_conn_arc);
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
    dbus_conn: Arc<zbus::Connection>,
    /// Local host's human-readable name (e.g. `gethostname(2)` output).
    /// Sent verbatim on the outbound `StreamKind::Hello` so the peer
    /// can surface "connected to <name>" instead of a pubkey prefix.
    device_name: String,
    capabilities: Capabilities,
    /// Our long-term identity. Only the public-key prefix is sent on
    /// the wire (in the Hello frame so the peer can recover our
    /// `DeviceId` for its own permission lookups).
    identity: IdentityKeypair,
    /// Shared with the D-Bus surface so transitions emit
    /// `Device.State` PropertiesChanged + `Manager.DeviceConnectivityChanged`.
    dbus_state: Arc<DaemonState>,
    /// Echo-loop guard shared with the host clipboard watcher (see
    /// [`ClipboardSync`]).
    clipboard_sync: ClipboardSync,
}

/// Per-peer mirror state. The window itself lives in a subprocess so
/// each ShowScreen gets a fresh `EventLoop::build` and the user can
/// close + reopen without winit's once-per-process guard blocking us.
#[derive(Default)]
pub struct MirrorRegistry {
    entries: StdMutex<HashMap<DeviceId, Arc<MirrorEntry>>>,
}

pub struct MirrorEntry {
    pub peer_name: String,
    /// `Some` while the peer is connected. `action_loop` reads this
    /// to open an outbound Input stream on ShowScreen. Cleared by
    /// `handle_connection` on disconnect.
    pub conn: StdMutex<Option<Arc<QuicConnection>>>,
    /// `Some` while a mirror subprocess for this peer is alive.
    /// `video_stream_loop` writes inbound encoded chunks here so the
    /// renderer subprocess can decode them; on `None` the chunks are
    /// dropped (no open window).
    pub video_tx: StdMutex<Option<UnboundedSender<bytes::Bytes>>>,
    /// `Some` while the subprocess is alive — holds the child handle
    /// and the sender wired to the input bridge.
    pub subprocess: StdMutex<Option<MirrorSubprocess>>,
    /// Flipped to `true` by `video_stream_loop` on first chunk;
    /// cleared on stream close. ShowScreen consults this so the
    /// QSTile-driven path doesn't trigger a second
    /// `RequestScreenCapture` (which re-pops the MediaProjection
    /// picker on the device).
    pub video_inbound: std::sync::atomic::AtomicBool,
}

/// Handle to a running mirror subprocess. The child itself is
/// awaited inside `mirror_subprocess::spawn_mirror_subprocess`'s wait
/// task; this struct only carries the pieces the action loop needs
/// to request a graceful shutdown.
pub struct MirrorSubprocess {
    /// `Some` so we can ask the child to exit cleanly via a
    /// `HostMsg::Shutdown` frame on stdin. Cleared after a successful
    /// shutdown so a follow-up HideScreen doesn't re-send.
    pub host_tx: Option<UnboundedSender<ansync_video::ipc::HostMsg>>,
    /// Best-effort PID for tracing diagnostics.
    pub pid: u32,
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
    /// Which D-Bus `StreamStateChanged` tile this inbound route maps
    /// to — `"mic"` for `StartMicrophone`, `"audio"` for the generic
    /// `StartAudioRoute`. Read by `audio_inbound_loop` on exit so a
    /// companion-side teardown fires the right tile-off signal.
    inbound_tile_kind: StdMutex<Option<&'static str>>,
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
                    inbound_tile_kind: StdMutex::new(None),
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

/// Shared SHA-256 fingerprint of the most recent clipboard payload
/// either side has handled. Used to short-circuit the host
/// `wlr_data_control` watcher ↔ companion `OnPrimaryClipChangedListener`
/// echo loop:
///
///   1. User copies in app X → host watcher fires → daemon reads
///      content + fingerprint(F) → store F in [`ClipboardSync`] →
///      push to companion.
///   2. Companion `setPrimaryClip` → Android listener fires → pushes
///      back to host.
///   3. Host `clipboard_inbound_loop` receives → fingerprint(F') → if
///      `F' == F` (which is normal in the round-trip case) skip the
///      `wl-clipboard-rs::copy` write. That write would otherwise
///      spawn a fresh selection holder process which *steals*
///      ownership from app X, breaking the user's ability to paste
///      rich MIMEs back into anything else.
///
/// Cleared implicitly by the next legitimate change in either
/// direction. Read+update are non-atomic on purpose — racing between
/// rapid clipboard changes is fine because the worst case is a single
/// extra push, not a divergent state.
#[derive(Default, Clone)]
pub struct ClipboardSync {
    last: Arc<StdMutex<Option<String>>>,
}

impl ClipboardSync {
    pub fn matches(&self, fp: &str) -> bool {
        self.last
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .as_deref()
            == Some(fp)
    }

    pub fn set(&self, fp: String) {
        if let Ok(mut g) = self.last.lock() {
            *g = Some(fp);
        }
    }
}

pub fn clipboard_fingerprint(content: &ClipboardContent) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    match content {
        ClipboardContent::Text(s) => {
            h.update(b"t:");
            h.update(s.as_bytes());
        }
        ClipboardContent::Blob { mime, data } => {
            h.update(b"b:");
            h.update(mime.as_bytes());
            h.update(b"\0");
            h.update(data);
        }
    }
    format!("{:x}", h.finalize())
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
                    peer_name: name.to_string(),
                    conn: StdMutex::new(None),
                    video_tx: StdMutex::new(None),
                    subprocess: StdMutex::new(None),
                    video_inbound: std::sync::atomic::AtomicBool::new(false),
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

    /// Snapshot all currently-tracked entries. Used by the host
    /// clipboard watcher to fan auto-push events out to every peer
    /// that has an active QUIC connection.
    pub fn entries(&self) -> Vec<(DeviceId, Arc<MirrorEntry>)> {
        self.entries
            .lock()
            .ok()
            .map(|e| e.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }
}

async fn action_loop(
    mut rx: UnboundedReceiver<DaemonAction>,
    self_tx: tokio::sync::mpsc::UnboundedSender<DaemonAction>,
    mirrors: Arc<MirrorRegistry>,
    cameras: Arc<CameraRegistry>,
    audios: Arc<AudioRegistry>,
    permissions: Arc<dyn ansync_permissions::PermissionsStore>,
    clipboard_sync: ClipboardSync,
    dbus_conn: Arc<zbus::Connection>,
) {
    while let Some(action) = rx.recv().await {
        match action {
            DaemonAction::StartAudioRoute { device, direction } => {
                match handle_start_audio(
                    &mirrors,
                    &audios,
                    &permissions,
                    &device,
                    direction,
                    "audio",
                )
                .await
                {
                    Ok(()) => emit_stream(&dbus_conn, &device, "audio", true).await,
                    Err(e) => warn!(%device, error = %e, "StartAudioRoute failed"),
                }
            }
            DaemonAction::StopAudioRoute { device } => {
                handle_stop_audio(&audios, &device).await;
                emit_stream(&dbus_conn, &device, "audio", false).await;
            }
            DaemonAction::StartMicrophone { device } => {
                match handle_start_audio(
                    &mirrors,
                    &audios,
                    &permissions,
                    &device,
                    AudioDirection::DeviceToHost,
                    "mic",
                )
                .await
                {
                    Ok(()) => emit_stream(&dbus_conn, &device, "mic", true).await,
                    Err(e) => warn!(%device, error = %e, "StartMicrophone failed"),
                }
            }
            DaemonAction::StopMicrophone { device } => {
                handle_stop_audio(&audios, &device).await;
                emit_stream(&dbus_conn, &device, "mic", false).await;
            }
            DaemonAction::SyncClipboard { device } => {
                if let Err(e) =
                    push_clipboard_to_peer(&mirrors, &permissions, &device, &clipboard_sync).await
                {
                    warn!(%device, error = %e, "SyncClipboard failed");
                }
            }
            DaemonAction::StartCamera { device, config } => {
                match handle_start_camera(&mirrors, &cameras, &permissions, &device, config).await
                {
                    Ok(()) => emit_stream(&dbus_conn, &device, "camera", true).await,
                    Err(e) => warn!(%device, error = %e, "StartCamera failed"),
                }
            }
            DaemonAction::StopCamera { device } => {
                match handle_stop_camera(&mirrors, &cameras, &device).await {
                    Ok(()) => emit_stream(&dbus_conn, &device, "camera", false).await,
                    Err(e) => warn!(%device, error = %e, "StopCamera failed"),
                }
            }
            DaemonAction::ShowScreen { device } => {
                let Some(entry) = mirrors.get(&device) else {
                    warn!(%device, "ShowScreen: no mirror entry (peer not connected?)");
                    continue;
                };
                // Idempotent: if a renderer is already up for this
                // peer, only refresh the input pipe + (maybe) re-ask
                // the companion to start capture.
                let already_up = entry
                    .subprocess
                    .lock()
                    .expect("subprocess slot poisoned")
                    .is_some();

                // Open the outbound Input stream so renderer-side
                // pointer/keyboard/gamepad events reach the peer.
                let conn = entry.conn.lock().expect("conn slot poisoned").clone();
                let input_tx = if let Some(conn) = conn.clone() {
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

                // Only ask the companion to start capture when the
                // tile-driven path hasn't already opened the Video
                // stream — otherwise we re-pop the MediaProjection
                // picker on the device.
                if !entry.video_inbound.load(std::sync::atomic::Ordering::Relaxed) {
                    if let Some(conn) = conn {
                        if let Err(e) = send_request_capture(&conn).await {
                            warn!(%device, error = %e, "RequestScreenCapture send failed");
                        }
                    }
                }

                if already_up {
                    info!(%device, "ShowScreen: subprocess already up");
                    // The existing subprocess keeps consuming chunks
                    // from `entry.video_tx`. Input pipe was per-conn
                    // and we already swapped a fresh one above (it'll
                    // simply replace whatever the previous Input
                    // writer was using on next user input).
                    let _ = input_tx;
                    continue;
                }

                let title = format!("ansync — {}", entry.peer_name);
                let exit_tx = self_tx.clone();
                let exit_device = device.clone();
                let entry_for_spawn = entry.clone();
                let entry_for_exit = entry.clone();
                match spawn_mirror_subprocess(
                    title,
                    entry_for_spawn,
                    input_tx,
                    move || {
                        // Renderer exited (user closed the window or
                        // process crashed). Clear the slots
                        // synchronously BEFORE enqueueing HideScreen
                        // so a fast-following ShowScreen sees a clean
                        // subprocess slot — otherwise the
                        // `already_up` check races against the
                        // queued HideScreen and silently swallows the
                        // user's reopen.
                        *entry_for_exit
                            .subprocess
                            .lock()
                            .expect("subprocess slot poisoned") = None;
                        *entry_for_exit
                            .video_tx
                            .lock()
                            .expect("video_tx slot poisoned") = None;
                        let _ = exit_tx.send(DaemonAction::HideScreen {
                            device: exit_device.clone(),
                        });
                    },
                )
                .await
                {
                    Ok(()) => {
                        info!(%device, "ShowScreen: subprocess spawned");
                        emit_stream(&dbus_conn, &device, "screen", true).await;
                    }
                    Err(e) => warn!(%device, error = %e, "ShowScreen subprocess failed"),
                }
            }
            DaemonAction::HideScreen { device } => {
                let Some(entry) = mirrors.get(&device) else {
                    continue;
                };
                // Pull the subprocess handle out and ask it to exit.
                // Drop the video fan-out sender so any in-flight
                // chunks stop reaching the (about-to-die) child.
                {
                    let mut tx_slot = entry.video_tx.lock().expect("video_tx slot poisoned");
                    *tx_slot = None;
                }
                let taken = entry
                    .subprocess
                    .lock()
                    .expect("subprocess slot poisoned")
                    .take();
                if let Some(mut sp) = taken {
                    if let Some(tx) = sp.host_tx.take() {
                        let _ = tx.send(ansync_video::ipc::HostMsg::Shutdown);
                    }
                    // Closing host_tx → writer task drops child_stdin
                    // → renderer sees EOF → exits. `kill_on_drop` on
                    // the child handle in `spawn_mirror_subprocess`
                    // is the safety net if the renderer hangs.
                }
                // Tell the companion to drop the encoder + projection
                // too so the device's foreground notification clears.
                let conn = entry.conn.lock().expect("conn slot poisoned").clone();
                if let Some(conn) = conn {
                    if let Err(e) = send_stop_capture(&conn).await {
                        warn!(%device, error = %e, "StopScreenCapture send failed");
                    }
                }
                info!(%device, "HideScreen: subprocess down, companion notified");
                emit_stream(&dbus_conn, &device, "screen", false).await;
            }
        }
    }
}

/// Fire-and-forget wrapper around `Device::emit_stream_state` so the
/// action loop doesn't have to spell out the error path on every
/// call. Failures (peer never connected → no D-Bus path) are logged
/// at debug since they're benign.
async fn emit_stream(
    conn: &Arc<zbus::Connection>,
    device: &DeviceId,
    kind: &str,
    active: bool,
) {
    if let Err(e) =
        ansync_dbus::Device::emit_stream_state(conn.as_ref(), device, kind, active).await
    {
        debug!(%device, kind, active, error = %e, "emit_stream_state failed");
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
                let dbus_conn = ctx.dbus_conn.clone();
                let device_name = ctx.device_name.clone();
                let capabilities = ctx.capabilities;
                let identity = ctx.identity.clone();
                let dbus_state = ctx.dbus_state.clone();
                let clipboard_sync = ctx.clipboard_sync.clone();
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
                        dbus_conn,
                        device_name,
                        capabilities,
                        identity,
                        dbus_state,
                        clipboard_sync,
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
    dbus_conn: Arc<zbus::Connection>,
    device_name: String,
    capabilities: Capabilities,
    identity: IdentityKeypair,
    dbus_state: Arc<DaemonState>,
    clipboard_sync: ClipboardSync,
) -> Result<(), DaemonError> {
    let pubkey = conn.peer_identity().as_bytes();
    let mut id_bytes = [0u8; 16];
    id_bytes.copy_from_slice(&pubkey[..16]);
    let peer_id = DeviceId(id_bytes);
    let peer = peers.get(&peer_id)?;
    info!(peer = %peer.name, %peer_id, "peer connected");

    if let Err(e) =
        Device::emit_state_changed(&dbus_conn, &dbus_state, &peer_id, ConnState::Authenticated)
            .await
    {
        warn!(%peer_id, error = %e, "emit Authenticated state failed");
    }

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
    // Video bidi stream. If a stale conn for the same peer is still
    // registered (companion redialed before keep-alive killed the
    // old session) close it explicitly here so its accept loop
    // unblocks and exits — preventing two concurrent
    // `handle_connection` tasks racing on the same per-peer
    // registries.
    let mirror_entry = mirrors.ensure(&peer_id, &peer.name.0);
    let prior = {
        let mut slot = mirror_entry.conn.lock().expect("conn slot poisoned");
        let prior = slot.take();
        *slot = Some(conn_arc.clone());
        prior
    };
    if let Some(prev) = prior {
        info!(%peer_id, "evicting stale conn for redialed peer");
        let _ = prev.close("superseded by redial").await;
    }
    let camera_entry = cameras.ensure(&peer_id, &peer.name.0);
    let audio_entry = audios.ensure(&peer_id, &peer.name.0);

    // Send our Hello on a dedicated one-shot stream so the peer
    // refreshes its cached name + capability bitmap for this session.
    // Done after registering the mirror entry so a fast-following
    // ShowScreen sees the live conn slot.
    match send_hello(&conn_arc, &peer_id, &identity, &device_name, capabilities).await {
        Ok(()) => {
            if let Err(e) = Device::emit_state_changed(
                &dbus_conn,
                &dbus_state,
                &peer_id,
                ConnState::Active,
            )
            .await
            {
                warn!(%peer_id, error = %e, "emit Active state failed");
            }
        }
        Err(e) => {
            warn!(%peer_id, error = %e, "outbound Hello failed; peer will keep stale name");
        }
    }

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
                let entry = mirror_entry.clone();
                let pid = peer_id.clone();
                entry
                    .video_inbound
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                let action_tx = dbus_state.actions.clone();
                // No auto-ShowScreen here on purpose. The user owns
                // the decision to open the mirror window — D-Bus
                // ShowScreen from DMS (or the CLI). Inbound Video
                // without an active subprocess gets fanned to
                // `entry.video_tx == None` and dropped, which is what
                // we want: the daemon shouldn't pop a window on its
                // own just because the phone started capturing.
                tokio::spawn(video_stream_loop(stream, entry, pid, action_tx));
            }
            StreamKind::Camera => {
                let entry = camera_entry.clone();
                let pid = peer_id.clone();
                let dbus = dbus_conn.clone();
                tokio::spawn(camera_stream_loop(stream, entry, pid, dbus));
            }
            StreamKind::Audio => {
                let entry = audio_entry.clone();
                let pid = peer_id.clone();
                let perms = permissions.clone();
                let dbus = dbus_conn.clone();
                tokio::spawn(audio_inbound_loop(stream, entry, pid, perms, dbus));
            }
            StreamKind::Clipboard => {
                let pid = peer_id.clone();
                let perms = permissions.clone();
                let sync = clipboard_sync.clone();
                tokio::spawn(clipboard_inbound_loop(stream, pid, perms, sync));
            }
            StreamKind::Notifications => {
                let pid = peer_id.clone();
                let perms = permissions.clone();
                let conn = dbus_conn.clone();
                tokio::spawn(notification_inbound_loop(stream, pid, perms, conn));
            }
            StreamKind::Hello => {
                let pid = peer_id.clone();
                let store = peers.clone();
                tokio::spawn(hello_inbound_loop(stream, pid, store));
            }
            other => {
                warn!(kind = ?other, "stream kind accepted but not wired yet — dropping");
                drop(stream);
            }
        }
    }
    input_session.lock().await.shutdown().await;
    // Only clear the conn slot + emit Disconnected when the conn we
    // just lost is the one currently registered for this peer. When
    // the companion races a redial against our keep-alive (rare but
    // visible — produces two `peer connected` log lines and two
    // concurrent `handle_connection` tasks) the stale task's exit
    // would otherwise wipe the live conn out from under the other
    // task and the UI would flap to Disconnected then back to Active.
    let conn_still_current = mirror_entry
        .conn
        .lock()
        .expect("conn slot poisoned")
        .as_ref()
        .map(|c| Arc::ptr_eq(c, &conn_arc))
        .unwrap_or(false);
    if conn_still_current {
        *mirror_entry.conn.lock().expect("conn slot poisoned") = None;
    }
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
    if conn_still_current {
        if let Err(e) = Device::emit_state_changed(
            &dbus_conn,
            &dbus_state,
            &peer_id,
            ConnState::Disconnected,
        )
        .await
        {
            warn!(%peer_id, error = %e, "emit Disconnected state failed");
        }
    } else {
        debug!(
            %peer_id,
            "handle_connection exit: superseded by a newer conn — skip Disconnected emit"
        );
    }
    Ok(())
}

async fn camera_stream_loop(
    mut stream: QuicStream,
    entry: Arc<CameraEntry>,
    peer_id: DeviceId,
    dbus_conn: Arc<zbus::Connection>,
) {
    info!(%peer_id, "camera stream wired");
    // Fire `StreamStateChanged("camera", false)` if the stream dies
    // while the daemon still thought the route was up (companion-side
    // tile flip, MediaCodec crash, projection revoked). Daemon-initiated
    // StopCamera already cleared `frame_tx` AND emitted, so the guard
    // becomes a no-op there.
    struct CameraExitGuard {
        entry: Arc<CameraEntry>,
        peer_id: DeviceId,
        dbus_conn: Arc<zbus::Connection>,
    }
    impl Drop for CameraExitGuard {
        fn drop(&mut self) {
            let still_active = {
                let mut slot = self.entry.frame_tx.lock().expect("frame tx slot poisoned");
                if slot.is_some() {
                    *slot = None;
                    true
                } else {
                    false
                }
            };
            if !still_active {
                return;
            }
            let conn = self.dbus_conn.clone();
            let device = self.peer_id.clone();
            tokio::spawn(async move {
                emit_stream(&conn, &device, "camera", false).await;
            });
        }
    }
    let _guard = CameraExitGuard {
        entry: entry.clone(),
        peer_id: peer_id.clone(),
        dbus_conn,
    };
    loop {
        let bytes = match stream.recv().await {
            Ok(b) => b,
            Err(ansync_transport::TransportError::Closed) => {
                info!(%peer_id, "camera stream closed");
                return;
            }
            Err(e) => {
                // StopCamera path: companion drops its end of the stream
                // and quinn surfaces that as `early eof` on the next
                // recv. The local side already cleared `frame_tx` so we
                // know this is a graceful teardown — keep the log at
                // info to avoid scaring the user.
                let graceful = entry
                    .frame_tx
                    .lock()
                    .expect("frame tx slot poisoned")
                    .is_none();
                if graceful {
                    info!(%peer_id, "camera stream closed by peer");
                } else {
                    warn!(%peer_id, error = %e, "camera stream recv error");
                }
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

/// Open a one-shot `StreamKind::Hello` outbound, send the local Hello
/// envelope, drop the stream. The peer side reads it via
/// `hello_inbound_loop`.
async fn send_hello(
    conn: &QuicConnection,
    peer_id: &DeviceId,
    identity: &IdentityKeypair,
    device_name: &str,
    capabilities: Capabilities,
) -> Result<(), DaemonError> {
    let pk = identity.public().as_bytes();
    let mut our_id_bytes = [0u8; 16];
    our_id_bytes.copy_from_slice(&pk[..16]);
    let env = Envelope {
        version: PROTOCOL_VERSION,
        message: Message::Hello(Hello {
            device_id: DeviceId(our_id_bytes),
            name: DeviceName(device_name.to_string()),
            capabilities,
        }),
    };
    let bytes = postcard::to_allocvec(&env)
        .map_err(|e| DaemonError::Startup(format!("encode Hello: {e}")))?;
    let mut stream = conn.open(StreamKind::Hello).await?;
    stream.send(bytes::Bytes::from(bytes)).await?;
    // Closing the send half tells the peer "no more frames coming".
    // quinn drops the rest on connection close.
    let _ = stream.finish().await;
    debug!(%peer_id, "outbound Hello sent");
    Ok(())
}

/// Push `ControlMessage::RequestScreenCapture` to the companion.
/// One-shot — the stream is dropped after the frame so each call
/// stands on its own (matches how the companion's `control_recv_loop`
/// treats Control: stream-per-message, no per-stream state).
async fn send_request_capture(conn: &QuicConnection) -> Result<(), DaemonError> {
    send_control(conn, ControlMessage::RequestScreenCapture).await
}

/// Inverse of [`send_request_capture`].
async fn send_stop_capture(conn: &QuicConnection) -> Result<(), DaemonError> {
    send_control(conn, ControlMessage::StopScreenCapture).await
}

/// One-shot Control envelope sender. Used by anything in the
/// `action_loop` that needs to ask the companion to do something
/// without opening a long-lived stream.
async fn send_control(
    conn: &QuicConnection,
    message: ControlMessage,
) -> Result<(), DaemonError> {
    let env = Envelope {
        version: PROTOCOL_VERSION,
        message: Message::Control(message),
    };
    let bytes = postcard::to_allocvec(&env)
        .map_err(|e| DaemonError::Startup(format!("encode Control: {e}")))?;
    let mut stream = conn.open(StreamKind::Control).await?;
    stream.send(bytes::Bytes::from(bytes)).await?;
    let _ = stream.finish().await;
    Ok(())
}

/// Consume the single Hello frame from a freshly accepted inbound
/// stream and refresh `StoredPeer.name` if the peer's self-reported
/// name has changed since pairing.
async fn hello_inbound_loop(mut stream: QuicStream, peer_id: DeviceId, peers: PeerStore) {
    let bytes = match stream.recv().await {
        Ok(b) => b,
        Err(e) => {
            warn!(%peer_id, error = %e, "Hello recv failed");
            return;
        }
    };
    let env: Envelope = match postcard::from_bytes(&bytes) {
        Ok(e) => e,
        Err(e) => {
            warn!(%peer_id, error = %e, "Hello postcard decode failed");
            return;
        }
    };
    let hello = match env.message {
        Message::Hello(h) => h,
        other => {
            warn!(%peer_id, ?other, "Hello stream carried non-Hello envelope");
            return;
        }
    };
    let mut stored = match peers.get(&peer_id) {
        Ok(p) => p,
        Err(e) => {
            warn!(%peer_id, error = %e, "Hello: peer no longer in store");
            return;
        }
    };
    let new_name = hello.name.0;
    if !new_name.is_empty() && stored.name.0 != new_name {
        info!(%peer_id, old = %stored.name, new = %new_name, "peer name refreshed via Hello");
        stored.name = DeviceName(new_name);
        if let Err(e) = peers.put(&stored) {
            warn!(%peer_id, error = %e, "PeerStore::put after Hello failed");
        }
    }
}

/// Read one `ClipboardMessage` per frame from the inbound stream and
/// stamp it into the host Wayland clipboard, gated by
/// `Permission::ClipboardIn`.
async fn clipboard_inbound_loop(
    mut stream: QuicStream,
    peer_id: DeviceId,
    permissions: Arc<dyn ansync_permissions::PermissionsStore>,
    sync: ClipboardSync,
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
        // Echo guard: if the inbound payload is exactly what we just
        // pushed outbound, skip the write. `wl-clipboard-rs::copy`
        // spawns a fresh selection-owner process — re-writing would
        // steal ownership from whichever app was offering the
        // original (typically the same app the user just copied
        // from), so they could no longer paste rich MIMEs anywhere.
        let fp = clipboard_fingerprint(&content);
        if sync.matches(&fp) {
            debug!(%peer_id, "skipping inbound clipboard write — matches outbound fingerprint");
            continue;
        }
        sync.set(fp);
        if let Err(e) = backend.write(content).await {
            warn!(%peer_id, error = %e, "WaylandClipboard write failed");
        }
    }
}

async fn notification_inbound_loop(
    mut stream: QuicStream,
    peer_id: DeviceId,
    permissions: Arc<dyn ansync_permissions::PermissionsStore>,
    dbus_conn: Arc<zbus::Connection>,
) {
    let device_path = ansync_dbus::path_device(&peer_id);
    let emitter =
        match zbus::object_server::SignalEmitter::new(dbus_conn.as_ref(), device_path.clone()) {
            Ok(e) => e,
            Err(e) => {
                warn!(%peer_id, error = %e, "build SignalEmitter failed; dropping notifications");
                return;
            }
        };
    // Companion opens a fresh QUIC stream per notification (see
    // `send_notification` on the device side — the stream is dropped
    // after `send`). So the daemon expects exactly ONE frame here;
    // anything after that would be a protocol error. Reading in a
    // loop floods the journal with `early eof` warnings as quinn
    // surfaces each finished stream's FIN to us.
    let bytes = match stream.recv().await {
        Ok(b) => b,
        Err(ansync_transport::TransportError::Closed) => return,
        Err(ansync_transport::TransportError::Io(e))
            if e.kind() == std::io::ErrorKind::UnexpectedEof =>
        {
            // Sender closed the stream without writing a frame. Not
            // an error worth logging at warn level.
            return;
        }
        Err(e) => {
            warn!(%peer_id, error = %e, "notification recv error");
            return;
        }
    };
    let msg: NotificationMessage = match postcard::from_bytes(&bytes) {
        Ok(m) => m,
        Err(e) => {
            warn!(%peer_id, error = %e, "notification postcard decode failed");
            return;
        }
    };
    // Per-message permission gate. Disabling `notifications` after
    // pairing drops the event silently without breaking the wire.
    match permissions.check(&peer_id, Permission::Notifications).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(e) => {
            warn!(%peer_id, error = %e, "notifications perm check failed; dropping event");
            return;
        }
    }
    let result = match &msg {
        NotificationMessage::Posted { id, app, title, body } => {
            ansync_dbus::Device::notification_posted(&emitter, *id, app, title, body).await
        }
        NotificationMessage::Removed { id } => {
            ansync_dbus::Device::notification_removed(&emitter, *id).await
        }
    };
    if let Err(e) = result {
        warn!(%peer_id, error = %e, "D-Bus signal emit failed");
    }
}

/// Push the current host Wayland clipboard to `peer_id`, gated by
/// `Permission::ClipboardOut`. Exposed via the D-Bus
/// `Device.SyncClipboard` method.
async fn push_clipboard_to_peer(
    mirrors: &MirrorRegistry,
    permissions: &Arc<dyn ansync_permissions::PermissionsStore>,
    device: &DeviceId,
    sync: &ClipboardSync,
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
    // Stash the fingerprint so the companion's auto-push back doesn't
    // re-write our own clipboard (which would steal selection
    // ownership from whatever app was offering — see ClipboardSync).
    sync.set(clipboard_fingerprint(&content));
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
    inbound_tile_kind: &'static str,
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
        *entry
            .inbound_tile_kind
            .lock()
            .expect("audio inbound tile slot poisoned") = Some(inbound_tile_kind);
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
        let perms_pump = permissions.clone();
        let peer_pump = device.clone();
        let handle = tokio::spawn(audio_pump_loop(stream, source, peer_pump, perms_pump));
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
    *entry
        .inbound_tile_kind
        .lock()
        .expect("audio inbound tile slot poisoned") = None;
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

async fn audio_pump_loop(
    mut stream: QuicStream,
    mut source: ansync_audio::CpalSource,
    peer_id: DeviceId,
    permissions: Arc<dyn ansync_permissions::PermissionsStore>,
) {
    loop {
        match source.read().await {
            Ok(bytes) => {
                match permissions.check(&peer_id, Permission::AudioOut).await {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(e) => {
                        warn!(%peer_id, error = %e, "audio_pump_loop: perm check failed; dropping chunk");
                        continue;
                    }
                }
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
    permissions: Arc<dyn ansync_permissions::PermissionsStore>,
    dbus_conn: Arc<zbus::Connection>,
) {
    // Emit `StreamStateChanged(<tile>, false)` if the companion drops
    // the inbound stream while the host still believed the route was
    // alive — same pattern as `CameraExitGuard`. `inbound_tile_kind`
    // holds whichever tile the route is attached to ("mic" / "audio");
    // the daemon-initiated stop path clears it before the stream
    // closes, turning the guard into a no-op.
    struct AudioExitGuard {
        entry: Arc<AudioEntry>,
        peer_id: DeviceId,
        dbus_conn: Arc<zbus::Connection>,
    }
    impl Drop for AudioExitGuard {
        fn drop(&mut self) {
            let kind = self
                .entry
                .inbound_tile_kind
                .lock()
                .expect("audio inbound tile slot poisoned")
                .take();
            let Some(kind) = kind else {
                return;
            };
            *self
                .entry
                .inbound_tx
                .lock()
                .expect("audio inbound tx poisoned") = None;
            let conn = self.dbus_conn.clone();
            let device = self.peer_id.clone();
            tokio::spawn(async move {
                emit_stream(&conn, &device, kind, false).await;
            });
        }
    }
    let _guard = AudioExitGuard {
        entry: entry.clone(),
        peer_id: peer_id.clone(),
        dbus_conn,
    };
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
        // Per-chunk permission gate. Revoking `audio_in` mid-stream
        // drops further chunks without tearing the QUIC stream; if the
        // user flips it back on the flow resumes seamlessly.
        match permissions.check(&peer_id, Permission::AudioIn).await {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => {
                warn!(%peer_id, error = %e, "audio inbound perm check failed; dropping chunk");
                continue;
            }
        }
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
    mirrors: &MirrorRegistry,
    cameras: &CameraRegistry,
    device: &DeviceId,
) -> Result<(), DaemonError> {
    // Push the StopCamera control to the companion FIRST so the
    // Android-side `CameraSession` tears down its sensor + encoder
    // before we drop the local sink. If we yanked the local
    // pipeline before notifying the device, Camera2 + MediaCodec
    // would keep running for ~60 s of idle frames (no consumer
    // backpressure on the Surface input path) and the LED + battery
    // drain would stay on. Best-effort: if the conn is gone we
    // proceed with the local teardown anyway.
    let conn_opt = mirrors.get(device).and_then(|mirror| {
        mirror
            .conn
            .lock()
            .expect("conn slot poisoned")
            .clone()
    });
    if let Some(conn) = conn_opt {
        if let Err(e) = send_control(&conn, ControlMessage::StopCamera).await {
            warn!(%device, error = %e, "StopCamera control push failed");
        }
    } else {
        warn!(%device, "StopCamera: peer not connected; skipping wire push");
    }
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
        // v4l2loopback consumers (OBS / browsers / Discord) expect a
        // tightly packed NV12 buffer: Y plane `w*h` bytes followed by
        // interleaved UV `w*h/2`. Hardware decoders almost always emit
        // either NV12 with a row stride > width (NVDEC pads to 256 / 512)
        // or I420 (three separate planes). Feeding the raw decoder buffer
        // straight to the sink produces solid-green output because the
        // chroma offsets land in the wrong place. Convert here.
        let packed = match frame.format {
            PixelFormat::Nv12 => repack_nv12(&frame),
            PixelFormat::I420 => i420_to_nv12(&frame),
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
                pixel_format: CameraPixelFormat::Nv12,
            };
            if let Err(e) = sink.register(&entry.peer_name, fmt).await {
                warn!(error = %e, "camera sink register failed");
                return;
            }
            sink_registered = true;
        }
        if let Err(e) = sink.write_frame(packed).await {
            warn!(error = %e, "camera sink write_frame failed");
        }
    }
    info!(name = %entry.peer_name, "camera_decode_loop: channel closed");
}

/// Copy a stride-padded NV12 buffer into a tightly packed `width * height *
/// 3 / 2` byte block. When the decoder already emits no padding this is a
/// single contiguous copy.
fn repack_nv12(frame: &DecodedFrame) -> bytes::Bytes {
    let w = frame.width as usize;
    let h = frame.height as usize;
    let stride = frame.stride.max(frame.width) as usize;
    if stride == w {
        return frame.data.clone();
    }
    let mut out = vec![0u8; w * h * 3 / 2];
    let src = frame.data.as_ref();
    for row in 0..h {
        let s = row * stride;
        let d = row * w;
        out[d..d + w].copy_from_slice(&src[s..s + w]);
    }
    let uv_src_base = stride * h;
    let uv_dst_base = w * h;
    for row in 0..(h / 2) {
        let s = uv_src_base + row * stride;
        let d = uv_dst_base + row * w;
        out[d..d + w].copy_from_slice(&src[s..s + w]);
    }
    bytes::Bytes::from(out)
}

/// Convert I420 (three planes Y / U / V, U+V each at half-stride and
/// half-height) into tightly packed NV12. v4l2loopback consumers can't read
/// I420 from us because we negotiated the FOURCC as `NV12` upstream.
fn i420_to_nv12(frame: &DecodedFrame) -> bytes::Bytes {
    let w = frame.width as usize;
    let h = frame.height as usize;
    let y_stride = frame.stride.max(frame.width) as usize;
    let uv_stride = y_stride / 2;
    let src = frame.data.as_ref();
    let mut out = vec![0u8; w * h * 3 / 2];
    for row in 0..h {
        let s = row * y_stride;
        let d = row * w;
        out[d..d + w].copy_from_slice(&src[s..s + w]);
    }
    let u_base = y_stride * h;
    let v_base = u_base + uv_stride * (h / 2);
    let uv_dst_base = w * h;
    let half_w = w / 2;
    for row in 0..(h / 2) {
        let u_row = u_base + row * uv_stride;
        let v_row = v_base + row * uv_stride;
        let d = uv_dst_base + row * w;
        for col in 0..half_w {
            out[d + col * 2] = src[u_row + col];
            out[d + col * 2 + 1] = src[v_row + col];
        }
    }
    bytes::Bytes::from(out)
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

async fn video_stream_loop(
    mut stream: QuicStream,
    entry: Arc<MirrorEntry>,
    peer_id: DeviceId,
    action_tx: Option<UnboundedSender<DaemonAction>>,
) {
    info!(%peer_id, "video stream wired");
    // The decoder lives in the per-window mirror subprocess now.
    // This loop's only job is to fan encoded NAL chunks off the QUIC
    // stream into whichever subprocess (if any) currently owns the
    // window for this peer. No window open → no subscriber → chunks
    // are dropped silently, which is fine: the companion keeps
    // capturing only because the device-side decision (QSTile / D-Bus)
    // told it to.
    // When the QUIC video stream ends (Android stopped capturing,
    // tile flipped off, peer disconnected, etc.) tear the mirror
    // window down too — there's no point keeping a renderer
    // subprocess alive showing the last frozen frame, and the user
    // explicitly does NOT want a window that outlives the stream.
    struct InboundGuard {
        entry: Arc<MirrorEntry>,
        peer_id: DeviceId,
        action_tx: Option<UnboundedSender<DaemonAction>>,
    }
    impl Drop for InboundGuard {
        fn drop(&mut self) {
            self.entry
                .video_inbound
                .store(false, std::sync::atomic::Ordering::Relaxed);
            if let Some(tx) = self.action_tx.as_ref() {
                let _ = tx.send(DaemonAction::HideScreen {
                    device: self.peer_id.clone(),
                });
            }
        }
    }
    let _guard = InboundGuard {
        entry: entry.clone(),
        peer_id: peer_id.clone(),
        action_tx,
    };
    let mut first_chunk_logged = false;
    let mut chunks_since_log: u64 = 0;
    let mut last_stat = std::time::Instant::now();
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
        if !first_chunk_logged {
            info!(%peer_id, bytes = bytes.len(), "first video chunk from peer");
            first_chunk_logged = true;
        }
        chunks_since_log += 1;
        let sender = entry
            .video_tx
            .lock()
            .expect("video_tx slot poisoned")
            .clone();
        if let Some(tx) = sender {
            if tx.send(bytes).is_err() {
                // Renderer subprocess died or was closed by the user.
                // Drop the sender so future ShowScreen actions
                // re-bootstrap from scratch.
                *entry.video_tx.lock().expect("video_tx slot poisoned") = None;
            }
        }
        if last_stat.elapsed() >= std::time::Duration::from_secs(5) {
            debug!(%peer_id, chunks = chunks_since_log / 5, "video stream stats");
            chunks_since_log = 0;
            last_stat = std::time::Instant::now();
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

/// Enumerate IPv4 addresses on non-loopback / non-docker interfaces.
/// Used to populate `DaemonState::listen_endpoints` so `ansyncctl
/// pair` can hand a direct-dial fallback to the companion (works
/// around mDNS multicast being dropped by Wi-Fi AP isolation).
///
/// Filters out `lo`, `docker*`, `br-*`, `veth*`, and tailscale —
/// they're not the host's LAN identity from the peer's POV.
fn enumerate_lan_ipv4() -> Vec<String> {
    use std::ffi::CStr;
    use std::net::Ipv4Addr;
    let mut out = Vec::new();
    let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut ifap) } != 0 {
        return out;
    }
    let mut cur = ifap;
    while !cur.is_null() {
        let ifa = unsafe { &*cur };
        cur = ifa.ifa_next;
        if ifa.ifa_addr.is_null() {
            continue;
        }
        let sa = unsafe { &*ifa.ifa_addr };
        if sa.sa_family != libc::AF_INET as libc::sa_family_t {
            continue;
        }
        let name = unsafe { CStr::from_ptr(ifa.ifa_name) }
            .to_string_lossy()
            .to_string();
        if name == "lo"
            || name.starts_with("docker")
            || name.starts_with("br-")
            || name.starts_with("veth")
            || name.starts_with("virbr")
            || name == "tailscale0"
        {
            continue;
        }
        let sin = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in) };
        let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
        if ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() {
            continue;
        }
        out.push(ip.to_string());
    }
    unsafe { libc::freeifaddrs(ifap) };
    out
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

/// Periodic resident-memory probe. Reads `VmRSS` + `VmHWM` from
/// `/proc/self/status` and emits one debug line per cycle. Goes to
/// debug (not info) so the journal isn't polluted in release; flip
/// `ansync_daemon_core` to debug to see the trace.
async fn mem_stats_loop() {
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30));
    // Skip the very first tick (fires instantly) — give the process a
    // moment to settle so the first sample isn't pre-LAN-up noise.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let Ok(status) = tokio::fs::read_to_string("/proc/self/status").await else {
            continue;
        };
        let mut rss_kb: Option<u64> = None;
        let mut hwm_kb: Option<u64> = None;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                rss_kb = parse_kb(rest);
            } else if let Some(rest) = line.strip_prefix("VmHWM:") {
                hwm_kb = parse_kb(rest);
            }
        }
        if let (Some(rss), Some(hwm)) = (rss_kb, hwm_kb) {
            debug!(
                rss_mib = rss / 1024,
                hwm_mib = hwm / 1024,
                "mem stats"
            );
        }
    }
}

fn parse_kb(rest: &str) -> Option<u64> {
    let trimmed = rest.trim();
    let num = trimmed.strip_suffix(" kB").unwrap_or(trimmed).trim();
    num.parse::<u64>().ok()
}

/// Long-lived background task that browses `_ansync-pair._tcp.local.`
/// continuously and emits `Manager.DeviceReachable` /
/// `DeviceUnreachable` signals every time a paired companion's mDNS
/// presence flips. Caller is responsible for aborting the returned
/// `JoinHandle` on shutdown — drop() of the inner `ServiceDaemon`
/// also tears the underlying socket down.
///
/// The watcher does NOT initiate a connection; the companion stays
/// the QUIC client (it dials the host on Wi-Fi up via its
/// `HostDialer`). The signals are presence indicators for the widget
/// — a paired companion can be "Reachable" (mDNS visible) without
/// being "Active" (QUIC + Hello complete) when the device is still
/// negotiating the handshake.
async fn companion_watcher(state: Arc<DaemonState>, conn: Arc<zbus::Connection>) {
    let (_daemon, mut rx) = match ansync_pairing::watch_pair_candidates() {
        Ok(pair) => pair,
        Err(e) => {
            warn!(error = %e, "companion_watcher: mdns browse start failed");
            return;
        }
    };

    let manager_path = match zbus::zvariant::ObjectPath::try_from(ansync_dbus::PATH_MANAGER) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "companion_watcher: build manager path failed");
            return;
        }
    };
    let emitter = match zbus::object_server::SignalEmitter::new(&*conn, manager_path) {
        Ok(e) => e,
        Err(e) => {
            warn!(error = %e, "companion_watcher: signal emitter build failed");
            return;
        }
    };

    // Map mDNS instance fullnames → DeviceId so a ServiceRemoved
    // event (which only carries the instance name) can be translated
    // back into the corresponding D-Bus path id.
    let mut instance_to_id: HashMap<String, DeviceId> = HashMap::new();

    while let Some(event) = rx.recv().await {
        match event {
            ansync_pairing::PairWatchEvent::Resolved(c) => {
                let known = match state.peers.list() {
                    Ok(list) => list,
                    Err(e) => {
                        warn!(error = %e, "companion_watcher: peers.list failed");
                        continue;
                    }
                };
                let Some(stored) = known.into_iter().find(|p| p.pubkey == c.pubkey) else {
                    // Unpaired companion advertising — ignore (the
                    // pair surface picks these up via `BrowseAvailable`).
                    continue;
                };
                let device_id = stored.id.clone();
                let id_str = device_id.to_string();
                let prev = {
                    let mut g = state.reachable.lock().expect("reachable poisoned");
                    g.insert(device_id.clone(), c.addr)
                };
                instance_to_id.insert(c.name.clone(), device_id.clone());
                if prev.map_or(true, |old| old != c.addr) {
                    info!(%id_str, addr = %c.addr, "companion reachable on LAN");
                    let _ = ansync_dbus::Manager::device_reachable(
                        &emitter,
                        &id_str,
                        &c.addr.to_string(),
                    )
                    .await;
                }
            }
            ansync_pairing::PairWatchEvent::Removed(instance) => {
                let Some(device_id) = instance_to_id.remove(&instance) else {
                    continue;
                };
                let removed = {
                    let mut g = state.reachable.lock().expect("reachable poisoned");
                    g.remove(&device_id)
                };
                if removed.is_some() {
                    let id_str = device_id.to_string();
                    info!(%id_str, "companion left LAN");
                    let _ = ansync_dbus::Manager::device_unreachable(&emitter, &id_str).await;
                }
            }
        }
    }
    warn!("companion_watcher: pair watch stream closed");
}

/// Native Wayland clipboard watcher → auto-push to every connected
/// peer with `ClipboardOut` on. Uses `zwlr_data_control_v1` (wlroots,
/// KDE Plasma 6, COSMIC, niri); gracefully degrades to manual-only
/// sync on compositors that don't expose the protocol (GNOME today).
///
/// Debounces back-to-back `selection` events with a 50 ms quiet
/// window — desktops emit two changes for some apps (clear + set)
/// which would otherwise double-push.
async fn host_clipboard_watcher(
    mirrors: Arc<MirrorRegistry>,
    permissions: Arc<dyn ansync_permissions::PermissionsStore>,
    sync: ClipboardSync,
) {
    let mut watcher = match ansync_clipboard::WaylandClipboardWatcher::start() {
        Ok(w) => w,
        Err(ansync_clipboard::WatcherError::ProtocolUnsupported) => {
            info!(
                "host clipboard watcher: compositor lacks zwlr_data_control_v1 — \
                 host→device clipboard remains manual via Device.SyncClipboard"
            );
            return;
        }
        Err(e) => {
            warn!(error = %e, "host clipboard watcher start failed; manual sync only");
            return;
        }
    };
    info!("host clipboard watcher active");
    let debounce = std::time::Duration::from_millis(50);
    loop {
        // Block until at least one change.
        if watcher.rx().recv().await.is_none() {
            warn!("host clipboard watcher channel closed");
            return;
        }
        // Drain coalesced events within the debounce window.
        let deadline = tokio::time::Instant::now() + debounce;
        loop {
            match tokio::time::timeout_at(deadline, watcher.rx().recv()).await {
                Ok(Some(())) => continue,
                Ok(None) => {
                    warn!("host clipboard watcher channel closed mid-drain");
                    return;
                }
                Err(_) => break,
            }
        }
        // Read once, fingerprint once, fan out: avoids re-reading the
        // Wayland clipboard per peer and lets the echo guard short-
        // circuit when the change originated from an inbound paste.
        let backend = WaylandClipboard::new();
        let content = match backend.read().await {
            Ok(c) => c,
            Err(e) => {
                debug!(error = %e, "host clipboard read failed; skipping fan-out");
                continue;
            }
        };
        let fp = clipboard_fingerprint(&content);
        if sync.matches(&fp) {
            debug!("clipboard watcher fired but content matches last inbound; not pushing");
            continue;
        }
        sync.set(fp);
        for (id, _entry) in mirrors.entries() {
            match permissions.check(&id, Permission::ClipboardOut).await {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    warn!(%id, error = %e, "clipboard_out perm check failed; skipping");
                    continue;
                }
            }
            if let Err(e) =
                send_clipboard_content_to_peer(&mirrors, &id, content.clone()).await
            {
                warn!(%id, error = %e, "auto-push clipboard failed");
            }
        }
    }
}

/// Push pre-read clipboard content to a peer without re-reading the
/// host's clipboard. Used by the watcher fan-out so all peers see the
/// same snapshot and the watcher's echo guard fingerprint stays
/// coherent.
async fn send_clipboard_content_to_peer(
    mirrors: &MirrorRegistry,
    device: &DeviceId,
    content: ClipboardContent,
) -> Result<(), DaemonError> {
    let mirror = mirrors
        .get(device)
        .ok_or_else(|| DaemonError::Startup(format!("no mirror entry for {device}")))?;
    let conn = mirror
        .conn
        .lock()
        .expect("conn slot poisoned")
        .clone()
        .ok_or_else(|| DaemonError::Startup(format!("peer {device} not connected")))?;
    let msg = match content {
        ClipboardContent::Text(s) => ClipboardMessage::Text { content: s },
        ClipboardContent::Blob { mime, data } => ClipboardMessage::Blob { mime, data },
    };
    let mut stream = conn.open(StreamKind::Clipboard).await?;
    let bytes = postcard::to_allocvec(&msg)
        .map_err(|e| DaemonError::Startup(format!("encode ClipboardMessage: {e}")))?;
    stream.send(bytes::Bytes::from(bytes)).await?;
    info!(%device, "host clipboard pushed (auto)");
    Ok(())
}
