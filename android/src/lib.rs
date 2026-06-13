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
use ansync_pairing::cable::bootstrap_companion;
use ansync_permissions::{PermissionsError, PermissionsStore};
use ansync_proto::{ClipboardMessage, ControlMessage, Envelope, FsOpMessage, InputMessage, Message};
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
    /// Per-Fs-stream pair of queues: native pushes inbound
    /// `FsOpMessage` requests as tag-binary blobs for the Kotlin
    /// worker; Kotlin replies through `fs_reply_tx` after running
    /// the SAF op. Sequential per stream.
    fs_req_rx: Arc<AsyncMutex<UnboundedReceiver<Vec<u8>>>>,
    fs_reply_tx: Arc<UnboundedSender<Vec<u8>>>,
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
    /// Outbound device→host Audio stream for mic forwarding.
    /// Lazy-opened on the first `nativeSendAudioChunk` (device-side).
    outbound_audio: Arc<AsyncMutex<Option<QuicStream>>>,
    /// Receiver side of host→device PCM. Kotlin polls it via
    /// `nativePollAudioChunk` and writes to AudioTrack.
    audio_in_rx: Arc<AsyncMutex<UnboundedReceiver<Vec<u8>>>>,
    /// Inbound clipboard text from the host. UTF-8 bytes.
    clipboard_in_rx: Arc<AsyncMutex<UnboundedReceiver<String>>>,
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
    // Convention: the OPENER of a stream uses it for send. The
    // host→device input stream is opened by the daemon on
    // ShowScreen; we accept it in `streams_accept_loop`. Device→host
    // input (9.5e) opens its own stream lazily when the user starts
    // interacting with the projected overlay.
    let (input_tx, input_rx) = unbounded_channel::<Vec<u8>>();
    let input_tx_arc = Arc::new(input_tx);

    let (fs_req_tx, fs_req_rx) = unbounded_channel::<Vec<u8>>();
    let (fs_reply_tx, fs_reply_rx) = unbounded_channel::<Vec<u8>>();
    let fs_req_tx = Arc::new(fs_req_tx);
    let fs_reply_tx = Arc::new(fs_reply_tx);
    let fs_reply_rx = Arc::new(AsyncMutex::new(fs_reply_rx));

    let (camera_ctrl_tx, camera_ctrl_rx) = unbounded_channel::<Vec<u8>>();
    let camera_ctrl_tx = Arc::new(camera_ctrl_tx);

    let (audio_ctrl_tx, audio_ctrl_rx) = unbounded_channel::<Vec<u8>>();
    let audio_ctrl_tx = Arc::new(audio_ctrl_tx);

    let (audio_in_tx, audio_in_rx) = unbounded_channel::<Vec<u8>>();
    let audio_in_tx = Arc::new(audio_in_tx);

    let (clip_in_tx, clip_in_rx) = unbounded_channel::<String>();
    let clip_in_tx = Arc::new(clip_in_tx);

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
        host_device_id,
        download_dir,
        fs_req_tx.clone(),
        fs_reply_rx.clone(),
        input_tx_arc.clone(),
        camera_ctrl_tx.clone(),
        audio_ctrl_tx.clone(),
        audio_in_tx.clone(),
        clip_in_tx.clone(),
    ));

    let session = ActiveSession {
        conn: conn_arc,
        video_stream: Arc::new(AsyncMutex::new(video_stream)),
        outbound_input: Arc::new(AsyncMutex::new(None)),
        input_rx: Arc::new(AsyncMutex::new(input_rx)),
        fs_req_rx: Arc::new(AsyncMutex::new(fs_req_rx)),
        fs_reply_tx,
        outbound_camera: Arc::new(AsyncMutex::new(None)),
        camera_ctrl_rx: Arc::new(AsyncMutex::new(camera_ctrl_rx)),
        audio_ctrl_rx: Arc::new(AsyncMutex::new(audio_ctrl_rx)),
        outbound_audio: Arc::new(AsyncMutex::new(None)),
        audio_in_rx: Arc::new(AsyncMutex::new(audio_in_rx)),
        clipboard_in_rx: Arc::new(AsyncMutex::new(clip_in_rx)),
    };
    let mut slot = state_slot().lock().expect("state mutex poisoned");
    if let Some(s) = slot.as_mut() {
        s.session = Some(session);
    }
    jni::sys::JNI_TRUE
}

async fn streams_accept_loop(
    conn: Arc<QuicConnection>,
    host_id: DeviceId,
    download_dir: PathBuf,
    fs_req_tx: Arc<UnboundedSender<Vec<u8>>>,
    fs_reply_rx: Arc<AsyncMutex<UnboundedReceiver<Vec<u8>>>>,
    input_inbound_tx: Arc<UnboundedSender<Vec<u8>>>,
    camera_ctrl_tx: Arc<UnboundedSender<Vec<u8>>>,
    audio_ctrl_tx: Arc<UnboundedSender<Vec<u8>>>,
    audio_in_tx: Arc<UnboundedSender<Vec<u8>>>,
    clip_in_tx: Arc<UnboundedSender<String>>,
) {
    let permissions: Arc<dyn PermissionsStore> = Arc::new(PermissivePermissions);
    loop {
        let (kind, stream) = match conn.accept().await {
            Ok(v) => v,
            Err(ansync_transport::TransportError::Closed) => {
                info!("streams_accept_loop: connection closed");
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
                let policy = Arc::new(AutoAcceptPolicy {
                    root: download_dir.clone(),
                });
                let host_id = host_id.clone();
                let perms = permissions.clone();
                tokio::spawn(async move {
                    match receive_file(&host_id, perms.as_ref(), &mut stream, policy.as_ref()).await
                    {
                        Ok(p) => info!("inbound file -> {}", p.display()),
                        Err(e) => warn!("inbound file failed: {e}"),
                    }
                });
            }
            StreamKind::Fs => {
                let tx = fs_req_tx.clone();
                let rx = fs_reply_rx.clone();
                tokio::spawn(fs_serve_loop(stream, tx, rx));
            }
            StreamKind::Control => {
                let cam_tx = camera_ctrl_tx.clone();
                let aud_tx = audio_ctrl_tx.clone();
                tokio::spawn(control_recv_loop(stream, (*cam_tx).clone(), (*aud_tx).clone()));
            }
            StreamKind::Audio => {
                let tx = audio_in_tx.clone();
                tokio::spawn(audio_in_loop(stream, (*tx).clone()));
            }
            StreamKind::Clipboard => {
                let tx = clip_in_tx.clone();
                tokio::spawn(clipboard_in_loop(stream, (*tx).clone()));
            }
            other => {
                warn!("streams_accept_loop: dropping unexpected stream {other:?}");
                drop(stream);
            }
        }
    }
}

async fn fs_serve_loop(
    mut stream: QuicStream,
    req_tx: Arc<UnboundedSender<Vec<u8>>>,
    reply_rx: Arc<AsyncMutex<UnboundedReceiver<Vec<u8>>>>,
) {
    loop {
        let bytes = match stream.recv().await {
            Ok(b) => b,
            Err(ansync_transport::TransportError::Closed) => {
                info!("fs_serve_loop: stream closed");
                return;
            }
            Err(e) => {
                warn!("fs_serve_loop: recv error: {e}");
                return;
            }
        };
        let msg: FsOpMessage = match postcard::from_bytes(&bytes) {
            Ok(m) => m,
            Err(e) => {
                warn!("fs_serve_loop: malformed FsOpMessage: {e}");
                let err_reply = FsOpMessage::Error {
                    code: 5, // EIO
                    message: "malformed request".into(),
                };
                if let Ok(bytes) = postcard::to_allocvec(&err_reply) {
                    let _ = stream.send(Bytes::from(bytes)).await;
                }
                continue;
            }
        };
        let req_blob = encode_fs_req_for_kotlin(&msg);
        if req_tx.send(req_blob).is_err() {
            info!("fs_serve_loop: req receiver gone; exiting");
            return;
        }
        let reply_blob = {
            let mut guard = reply_rx.lock().await;
            match guard.recv().await {
                Some(b) => b,
                None => {
                    info!("fs_serve_loop: reply sender gone; exiting");
                    return;
                }
            }
        };
        let reply_msg = match decode_fs_reply_from_kotlin(&reply_blob) {
            Ok(m) => m,
            Err(e) => {
                warn!("fs_serve_loop: bad reply blob: {e}");
                FsOpMessage::Error {
                    code: 5,
                    message: format!("bad reply: {e}"),
                }
            }
        };
        let out = match postcard::to_allocvec(&reply_msg) {
            Ok(v) => v,
            Err(e) => {
                warn!("fs_serve_loop: postcard encode reply: {e}");
                return;
            }
        };
        if let Err(e) = stream.send(Bytes::from(out)).await {
            warn!("fs_serve_loop: stream send reply failed: {e}");
            return;
        }
    }
}

/// Tag-binary FsOp wire format — native → Kotlin direction.
///
/// Layout per tag (multi-byte ints little-endian, strings are length-
/// prefixed u32 + UTF-8 bytes, byte blobs are u32 + bytes):
///   0 Stat(path)
///   1 ReadDir(path)
///   2 Open(path, flags u32)
///   3 Read(handle u64, offset u64, len u32)
///   4 Write(handle u64, offset u64, data blob)
///   5 Close(handle u64)
///   6 Create(path, mode u32)
///   7 Unlink(path)
///   8 Rename(from, to)
///   9 Truncate(path, size u64)
///  10 Chmod(path, mode u32)
///
/// Mirrored verbatim in Kotlin `FsOpCodec.kt`. Any change requires
/// matching diffs on both files in the same commit.
fn encode_fs_req_for_kotlin(msg: &FsOpMessage) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    match msg {
        FsOpMessage::Stat { path } => {
            out.push(0);
            write_str(&mut out, path);
        }
        FsOpMessage::ReadDir { path } => {
            out.push(1);
            write_str(&mut out, path);
        }
        FsOpMessage::Open { path, flags } => {
            out.push(2);
            write_str(&mut out, path);
            out.extend_from_slice(&flags.to_le_bytes());
        }
        FsOpMessage::Read { handle, offset, len } => {
            out.push(3);
            out.extend_from_slice(&handle.to_le_bytes());
            out.extend_from_slice(&offset.to_le_bytes());
            out.extend_from_slice(&len.to_le_bytes());
        }
        FsOpMessage::Write { handle, offset, data } => {
            out.push(4);
            out.extend_from_slice(&handle.to_le_bytes());
            out.extend_from_slice(&offset.to_le_bytes());
            write_blob(&mut out, data);
        }
        FsOpMessage::Close { handle } => {
            out.push(5);
            out.extend_from_slice(&handle.to_le_bytes());
        }
        FsOpMessage::Create { path, mode } => {
            out.push(6);
            write_str(&mut out, path);
            out.extend_from_slice(&mode.to_le_bytes());
        }
        FsOpMessage::Unlink { path } => {
            out.push(7);
            write_str(&mut out, path);
        }
        FsOpMessage::Rename { from, to } => {
            out.push(8);
            write_str(&mut out, from);
            write_str(&mut out, to);
        }
        FsOpMessage::Truncate { path, size } => {
            out.push(9);
            write_str(&mut out, path);
            out.extend_from_slice(&size.to_le_bytes());
        }
        FsOpMessage::Chmod { path, mode } => {
            out.push(10);
            write_str(&mut out, path);
            out.extend_from_slice(&mode.to_le_bytes());
        }
        FsOpMessage::Ok
        | FsOpMessage::StatReply { .. }
        | FsOpMessage::ReadDirReply { .. }
        | FsOpMessage::OpenReply { .. }
        | FsOpMessage::ReadReply { .. }
        | FsOpMessage::WriteReply { .. }
        | FsOpMessage::CreateReply { .. }
        | FsOpMessage::Error { .. } => {
            // Reply-side variants — should not appear as inbound requests.
            // Emit a single zero byte so Kotlin can flag "unexpected".
            out.clear();
            out.push(0xFF);
        }
    }
    out
}

/// Tag-binary FsOp reply wire — Kotlin → native. Tag layout:
///   0  Ok                 : (no payload)
///   1  StatReply          : meta(size u64, mode u32, mtime u64, is_dir u8)
///   2  ReadDirReply       : count u32, then count × { name(str), meta(20B) }
///   3  OpenReply          : handle u64
///   4  ReadReply          : data(blob)
///   5  WriteReply         : written u32
///   6  CreateReply        : handle u64
///   7  Error              : code i32, message(str)
fn decode_fs_reply_from_kotlin(bytes: &[u8]) -> Result<FsOpMessage, String> {
    let mut c = Cursor::new(bytes);
    let tag = c.take(1)?[0];
    match tag {
        0 => Ok(FsOpMessage::Ok),
        1 => {
            let meta = read_meta(&mut c)?;
            Ok(FsOpMessage::StatReply { meta })
        }
        2 => {
            let count = c.take_u32()?;
            let mut entries = Vec::with_capacity(count as usize);
            for _ in 0..count {
                let name = c.take_str()?;
                let meta = read_meta(&mut c)?;
                entries.push(ansync_proto::FsEntry { name, meta });
            }
            Ok(FsOpMessage::ReadDirReply { entries })
        }
        3 => Ok(FsOpMessage::OpenReply {
            handle: c.take_u64()?,
        }),
        4 => Ok(FsOpMessage::ReadReply {
            data: c.take_blob()?,
        }),
        5 => Ok(FsOpMessage::WriteReply {
            written: c.take_u32()?,
        }),
        6 => Ok(FsOpMessage::CreateReply {
            handle: c.take_u64()?,
        }),
        7 => Ok(FsOpMessage::Error {
            code: c.take_i32()?,
            message: c.take_str()?,
        }),
        other => Err(format!("unknown reply tag {other}")),
    }
}

fn read_meta(c: &mut Cursor<'_>) -> Result<ansync_proto::FsMeta, String> {
    Ok(ansync_proto::FsMeta {
        size: c.take_u64()?,
        mode: c.take_u32()?,
        mtime: c.take_u64()?,
        is_dir: c.take(1)?[0] != 0,
    })
}

fn write_str(out: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    let len = bytes.len() as u32;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(bytes);
}

fn write_blob(out: &mut Vec<u8>, b: &[u8]) {
    let len = b.len() as u32;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(b);
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
    fn take_u64(&mut self) -> Result<u64, String> {
        let s = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(s);
        Ok(u64::from_le_bytes(a))
    }
    fn take_str(&mut self) -> Result<String, String> {
        let n = self.take_u32()? as usize;
        let s = self.take(n)?;
        String::from_utf8(s.to_vec()).map_err(|e| e.to_string())
    }
    fn take_blob(&mut self) -> Result<Vec<u8>, String> {
        let n = self.take_u32()? as usize;
        Ok(self.take(n)?.to_vec())
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
) {
    loop {
        let bytes = match stream.recv().await {
            Ok(b) => b,
            Err(ansync_transport::TransportError::Closed) => {
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
            Err(ansync_transport::TransportError::Closed) => {
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

async fn clipboard_in_loop(mut stream: QuicStream, tx: UnboundedSender<String>) {
    loop {
        let bytes = match stream.recv().await {
            Ok(b) => b,
            Err(ansync_transport::TransportError::Closed) => return,
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
        let text = match msg {
            ClipboardMessage::Text { content } => content,
            ClipboardMessage::Blob { mime, .. } => {
                warn!("clipboard_in_loop: ignoring blob (mime={mime})");
                continue;
            }
        };
        if tx.send(text).is_err() {
            info!("clipboard_in_loop: receiver dropped; exiting");
            return;
        }
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
        let peer = bootstrap_companion(&mut stream, &identity, &companion_name)
            .await
            .map_err(|e| std::io::Error::other(format!("bootstrap: {e}")))?;
        Ok::<_, std::io::Error>(peer)
    });
    let peer = match result {
        Ok(p) => p,
        Err(e) => {
            error!("nativePairOverCable: pair failed: {e}");
            return std::ptr::null_mut();
        }
    };
    info!("cable pairing complete with host {}", peer.name.0);
    let hex = hex_encode(&peer.pubkey);
    let response = format!("{hex}|{}", peer.name.0);
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
            let pressure_lo = c.take(1)?[0];
            let pressure_hi = c.take(1)?[0];
            let tracking_id = c.take_i32()?;
            Ok(InputMessage::TouchSlot {
                slot,
                x,
                y,
                pressure: u16::from_le_bytes([pressure_lo, pressure_hi]),
                tracking_id,
            })
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
        Ok(()) => jni::sys::JNI_TRUE,
        Err(e) => {
            warn!("nativeSendInputMessage: send failed: {e}");
            jni::sys::JNI_FALSE
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativePollFsRequest<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jni::sys::jbyteArray {
    let fs_req_rx = {
        let slot = state_slot().lock().expect("state mutex poisoned");
        match slot.as_ref().and_then(|s| s.session.as_ref()) {
            Some(sess) => sess.fs_req_rx.clone(),
            None => return std::ptr::null_mut(),
        }
    };
    let bytes = runtime().block_on(async move {
        let mut guard = fs_req_rx.lock().await;
        guard.recv().await
    });
    match bytes {
        Some(b) => match env.byte_array_from_slice(&b) {
            Ok(arr) => arr.into_raw(),
            Err(e) => {
                error!("nativePollFsRequest: byte_array_from_slice: {e}");
                std::ptr::null_mut()
            }
        },
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativeFsReply<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
    reply: JByteArray<'local>,
) -> jboolean {
    let bytes = match env.convert_byte_array(&reply) {
        Ok(b) => b,
        Err(e) => {
            error!("nativeFsReply: convert_byte_array: {e}");
            return jni::sys::JNI_FALSE;
        }
    };
    let fs_reply_tx = {
        let slot = state_slot().lock().expect("state mutex poisoned");
        match slot.as_ref().and_then(|s| s.session.as_ref()) {
            Some(sess) => sess.fs_reply_tx.clone(),
            None => return jni::sys::JNI_FALSE,
        }
    };
    if fs_reply_tx.send(bytes).is_err() {
        warn!("nativeFsReply: reply channel closed");
        return jni::sys::JNI_FALSE;
    }
    jni::sys::JNI_TRUE
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

