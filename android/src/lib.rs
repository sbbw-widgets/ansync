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
use std::sync::{Mutex, OnceLock};

use ansync_core::{DeviceId, DevicePermissions, Permission};
use ansync_crypto::IdentityKeypair;
use ansync_files::{AutoAcceptPolicy, receive_file};
use ansync_permissions::{PermissionsError, PermissionsStore};
use ansync_proto::InputMessage;
use ansync_transport::{
    Connection, QuicConnection, QuicStream, QuicTransport, Stream as _, StreamKind,
};
use bytes::Bytes;
use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JString};
use jni::sys::{jboolean, jint, jlong};
use log::{error, info, warn};
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

fn runtime() -> &'static Runtime {
    RUNTIME.get().expect("nativeInit() not called before runtime use")
}

struct CompanionState {
    identity: IdentityKeypair,
    /// Path the inbound files accept loop writes received files
    /// into. Defaults to the app's `filesDir/incoming/` until the
    /// Kotlin side picks a SAF tree URI.
    download_dir: PathBuf,
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
    _conn: Arc<QuicConnection>,
    video_stream: Arc<AsyncMutex<QuicStream>>,
    /// Receiver side of the reverse-input pump. `Mutex<>` so Kotlin
    /// can call `nativePollInputMessage` from any thread without
    /// reading-while-spawning races against the recv task.
    input_rx: Arc<AsyncMutex<UnboundedReceiver<Vec<u8>>>>,
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
    let download_dir = files_dir.join("incoming");
    *slot = Some(CompanionState {
        identity,
        download_dir,
        session: None,
    });
    jni::sys::JNI_TRUE
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

    let identity = {
        let slot = state_slot().lock().expect("state mutex poisoned");
        match slot.as_ref() {
            Some(s) => IdentityKeypair::from_seed(*s.identity.seed_bytes()),
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

    let transport = QuicTransport::new(identity);
    let conn = match runtime().block_on(transport.connect(addr, expected_server)) {
        Ok(c) => c,
        Err(e) => {
            error!("nativeOpenConnection: dial {addr}: {e}");
            return jni::sys::JNI_FALSE;
        }
    };
    info!("nativeOpenConnection: handshake ok with {addr}");

    let video_stream = match runtime().block_on(conn.open(StreamKind::Video)) {
        Ok(s) => s,
        Err(e) => {
            error!("nativeOpenConnection: open Video stream: {e}");
            return jni::sys::JNI_FALSE;
        }
    };
    let input_stream = match runtime().block_on(conn.open(StreamKind::Input)) {
        Ok(s) => s,
        Err(e) => {
            error!("nativeOpenConnection: open Input stream: {e}");
            return jni::sys::JNI_FALSE;
        }
    };
    let (input_tx, input_rx) = unbounded_channel::<Vec<u8>>();
    runtime().spawn(input_recv_loop(input_stream, input_tx));

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
    runtime().spawn(files_accept_loop(
        conn_arc.clone(),
        host_device_id,
        download_dir,
    ));

    let session = ActiveSession {
        _conn: conn_arc,
        video_stream: Arc::new(AsyncMutex::new(video_stream)),
        input_rx: Arc::new(AsyncMutex::new(input_rx)),
    };
    let mut slot = state_slot().lock().expect("state mutex poisoned");
    if let Some(s) = slot.as_mut() {
        s.session = Some(session);
    }
    jni::sys::JNI_TRUE
}

async fn files_accept_loop(
    conn: Arc<QuicConnection>,
    host_id: DeviceId,
    download_dir: PathBuf,
) {
    let permissions: Arc<dyn PermissionsStore> = Arc::new(PermissivePermissions);
    loop {
        let (kind, mut stream) = match conn.accept().await {
            Ok(v) => v,
            Err(ansync_transport::TransportError::Closed) => {
                info!("files_accept_loop: connection closed");
                return;
            }
            Err(e) => {
                warn!("files_accept_loop: accept failed: {e}");
                return;
            }
        };
        if kind != StreamKind::Files {
            warn!("files_accept_loop: dropping unexpected stream {kind:?}");
            drop(stream);
            continue;
        }
        let policy = Arc::new(AutoAcceptPolicy {
            root: download_dir.clone(),
        });
        let host_id = host_id.clone();
        let perms = permissions.clone();
        tokio::spawn(async move {
            match receive_file(&host_id, perms.as_ref(), &mut stream, policy.as_ref()).await {
                Ok(p) => info!("inbound file -> {}", p.display()),
                Err(e) => warn!("inbound file failed: {e}"),
            }
        });
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
            Err(ansync_transport::TransportError::Closed) => {
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

