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

use ansync_audio::{
    AudioBackend, AudioFormat, AudioStats, BoxedSink, BoxedSource, OPUS_FRAME_SAMPLES,
    OpusDecoderWrap, OpusEncoderWrap, SampleFormat, SharedAudioBackend, select_audio_backend,
};
use ansync_camera::{CameraFormat, CameraPixelFormat, VirtualCameraSink};
use ansync_clipboard::{ClipboardBackend, ClipboardContent, WaylandClipboard};
use ansync_core::{Capabilities, DeviceId, DeviceName, Permission};
use ansync_crypto::IdentityKeypair;
use ansync_dbus::{ConnState, DaemonAction, DaemonState, Device, serve};
use ansync_discovery::{Discovery, MdnsDiscovery};
use ansync_files::{
    AutoAcceptPolicy, Direction as TransferDirection, ProgressEvent, ProgressFn, receive_file,
    send_file,
};
use ansync_input::{InputDeviceFactory, InputSession, UinputFactory};
use ansync_pairing::PeerStore;
use perms_backend::PeerStorePermissions;
use ansync_proto::{
    AudioCodec, AudioDirection, AudioStreamInit, CameraConfig, ClipboardMessage, ControlMessage,
    Envelope, Hello, InputMessage, Message, NotificationMessage, PROTOCOL_VERSION, UrlMessage,
    VideoCodec as ProtoVideoCodec,
};
use ansync_transport::pinning::TrustedPeers;
use ansync_transport::{
    Connection, QuicConnection, QuicServer, QuicStream, QuicTransport, Stream as _, StreamKind,
};
use ansync_video::{DecodedFrame, HostDecoder, PixelFormat, VideoCodec, VideoDecoder};

mod mirror_subprocess;
mod perms_backend;
use mirror_subprocess::spawn_mirror_subprocess;
use directories::{BaseDirs, UserDirs};
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
    #[error("audio: {0}")]
    Audio(#[from] ansync_audio::AudioError),
    #[error("startup: {0}")]
    Startup(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputBackend {
    /// Local kernel uinput devices. Works on every Linux host with
    /// the `uinput` module loaded + a udev rule that lets the
    /// daemon's user write `/dev/uinput`.
    Uinput,
}

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub device_name: String,
    /// Override the on-disk identity path. Defaults to
    /// `$XDG_DATA_HOME/ansync/identity.key`.
    pub identity_path: Option<PathBuf>,
    /// Override the peers directory. Defaults to
    /// `$XDG_DATA_HOME/ansync/peers/`. Per-device permissions live in
    /// the same toml as the peer record — there is no separate
    /// permissions tree to override.
    pub peers_dir: Option<PathBuf>,
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
                | Capabilities::CLIPBOARD
                | Capabilities::SHARE,
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
        let download_dir = self
            .config
            .download_dir
            .clone()
            .unwrap_or_else(default_download_dir);

        let identity = IdentityKeypair::load_or_generate(&identity_path)?;
        info!(device_id = %identity.device_id(), "identity loaded");

        let peers = PeerStore::open(peers_dir)?;
        let permissions: Arc<dyn ansync_permissions::PermissionsStore> =
            Arc::new(PeerStorePermissions::new(peers.clone()));

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
        let inputs = Arc::new(InputRegistry::default());
        let clipboard_sync = ClipboardSync::default();
        // Backend chosen once at startup so every per-peer sink/source
        // shares the same kind. ANSYNC_AUDIO_BACKEND overrides the
        // auto-detect chain (pipewire → aloop → cpal).
        let audio_backend = select_audio_backend()?;
        info!(backend = %audio_backend.kind(), "audio backend selected");
        let action_handle = tokio::spawn(action_loop(
            action_rx,
            action_tx.clone(),
            mirrors.clone(),
            cameras.clone(),
            audios.clone(),
            audio_backend.clone(),
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
        let inbound_coalescer = InboundCoalescer::new(std::time::Duration::from_secs(2));
        let accept_handle = tokio::spawn(accept_loop(AcceptCtx {
            server,
            peers,
            permissions: permissions.clone(),
            factory,
            download_dir,
            mirrors: mirrors.clone(),
            cameras: cameras.clone(),
            audios: audios.clone(),
            inputs: inputs.clone(),
            dbus_conn: dbus_conn_arc.clone(),
            device_name: self.config.device_name.clone(),
            capabilities: self.config.capabilities,
            identity: identity.clone(),
            dbus_state: state.clone(),
            clipboard_sync: clipboard_sync.clone(),
            inbound_coalescer,
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
    inputs: Arc<InputRegistry>,
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
    /// Coalesces inbound file completions so multi-share bursts
    /// surface as one notif instead of N stacked entries.
    inbound_coalescer: Arc<InboundCoalescer>,
}

/// Per-peer [`InputSession`] cache. The session owns the uinput
/// device handles, so the lifetime of *this* map is the lifetime of
/// every virtual `/dev/input/eventN` we ever create. Critically the
/// session is NOT torn down on peer disconnect — keeping the uinput
/// devices alive across companion reconnects (which can happen as
/// often as every few seconds when the QUIC keep-alive expires)
/// avoids the libinput / X11 / Wayland tablet-tool reattach hiccup
/// that otherwise drops the cursor every cycle and makes apps like
/// Krita re-acquire the device mid-stroke.
#[derive(Default)]
pub struct InputRegistry {
    entries: StdMutex<HashMap<DeviceId, Arc<Mutex<InputSession>>>>,
}

impl InputRegistry {
    /// Get-or-create the input session for `id`. Subsequent reconnects
    /// from the same peer reuse the existing session and its already-
    /// created uinput devices.
    pub fn ensure(
        &self,
        id: &DeviceId,
        name: &DeviceName,
        permissions: Arc<dyn ansync_permissions::PermissionsStore>,
        factory: Arc<dyn InputDeviceFactory>,
    ) -> Arc<Mutex<InputSession>> {
        let mut entries = self.entries.lock().expect("input registry poisoned");
        entries
            .entry(id.clone())
            .or_insert_with(|| {
                Arc::new(Mutex::new(InputSession::new(
                    id.clone(),
                    name.clone(),
                    permissions,
                    factory,
                )))
            })
            .clone()
    }
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
    /// Plays *into* the host (peer → host direction). Boxed so the
    /// concrete backend (pipewire / cpal) lives behind the
    /// `AudioSink` trait — switched at daemon init via
    /// `ANSYNC_AUDIO_BACKEND` and re-used across reconnects.
    sink: tokio::sync::Mutex<Option<Arc<tokio::sync::Mutex<BoxedSink>>>>,
    /// Encode/decode counters surfaced by the per-peer audio stats
    /// loop (Step 18e). Shared with both inbound + outbound loops via
    /// `Arc` so atomics are global to the entry.
    stats: Arc<AudioStats>,
    /// Stats logger handle. Spawned in `handle_start_audio`, aborted
    /// in `handle_stop_audio`.
    stats_handle: StdMutex<Option<tokio::task::JoinHandle<()>>>,
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
                    stats: Arc::new(AudioStats::default()),
                    stats_handle: StdMutex::new(None),
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
    /// True once `sink.register()` has run successfully. Persists
    /// across companion reconnects: every new `camera_decode_loop`
    /// spawn checks this before calling `register()` again so we
    /// don't keep dyn-adding `/dev/video<N>` nodes on every cycle
    /// (which is what makes OBS / browsers see a new device after
    /// every keep-alive expiry). Cleared by `handle_stop_camera` (the
    /// explicit D-Bus tear-down path) so a follow-up StartCamera with
    /// a different format does re-register.
    sink_registered: std::sync::atomic::AtomicBool,
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
                    sink_registered: std::sync::atomic::AtomicBool::new(false),
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
    audio_backend: SharedAudioBackend,
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
                    audio_backend.as_ref(),
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
                    audio_backend.as_ref(),
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
            DaemonAction::SendFiles { device, paths } => {
                handle_send_files(&mirrors, &permissions, &dbus_conn, &device, paths).await;
            }
            DaemonAction::SendUrl { device, url } => {
                handle_send_url(&mirrors, &device, url).await;
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
                let inputs = ctx.inputs.clone();
                let dbus_conn = ctx.dbus_conn.clone();
                let device_name = ctx.device_name.clone();
                let capabilities = ctx.capabilities;
                let identity = ctx.identity.clone();
                let dbus_state = ctx.dbus_state.clone();
                let clipboard_sync = ctx.clipboard_sync.clone();
                let inbound_coalescer = ctx.inbound_coalescer.clone();
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
                        inputs,
                        dbus_conn,
                        device_name,
                        capabilities,
                        identity,
                        dbus_state,
                        clipboard_sync,
                        inbound_coalescer,
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
    inputs: Arc<InputRegistry>,
    dbus_conn: Arc<zbus::Connection>,
    device_name: String,
    capabilities: Capabilities,
    identity: IdentityKeypair,
    dbus_state: Arc<DaemonState>,
    clipboard_sync: ClipboardSync,
    inbound_coalescer: Arc<InboundCoalescer>,
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

    // Per-peer InputSession lives in `InputRegistry` for the lifetime
    // of the daemon — that is, the uinput devices we create on first
    // input event survive companion reconnects. Without this, every
    // QUIC keep-alive expiry (~6 s on a flaky network) would tear the
    // device down and force libinput / Wayland to re-acquire it,
    // dropping the cursor mid-stroke and confusing apps like Krita
    // that hold an open fd to the stylus.
    let input_session = inputs.ensure(
        &peer_id,
        &peer.name,
        permissions.clone(),
        factory.clone(),
    );

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

    let input_rx_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let stats_handle = tokio::spawn(stats_telemetry_loop(
        conn_arc.clone(),
        peer_id.clone(),
        input_rx_counter.clone(),
    ));

    loop {
        let (kind, stream) = match conn_arc.accept().await {
            Ok(v) => v,
            Err(ansync_transport::TransportError::Closed) => {
                info!(%peer_id, "peer closed connection");
                break;
            }
            Err(ansync_transport::TransportError::TimedOut) => {
                info!(%peer_id, "peer keep-alive timed out");
                break;
            }
            Err(e) => return Err(e.into()),
        };
        match kind {
            StreamKind::Input => {
                let session = input_session.clone();
                let counter = input_rx_counter.clone();
                tokio::spawn(input_stream_loop(stream, session, counter));
            }
            StreamKind::Files => {
                let perms = permissions.clone();
                let peer_id_inbound = peer_id.clone();
                let peer_name_inbound = peer.name.0.clone();
                let dbus = dbus_conn.clone();
                let policy = Arc::new(AutoAcceptPolicy {
                    root: download_dir.clone(),
                });
                tokio::spawn(files_stream_loop(
                    stream,
                    peer_id_inbound,
                    peer_name_inbound,
                    perms,
                    policy,
                    dbus,
                    inbound_coalescer.clone(),
                ));
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
            StreamKind::Url => {
                let pid = peer_id.clone();
                let pname = peer.name.0.clone();
                let perms = permissions.clone();
                tokio::spawn(url_inbound_loop(stream, pid, pname, perms));
            }
            other => {
                warn!(kind = ?other, "stream kind accepted but not wired yet — dropping");
                drop(stream);
            }
        }
    }
    // NOTE: the per-peer InputSession is owned by `InputRegistry` and
    // intentionally NOT torn down here. Companion reconnects reuse the
    // same uinput devices so apps holding `/dev/input/eventN` don't see
    // the device vanish every cycle. The session is dropped (and the
    // uinput nodes destroyed) only when the daemon itself exits.
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
    // Camera pipeline: the decoder task and the frame_tx pipe are
    // wired to the now-dead QUIC stream, so tear them down. The
    // v4l2loopback *sink* itself is intentionally NOT unregistered —
    // keeping `/dev/video<N>` alive across reconnects means OBS /
    // Discord / browsers don't lose the device from their picker (and
    // don't have to re-select it after every keep-alive cycle). The
    // sink is only released on explicit `StopCamera` (D-Bus) or on
    // daemon shutdown (Drop chain).
    if let Some(handle) = camera_entry
        .handle
        .lock()
        .expect("handle slot poisoned")
        .take()
    {
        handle.abort();
    }
    *camera_entry.frame_tx.lock().expect("frame tx slot poisoned") = None;
    // Audio: tear down the conn-bound pump / inbound tasks (the QUIC
    // streams they hold are dead). The CpalSink stays alive for the
    // same reason as the camera sink — disappearing audio routes
    // confuse PipeWire's port-watching clients.
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
    stats_handle.abort();
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

/// Periodic per-peer QUIC stats. Runs alongside `handle_connection`,
/// aborted by the join handle when the parent loop exits. Output is
/// `debug!` so default journald stays quiet; flip on with
/// `RUST_LOG=ansync_daemon_core=debug` to diagnose packet-loss /
/// rtt regressions reported as "cursor feels heavy" or "Conn cycle".
async fn stats_telemetry_loop(
    conn: Arc<QuicConnection>,
    peer_id: DeviceId,
    input_rx_counter: Arc<std::sync::atomic::AtomicU64>,
) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
    // Skip the very first tick (fires immediately) — first useful
    // sample needs at least one keep-alive RTT.
    tick.tick().await;
    let mut prev_sent: u64 = 0;
    let mut prev_lost: u64 = 0;
    let mut prev_input: u64 = 0;
    loop {
        tick.tick().await;
        let s = conn.stats();
        let sent = s.path.sent_packets;
        let lost = s.path.lost_packets;
        let dsent = sent.saturating_sub(prev_sent);
        let dlost = lost.saturating_sub(prev_lost);
        prev_sent = sent;
        prev_lost = lost;
        let loss_pct = if dsent == 0 {
            0.0
        } else {
            (dlost as f64 / dsent as f64) * 100.0
        };
        let input_total = input_rx_counter.load(std::sync::atomic::Ordering::Relaxed);
        let dinput = input_total.saturating_sub(prev_input);
        prev_input = input_total;
        debug!(
            %peer_id,
            rtt_ms = conn.rtt().as_millis() as u64,
            sent = dsent,
            lost = dlost,
            loss_pct = format!("{loss_pct:.2}"),
            cwnd = s.path.cwnd,
            black_holes = s.path.black_holes_detected,
            input_rx = dinput,
            "quic stats"
        );
    }
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
            Err(ansync_transport::TransportError::Closed)
            | Err(ansync_transport::TransportError::TimedOut) => {
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
    let new_caps = hello.capabilities;
    let mut dirty = false;
    if !new_name.is_empty() && stored.name.0 != new_name {
        info!(%peer_id, old = %stored.name, new = %new_name, "peer name refreshed via Hello");
        stored.name = DeviceName(new_name);
        dirty = true;
    }
    // Capabilities only become known post-handshake (the pair flow
    // happens before the Noise/QUIC tunnel and so cannot exchange
    // them — every StoredPeer is born with `Capabilities::empty()`).
    // The Hello frame is the one place we learn what the peer can
    // serve, so refresh the persisted snapshot here.
    if stored.capabilities != new_caps {
        info!(
            %peer_id,
            old = ?stored.capabilities,
            new = ?new_caps,
            "peer capabilities refreshed via Hello",
        );
        stored.capabilities = new_caps;
        dirty = true;
    }
    if dirty {
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
            Err(ansync_transport::TransportError::Closed)
            | Err(ansync_transport::TransportError::TimedOut) => return,
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
        Err(ansync_transport::TransportError::Closed)
        | Err(ansync_transport::TransportError::TimedOut) => return,
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
    audio_backend: &dyn AudioBackend,
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

    // Stats logger — first call only. Replace-on-restart so the
    // counters keep accumulating across pause/resume cycles.
    {
        let mut slot = entry
            .stats_handle
            .lock()
            .expect("audio stats slot poisoned");
        if slot.as_ref().map(|h| h.is_finished()).unwrap_or(true) {
            let h = tokio::spawn(audio_stats_loop(device.clone(), entry.clone()));
            if let Some(prev) = slot.replace(h) {
                prev.abort();
            }
        }
    }

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

    // Inbound: provision sink + tile slot now; the render task is
    // spawned by `audio_inbound_loop` once the wire `AudioStreamInit`
    // header arrives — that's when we know the codec the companion
    // picked (OpusVoip vs Raw fallback).
    if need_in {
        *entry
            .inbound_tile_kind
            .lock()
            .expect("audio inbound tile slot poisoned") = Some(inbound_tile_kind);
        // Reuse the existing CpalSink across reconnects / re-Starts
        // so PipeWire / PulseAudio clients (Discord, browsers, OBS)
        // keep their "ansync-in-..." selection. Building a new sink
        // every time creates a fresh device node and apps fall back
        // to whatever default was active before.
        let mut sink_guard = entry.sink.lock().await;
        if sink_guard.is_none() {
            let label = format!("ansync-in-{}", entry.peer_name);
            let format = AudioFormat {
                sample_rate: 48_000,
                channels: 2,
                format: SampleFormat::S16Le,
            };
            let built = match audio_backend.create_sink(&label, format).await {
                Ok(s) => Arc::new(tokio::sync::Mutex::new(s)),
                Err(e) => {
                    warn!(%device, error = %e, "open audio sink failed");
                    return Ok(());
                }
            };
            *sink_guard = Some(built);
        }
    }

    if need_out {
        let mut stream = conn.open(StreamKind::Audio).await?;
        // Host → device renders general audio (music, game sound, VOIP
        // mixed in PipeWire). OpusAudio profile @ 128 kbps gives clean
        // music; bandwidth still fits LAN comfortably.
        let codec = AudioCodec::OpusAudio;
        let init = AudioStreamInit {
            sample_rate: 48_000,
            channels: 2,
            direction: AudioDirection::HostToDevice,
            codec,
            frame_samples: OPUS_FRAME_SAMPLES as u16,
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
        let source = match audio_backend.create_source(&label, format).await {
            Ok(s) => s,
            Err(e) => {
                warn!(%device, error = %e, "open audio source failed");
                return Ok(());
            }
        };
        let encoder = OpusEncoderWrap::new(codec)
            .map_err(|e| DaemonError::Startup(format!("opus encoder: {e}")))?;
        let perms_pump = permissions.clone();
        let peer_pump = device.clone();
        let stats_pump = entry.stats.clone();
        let handle = tokio::spawn(audio_pump_loop(
            stream, source, encoder, peer_pump, perms_pump, stats_pump,
        ));
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
    if let Some(h) = entry
        .stats_handle
        .lock()
        .expect("audio stats slot poisoned")
        .take()
    {
        h.abort();
    }
    info!(%device, "StopAudioRoute done");
}

async fn audio_render_loop(
    mut rx: UnboundedReceiver<bytes::Bytes>,
    sink: Arc<tokio::sync::Mutex<BoxedSink>>,
    codec: AudioCodec,
    stats: Arc<AudioStats>,
) {
    let mut decoder = match codec {
        AudioCodec::Raw => None,
        AudioCodec::OpusVoip | AudioCodec::OpusAudio => match OpusDecoderWrap::new() {
            Ok(d) => Some(d),
            Err(e) => {
                warn!(error = %e, "audio_render_loop: opus decoder init failed");
                return;
            }
        },
    };
    while let Some(bytes) = rx.recv().await {
        let pcm = match decoder.as_mut() {
            Some(dec) => match dec.decode(&bytes) {
                Ok(p) => p,
                Err(e) => {
                    stats.record_decode_fail();
                    warn!(error = %e, "audio_render_loop: opus decode failed; dropping packet");
                    continue;
                }
            },
            None => bytes,
        };
        let mut guard = sink.lock().await;
        if let Err(e) = guard.write(pcm).await {
            warn!(error = %e, "audio_render_loop: sink write failed");
            return;
        }
    }
}

async fn audio_pump_loop(
    mut stream: QuicStream,
    mut source: BoxedSource,
    mut encoder: OpusEncoderWrap,
    peer_id: DeviceId,
    permissions: Arc<dyn ansync_permissions::PermissionsStore>,
    stats: Arc<AudioStats>,
) {
    loop {
        match source.read().await {
            Ok(pcm) => {
                match permissions.check(&peer_id, Permission::AudioOut).await {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(e) => {
                        warn!(%peer_id, error = %e, "audio_pump_loop: perm check failed; dropping chunk");
                        continue;
                    }
                }
                let packets = match encoder.feed(&pcm) {
                    Ok(p) => p,
                    Err(e) => {
                        stats.record_encode_fail();
                        warn!(error = %e, "audio_pump_loop: opus encode failed");
                        continue;
                    }
                };
                for packet in packets {
                    let packet_len = packet.len();
                    if let Err(e) = stream.send(packet).await {
                        warn!(error = %e, "audio_pump_loop: stream send failed");
                        return;
                    }
                    stats.record_out(packet_len);
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
    // First frame: AudioStreamInit. The codec field tells us whether
    // subsequent bytes are raw PCM (legacy companion) or Opus packets
    // (one per recv). We spawn `audio_render_loop` here — not in
    // `handle_start_audio` — so the decoder is wired with the exact
    // codec the companion actually picked.
    let header_bytes = match stream.recv().await {
        Ok(b) => b,
        Err(_) => {
            info!(%peer_id, "audio_inbound_loop: stream closed before header");
            return;
        }
    };
    let header: AudioStreamInit = match postcard::from_bytes(&header_bytes) {
        Ok(h) => h,
        Err(e) => {
            warn!(%peer_id, error = %e, "audio_inbound_loop: bad header");
            return;
        }
    };
    info!(%peer_id, codec = ?header.codec, "audio inbound stream wired");
    // Provision the render task now that we know the codec. The
    // `inbound_tx` slot was set up by `handle_start_audio`; we drain
    // through it into `audio_render_loop`.
    {
        let (tx, rx) = unbounded_channel::<bytes::Bytes>();
        *entry.inbound_tx.lock().expect("audio inbound tx poisoned") = Some(tx);
        let sink = match entry.sink.lock().await.clone() {
            Some(s) => s,
            None => {
                warn!(%peer_id, "audio_inbound_loop: sink missing; aborting render");
                return;
            }
        };
        let stats_render = entry.stats.clone();
        let handle = tokio::spawn(audio_render_loop(rx, sink, header.codec, stats_render));
        if let Some(prev) = entry
            .inbound_handle
            .lock()
            .expect("audio inbound slot poisoned")
            .replace(handle)
        {
            prev.abort();
        }
    }
    loop {
        let bytes = match stream.recv().await {
            Ok(b) => b,
            Err(ansync_transport::TransportError::Closed)
            | Err(ansync_transport::TransportError::TimedOut) => {
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
        entry.stats.record_in(bytes.len());
        if tx.send(bytes).is_err() {
            info!(%peer_id, "audio inbound receiver dropped; exiting");
            return;
        }
    }
}

async fn audio_stats_loop(device: DeviceId, entry: Arc<AudioEntry>) {
    use std::sync::atomic::Ordering::Relaxed;
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut prev_pkts_in = 0u64;
    let mut prev_pkts_out = 0u64;
    let mut prev_bytes_in = 0u64;
    let mut prev_bytes_out = 0u64;
    loop {
        tick.tick().await;
        let pkts_in = entry.stats.pkts_in.load(Relaxed);
        let pkts_out = entry.stats.pkts_out.load(Relaxed);
        let bytes_in = entry.stats.bytes_in.load(Relaxed);
        let bytes_out = entry.stats.bytes_out.load(Relaxed);
        let decode_fail = entry.stats.decode_fail.load(Relaxed);
        let encode_fail = entry.stats.encode_fail.load(Relaxed);
        let dp_in = pkts_in.saturating_sub(prev_pkts_in);
        let dp_out = pkts_out.saturating_sub(prev_pkts_out);
        let db_in = bytes_in.saturating_sub(prev_bytes_in);
        let db_out = bytes_out.saturating_sub(prev_bytes_out);
        prev_pkts_in = pkts_in;
        prev_pkts_out = pkts_out;
        prev_bytes_in = bytes_in;
        prev_bytes_out = bytes_out;
        // 5 s window — kbps shorthand: bytes/s * 8 / 1000.
        let kbps_in = (db_in as f64) * 8.0 / 5_000.0;
        let kbps_out = (db_out as f64) * 8.0 / 5_000.0;
        debug!(
            %device,
            in_pkts = dp_in,
            in_kbps = format!("{:.1}", kbps_in),
            out_pkts = dp_out,
            out_kbps = format!("{:.1}", kbps_out),
            decode_fail,
            encode_fail,
            "audio stats",
        );
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
    // Reset the persisted registration flag so a follow-up StartCamera
    // (possibly with a different format) re-runs `register()` on the
    // freshly-built sink instead of skipping it.
    entry
        .sink_registered
        .store(false, std::sync::atomic::Ordering::Release);
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
    // Lazy-register the sink on the first decoded frame so we know
    // the actual frame dimensions (decoder may re-derive from SPS).
    // The flag lives on `entry` (not in a local) so that subsequent
    // decoder spawns after a companion reconnect skip the re-register
    // path and keep writing to the *same* `/dev/video<N>` node — every
    // call to `sink.register()` dyn-adds a fresh loopback node, which
    // would yank the device out from under OBS / browsers / Discord.
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
        if !entry
            .sink_registered
            .load(std::sync::atomic::Ordering::Acquire)
        {
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
            entry
                .sink_registered
                .store(true, std::sync::atomic::Ordering::Release);
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
            Err(ansync_transport::TransportError::Closed)
            | Err(ansync_transport::TransportError::TimedOut) => {
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
    peer_name: String,
    permissions: Arc<dyn ansync_permissions::PermissionsStore>,
    policy: Arc<AutoAcceptPolicy>,
    dbus_conn: Arc<zbus::Connection>,
    coalescer: Arc<InboundCoalescer>,
) {
    let last_pct = Arc::new(std::sync::atomic::AtomicU8::new(255));
    let cb_device = peer_id.clone();
    let cb_dbus = dbus_conn.clone();
    let cb_pct = last_pct.clone();
    let progress: ProgressFn = Arc::new(move |ev: ProgressEvent| {
        if ev.direction != TransferDirection::Receive {
            return;
        }
        let pct = if ev.total == 0 {
            100u8
        } else {
            ((ev.bytes.saturating_mul(100) / ev.total).min(100)) as u8
        };
        let last = cb_pct.load(std::sync::atomic::Ordering::Relaxed);
        let is_final = ev.bytes == ev.total && ev.total > 0;
        if pct == last && !is_final {
            return;
        }
        cb_pct.store(pct, std::sync::atomic::Ordering::Relaxed);
        let dbus = cb_dbus.clone();
        let device = cb_device.clone();
        let name = ev.name.clone();
        let bytes = ev.bytes;
        let total = ev.total;
        let transfer_id = ev.transfer_id;
        tokio::spawn(async move {
            let _ = ansync_dbus::Device::emit_file_transfer_progress(
                &dbus,
                &device,
                0,
                transfer_id,
                &name,
                bytes,
                total,
                0,
                0,
                0,
                0,
                "receive",
            )
            .await;
        });
    });
    match receive_file(
        &peer_id,
        permissions.as_ref(),
        &mut stream,
        policy.as_ref(),
        Some(progress),
    )
    .await
    {
        Ok(path) => {
            info!(%peer_id, dest = %path.display(), "inbound transfer ok");
            let path_str = path.display().to_string();
            // Per-file D-Bus signal stays — programmatic consumers
            // want one event per file, not the coalesced batch.
            if let Err(e) =
                ansync_dbus::Device::emit_file_received(&dbus_conn, &peer_id, &path_str).await
            {
                debug!(%peer_id, error = %e, "emit FileReceived failed");
            }
            // Coalesce the notif UX: bursts of arrivals from the
            // same peer within `window` collapse into a single
            // "Received N files" toast.
            coalescer.record(peer_id, peer_name, path).await;
        }
        Err(e) => warn!(%peer_id, error = %e, "inbound transfer failed"),
    }
}

/// Coalesce inbound file completions per peer within a TTL so a
/// burst (e.g. multi-share of 5 photos) collapses into a single
/// "Received 5 files from <peer>" notif instead of 5 stacked entries.
///
/// Generation tagging is the trick that lets a single sleeping task
/// be the canonical flusher even while later arrivals reset the TTL:
/// every new record bumps the slot's `generation`; the deferred
/// flush snapshots the gen it was spawned for and aborts on mismatch.
pub struct InboundCoalescer {
    window: std::time::Duration,
    slots: tokio::sync::Mutex<HashMap<DeviceId, CoalesceSlot>>,
}

struct CoalesceSlot {
    peer_name: String,
    paths: Vec<PathBuf>,
    generation: u64,
}

impl InboundCoalescer {
    pub fn new(window: std::time::Duration) -> Arc<Self> {
        Arc::new(Self {
            window,
            slots: tokio::sync::Mutex::new(HashMap::new()),
        })
    }

    /// Stash an inbound completion. Always spawns a flush timer; the
    /// generation check inside the task guarantees only the latest
    /// timer wins, so concurrent records collapse cleanly.
    pub async fn record(self: &Arc<Self>, peer_id: DeviceId, peer_name: String, path: PathBuf) {
        let mut slots = self.slots.lock().await;
        let slot = slots.entry(peer_id.clone()).or_insert_with(|| CoalesceSlot {
            peer_name: peer_name.clone(),
            paths: Vec::new(),
            generation: 0,
        });
        slot.peer_name = peer_name;
        slot.paths.push(path);
        slot.generation = slot.generation.wrapping_add(1);
        let generation = slot.generation;
        let window = self.window;
        let coalescer = self.clone();
        let pid = peer_id.clone();
        drop(slots);
        tokio::spawn(async move {
            tokio::time::sleep(window).await;
            coalescer.flush_if_generation(pid, generation).await;
        });
    }

    async fn flush_if_generation(self: &Arc<Self>, peer_id: DeviceId, expected_gen: u64) {
        let mut slots = self.slots.lock().await;
        let Some(slot) = slots.get(&peer_id) else {
            return;
        };
        if slot.generation != expected_gen {
            return;
        }
        let slot = slots.remove(&peer_id).expect("just checked present");
        drop(slots);

        let count = slot.paths.len();
        if count == 0 {
            return;
        }
        if count == 1 {
            let path = &slot.paths[0];
            spawn_share_notif(
                &slot.peer_name,
                "File received",
                &format!("{}", path.display()),
            );
        } else {
            let mut sample = slot
                .paths
                .iter()
                .take(3)
                .map(|p| {
                    p.file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| p.display().to_string())
                })
                .collect::<Vec<_>>()
                .join(", ");
            if count > 3 {
                sample.push_str(", …");
            }
            spawn_share_notif(
                &slot.peer_name,
                &format!("Received {count} files"),
                &sample,
            );
        }
    }
}

async fn url_inbound_loop(
    mut stream: QuicStream,
    peer_id: DeviceId,
    peer_name: String,
    permissions: Arc<dyn ansync_permissions::PermissionsStore>,
) {
    let bytes = match stream.recv().await {
        Ok(b) => b,
        Err(e) => {
            warn!(%peer_id, error = %e, "url stream recv failed");
            return;
        }
    };
    let env: Envelope = match postcard::from_bytes(&bytes) {
        Ok(e) => e,
        Err(e) => {
            warn!(%peer_id, error = %e, "url postcard decode failed");
            return;
        }
    };
    let Message::Url(UrlMessage { url }) = env.message else {
        warn!(%peer_id, "url stream carried unexpected message kind");
        return;
    };
    match permissions.check(&peer_id, Permission::ShareReceive).await {
        Ok(true) => {}
        Ok(false) => {
            debug!(%peer_id, "share_receive off; dropping url");
            return;
        }
        Err(e) => {
            warn!(%peer_id, error = %e, "share_receive check failed; dropping url");
            return;
        }
    }
    // Linux side opens directly — paired peers are trusted at the
    // same level as the local clipboard. Android does the prompt
    // dance on its side. `xdg-open` is shelled out so we don't drag
    // in a portal client; users without `xdg-open` can install
    // `xdg-utils` (every desktop distro ships it).
    let url_for_open = url.clone();
    let peer_name_for_notif = peer_name.clone();
    let url_for_notif = url.clone();
    std::thread::spawn(move || {
        match std::process::Command::new("xdg-open").arg(&url_for_open).status() {
            Ok(status) if status.success() => {}
            Ok(status) => warn!(%status, "xdg-open returned non-zero status"),
            Err(e) => warn!(error = %e, "xdg-open invoke failed"),
        }
    });
    spawn_share_notif(&peer_name_for_notif, "Opened URL", &url_for_notif);
    info!(%peer_id, url, "inbound url opened");
}

/// Cross-file shared state captured by every per-file `ProgressFn` so
/// the batch sender can render a single notif ("Sending 2 of 5 · 47%")
/// instead of five independent ones. Atomics keep the callback
/// allocation-free on the chunk path.
struct BatchProgress {
    batch_id: u64,
    total_files: u32,
    total_bytes: u64,
    files_done: std::sync::atomic::AtomicU32,
    bytes_done: std::sync::atomic::AtomicU64,
    last_pct: std::sync::atomic::AtomicU8,
}

async fn handle_send_files(
    mirrors: &MirrorRegistry,
    permissions: &Arc<dyn ansync_permissions::PermissionsStore>,
    dbus_conn: &Arc<zbus::Connection>,
    device: &DeviceId,
    paths: Vec<PathBuf>,
) {
    let Some(entry) = mirrors.get(device) else {
        warn!(%device, "SendFiles: no live mirror entry");
        return;
    };
    let conn = entry.conn.lock().expect("conn slot poisoned").clone();
    let Some(conn) = conn else {
        warn!(%device, "SendFiles: peer not connected");
        return;
    };
    let peer_name = entry.peer_name.clone();

    // Upfront sizing pass: stat every path so the batch notif has a
    // real total to divide against. Anything that fails to stat is
    // skipped from the batch entirely — better to under-report than
    // to inflate the denominator and stall the bar at 99%.
    let mut sized: Vec<(PathBuf, u64)> = Vec::with_capacity(paths.len());
    let mut total_bytes: u64 = 0;
    for path in paths {
        match tokio::fs::metadata(&path).await {
            Ok(m) => {
                total_bytes = total_bytes.saturating_add(m.len());
                sized.push((path, m.len()));
            }
            Err(e) => warn!(%device, error = %e, path = %path.display(), "stat failed; skipping path"),
        }
    }
    if sized.is_empty() {
        warn!(%device, "SendFiles: nothing to send after stat");
        return;
    }

    let batch = Arc::new(BatchProgress {
        batch_id: next_transfer_id(),
        total_files: sized.len() as u32,
        total_bytes,
        files_done: std::sync::atomic::AtomicU32::new(0),
        bytes_done: std::sync::atomic::AtomicU64::new(0),
        last_pct: std::sync::atomic::AtomicU8::new(255),
    });

    for (idx, (path, file_size)) in sized.into_iter().enumerate() {
        let mut stream = match conn.open(StreamKind::Files).await {
            Ok(s) => s,
            Err(e) => {
                warn!(%device, error = %e, "open Files stream failed");
                continue;
            }
        };
        let tid = next_transfer_id();

        let cb_batch = batch.clone();
        let cb_peer = peer_name.clone();
        let cb_device = device.clone();
        let cb_dbus = dbus_conn.clone();
        let prev_in_file = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let progress: ProgressFn = Arc::new(move |ev: ProgressEvent| {
            // Receive callbacks share the type with us via the trait;
            // ignore the wrong direction defensively.
            if ev.direction != TransferDirection::Send {
                return;
            }
            let prev = prev_in_file.swap(ev.bytes, std::sync::atomic::Ordering::Relaxed);
            let delta = ev.bytes.saturating_sub(prev);
            let cum = cb_batch
                .bytes_done
                .fetch_add(delta, std::sync::atomic::Ordering::Relaxed)
                + delta;
            let pct = if cb_batch.total_bytes == 0 {
                100u8
            } else {
                ((cum.saturating_mul(100) / cb_batch.total_bytes).min(100)) as u8
            };
            let last = cb_batch.last_pct.load(std::sync::atomic::Ordering::Relaxed);
            let is_final = ev.bytes == ev.total && ev.total > 0;
            if pct != last || is_final {
                cb_batch
                    .last_pct
                    .store(pct, std::sync::atomic::Ordering::Relaxed);
                let files_done = cb_batch.files_done.load(std::sync::atomic::Ordering::Relaxed);
                let summary = format!(
                    "Sending {} of {} to {}",
                    (files_done + 1).min(cb_batch.total_files),
                    cb_batch.total_files,
                    cb_peer
                );
                let body = format!("{} · {}%", ev.name, pct);
                spawn_progress_notif(cb_batch.batch_id, &summary, &body, pct);

                // Fan the same throttled event onto D-Bus so external
                // UIs (DMS plugin, ansyncctl) can render their own
                // progress without spawning notify-send themselves.
                let dbus = cb_dbus.clone();
                let device = cb_device.clone();
                let batch_id = cb_batch.batch_id;
                let name = ev.name.clone();
                let bytes = ev.bytes;
                let total = ev.total;
                let batch_files = cb_batch.total_files;
                let batch_total_bytes = cb_batch.total_bytes;
                let transfer_id = ev.transfer_id;
                tokio::spawn(async move {
                    let _ = ansync_dbus::Device::emit_file_transfer_progress(
                        &dbus,
                        &device,
                        batch_id,
                        transfer_id,
                        &name,
                        bytes,
                        total,
                        batch_files,
                        files_done,
                        cum,
                        batch_total_bytes,
                        "send",
                    )
                    .await;
                });
            }
            if is_final {
                cb_batch
                    .files_done
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        });

        match send_file(
            device,
            permissions.as_ref(),
            &mut stream,
            &path,
            tid,
            Some(progress),
        )
        .await
        {
            Ok(_) => info!(%device, path = %path.display(), "outbound file sent"),
            Err(e) => warn!(%device, error = %e, path = %path.display(), "outbound file failed"),
        }
        let _ = (idx, file_size);
    }

    // Final summary: collapse the synchronous progress notif into a
    // single "Sent N files" toast. Same tag → libnotify replaces the
    // progress notif in-place on KDE / GNOME.
    let files_done = batch.files_done.load(std::sync::atomic::Ordering::Relaxed);
    let summary = format!("Sent {} of {} to {}", files_done, batch.total_files, peer_name);
    spawn_batch_done_notif(batch.batch_id, &summary);
}

/// Fire a progress notif. Uses the synchronous tag so KDE / GNOME
/// replace the previous progress entry instead of stacking N of them
/// in the shade. `pct` is encoded as the `value` hint libnotify
/// renders as a built-in progress bar.
fn spawn_progress_notif(batch_id: u64, summary: &str, body: &str, pct: u8) {
    let summary = format!("ansync · {summary}");
    let body = body.to_string();
    let tag = format!("ansync-xfer-{batch_id}");
    let pct = pct.min(100);
    std::thread::spawn(move || {
        if let Err(e) = std::process::Command::new("notify-send")
            .arg("--app-name=ansync")
            .arg("--icon=document-send")
            .arg(format!("--hint=int:value:{pct}"))
            .arg(format!("--hint=string:x-canonical-private-synchronous:{tag}"))
            .arg(summary)
            .arg(body)
            .status()
        {
            debug!(error = %e, "notify-send progress invoke failed");
        }
    });
}

/// Final-summary notif replacing the synchronous progress entry for
/// `batch_id` once the last file completes.
fn spawn_batch_done_notif(batch_id: u64, summary: &str) {
    let summary = format!("ansync · {summary}");
    let tag = format!("ansync-xfer-{batch_id}");
    std::thread::spawn(move || {
        if let Err(e) = std::process::Command::new("notify-send")
            .arg("--app-name=ansync")
            .arg("--icon=document-send")
            .arg(format!("--hint=string:x-canonical-private-synchronous:{tag}"))
            .arg(summary)
            .status()
        {
            debug!(error = %e, "notify-send batch-done invoke failed");
        }
    });
}

async fn handle_send_url(
    mirrors: &MirrorRegistry,
    device: &DeviceId,
    url: String,
) {
    let Some(entry) = mirrors.get(device) else {
        warn!(%device, "SendUrl: no live mirror entry");
        return;
    };
    let conn = entry.conn.lock().expect("conn slot poisoned").clone();
    let Some(conn) = conn else {
        warn!(%device, "SendUrl: peer not connected");
        return;
    };
    let mut stream = match conn.open(StreamKind::Url).await {
        Ok(s) => s,
        Err(e) => {
            warn!(%device, error = %e, "open Url stream failed");
            return;
        }
    };
    let env = Envelope {
        version: PROTOCOL_VERSION,
        message: Message::Url(UrlMessage { url: url.clone() }),
    };
    let bytes = match postcard::to_allocvec(&env) {
        Ok(b) => b,
        Err(e) => {
            warn!(%device, error = %e, "encode UrlMessage failed");
            return;
        }
    };
    if let Err(e) = stream.send(bytes::Bytes::from(bytes)).await {
        warn!(%device, error = %e, "url stream send failed");
        return;
    }
    info!(%device, url, "outbound url sent");
}

/// Monotonically increasing transfer id seed used for outbound
/// `Device.SendFile` calls. The wire protocol carries the id so the
/// receiver can match `Offer` ↔ `Accept` ↔ chunks across one stream.
fn next_transfer_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEED: AtomicU64 = AtomicU64::new(1);
    SEED.fetch_add(1, Ordering::Relaxed)
}

/// Fire-and-forget desktop notification via `notify-send`. We shell
/// out instead of taking a `libnotify` Rust binding to keep the
/// daemon's runtime deps small — `notify-send` ships with every
/// desktop libnotify install we care about.
fn spawn_share_notif(peer_name: &str, summary: &str, body: &str) {
    let summary = format!("ansync · {peer_name}: {summary}");
    let body = body.to_string();
    std::thread::spawn(move || {
        if let Err(e) = std::process::Command::new("notify-send")
            .arg("--app-name=ansync")
            .arg("--icon=document-send")
            .arg(summary)
            .arg(body)
            .status()
        {
            debug!(error = %e, "notify-send invoke failed");
        }
    });
}

async fn input_stream_loop(
    mut stream: QuicStream,
    session: Arc<Mutex<InputSession>>,
    rx_counter: Arc<std::sync::atomic::AtomicU64>,
) {
    loop {
        let bytes = match stream.recv().await {
            Ok(b) => b,
            Err(ansync_transport::TransportError::Closed)
            | Err(ansync_transport::TransportError::TimedOut) => break,
            Err(e) => {
                warn!(error = %e, "input stream recv failed");
                break;
            }
        };
        rx_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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

/// `$XDG_DOWNLOAD_DIR/ansync` when XDG resolves the Downloads dir,
/// falling back to `$HOME/Downloads/ansync` and finally `./ansync`.
/// Picked so received files land somewhere the user actually opens
/// (file manager, browser downloads list) instead of buried inside
/// `~/.local/share/ansync/incoming`.
fn default_download_dir() -> PathBuf {
    if let Some(u) = UserDirs::new() {
        if let Some(d) = u.download_dir() {
            return d.join("ansync");
        }
        return u.home_dir().join("Downloads").join("ansync");
    }
    PathBuf::from("ansync")
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
