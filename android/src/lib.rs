//! Native (Rust) half of the ansync companion.
//!
//! Exposes a small JNI surface that the Kotlin `AnsyncCompanionService`
//! calls into. Internally owns a `tokio` runtime + a `quinn` QUIC
//! client to the paired host. Wire format is identical to the host
//! (`ansync_proto`) so the daemon's `StreamKind::Input` /
//! `StreamKind::Video` accept loop just works.
//!
//! Step 7d-2 wires the real `quinn` dial + per-direction streams:
//! the companion *sends* Video, *receives* Input. Reverse-input
//! frames land on an `mpsc::UnboundedSender` and Kotlin pulls them
//! via `nativePollInputMessage`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// Global liveness flag for the active QUIC session against the host.
/// Set to `true` after a successful `nativeOpenConnection` handshake
/// and cleared by `streams_accept_loop` the moment the accept side
/// errors out (peer closed, daemon restarted, network dropped, …).
/// `HostDialer` polls this so it can clear its Kotlin-side
/// `connected` cache + reschedule a dial without waiting for a
/// network transition.
static CONNECTED: AtomicBool = AtomicBool::new(false);

/// Outbound InputMessage counter. Bumped per successful
/// `nativeSendInputMessage` send; sampled (delta) by
/// `stats_telemetry_loop` and logged so we can cross-check against the
/// daemon-side `input_rx` field. Mismatch == packet loss / decode
/// failure on the wire.
static INPUT_TX_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn mark_connected(state: bool) {
    CONNECTED.store(state, Ordering::Relaxed);
}

use ansync_core::{Capabilities, DeviceId, DeviceName, DevicePermissions, Permission};
use ansync_crypto::IdentityKeypair;
use ansync_files::{
    AutoAcceptPolicy, Direction as TransferDirection, ProgressEvent, ProgressFn, receive_file,
    send_file,
};
use ansync_pairing::cable::bootstrap_companion;
use ansync_pairing::wifi::{read_pair_hello, respond_pair_pin, CompanionWifiOutcome};
use ansync_permissions::{PermissionsError, PermissionsStore};
use ansync_proto::{
    ClipboardMessage, ControlMessage, Envelope, GamepadState, Hello, InputMessage, Message,
    NotificationMessage, PROTOCOL_VERSION, UrlMessage,
};
use ansync_transport::{
    Connection, QuicConnection, QuicStream, QuicTransport, Stream as _, StreamKind,
};
use bytes::Bytes;
use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JString};
use jni::sys::{jboolean, jint, jlong};
use log::{debug, error, info, warn};
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::Mutex as AsyncMutex;
use std::sync::Arc;

/// Process-wide tokio runtime. Initialised on first `nativeInit` call
/// and never torn down — the companion's foreground service owns the
/// process lifecycle, and recreating the runtime on each Kotlin
/// reconnect would leak background workers.
static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// One-shot global stored at `nativeInit` time. Holds the identity
/// keypair (loaded from / saved to `{filesDir}/identity.key`) and the
/// active session if any.
static STATE: OnceLock<Mutex<Option<CompanionState>>> = OnceLock::new();

fn state_slot() -> &'static Mutex<Option<CompanionState>> {
    STATE.get_or_init(|| Mutex::new(None))
}

/// Always-on WiFi-pair listener slot, owned by the foreground service.
/// The companion service calls `nativeWifiPairListenerStart` on
/// `onCreate` (idempotent) and `nativeWifiPairListenerStop` on
/// `onDestroy`. A worker thread in the service polls
/// `nativePollPairEvent` to convert protocol events into OS heads-up
/// notifications and pair persistence.
static WIFI_PAIR: OnceLock<Mutex<Option<WifiPairSlot>>> = OnceLock::new();

fn wifi_pair_slot() -> &'static Mutex<Option<WifiPairSlot>> {
    WIFI_PAIR.get_or_init(|| Mutex::new(None))
}

struct WifiPairSlot {
    port: u16,
    task: tokio::task::JoinHandle<()>,
    events_rx: Arc<AsyncMutex<UnboundedReceiver<PairEvent>>>,
}

/// Protocol-level events emitted by the always-on WiFi pair listener.
/// Encoded as `String` for JNI transport (Kotlin parses on receipt).
#[derive(Debug, Clone)]
enum PairEvent {
    /// Host has sent `BootstrapHello`. PIN has been generated and is
    /// safe to display on screen now; the listener is waiting for the
    /// host's `PinConfirm`.
    Request {
        host_pubkey: [u8; 32],
        host_name: String,
        pin: [u8; 6],
    },
    /// Host's PIN MAC did not match. `remaining` is the number of
    /// attempts left before the listener locks the PIN and rotates.
    BadPin { host_name: String, remaining: u8 },
    /// Listener has hit the 3-strike lockout for the active PIN; the
    /// PIN is rotated and a future `Request` will follow with the new
    /// value if the same host (or another) retries.
    Lockout { host_name: String },
    /// Pairing completed successfully.
    Ok {
        host_pubkey: [u8; 32],
        host_name: String,
    },
}

impl PairEvent {
    /// Wire encoding for JNI. Single line so Kotlin can split on `|`.
    /// Tag prefix lets the caller dispatch without parsing the rest if
    /// they only care about, e.g., `OK` events.
    fn encode(&self) -> String {
        match self {
            PairEvent::Request { host_pubkey, host_name, pin } => format!(
                "REQUEST|{}|{}|{}",
                hex_encode(host_pubkey),
                host_name,
                std::str::from_utf8(pin).unwrap_or("000000"),
            ),
            PairEvent::BadPin { host_name, remaining } => {
                format!("BAD|{remaining}|{host_name}")
            }
            PairEvent::Lockout { host_name } => format!("LOCK|{host_name}"),
            PairEvent::Ok { host_pubkey, host_name } => {
                format!("OK|{}|{}", hex_encode(host_pubkey), host_name)
            }
        }
    }
}

fn runtime() -> &'static Runtime {
    RUNTIME.get().expect("nativeInit() not called before runtime use")
}

/// Prefer the public `Download/ansync` tree (user can find received
/// files in any file manager / gallery) when MANAGE_EXTERNAL_STORAGE
/// is granted, otherwise fall back to the app-private sandbox so
/// transfers still complete. Re-evaluated on every `nativeInit`, so
/// revoking the perm + relaunching the service rebuilds with the
/// fallback path while the setup notif re-flags the missing grant.
fn resolve_download_root(files_dir: &std::path::Path) -> PathBuf {
    let public = PathBuf::from("/storage/emulated/0/Download/ansync");
    match std::fs::create_dir_all(&public) {
        Ok(()) => {
            info!("download root: {}", public.display());
            public
        }
        Err(e) => {
            warn!(
                "download root {} unavailable ({e}); falling back to sandbox",
                public.display()
            );
            files_dir.join("incoming")
        }
    }
}

struct CompanionState {
    identity: IdentityKeypair,
    /// Path the inbound files accept loop writes received files
    /// into. Defaults to the app's `filesDir/incoming/` until the
    /// Kotlin side picks a SAF tree URI.
    download_dir: PathBuf,
    /// Human-readable device name. Pushed to the host on every
    /// connect via `StreamKind::Hello`. `None` until Kotlin calls
    /// `nativeSetDeviceName` (typically once at service onCreate
    /// with `Build.MODEL`).
    device_name: Option<String>,
    /// Latest host name learned from the inbound Hello frame. Kotlin
    /// polls via `nativePollHostName` for the paired-host card. Stays
    /// `None` until the first session post-handshake completes.
    last_host_name: Arc<Mutex<Option<String>>>,
    session: Option<ActiveSession>,
}

/// In-memory permissions store the companion uses for the single
/// paired daemon. Defaults to "everything on" because the daemon's
/// pubkey was already accepted at pairing time; mid-session revoke
/// UX surfaces in Step 12 (clipboard) and onward.
#[derive(Debug)]
struct PermissivePermissions;

#[async_trait::async_trait]
impl PermissionsStore for PermissivePermissions {
    async fn load(&self, _id: &DeviceId) -> Result<DevicePermissions, PermissionsError> {
        Ok(DevicePermissions::default())
    }
    async fn save(
        &self,
        _id: &DeviceId,
        _perms: &DevicePermissions,
    ) -> Result<(), PermissionsError> {
        Ok(())
    }
    async fn delete(&self, _id: &DeviceId) -> Result<(), PermissionsError> {
        Ok(())
    }
    async fn check(
        &self,
        _id: &DeviceId,
        _permission: Permission,
    ) -> Result<bool, PermissionsError> {
        Ok(true)
    }
}

struct ActiveSession {
    /// Held purely for its drop-side teardown — the connection
    /// closes when this is taken.
    conn: Arc<QuicConnection>,
    video_stream: Arc<AsyncMutex<QuicStream>>,
    /// Outbound device→host Input stream. Lazy-opened on first
    /// `nativeSendInputMessage` call so the wire is only used when
    /// the user actually drives the touchpad activity.
    outbound_input: Arc<AsyncMutex<Option<QuicStream>>>,
    /// Receiver side of the reverse-input pump. `Mutex<>` so Kotlin
    /// can call `nativePollInputMessage` from any thread without
    /// reading-while-spawning races against the recv task.
    input_rx: Arc<AsyncMutex<UnboundedReceiver<Vec<u8>>>>,
    /// Outbound device→host Camera stream. Lazy-opened on first
    /// `nativeSendCameraChunk` call (typically right after Kotlin
    /// processes a StartCamera control message).
    outbound_camera: Arc<AsyncMutex<Option<QuicStream>>>,
    /// Inbound `ControlMessage::StartCamera` / `StopCamera` decoded
    /// from the host's Control stream. Encoded as tag-binary blobs
    /// for the Kotlin polling loop. Mirrors the FS request channel
    /// pattern.
    camera_ctrl_rx: Arc<AsyncMutex<UnboundedReceiver<Vec<u8>>>>,
    /// Inbound `ControlMessage::StartAudioRoute` / `StopAudioRoute`.
    /// Same tag-binary fanout pattern as camera_ctrl_rx.
    audio_ctrl_rx: Arc<AsyncMutex<UnboundedReceiver<Vec<u8>>>>,
    /// Inbound `ControlMessage::RequestScreenCapture` /
    /// `StopScreenCapture`. Two single-byte tags so Kotlin can poll
    /// without postcard.
    capture_ctrl_rx: Arc<AsyncMutex<UnboundedReceiver<Vec<u8>>>>,
    /// Outbound device→host Audio stream for mic forwarding.
    /// Lazy-opened on the first `nativeSendAudioChunk` (device-side).
    outbound_audio: Arc<AsyncMutex<Option<QuicStream>>>,
    /// Receiver side of host→device PCM. Kotlin polls it via
    /// `nativePollAudioChunk` and writes to AudioTrack.
    audio_in_rx: Arc<AsyncMutex<UnboundedReceiver<Vec<u8>>>>,
    /// Inbound clipboard text from the host. UTF-8 bytes.
    clipboard_in_rx: Arc<AsyncMutex<UnboundedReceiver<String>>>,
    /// Inbound clipboard blob: `(mime, data)`. Kotlin polls via
    /// `nativePollClipboardBlob` which returns a flat
    /// `[mime_len u32 LE | mime utf8 | data]` encoding.
    clipboard_in_blob_rx: Arc<AsyncMutex<UnboundedReceiver<(String, Vec<u8>)>>>,
    /// Inbound "open this URL" requests forwarded by the host. Kotlin
    /// pops via `nativePollIncomingUrl` and posts the consent
    /// notification — see `WireUrlMessage` design notes.
    url_in_rx: Arc<AsyncMutex<UnboundedReceiver<String>>>,
    /// Absolute paths of inbound files that finished downloading.
    /// Kotlin pops via `nativePollReceivedFile` to fire the "tap to
    /// open" notification + run a `MediaScannerConnection.scanFile`.
    received_files_rx: Arc<AsyncMutex<UnboundedReceiver<String>>>,
    /// Host's stable device id derived from its Ed25519 pubkey. Used
    /// as the `peer_id` argument to `send_file` for outbound shares.
    host_id: DeviceId,
    /// Progress events emitted by `nativeSendFiles`. Drained by the
    /// Kotlin service via `nativePollTransferProgress`. Tag-binary
    /// blobs encoded by `encode_progress_for_kotlin`.
    progress_tx: Arc<UnboundedSender<Vec<u8>>>,
    progress_rx: Arc<AsyncMutex<UnboundedReceiver<Vec<u8>>>>,
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativeInit(
    mut env: JNIEnv,
    _class: JClass,
    files_dir: JString,
) -> jboolean {
    let files_dir: String = match env.get_string(&files_dir) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("nativeInit: invalid filesDir string: {e}");
            return jni::sys::JNI_FALSE;
        }
    };
    let files_dir = PathBuf::from(files_dir);

    // Runtime is created once; subsequent calls are no-ops + return
    // success so the Kotlin side can call this idempotently after a
    // service restart.
    let _ = RUNTIME.set(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("ansync-native")
            .build()
            .expect("tokio runtime build"),
    );
    android_logger::init_once(
        android_logger::Config::default()
            .with_tag("ansync")
            .with_max_level(log::LevelFilter::Info),
    );

    let identity = match IdentityKeypair::load_or_generate(&files_dir.join("identity.key")) {
        Ok(k) => k,
        Err(e) => {
            error!("nativeInit: identity load_or_generate failed: {e}");
            return jni::sys::JNI_FALSE;
        }
    };
    info!(
        "ansync companion ready (device_id={})",
        identity.device_id()
    );
    let mut slot = state_slot().lock().expect("state mutex poisoned");
    let download_dir = resolve_download_root(&files_dir);
    *slot = Some(CompanionState {
        identity,
        download_dir,
        device_name: None,
        last_host_name: Arc::new(Mutex::new(None)),
        session: None,
    });
    jni::sys::JNI_TRUE
}

/// Stash the human-readable device name. Called by Kotlin once per
/// service lifetime with `Build.MANUFACTURER + " " + Build.MODEL`. The
/// stashed name is forwarded to the host inside every Hello frame so
/// the daemon's `PeerStore.name` stays in sync with what the user
/// renamed the device to in Settings.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativeSetDeviceName<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    name: JString<'local>,
) -> jboolean {
    let name: String = match env.get_string(&name) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("nativeSetDeviceName: invalid string: {e}");
            return jni::sys::JNI_FALSE;
        }
    };
    let mut slot = state_slot().lock().expect("state mutex poisoned");
    if let Some(s) = slot.as_mut() {
        info!("device name set to {name}");
        s.device_name = Some(name);
        jni::sys::JNI_TRUE
    } else {
        warn!("nativeSetDeviceName: state not initialised");
        jni::sys::JNI_FALSE
    }
}

/// Return the latest host name observed on a Hello frame, or `null`
/// if no session has completed a handshake yet. Cheap; the value is
/// just a `String` clone behind a mutex.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativePollHostName<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jni::sys::jstring {
    let name = {
        let slot = state_slot().lock().expect("state mutex poisoned");
        slot.as_ref()
            .and_then(|s| s.last_host_name.lock().ok().and_then(|g| g.clone()))
    };
    match name {
        Some(s) => match env.new_string(s) {
            Ok(js) => js.into_raw(),
            Err(_) => std::ptr::null_mut(),
        },
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativeOurPubkeyHex<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jni::sys::jstring {
    let slot = state_slot().lock().expect("state mutex poisoned");
    let identity = match slot.as_ref() {
        Some(s) => &s.identity,
        None => return std::ptr::null_mut(),
    };
    let hex = hex_encode(&identity.public().as_bytes());
    match env.new_string(hex) {
        Ok(s) => s.into_raw(),
        Err(e) => {
            error!("nativeOurPubkeyHex: env.new_string failed: {e}");
            std::ptr::null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativeOpenConnection<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    host: JString<'local>,
    port: jint,
    daemon_pubkey_hex: JString<'local>,
) -> jboolean {
    let host: String = match env.get_string(&host) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("nativeOpenConnection: invalid host string: {e}");
            return jni::sys::JNI_FALSE;
        }
    };
    let port = match u16::try_from(port) {
        Ok(p) => p,
        Err(_) => {
            error!("nativeOpenConnection: port {port} out of range");
            return jni::sys::JNI_FALSE;
        }
    };
    let pubkey_hex: String = match env.get_string(&daemon_pubkey_hex) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("nativeOpenConnection: invalid pubkey string: {e}");
            return jni::sys::JNI_FALSE;
        }
    };
    let expected_server = match hex_decode_32(&pubkey_hex) {
        Some(k) => k,
        None => {
            error!("nativeOpenConnection: pubkey hex must be 64 chars");
            return jni::sys::JNI_FALSE;
        }
    };

    let (identity, device_name, host_name_slot) = {
        let slot = state_slot().lock().expect("state mutex poisoned");
        match slot.as_ref() {
            Some(s) => (
                IdentityKeypair::from_seed(*s.identity.seed_bytes()),
                s.device_name.clone(),
                s.last_host_name.clone(),
            ),
            None => {
                error!("nativeOpenConnection: state not initialised");
                return jni::sys::JNI_FALSE;
            }
        }
    };

    let addr_str = format!("{host}:{port}");
    let addr: std::net::SocketAddr = match addr_str.parse() {
        Ok(a) => a,
        Err(_) => {
            // Caller may have passed a DNS name; attempt resolution.
            match runtime().block_on(tokio::net::lookup_host(addr_str.as_str())) {
                Ok(mut it) => match it.next() {
                    Some(a) => a,
                    None => {
                        error!("nativeOpenConnection: lookup_host empty for {addr_str}");
                        return jni::sys::JNI_FALSE;
                    }
                },
                Err(e) => {
                    error!("nativeOpenConnection: lookup_host {addr_str}: {e}");
                    return jni::sys::JNI_FALSE;
                }
            }
        }
    };

    let identity_for_hello = IdentityKeypair::from_seed(*identity.seed_bytes());
    let transport = QuicTransport::new(identity);
    let conn = match runtime().block_on(transport.connect(addr, expected_server)) {
        Ok(c) => c,
        Err(e) => {
            error!("nativeOpenConnection: dial {addr}: {e}");
            return jni::sys::JNI_FALSE;
        }
    };
    info!("nativeOpenConnection: handshake ok with {addr}");

    // Send our Hello before opening any media stream — gives the host
    // the freshest name/caps the moment the connection is up. Failure
    // here is logged but not fatal; the host falls back to the stored
    // name from pairing.
    {
        let name = device_name
            .clone()
            .unwrap_or_else(|| "android".to_string());
        if let Err(e) = runtime().block_on(send_hello(&conn, &identity_for_hello, &name)) {
            warn!("nativeOpenConnection: send_hello failed: {e}");
        }
    }

    let video_stream = match runtime().block_on(conn.open(StreamKind::Video)) {
        Ok(s) => s,
        Err(e) => {
            error!("nativeOpenConnection: open Video stream: {e}");
            return jni::sys::JNI_FALSE;
        }
    };
    // Convention: the OPENER of a stream uses it for send. The
    // host→device input stream is opened by the daemon on
    // ShowScreen; we accept it in `streams_accept_loop`. Device→host
    // input (9.5e) opens its own stream lazily when the user starts
    // interacting with the projected overlay.
    let (input_tx, input_rx) = unbounded_channel::<Vec<u8>>();
    let input_tx_arc = Arc::new(input_tx);

    let (camera_ctrl_tx, camera_ctrl_rx) = unbounded_channel::<Vec<u8>>();
    let camera_ctrl_tx = Arc::new(camera_ctrl_tx);

    let (audio_ctrl_tx, audio_ctrl_rx) = unbounded_channel::<Vec<u8>>();
    let audio_ctrl_tx = Arc::new(audio_ctrl_tx);

    let (capture_ctrl_tx, capture_ctrl_rx) = unbounded_channel::<Vec<u8>>();
    let capture_ctrl_tx = Arc::new(capture_ctrl_tx);

    let (audio_in_tx, audio_in_rx) = unbounded_channel::<Vec<u8>>();
    let audio_in_tx = Arc::new(audio_in_tx);

    let (clip_in_tx, clip_in_rx) = unbounded_channel::<String>();
    let clip_in_tx = Arc::new(clip_in_tx);

    let (clip_blob_tx, clip_blob_rx) = unbounded_channel::<(String, Vec<u8>)>();
    let clip_blob_tx = Arc::new(clip_blob_tx);

    let (url_in_tx, url_in_rx) = unbounded_channel::<String>();
    let url_in_tx = Arc::new(url_in_tx);

    let (received_file_tx, received_file_rx) = unbounded_channel::<String>();
    let received_file_tx = Arc::new(received_file_tx);

    let (progress_tx, progress_rx) = unbounded_channel::<Vec<u8>>();
    let progress_tx = Arc::new(progress_tx);

    let conn_arc = Arc::new(conn);
    let download_dir = {
        let slot = state_slot().lock().expect("state mutex poisoned");
        slot.as_ref()
            .map(|s| s.download_dir.clone())
            .unwrap_or_else(|| PathBuf::from("/data/local/tmp/ansync-incoming"))
    };
    // Companion's peer identity for permission checks. We use the
    // host's pubkey-derived DeviceId so the permissive store sees the
    // same id the host would query against.
    let host_device_id = {
        let mut id_bytes = [0u8; 16];
        id_bytes.copy_from_slice(&expected_server[..16]);
        DeviceId(id_bytes)
    };
    runtime().spawn(streams_accept_loop(
        conn_arc.clone(),
        host_device_id.clone(),
        download_dir,
        input_tx_arc.clone(),
        camera_ctrl_tx.clone(),
        audio_ctrl_tx.clone(),
        audio_in_tx.clone(),
        clip_in_tx.clone(),
        clip_blob_tx.clone(),
        host_name_slot,
        capture_ctrl_tx.clone(),
        url_in_tx.clone(),
        received_file_tx.clone(),
        progress_tx.clone(),
    ));

    // Periodic QUIC stats. Auto-exits when streams_accept_loop drops
    // the conn (last Arc gone → Weak::upgrade returns None).
    runtime().spawn(stats_telemetry_loop(Arc::downgrade(&conn_arc)));

    let session = ActiveSession {
        conn: conn_arc,
        video_stream: Arc::new(AsyncMutex::new(video_stream)),
        outbound_input: Arc::new(AsyncMutex::new(None)),
        input_rx: Arc::new(AsyncMutex::new(input_rx)),
        outbound_camera: Arc::new(AsyncMutex::new(None)),
        camera_ctrl_rx: Arc::new(AsyncMutex::new(camera_ctrl_rx)),
        audio_ctrl_rx: Arc::new(AsyncMutex::new(audio_ctrl_rx)),
        outbound_audio: Arc::new(AsyncMutex::new(None)),
        audio_in_rx: Arc::new(AsyncMutex::new(audio_in_rx)),
        clipboard_in_rx: Arc::new(AsyncMutex::new(clip_in_rx)),
        clipboard_in_blob_rx: Arc::new(AsyncMutex::new(clip_blob_rx)),
        capture_ctrl_rx: Arc::new(AsyncMutex::new(capture_ctrl_rx)),
        url_in_rx: Arc::new(AsyncMutex::new(url_in_rx)),
        received_files_rx: Arc::new(AsyncMutex::new(received_file_rx)),
        host_id: host_device_id,
        progress_tx,
        progress_rx: Arc::new(AsyncMutex::new(progress_rx)),
    };
    let mut slot = state_slot().lock().expect("state mutex poisoned");
    if let Some(s) = slot.as_mut() {
        s.session = Some(session);
    }
    mark_connected(true);
    jni::sys::JNI_TRUE
}

/// Polled by `HostDialer` (Kotlin) to detect post-handshake drops
/// the network callbacks can't see — daemon restart, QUIC idle
/// timeout, peer reset. Returns `true` while the session is live.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativeIsConnected<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jboolean {
    if CONNECTED.load(Ordering::Relaxed) {
        jni::sys::JNI_TRUE
    } else {
        jni::sys::JNI_FALSE
    }
}

/// Periodic per-session QUIC stats. Mirror of the daemon-side loop in
/// `ansync-daemon-core::stats_telemetry_loop`. Exits when the last
/// strong `Arc<QuicConnection>` is dropped (session torn down).
/// Tagged at log-level `Debug` — gate with
/// `RUST_LOG=ansync_companion_native=debug` to surface in logcat.
async fn stats_telemetry_loop(conn: std::sync::Weak<QuicConnection>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
    tick.tick().await;
    let mut prev_sent: u64 = 0;
    let mut prev_lost: u64 = 0;
    let mut prev_input: u64 = 0;
    loop {
        tick.tick().await;
        let Some(conn) = conn.upgrade() else {
            return;
        };
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
        let input_total = INPUT_TX_COUNT.load(std::sync::atomic::Ordering::Relaxed);
        let dinput = input_total.saturating_sub(prev_input);
        prev_input = input_total;
        debug!(
            "quic stats: rtt={}ms sent={} lost={} loss={:.2}% cwnd={} black_holes={} input_tx={}",
            conn.rtt().as_millis() as u64,
            dsent,
            dlost,
            loss_pct,
            s.path.cwnd,
            s.path.black_holes_detected,
            dinput,
        );
    }
}

async fn streams_accept_loop(
    conn: Arc<QuicConnection>,
    host_id: DeviceId,
    download_dir: PathBuf,
    input_inbound_tx: Arc<UnboundedSender<Vec<u8>>>,
    camera_ctrl_tx: Arc<UnboundedSender<Vec<u8>>>,
    audio_ctrl_tx: Arc<UnboundedSender<Vec<u8>>>,
    audio_in_tx: Arc<UnboundedSender<Vec<u8>>>,
    clip_in_tx: Arc<UnboundedSender<String>>,
    clip_blob_tx: Arc<UnboundedSender<(String, Vec<u8>)>>,
    host_name_slot: Arc<Mutex<Option<String>>>,
    capture_ctrl_tx: Arc<UnboundedSender<Vec<u8>>>,
    url_in_tx: Arc<UnboundedSender<String>>,
    received_file_tx: Arc<UnboundedSender<String>>,
    progress_tx: Arc<UnboundedSender<Vec<u8>>>,
) {
    let permissions: Arc<dyn PermissionsStore> = Arc::new(PermissivePermissions);
    // Helper struct so any early return out of the accept loop —
    // graceful Closed or a hard error — flips the global liveness
    // flag and unblocks HostDialer's poll. Drop = transport gone.
    struct ConnGuard;
    impl Drop for ConnGuard {
        fn drop(&mut self) {
            mark_connected(false);
        }
    }
    let _guard = ConnGuard;
    loop {
        let (kind, stream) = match conn.accept().await {
            Ok(v) => v,
            Err(ansync_transport::TransportError::Closed) => {
                info!("streams_accept_loop: connection closed");
                return;
            }
            Err(ansync_transport::TransportError::TimedOut) => {
                info!("streams_accept_loop: keep-alive timed out");
                return;
            }
            Err(e) => {
                warn!("streams_accept_loop: accept failed: {e}");
                return;
            }
        };
        match kind {
            StreamKind::Input => {
                // Host opened an Input stream to push host→device
                // events. Drain into the same mpsc that
                // `nativePollInputMessage` reads from so the
                // AccessibilityService replays them.
                let tx = input_inbound_tx.clone();
                tokio::spawn(input_recv_loop(stream, (*tx).clone()));
            }
            StreamKind::Files => {
                let mut stream = stream;
                let host_id = host_id.clone();
                let policy = Arc::new(AutoAcceptPolicy {
                    root: download_dir.clone(),
                });
                let perms = permissions.clone();
                let received_tx = received_file_tx.clone();
                let prog_tx = progress_tx.clone();
                let prog: ProgressFn = Arc::new(move |ev: ProgressEvent| {
                    let blob = encode_progress_for_kotlin(0, &ev, 0, 0, 0, 0);
                    let _ = prog_tx.send(blob);
                });
                tokio::spawn(async move {
                    match receive_file(
                        &host_id,
                        perms.as_ref(),
                        &mut stream,
                        policy.as_ref(),
                        Some(prog),
                    )
                    .await
                    {
                        Ok(p) => {
                            info!("inbound file -> {}", p.display());
                            let _ = received_tx.send(p.display().to_string());
                        }
                        Err(e) => warn!("inbound file failed: {e}"),
                    }
                });
            }
            StreamKind::Control => {
                let cam_tx = camera_ctrl_tx.clone();
                let aud_tx = audio_ctrl_tx.clone();
                let cap_tx = capture_ctrl_tx.clone();
                tokio::spawn(control_recv_loop(
                    stream,
                    (*cam_tx).clone(),
                    (*aud_tx).clone(),
                    (*cap_tx).clone(),
                ));
            }
            StreamKind::Audio => {
                let tx = audio_in_tx.clone();
                tokio::spawn(audio_in_loop(stream, (*tx).clone()));
            }
            StreamKind::Clipboard => {
                let tx = clip_in_tx.clone();
                let btx = clip_blob_tx.clone();
                tokio::spawn(clipboard_in_loop(stream, (*tx).clone(), (*btx).clone()));
            }
            StreamKind::Hello => {
                let slot = host_name_slot.clone();
                tokio::spawn(hello_in_loop(stream, slot));
            }
            StreamKind::Url => {
                let tx = url_in_tx.clone();
                tokio::spawn(url_in_loop(stream, (*tx).clone()));
            }
            other => {
                warn!("streams_accept_loop: dropping unexpected stream {other:?}");
                drop(stream);
            }
        }
    }
}

async fn url_in_loop(mut stream: QuicStream, tx: UnboundedSender<String>) {
    let bytes = match stream.recv().await {
        Ok(b) => b,
        Err(e) => {
            warn!("url_in_loop: recv failed: {e}");
            return;
        }
    };
    let env: ansync_proto::Envelope = match postcard::from_bytes(&bytes) {
        Ok(e) => e,
        Err(e) => {
            warn!("url_in_loop: decode failed: {e}");
            return;
        }
    };
    let url = match env.message {
        Message::Url(u) => u.url,
        other => {
            warn!("url_in_loop: unexpected message {other:?}");
            return;
        }
    };
    if tx.send(url).is_err() {
        info!("url_in_loop: receiver dropped");
    }
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.pos + n > self.buf.len() {
            return Err(format!("short read: need {n}, have {}", self.buf.len() - self.pos));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn take_u32(&mut self) -> Result<u32, String> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn take_i32(&mut self) -> Result<i32, String> {
        Ok(self.take_u32()? as i32)
    }
    fn take_u16(&mut self) -> Result<u16, String> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }
    fn take_i16(&mut self) -> Result<i16, String> {
        Ok(self.take_u16()? as i16)
    }
}

/// Decode `Envelope`s off the Control stream and surface the
/// `ControlMessage::StartCamera` / `StopCamera` ones to Kotlin via
/// a tag-binary blob.
///
/// Layout (mirrored in `WireCameraControl.kt`):
///   tag 0  StartCamera : str camera_id | u32 w | u32 h | u8 fps |
///                        u32 bitrate_kbps | u8 codec(0=H264,1=H265) |
///                        u8 aspect(0=Crop,1=Letterbox,2=Stretch) |
///                        u8 stabilization
///   tag 1  StopCamera  : (no payload)
async fn control_recv_loop(
    mut stream: QuicStream,
    camera_tx: UnboundedSender<Vec<u8>>,
    audio_tx: UnboundedSender<Vec<u8>>,
    capture_tx: UnboundedSender<Vec<u8>>,
) {
    loop {
        let bytes = match stream.recv().await {
            Ok(b) => b,
            Err(ansync_transport::TransportError::Closed)
            | Err(ansync_transport::TransportError::TimedOut) => {
                info!("control_recv_loop: stream closed");
                return;
            }
            Err(e) => {
                warn!("control_recv_loop: recv error: {e}");
                return;
            }
        };
        let env: Envelope = match postcard::from_bytes(&bytes) {
            Ok(e) => e,
            Err(e) => {
                warn!("control_recv_loop: malformed Envelope: {e}");
                continue;
            }
        };
        match env.message {
            Message::Control(ControlMessage::StartCamera(cfg)) => {
                let mut out = Vec::with_capacity(32);
                out.push(0u8);
                let id = cfg.camera_id.as_bytes();
                out.extend_from_slice(&(id.len() as u32).to_le_bytes());
                out.extend_from_slice(id);
                out.extend_from_slice(&cfg.width.to_le_bytes());
                out.extend_from_slice(&cfg.height.to_le_bytes());
                out.push(cfg.fps);
                out.extend_from_slice(&cfg.bitrate_kbps.to_le_bytes());
                out.push(match cfg.codec {
                    ansync_proto::VideoCodec::H264 => 0,
                    ansync_proto::VideoCodec::H265 => 1,
                });
                out.push(match cfg.aspect {
                    ansync_proto::CameraAspect::Crop => 0,
                    ansync_proto::CameraAspect::Letterbox => 1,
                    ansync_proto::CameraAspect::Stretch => 2,
                });
                out.push(if cfg.stabilization { 1 } else { 0 });
                if camera_tx.send(out).is_err() {
                    info!("control_recv_loop: camera receiver dropped; exiting");
                    return;
                }
            }
            Message::Control(ControlMessage::StopCamera) => {
                if camera_tx.send(vec![1u8]).is_err() {
                    return;
                }
            }
            Message::Control(ControlMessage::StartAudioRoute { direction }) => {
                let dir_byte = match direction {
                    ansync_proto::AudioDirection::HostToDevice => 0u8,
                    ansync_proto::AudioDirection::DeviceToHost => 1,
                    ansync_proto::AudioDirection::Both => 2,
                };
                if audio_tx.send(vec![0u8, dir_byte]).is_err() {
                    return;
                }
            }
            Message::Control(ControlMessage::StopAudioRoute) => {
                if audio_tx.send(vec![1u8]).is_err() {
                    return;
                }
            }
            Message::Control(ControlMessage::RequestScreenCapture) => {
                // Single-byte signal — Kotlin matches on tag 0 = start
                // request, tag 1 = stop. No payload either way.
                if capture_tx.send(vec![0u8]).is_err() {
                    return;
                }
            }
            Message::Control(ControlMessage::StopScreenCapture) => {
                if capture_tx.send(vec![1u8]).is_err() {
                    return;
                }
            }
            other => {
                warn!("control_recv_loop: ignoring Control message {other:?}");
            }
        }
    }
}

/// Inbound `StreamKind::Audio` from the host. First frame is the
/// `AudioStreamInit` header (postcard), subsequent frames are raw
/// little-endian S16 PCM. We forward the raw PCM straight to Kotlin
/// via the `audio_in_tx` channel — the header is logged + discarded
/// because both sides hardcode 48 kHz stereo today.
async fn audio_in_loop(mut stream: QuicStream, tx: UnboundedSender<Vec<u8>>) {
    let _header = match stream.recv().await {
        Ok(b) => b,
        Err(_) => return,
    };
    info!("audio_in_loop: header received, streaming PCM");
    loop {
        match stream.recv().await {
            Ok(bytes) => {
                if tx.send(bytes.to_vec()).is_err() {
                    info!("audio_in_loop: receiver dropped; exiting");
                    return;
                }
            }
            Err(ansync_transport::TransportError::Closed)
            | Err(ansync_transport::TransportError::TimedOut) => {
                info!("audio_in_loop: stream closed");
                return;
            }
            Err(e) => {
                warn!("audio_in_loop: recv error: {e}");
                return;
            }
        }
    }
}

async fn clipboard_in_loop(
    mut stream: QuicStream,
    tx: UnboundedSender<String>,
    blob_tx: UnboundedSender<(String, Vec<u8>)>,
) {
    loop {
        let bytes = match stream.recv().await {
            Ok(b) => b,
            Err(ansync_transport::TransportError::Closed)
            | Err(ansync_transport::TransportError::TimedOut) => return,
            Err(e) => {
                warn!("clipboard_in_loop: recv error: {e}");
                return;
            }
        };
        let msg: ClipboardMessage = match postcard::from_bytes(&bytes) {
            Ok(m) => m,
            Err(e) => {
                warn!("clipboard_in_loop: decode failed: {e}");
                continue;
            }
        };
        match msg {
            ClipboardMessage::Text { content } => {
                if tx.send(content).is_err() {
                    info!("clipboard_in_loop: text receiver dropped; exiting");
                    return;
                }
            }
            ClipboardMessage::Blob { mime, data } => {
                if blob_tx.send((mime, data)).is_err() {
                    info!("clipboard_in_loop: blob receiver dropped; exiting");
                    return;
                }
            }
        }
    }
}

/// Open `StreamKind::Hello` outbound and push a single `Hello` envelope
/// carrying our device id + name + capability bits, then close. Lets
/// the daemon refresh its `PeerStore.name` cache without waiting for
/// the next pair.
async fn send_hello(
    conn: &QuicConnection,
    identity: &IdentityKeypair,
    device_name: &str,
) -> Result<(), ansync_transport::TransportError> {
    let pk = identity.public().as_bytes();
    let mut id_bytes = [0u8; 16];
    id_bytes.copy_from_slice(&pk[..16]);
    let env = Envelope {
        version: PROTOCOL_VERSION,
        message: Message::Hello(Hello {
            device_id: DeviceId(id_bytes),
            name: DeviceName(device_name.to_string()),
            // Companion-side caps reflect what the device can offer to
            // the host. Keeping it as the union of everything we wire
            // today; the host gates per-feature with its own perms.
            capabilities: Capabilities::SCREEN_MIRROR
                | Capabilities::CAMERA_VIDEO
                | Capabilities::AUDIO_IN
                | Capabilities::AUDIO_OUT
                | Capabilities::MIC
                | Capabilities::FILES
                | Capabilities::CLIPBOARD
                | Capabilities::NOTIFICATIONS
                | Capabilities::SHARE,
        }),
    };
    let bytes = postcard::to_allocvec(&env).map_err(|e| {
        ansync_transport::TransportError::Handshake(format!("encode Hello: {e}"))
    })?;
    let mut stream = conn.open(StreamKind::Hello).await?;
    stream.send(Bytes::from(bytes)).await?;
    let _ = stream.finish().await;
    Ok(())
}

/// Drain the host's Hello frame off a freshly accepted Hello stream
/// and stash the name so Kotlin can surface it on the paired-host
/// card.
async fn hello_in_loop(mut stream: QuicStream, slot: Arc<Mutex<Option<String>>>) {
    let bytes = match stream.recv().await {
        Ok(b) => b,
        Err(e) => {
            warn!("hello_in_loop: recv failed: {e}");
            return;
        }
    };
    let env: Envelope = match postcard::from_bytes(&bytes) {
        Ok(e) => e,
        Err(e) => {
            warn!("hello_in_loop: decode failed: {e}");
            return;
        }
    };
    match env.message {
        Message::Hello(h) => {
            info!("host Hello: name={} caps={:#x}", h.name, h.capabilities.bits());
            if let Ok(mut g) = slot.lock() {
                *g = Some(h.name.0);
            }
        }
        other => warn!("hello_in_loop: non-Hello envelope: {other:?}"),
    }
}

async fn input_recv_loop(mut stream: QuicStream, tx: UnboundedSender<Vec<u8>>) {
    loop {
        match stream.recv().await {
            Ok(bytes) => {
                let msg: InputMessage = match postcard::from_bytes(&bytes) {
                    Ok(m) => m,
                    Err(e) => {
                        warn!("input_recv_loop: malformed InputMessage: {e}");
                        continue;
                    }
                };
                if tx.send(encode_for_kotlin(&msg)).is_err() {
                    info!("input_recv_loop: receiver dropped; exiting");
                    return;
                }
            }
            Err(ansync_transport::TransportError::Closed)
            | Err(ansync_transport::TransportError::TimedOut) => {
                info!("input_recv_loop: stream closed");
                return;
            }
            Err(e) => {
                warn!("input_recv_loop: recv error: {e}");
                return;
            }
        }
    }
}

/// Flat binary tag+payload format consumed by `WireInputMessage`
/// on the Kotlin side. All multi-byte integers are little-endian.
/// Defined in one place so the Kotlin decoder can mirror it exactly.
///
/// Layout per tag (in bytes):
///   0  KeyPress     : tag(1) u32 keycode | u8 pressed
///   1  MouseMove    : tag(1) i32 dx | i32 dy
///   2  MouseButton  : tag(1) u8 button | u8 pressed
///   3  MouseWheel   : tag(1) i32 dx | i32 dy
///   4  TouchSlot    : tag(1) u8 slot | i32 x | i32 y | u16 pressure | i32 tracking_id
///   5  Stylus       : tag(1) i32 x | i32 y | u16 pressure | i16 tilt_x | i16 tilt_y | u8 btn
///   6  Gamepad      : tag(1) u32 buttons | i16 lx | i16 ly | i16 rx | i16 ry | u8 lt | u8 rt
///   7  Text         : tag(1) u32 len | bytes(len)  (UTF-8)
///   8  TouchpadSlot : same wire layout as TouchSlot — routes to the
///                    host's Mac-style clickpad uinput device instead
///                    of the absolute touchscreen one
fn encode_for_kotlin(msg: &InputMessage) -> Vec<u8> {
    let mut out = Vec::with_capacity(24);
    match msg {
        InputMessage::KeyPress { keycode, pressed } => {
            out.push(0);
            out.extend_from_slice(&keycode.to_le_bytes());
            out.push(if *pressed { 1 } else { 0 });
        }
        InputMessage::MouseMove { dx, dy } => {
            out.push(1);
            out.extend_from_slice(&dx.to_le_bytes());
            out.extend_from_slice(&dy.to_le_bytes());
        }
        InputMessage::MouseButton { button, pressed } => {
            out.push(2);
            out.push(*button);
            out.push(if *pressed { 1 } else { 0 });
        }
        InputMessage::MouseWheel { dx, dy } => {
            out.push(3);
            out.extend_from_slice(&dx.to_le_bytes());
            out.extend_from_slice(&dy.to_le_bytes());
        }
        InputMessage::TouchSlot { slot, x, y, pressure, tracking_id } => {
            out.push(4);
            out.push(*slot);
            out.extend_from_slice(&x.to_le_bytes());
            out.extend_from_slice(&y.to_le_bytes());
            out.extend_from_slice(&pressure.to_le_bytes());
            out.extend_from_slice(&tracking_id.to_le_bytes());
        }
        InputMessage::Stylus { x, y, pressure, tilt_x, tilt_y, btn } => {
            out.push(5);
            out.extend_from_slice(&x.to_le_bytes());
            out.extend_from_slice(&y.to_le_bytes());
            out.extend_from_slice(&pressure.to_le_bytes());
            out.extend_from_slice(&tilt_x.to_le_bytes());
            out.extend_from_slice(&tilt_y.to_le_bytes());
            out.push(*btn);
        }
        InputMessage::Gamepad(state) => {
            out.push(6);
            out.extend_from_slice(&state.buttons.to_le_bytes());
            out.extend_from_slice(&state.lx.to_le_bytes());
            out.extend_from_slice(&state.ly.to_le_bytes());
            out.extend_from_slice(&state.rx.to_le_bytes());
            out.extend_from_slice(&state.ry.to_le_bytes());
            out.push(state.lt);
            out.push(state.rt);
        }
        InputMessage::Text(s) => {
            out.push(7);
            let bytes = s.as_bytes();
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(bytes);
        }
        InputMessage::TouchpadSlot { slot, x, y, pressure, tracking_id } => {
            out.push(8);
            out.push(*slot);
            out.extend_from_slice(&x.to_le_bytes());
            out.extend_from_slice(&y.to_le_bytes());
            out.extend_from_slice(&pressure.to_le_bytes());
            out.extend_from_slice(&tracking_id.to_le_bytes());
        }
    }
    out
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativeSendVideoChunk<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
    chunk: JByteArray<'local>,
    _pts_us: jlong,
) -> jboolean {
    let bytes = match env.convert_byte_array(&chunk) {
        Ok(b) => b,
        Err(e) => {
            error!("nativeSendVideoChunk: convert_byte_array failed: {e}");
            return jni::sys::JNI_FALSE;
        }
    };
    let video_stream = {
        let slot = state_slot().lock().expect("state mutex poisoned");
        match slot.as_ref().and_then(|s| s.session.as_ref()) {
            Some(sess) => sess.video_stream.clone(),
            None => {
                warn!("nativeSendVideoChunk: no active session");
                return jni::sys::JNI_FALSE;
            }
        }
    };
    let result = runtime().block_on(async move {
        let mut guard = video_stream.lock().await;
        guard.send(Bytes::from(bytes)).await
    });
    match result {
        Ok(()) => jni::sys::JNI_TRUE,
        Err(e) => {
            warn!("nativeSendVideoChunk: stream send failed: {e}");
            jni::sys::JNI_FALSE
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativePollInputMessage<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jni::sys::jbyteArray {
    let input_rx = {
        let slot = state_slot().lock().expect("state mutex poisoned");
        match slot.as_ref().and_then(|s| s.session.as_ref()) {
            Some(sess) => sess.input_rx.clone(),
            None => return std::ptr::null_mut(),
        }
    };
    let bytes = runtime().block_on(async move {
        let mut guard = input_rx.lock().await;
        guard.recv().await
    });
    match bytes {
        Some(b) => match env.byte_array_from_slice(&b) {
            Ok(arr) => arr.into_raw(),
            Err(e) => {
                error!("nativePollInputMessage: byte_array_from_slice: {e}");
                std::ptr::null_mut()
            }
        },
        None => std::ptr::null_mut(),
    }
}

/// Run the companion side of the cable pairing flow against
/// `127.0.0.1:port` (where the host has already configured an `adb
/// reverse`). Returns `"<host_pubkey_hex>|<host_name>"` on success
/// and `null` on any failure. The caller persists the pair to
/// `{filesDir}/paired_host.toml`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativePairOverCable<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    port: jint,
    companion_name: JString<'local>,
) -> jni::sys::jstring {
    let port = match u16::try_from(port) {
        Ok(p) => p,
        Err(_) => {
            error!("nativePairOverCable: port {port} out of range");
            return std::ptr::null_mut();
        }
    };
    let companion_name: String = match env.get_string(&companion_name) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("nativePairOverCable: invalid name: {e}");
            return std::ptr::null_mut();
        }
    };
    let identity = {
        let slot = state_slot().lock().expect("state mutex poisoned");
        match slot.as_ref() {
            Some(s) => IdentityKeypair::from_seed(*s.identity.seed_bytes()),
            None => {
                error!("nativePairOverCable: state not initialised");
                return std::ptr::null_mut();
            }
        }
    };
    let result = runtime().block_on(async move {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
        let result = bootstrap_companion(&mut stream, &identity, &companion_name)
            .await
            .map_err(|e| std::io::Error::other(format!("bootstrap: {e}")))?;
        Ok::<_, std::io::Error>(result)
    });
    let pair_result = match result {
        Ok(p) => p,
        Err(e) => {
            error!("nativePairOverCable: pair failed: {e}");
            return std::ptr::null_mut();
        }
    };
    info!("cable pairing complete with host {}", pair_result.peer.name.0);
    let hex = hex_encode(&pair_result.peer.pubkey);
    // Wire to Kotlin: `<hex>|<name>|<ip:port>,<ip:port>,...` — the
    // endpoints slot is empty when the host didn't advertise any
    // (older daemon, no LAN). Kotlin parses + persists to
    // `PREF_HOST_ADDR` so `HostDialer` can fall back to direct dial
    // when mDNS multicast doesn't reach.
    let endpoints = pair_result
        .lan_endpoints
        .iter()
        .map(|(ip, port)| format!("{ip}:{port}"))
        .collect::<Vec<_>>()
        .join(",");
    let response = format!("{hex}|{}|{endpoints}", pair_result.peer.name.0);
    match env.new_string(response) {
        Ok(s) => s.into_raw(),
        Err(e) => {
            error!("nativePairOverCable: env.new_string failed: {e}");
            std::ptr::null_mut()
        }
    }
}

/// Decode a tag-binary `InputMessage` (same layout as
/// `encode_for_kotlin`) emitted by the Kotlin touchpad activity.
fn decode_input_from_kotlin(bytes: &[u8]) -> Result<InputMessage, String> {
    let mut c = Cursor::new(bytes);
    let tag = c.take(1)?[0];
    match tag {
        0 => Ok(InputMessage::KeyPress {
            keycode: c.take_u32()?,
            pressed: c.take(1)?[0] != 0,
        }),
        1 => Ok(InputMessage::MouseMove {
            dx: c.take_i32()?,
            dy: c.take_i32()?,
        }),
        2 => Ok(InputMessage::MouseButton {
            button: c.take(1)?[0],
            pressed: c.take(1)?[0] != 0,
        }),
        3 => Ok(InputMessage::MouseWheel {
            dx: c.take_i32()?,
            dy: c.take_i32()?,
        }),
        4 => {
            let slot = c.take(1)?[0];
            let x = c.take_i32()?;
            let y = c.take_i32()?;
            let pressure = c.take_u16()?;
            let tracking_id = c.take_i32()?;
            Ok(InputMessage::TouchSlot { slot, x, y, pressure, tracking_id })
        }
        5 => Ok(InputMessage::Stylus {
            x: c.take_i32()?,
            y: c.take_i32()?,
            pressure: c.take_u16()?,
            tilt_x: c.take_i16()?,
            tilt_y: c.take_i16()?,
            btn: c.take(1)?[0],
        }),
        6 => Ok(InputMessage::Gamepad(GamepadState {
            buttons: c.take_u32()?,
            lx: c.take_i16()?,
            ly: c.take_i16()?,
            rx: c.take_i16()?,
            ry: c.take_i16()?,
            lt: c.take(1)?[0],
            rt: c.take(1)?[0],
        })),
        7 => {
            let len = c.take_u32()? as usize;
            let bytes = c.take(len)?;
            let s = std::str::from_utf8(bytes)
                .map_err(|e| format!("Text: invalid utf8: {e}"))?
                .to_string();
            Ok(InputMessage::Text(s))
        }
        8 => {
            let slot = c.take(1)?[0];
            let x = c.take_i32()?;
            let y = c.take_i32()?;
            let pressure = c.take_u16()?;
            let tracking_id = c.take_i32()?;
            Ok(InputMessage::TouchpadSlot { slot, x, y, pressure, tracking_id })
        }
        other => Err(format!("unknown InputMessage tag {other}")),
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativeSendInputMessage<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
    blob: JByteArray<'local>,
) -> jboolean {
    let bytes = match env.convert_byte_array(&blob) {
        Ok(b) => b,
        Err(e) => {
            error!("nativeSendInputMessage: convert_byte_array: {e}");
            return jni::sys::JNI_FALSE;
        }
    };
    let msg = match decode_input_from_kotlin(&bytes) {
        Ok(m) => m,
        Err(e) => {
            warn!("nativeSendInputMessage: bad blob: {e}");
            return jni::sys::JNI_FALSE;
        }
    };
    let (conn, outbound_input) = {
        let slot = state_slot().lock().expect("state mutex poisoned");
        match slot.as_ref().and_then(|s| s.session.as_ref()) {
            Some(sess) => (sess.conn.clone(), sess.outbound_input.clone()),
            None => {
                warn!("nativeSendInputMessage: no active session");
                return jni::sys::JNI_FALSE;
            }
        }
    };
    let postcard_bytes = match postcard::to_allocvec(&msg) {
        Ok(v) => v,
        Err(e) => {
            warn!("nativeSendInputMessage: postcard encode: {e}");
            return jni::sys::JNI_FALSE;
        }
    };
    let result = runtime().block_on(async move {
        let mut guard = outbound_input.lock().await;
        if guard.is_none() {
            let stream = conn.open(StreamKind::Input).await?;
            *guard = Some(stream);
        }
        guard
            .as_mut()
            .expect("just inserted")
            .send(bytes::Bytes::from(postcard_bytes))
            .await
    });
    match result {
        Ok(()) => {
            INPUT_TX_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            jni::sys::JNI_TRUE
        }
        Err(e) => {
            warn!("nativeSendInputMessage: send failed: {e}");
            jni::sys::JNI_FALSE
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativePollCameraControl<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jni::sys::jbyteArray {
    let camera_ctrl_rx = {
        let slot = state_slot().lock().expect("state mutex poisoned");
        match slot.as_ref().and_then(|s| s.session.as_ref()) {
            Some(sess) => sess.camera_ctrl_rx.clone(),
            None => return std::ptr::null_mut(),
        }
    };
    let bytes = runtime().block_on(async move {
        let mut guard = camera_ctrl_rx.lock().await;
        guard.recv().await
    });
    match bytes {
        Some(b) => match env.byte_array_from_slice(&b) {
            Ok(arr) => arr.into_raw(),
            Err(e) => {
                error!("nativePollCameraControl: byte_array_from_slice: {e}");
                std::ptr::null_mut()
            }
        },
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativeSendCameraChunk<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
    chunk: JByteArray<'local>,
    _pts_us: jlong,
) -> jboolean {
    let bytes = match env.convert_byte_array(&chunk) {
        Ok(b) => b,
        Err(e) => {
            error!("nativeSendCameraChunk: convert_byte_array: {e}");
            return jni::sys::JNI_FALSE;
        }
    };
    let (conn, outbound_camera) = {
        let slot = state_slot().lock().expect("state mutex poisoned");
        match slot.as_ref().and_then(|s| s.session.as_ref()) {
            Some(sess) => (sess.conn.clone(), sess.outbound_camera.clone()),
            None => {
                warn!("nativeSendCameraChunk: no active session");
                return jni::sys::JNI_FALSE;
            }
        }
    };
    let result = runtime().block_on(async move {
        let mut guard = outbound_camera.lock().await;
        if guard.is_none() {
            let stream = conn.open(StreamKind::Camera).await?;
            *guard = Some(stream);
        }
        guard
            .as_mut()
            .expect("just inserted")
            .send(Bytes::from(bytes))
            .await
    });
    match result {
        Ok(()) => jni::sys::JNI_TRUE,
        Err(e) => {
            warn!("nativeSendCameraChunk: stream send failed: {e}");
            jni::sys::JNI_FALSE
        }
    }
}

/// Tear down the outbound camera stream — typically called by Kotlin
/// after the encoder drains in response to a StopCamera control.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativeStopCameraStream<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jboolean {
    let outbound_camera = {
        let slot = state_slot().lock().expect("state mutex poisoned");
        match slot.as_ref().and_then(|s| s.session.as_ref()) {
            Some(sess) => sess.outbound_camera.clone(),
            None => return jni::sys::JNI_FALSE,
        }
    };
    runtime().block_on(async move {
        let mut guard = outbound_camera.lock().await;
        *guard = None;
    });
    jni::sys::JNI_TRUE
}

/// Block (in native) until the host sends a
/// `ControlMessage::RequestScreenCapture` / `StopScreenCapture` and
/// return a single-byte tag (0 = start, 1 = stop). Returns `null`
/// on session teardown.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativePollCaptureControl<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jni::sys::jbyteArray {
    let rx = {
        let slot = state_slot().lock().expect("state mutex poisoned");
        match slot.as_ref().and_then(|s| s.session.as_ref()) {
            Some(sess) => sess.capture_ctrl_rx.clone(),
            None => return std::ptr::null_mut(),
        }
    };
    let bytes = runtime().block_on(async move {
        let mut guard = rx.lock().await;
        guard.recv().await
    });
    match bytes {
        Some(b) => match env.byte_array_from_slice(&b) {
            Ok(arr) => arr.into_raw(),
            Err(e) => {
                error!("nativePollCaptureControl: byte_array_from_slice: {e}");
                std::ptr::null_mut()
            }
        },
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativePollAudioControl<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jni::sys::jbyteArray {
    let rx = {
        let slot = state_slot().lock().expect("state mutex poisoned");
        match slot.as_ref().and_then(|s| s.session.as_ref()) {
            Some(sess) => sess.audio_ctrl_rx.clone(),
            None => return std::ptr::null_mut(),
        }
    };
    let bytes = runtime().block_on(async move {
        let mut guard = rx.lock().await;
        guard.recv().await
    });
    match bytes {
        Some(b) => match env.byte_array_from_slice(&b) {
            Ok(arr) => arr.into_raw(),
            Err(e) => {
                error!("nativePollAudioControl: byte_array_from_slice: {e}");
                std::ptr::null_mut()
            }
        },
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativePollAudioChunk<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jni::sys::jbyteArray {
    let rx = {
        let slot = state_slot().lock().expect("state mutex poisoned");
        match slot.as_ref().and_then(|s| s.session.as_ref()) {
            Some(sess) => sess.audio_in_rx.clone(),
            None => return std::ptr::null_mut(),
        }
    };
    let bytes = runtime().block_on(async move {
        let mut guard = rx.lock().await;
        guard.recv().await
    });
    match bytes {
        Some(b) => match env.byte_array_from_slice(&b) {
            Ok(arr) => arr.into_raw(),
            Err(e) => {
                error!("nativePollAudioChunk: byte_array_from_slice: {e}");
                std::ptr::null_mut()
            }
        },
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativeSendAudioChunk<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
    chunk: JByteArray<'local>,
) -> jboolean {
    let bytes = match env.convert_byte_array(&chunk) {
        Ok(b) => b,
        Err(e) => {
            error!("nativeSendAudioChunk: convert_byte_array: {e}");
            return jni::sys::JNI_FALSE;
        }
    };
    let (conn, outbound_audio) = {
        let slot = state_slot().lock().expect("state mutex poisoned");
        match slot.as_ref().and_then(|s| s.session.as_ref()) {
            Some(sess) => (sess.conn.clone(), sess.outbound_audio.clone()),
            None => {
                warn!("nativeSendAudioChunk: no active session");
                return jni::sys::JNI_FALSE;
            }
        }
    };
    let result = runtime().block_on(async move {
        let mut guard = outbound_audio.lock().await;
        if guard.is_none() {
            let mut stream = conn.open(StreamKind::Audio).await?;
            let init = ansync_proto::AudioStreamInit {
                sample_rate: 48_000,
                channels: 2,
                direction: ansync_proto::AudioDirection::DeviceToHost,
            };
            let header = postcard::to_allocvec(&init)
                .map_err(|e| ansync_transport::TransportError::Handshake(format!("encode header: {e}")))?;
            stream.send(Bytes::from(header)).await?;
            *guard = Some(stream);
        }
        guard
            .as_mut()
            .expect("just inserted")
            .send(Bytes::from(bytes))
            .await
    });
    match result {
        Ok(()) => jni::sys::JNI_TRUE,
        Err(e) => {
            warn!("nativeSendAudioChunk: send failed: {e}");
            jni::sys::JNI_FALSE
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativeStopAudioStream<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jboolean {
    let outbound_audio = {
        let slot = state_slot().lock().expect("state mutex poisoned");
        match slot.as_ref().and_then(|s| s.session.as_ref()) {
            Some(sess) => sess.outbound_audio.clone(),
            None => return jni::sys::JNI_FALSE,
        }
    };
    runtime().block_on(async move {
        let mut guard = outbound_audio.lock().await;
        *guard = None;
    });
    jni::sys::JNI_TRUE
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativePollClipboardText<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jni::sys::jstring {
    let rx = {
        let slot = state_slot().lock().expect("state mutex poisoned");
        match slot.as_ref().and_then(|s| s.session.as_ref()) {
            Some(sess) => sess.clipboard_in_rx.clone(),
            None => return std::ptr::null_mut(),
        }
    };
    let text = runtime().block_on(async move {
        let mut guard = rx.lock().await;
        guard.recv().await
    });
    match text {
        Some(s) => match env.new_string(s) {
            Ok(js) => js.into_raw(),
            Err(e) => {
                error!("nativePollClipboardText: new_string failed: {e}");
                std::ptr::null_mut()
            }
        },
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativeSendClipboardText<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    text: JString<'local>,
) -> jboolean {
    let text: String = match env.get_string(&text) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("nativeSendClipboardText: get_string failed: {e}");
            return jni::sys::JNI_FALSE;
        }
    };
    let conn = {
        let slot = state_slot().lock().expect("state mutex poisoned");
        match slot.as_ref().and_then(|s| s.session.as_ref()) {
            Some(sess) => sess.conn.clone(),
            None => {
                warn!("nativeSendClipboardText: no active session");
                return jni::sys::JNI_FALSE;
            }
        }
    };
    let msg = ClipboardMessage::Text { content: text };
    let payload = match postcard::to_allocvec(&msg) {
        Ok(b) => b,
        Err(e) => {
            warn!("nativeSendClipboardText: encode failed: {e}");
            return jni::sys::JNI_FALSE;
        }
    };
    let result = runtime().block_on(async move {
        let mut stream = conn.open(StreamKind::Clipboard).await?;
        stream.send(Bytes::from(payload)).await
    });
    match result {
        Ok(()) => jni::sys::JNI_TRUE,
        Err(e) => {
            warn!("nativeSendClipboardText: send failed: {e}");
            jni::sys::JNI_FALSE
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativePollClipboardBlob<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jni::sys::jbyteArray {
    let rx = {
        let slot = state_slot().lock().expect("state mutex poisoned");
        match slot.as_ref().and_then(|s| s.session.as_ref()) {
            Some(sess) => sess.clipboard_in_blob_rx.clone(),
            None => return std::ptr::null_mut(),
        }
    };
    let entry = runtime().block_on(async move {
        let mut guard = rx.lock().await;
        guard.recv().await
    });
    let Some((mime, data)) = entry else {
        return std::ptr::null_mut();
    };
    let mime_bytes = mime.as_bytes();
    let mut out = Vec::with_capacity(4 + mime_bytes.len() + data.len());
    out.extend_from_slice(&(mime_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(mime_bytes);
    out.extend_from_slice(&data);
    match env.byte_array_from_slice(&out) {
        Ok(arr) => arr.into_raw(),
        Err(e) => {
            error!("nativePollClipboardBlob: byte_array_from_slice failed: {e}");
            std::ptr::null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativeSendClipboardBlob<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    mime: JString<'local>,
    data: JByteArray<'local>,
) -> jboolean {
    let mime: String = match env.get_string(&mime) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("nativeSendClipboardBlob: mime get_string failed: {e}");
            return jni::sys::JNI_FALSE;
        }
    };
    let data: Vec<u8> = match env.convert_byte_array(&data) {
        Ok(v) => v,
        Err(e) => {
            error!("nativeSendClipboardBlob: convert_byte_array failed: {e}");
            return jni::sys::JNI_FALSE;
        }
    };
    let conn = {
        let slot = state_slot().lock().expect("state mutex poisoned");
        match slot.as_ref().and_then(|s| s.session.as_ref()) {
            Some(sess) => sess.conn.clone(),
            None => {
                warn!("nativeSendClipboardBlob: no active session");
                return jni::sys::JNI_FALSE;
            }
        }
    };
    let msg = ClipboardMessage::Blob { mime, data };
    let payload = match postcard::to_allocvec(&msg) {
        Ok(b) => b,
        Err(e) => {
            warn!("nativeSendClipboardBlob: encode failed: {e}");
            return jni::sys::JNI_FALSE;
        }
    };
    let result = runtime().block_on(async move {
        let mut stream = conn.open(StreamKind::Clipboard).await?;
        stream.send(Bytes::from(payload)).await
    });
    match result {
        Ok(()) => jni::sys::JNI_TRUE,
        Err(e) => {
            warn!("nativeSendClipboardBlob: send failed: {e}");
            jni::sys::JNI_FALSE
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativeSendNotificationPosted<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    id: jlong,
    app: JString<'local>,
    title: JString<'local>,
    body: JString<'local>,
) -> jboolean {
    let app: String = match env.get_string(&app) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("nativeSendNotificationPosted: app get_string failed: {e}");
            return jni::sys::JNI_FALSE;
        }
    };
    let title: String = match env.get_string(&title) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("nativeSendNotificationPosted: title get_string failed: {e}");
            return jni::sys::JNI_FALSE;
        }
    };
    let body: String = match env.get_string(&body) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("nativeSendNotificationPosted: body get_string failed: {e}");
            return jni::sys::JNI_FALSE;
        }
    };
    send_notification(NotificationMessage::Posted {
        id: id as u64,
        app,
        title,
        body,
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativeSendNotificationRemoved(
    _env: JNIEnv,
    _class: JClass,
    id: jlong,
) -> jboolean {
    send_notification(NotificationMessage::Removed { id: id as u64 })
}

fn send_notification(msg: NotificationMessage) -> jboolean {
    let conn = {
        let slot = state_slot().lock().expect("state mutex poisoned");
        match slot.as_ref().and_then(|s| s.session.as_ref()) {
            Some(sess) => sess.conn.clone(),
            None => {
                warn!("send_notification: no active session");
                return jni::sys::JNI_FALSE;
            }
        }
    };
    let payload = match postcard::to_allocvec(&msg) {
        Ok(b) => b,
        Err(e) => {
            warn!("send_notification: encode failed: {e}");
            return jni::sys::JNI_FALSE;
        }
    };
    let result = runtime().block_on(async move {
        let mut stream = conn.open(StreamKind::Notifications).await?;
        stream.send(Bytes::from(payload)).await
    });
    match result {
        Ok(()) => jni::sys::JNI_TRUE,
        Err(e) => {
            warn!("send_notification: send failed: {e}");
            jni::sys::JNI_FALSE
        }
    }
}

/// Push a batch of files over fresh `StreamKind::Files` streams.
/// `paths` is a Java `String[]`. `batch_id` is opaque to the native
/// side — Kotlin owns its lifecycle and uses it as the notif key so
/// every per-chunk progress event can be reattached to the right
/// notif. Blocking on the JNI thread is intentional: callers
/// (`ShareActivity`, `ShareTile`) already run on a worker pool.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativeSendFiles<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    batch_id: jni::sys::jlong,
    paths: jni::objects::JObjectArray<'local>,
) -> jboolean {
    let count = match env.get_array_length(&paths) {
        Ok(n) => n,
        Err(e) => {
            error!("nativeSendFiles: array length: {e}");
            return jni::sys::JNI_FALSE;
        }
    };
    let mut path_vec: Vec<String> = Vec::with_capacity(count as usize);
    for i in 0..count {
        let elt = match env.get_object_array_element(&paths, i) {
            Ok(o) => o,
            Err(e) => {
                warn!("nativeSendFiles: get element {i}: {e}");
                continue;
            }
        };
        let js: JString = elt.into();
        match env.get_string(&js) {
            Ok(s) => path_vec.push(s.into()),
            Err(e) => warn!("nativeSendFiles: get_string {i}: {e}"),
        }
    }
    if path_vec.is_empty() {
        warn!("nativeSendFiles: empty paths array");
        return jni::sys::JNI_FALSE;
    }
    if send_files_blocking(batch_id as u64, path_vec) {
        jni::sys::JNI_TRUE
    } else {
        jni::sys::JNI_FALSE
    }
}

/// Legacy single-path entry point — kept so existing Kotlin callers
/// keep linking. Wraps the input in a 1-element batch so the batch
/// notif / progress wiring is exercised even for one-off shares.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativeSendFile<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    path: JString<'local>,
) -> jboolean {
    let path_str: String = match env.get_string(&path) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("nativeSendFile: get_string path: {e}");
            return jni::sys::JNI_FALSE;
        }
    };
    if send_files_blocking(next_transfer_id(), vec![path_str]) {
        jni::sys::JNI_TRUE
    } else {
        jni::sys::JNI_FALSE
    }
}

/// Core send loop shared between `nativeSendFile` and
/// `nativeSendFiles`. Walks the batch sequentially, opens a fresh
/// Files stream per file, and emits a `ProgressEvent` per chunk into
/// the per-session progress channel.
fn send_files_blocking(batch_id: u64, paths: Vec<String>) -> bool {
    let (conn, host_id, progress_tx) = {
        let slot = state_slot().lock().expect("state mutex poisoned");
        match slot.as_ref().and_then(|s| s.session.as_ref()) {
            Some(sess) => (
                sess.conn.clone(),
                sess.host_id.clone(),
                sess.progress_tx.clone(),
            ),
            None => {
                warn!("send_files_blocking: no active session");
                return false;
            }
        }
    };
    let permissions: Arc<dyn PermissionsStore> = Arc::new(PermissivePermissions);
    runtime().block_on(async move {
        // Stat pass: anything that fails to size drops out of the
        // batch — better to under-report the denominator than to
        // stall at 99% forever.
        let mut sized: Vec<(std::path::PathBuf, u64)> = Vec::with_capacity(paths.len());
        let mut total_bytes: u64 = 0;
        for p in paths.into_iter() {
            let pb = std::path::PathBuf::from(p);
            match tokio::fs::metadata(&pb).await {
                Ok(m) => {
                    total_bytes = total_bytes.saturating_add(m.len());
                    sized.push((pb, m.len()));
                }
                Err(e) => warn!("send_files_blocking: stat {} failed: {e}", pb.display()),
            }
        }
        if sized.is_empty() {
            return false;
        }
        let total_files = sized.len() as u32;
        let batch_bytes_done = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let files_done = Arc::new(std::sync::atomic::AtomicU32::new(0));

        let mut all_ok = true;
        for (path, _file_size) in sized.into_iter() {
            let mut stream = match conn.open(StreamKind::Files).await {
                Ok(s) => s,
                Err(e) => {
                    warn!("send_files_blocking: open Files stream failed: {e}");
                    all_ok = false;
                    continue;
                }
            };
            let tid = next_transfer_id();

            let prev = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let cb_batch_bytes = batch_bytes_done.clone();
            let cb_files = files_done.clone();
            let cb_tx = progress_tx.clone();
            let prog: ProgressFn = Arc::new(move |ev: ProgressEvent| {
                if ev.direction != TransferDirection::Send {
                    return;
                }
                let prev_v = prev.swap(ev.bytes, std::sync::atomic::Ordering::Relaxed);
                let delta = ev.bytes.saturating_sub(prev_v);
                let cum = cb_batch_bytes
                    .fetch_add(delta, std::sync::atomic::Ordering::Relaxed)
                    + delta;
                let fd = cb_files.load(std::sync::atomic::Ordering::Relaxed);
                let blob = encode_progress_for_kotlin(
                    batch_id,
                    &ev,
                    total_files,
                    fd,
                    cum,
                    total_bytes,
                );
                let _ = cb_tx.send(blob);
                if ev.bytes == ev.total && ev.total > 0 {
                    cb_files.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            });

            match send_file(
                &host_id,
                permissions.as_ref(),
                &mut stream,
                &path,
                tid,
                Some(prog),
            )
            .await
            {
                Ok(_) => {}
                Err(e) => {
                    warn!("send_files_blocking: send_file {} failed: {e}", path.display());
                    all_ok = false;
                }
            }
        }
        all_ok
    })
}

/// Block until the next `ProgressEvent` lands on the per-session
/// channel. Returns a tag-binary blob mirroring
/// `encode_progress_for_kotlin`; `null` once the session is torn
/// down or no events are pending.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativePollTransferProgress<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jni::sys::jbyteArray {
    let rx = {
        let slot = state_slot().lock().expect("state mutex poisoned");
        match slot.as_ref().and_then(|s| s.session.as_ref()) {
            Some(sess) => sess.progress_rx.clone(),
            None => return std::ptr::null_mut(),
        }
    };
    let blob = runtime().block_on(async move {
        let mut guard = rx.lock().await;
        guard.recv().await
    });
    let Some(blob) = blob else {
        return std::ptr::null_mut();
    };
    match env.byte_array_from_slice(&blob) {
        Ok(arr) => arr.into_raw(),
        Err(e) => {
            error!("nativePollTransferProgress: byte_array_from_slice failed: {e}");
            std::ptr::null_mut()
        }
    }
}

/// Open a `StreamKind::Url` to the host, send a single postcard
/// `Envelope { Message::Url(UrlMessage) }` frame, drop the stream.
/// `xdg-open <url>` fires on the Linux side automatically; this is
/// the "share link to PC" path.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativeSendUrl<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    url: JString<'local>,
) -> jboolean {
    let url_str: String = match env.get_string(&url) {
        Ok(s) => s.into(),
        Err(e) => {
            error!("nativeSendUrl: get_string url: {e}");
            return jni::sys::JNI_FALSE;
        }
    };
    let conn = {
        let slot = state_slot().lock().expect("state mutex poisoned");
        match slot.as_ref().and_then(|s| s.session.as_ref()) {
            Some(sess) => sess.conn.clone(),
            None => {
                warn!("nativeSendUrl: no active session");
                return jni::sys::JNI_FALSE;
            }
        }
    };
    let ok = runtime().block_on(async move {
        let mut stream = match conn.open(StreamKind::Url).await {
            Ok(s) => s,
            Err(e) => {
                warn!("nativeSendUrl: open Url stream failed: {e}");
                return false;
            }
        };
        let env_msg = Envelope {
            version: PROTOCOL_VERSION,
            message: Message::Url(UrlMessage { url: url_str }),
        };
        let bytes = match postcard::to_allocvec(&env_msg) {
            Ok(b) => b,
            Err(e) => {
                warn!("nativeSendUrl: encode UrlMessage: {e}");
                return false;
            }
        };
        if let Err(e) = stream.send(Bytes::from(bytes)).await {
            warn!("nativeSendUrl: stream send: {e}");
            return false;
        }
        true
    });
    if ok { jni::sys::JNI_TRUE } else { jni::sys::JNI_FALSE }
}

/// Block until the host pushes the next inbound URL. Kotlin polls
/// this from a worker, posts the consent notification, opens via
/// `Intent.ACTION_VIEW` once the user taps "Open".
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativePollIncomingUrl<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jni::sys::jstring {
    let url_in_rx = {
        let slot = state_slot().lock().expect("state mutex poisoned");
        match slot.as_ref().and_then(|s| s.session.as_ref()) {
            Some(sess) => sess.url_in_rx.clone(),
            None => return std::ptr::null_mut(),
        }
    };
    let url = runtime().block_on(async move {
        let mut guard = url_in_rx.lock().await;
        guard.recv().await
    });
    match url {
        Some(u) => match env.new_string(u) {
            Ok(j) => j.into_raw(),
            Err(e) => {
                error!("nativePollIncomingUrl: new_string: {e}");
                std::ptr::null_mut()
            }
        },
        None => std::ptr::null_mut(),
    }
}

/// Block until the next inbound file finishes downloading. Returns
/// the absolute path under `incoming/{host}/`. Kotlin uses it to
/// register the file with `MediaScannerConnection` + post a
/// "tap to open" notification.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativePollReceivedFile<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jni::sys::jstring {
    let rx = {
        let slot = state_slot().lock().expect("state mutex poisoned");
        match slot.as_ref().and_then(|s| s.session.as_ref()) {
            Some(sess) => sess.received_files_rx.clone(),
            None => return std::ptr::null_mut(),
        }
    };
    let path = runtime().block_on(async move {
        let mut guard = rx.lock().await;
        guard.recv().await
    });
    match path {
        Some(p) => match env.new_string(p) {
            Ok(j) => j.into_raw(),
            Err(e) => {
                error!("nativePollReceivedFile: new_string: {e}");
                std::ptr::null_mut()
            }
        },
        None => std::ptr::null_mut(),
    }
}

/// Flat binary blob consumed by `WireProgress.decode` on the Kotlin
/// side. All multi-byte integers little-endian. Defined in one place
/// so the Kotlin decoder can mirror it exactly:
///
///   batch_id           u64
///   transfer_id        u64
///   name_len           u16
///   name               utf8[name_len]
///   bytes              u64
///   total              u64
///   direction          u8     (0 send, 1 receive)
///   batch_files        u32
///   batch_files_done   u32
///   batch_bytes_done   u64
///   batch_total_bytes  u64
fn encode_progress_for_kotlin(
    batch_id: u64,
    ev: &ProgressEvent,
    batch_files: u32,
    batch_files_done: u32,
    batch_bytes_done: u64,
    batch_total_bytes: u64,
) -> Vec<u8> {
    let name = ev.name.as_bytes();
    let name_len = name.len().min(u16::MAX as usize) as u16;
    let mut out = Vec::with_capacity(8 + 8 + 2 + name_len as usize + 8 + 8 + 1 + 4 + 4 + 8 + 8);
    out.extend_from_slice(&batch_id.to_le_bytes());
    out.extend_from_slice(&ev.transfer_id.to_le_bytes());
    out.extend_from_slice(&name_len.to_le_bytes());
    out.extend_from_slice(&name[..name_len as usize]);
    out.extend_from_slice(&ev.bytes.to_le_bytes());
    out.extend_from_slice(&ev.total.to_le_bytes());
    out.push(match ev.direction {
        TransferDirection::Send => 0,
        TransferDirection::Receive => 1,
    });
    out.extend_from_slice(&batch_files.to_le_bytes());
    out.extend_from_slice(&batch_files_done.to_le_bytes());
    out.extend_from_slice(&batch_bytes_done.to_le_bytes());
    out.extend_from_slice(&batch_total_bytes.to_le_bytes());
    out
}

/// Monotonic transfer id for outbound `nativeSendFile` calls.
fn next_transfer_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEED: AtomicU64 = AtomicU64::new(1);
    SEED.fetch_add(1, Ordering::Relaxed)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativeClose(
    _env: JNIEnv,
    _class: JClass,
) {
    let mut slot = state_slot().lock().expect("state mutex poisoned");
    if let Some(s) = slot.as_mut() {
        if s.session.take().is_some() {
            info!("nativeClose: session torn down");
        } else {
            warn!("nativeClose: no active session");
        }
    }
}

/// Start the always-on WiFi pair listener. Idempotent: subsequent
/// calls return the existing port. The listener accepts TCP
/// connections from any host on the LAN; each session generates a
/// fresh 6-digit PIN once `BootstrapHello` arrives so the OS notif
/// can render `"{host_name} wants to pair — PIN {pin}"`. The accept
/// loop survives MAC mismatches, bad protocol envelopes, and
/// successful pairs alike — it only exits on listener bind failure
/// or an explicit `nativeWifiPairListenerStop`.
///
/// Returns the listener port (positive `jlong`) on success, or `-1`
/// on bind failure.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativeWifiPairListenerStart(
    _env: JNIEnv,
    _class: JClass,
) -> jlong {
    if let Some(slot) = wifi_pair_slot().lock().expect("wifi pair mutex poisoned").as_ref() {
        info!("wifi pair listener already running on :{}", slot.port);
        return slot.port as jlong;
    }

    let (identity, device_name) = {
        let slot = state_slot().lock().expect("state mutex poisoned");
        match slot.as_ref() {
            Some(s) => (
                IdentityKeypair::from_seed(*s.identity.seed_bytes()),
                s.device_name.clone().unwrap_or_else(|| "ansync companion".to_string()),
            ),
            None => {
                error!("nativeWifiPairListenerStart: state not initialised");
                return -1;
            }
        }
    };

    let listener = match runtime().block_on(async {
        tokio::net::TcpListener::bind(("0.0.0.0", 0)).await
    }) {
        Ok(l) => l,
        Err(e) => {
            error!("nativeWifiPairListenerStart: bind failed: {e}");
            return -1;
        }
    };
    let port = match listener.local_addr() {
        Ok(addr) => addr.port(),
        Err(e) => {
            error!("nativeWifiPairListenerStart: local_addr failed: {e}");
            return -1;
        }
    };

    let (events_tx, events_rx) = unbounded_channel();
    let task = runtime().spawn(wifi_pair_accept_loop(
        listener, identity, device_name, events_tx,
    ));

    {
        let mut slot = wifi_pair_slot().lock().expect("wifi pair mutex poisoned");
        *slot = Some(WifiPairSlot {
            port,
            task,
            events_rx: Arc::new(AsyncMutex::new(events_rx)),
        });
    }

    info!("wifi pair listening on :{port}");
    port as jlong
}

async fn wifi_pair_accept_loop(
    listener: tokio::net::TcpListener,
    identity: IdentityKeypair,
    device_name: String,
    events_tx: UnboundedSender<PairEvent>,
) {
    loop {
        let (mut conn, peer_addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!("wifi pair: accept failed: {e}");
                // Bail — the listener fd is unusable; companion
                // service should restart us if it cares.
                return;
            }
        };
        info!("wifi pair: peer connected from {peer_addr}");
        let identity = identity.clone();
        let device_name = device_name.clone();
        let events_tx = events_tx.clone();
        // One task per connection so a stalled host doesn't block the
        // listener from accepting others.
        tokio::spawn(async move {
            wifi_pair_handle_connection(&mut conn, identity, device_name, events_tx).await;
        });
    }
}

async fn wifi_pair_handle_connection<S>(
    stream: &mut S,
    identity: IdentityKeypair,
    device_name: String,
    events_tx: UnboundedSender<PairEvent>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (host_pk, host_name) = match read_pair_hello(stream).await {
        Ok(v) => v,
        Err(e) => {
            warn!("wifi pair: read_pair_hello failed: {e}");
            return;
        }
    };
    // Fresh PIN per connection — the on-screen value the user reads
    // must match what THIS host is about to type, even if a previous
    // host hit our listener moments ago.
    let pin = ansync_crypto::generate_pin();
    let _ = events_tx.send(PairEvent::Request {
        host_pubkey: host_pk,
        host_name: host_name.clone(),
        pin,
    });

    let mut attempts: u8 = 0;
    // Re-drive Ack + MAC up to 3 times per connection. Each `BadPin`
    // is the host typing a wrong code; we let them retry without
    // closing the socket so the UX is "type again" instead of
    // "reconnect + read new PIN".
    //
    // NB: `respond_pair_pin` consumes its own Ack write + MAC read on
    // every call. Looping it on the same stream would replay the Ack;
    // simpler to give the host one shot per TCP connection and let
    // their CLI dial again on bad PIN. That matches the threat model
    // (each TCP attempt is an attempt under the 3-strike lockout).
    attempts += 1;
    match respond_pair_pin(stream, &identity, &device_name, &host_pk, &host_name, &pin).await {
        Ok(CompanionWifiOutcome::Ok(peer)) => {
            info!("wifi pair: success, peer={}", peer.name);
            let _ = events_tx.send(PairEvent::Ok {
                host_pubkey: peer.pubkey,
                host_name: peer.name.0.clone(),
            });
        }
        Ok(CompanionWifiOutcome::BadPin) => {
            warn!(
                "wifi pair: bad PIN from {host_name} (attempts={attempts})"
            );
            let remaining = 3u8.saturating_sub(attempts);
            let evt = if remaining == 0 {
                PairEvent::Lockout { host_name: host_name.clone() }
            } else {
                PairEvent::BadPin { host_name: host_name.clone(), remaining }
            };
            let _ = events_tx.send(evt);
        }
        Err(e) => {
            warn!("wifi pair: protocol error from {host_name}: {e}");
        }
    }
}

/// Block (up to `timeout_ms`) waiting for the next protocol event
/// from the always-on pair listener. Returns the event's wire
/// encoding (see [`PairEvent::encode`]), or `null` on timeout. Safe
/// to call from a dedicated worker thread in a tight loop.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativePollPairEvent<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
    timeout_ms: jlong,
) -> jni::sys::jstring {
    let events_rx = {
        let slot = wifi_pair_slot().lock().expect("wifi pair mutex poisoned");
        match slot.as_ref() {
            Some(s) => s.events_rx.clone(),
            None => {
                warn!("nativePollPairEvent: listener not started");
                return std::ptr::null_mut();
            }
        }
    };
    let timeout = if timeout_ms <= 0 {
        std::time::Duration::from_secs(3600)
    } else {
        std::time::Duration::from_millis(timeout_ms as u64)
    };
    let event = runtime().block_on(async move {
        let mut guard = events_rx.lock().await;
        tokio::time::timeout(timeout, guard.recv()).await
    });
    let event = match event {
        Ok(Some(e)) => e,
        Ok(None) => {
            warn!("nativePollPairEvent: events channel closed");
            return std::ptr::null_mut();
        }
        Err(_) => return std::ptr::null_mut(),
    };
    match env.new_string(event.encode()) {
        Ok(s) => s.into_raw(),
        Err(e) => {
            error!("nativePollPairEvent: new_string failed: {e}");
            std::ptr::null_mut()
        }
    }
}

/// Stop the always-on WiFi pair listener and drain its event channel.
/// Idempotent — calling it while no listener is running is a no-op.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativeWifiPairListenerStop(
    _env: JNIEnv,
    _class: JClass,
) {
    let mut slot = wifi_pair_slot().lock().expect("wifi pair mutex poisoned");
    if let Some(s) = slot.take() {
        s.task.abort();
        info!("wifi pair listener on :{} stopped", s.port);
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

fn hex_decode_32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let h = std::str::from_utf8(chunk).ok()?;
        out[i] = u8::from_str_radix(h, 16).ok()?;
    }
    Some(out)
}

