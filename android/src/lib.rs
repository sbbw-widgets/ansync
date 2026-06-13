//! Native (Rust) half of the ansync companion.
//!
//! Exposes a small JNI surface that the Kotlin `AnsyncCompanionService`
//! calls into. Internally owns a `tokio` runtime + a `quinn` QUIC
//! client to the paired host. Wire format is identical to the host
//! (`ansync_proto`) so the daemon's `StreamKind::Input` /
//! `StreamKind::Video` accept loop just works.
//!
//! Step 7d-1 (this file) ships the JNI scaffolding + tokio runtime
//! lifecycle. The actual QUIC dial, screen capture stream pump, and
//! reverse input dispatch land in 7d-2.

use std::sync::{Mutex, OnceLock};

use jni::JNIEnv;
use jni::objects::{JClass, JString};
use jni::sys::{jboolean, jlong, jint};
use log::{error, info, warn};
use tokio::runtime::Runtime;

/// Process-wide tokio runtime. Initialised on first `nativeInit` call
/// and never torn down — the companion's foreground service owns the
/// process lifecycle, and recreating the runtime on each Kotlin
/// reconnect would leak background workers.
static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// Active companion session, if any. Wrapped in a Mutex so callers
/// from different Kotlin threads (capture loop, accessibility
/// service) serialise their access to the underlying connection.
static SESSION: OnceLock<Mutex<Option<CompanionSession>>> = OnceLock::new();

fn session_slot() -> &'static Mutex<Option<CompanionSession>> {
    SESSION.get_or_init(|| Mutex::new(None))
}

struct CompanionSession {
    /// Placeholder for the real `quinn::Connection` that lands in
    /// 7d-2. Today the session just records its target host so the
    /// JNI smoke-test can verify the round-trip works.
    host: String,
    port: u16,
}

#[no_mangle]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativeInit(
    _env: JNIEnv,
    _class: JClass,
) -> jboolean {
    if RUNTIME
        .set(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("ansync-native")
                .build()
                .expect("tokio runtime build"),
        )
        .is_err()
    {
        // Already initialised — return success; idempotent.
        return jni::sys::JNI_TRUE;
    }
    android_logger::init_once(
        android_logger::Config::default()
            .with_tag("ansync")
            .with_max_level(log::LevelFilter::Info),
    );
    info!("ansync companion native runtime ready");
    jni::sys::JNI_TRUE
}

#[no_mangle]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativeOpenConnection(
    mut env: JNIEnv,
    _class: JClass,
    host: JString,
    port: jint,
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
    // Real `quinn::Endpoint::connect` plus Ed25519 verifier wires in
    // 7d-2. Step 7d-1 just records the requested target so the JNI
    // contract is exercised end-to-end.
    let mut slot = session_slot().lock().expect("session mutex poisoned");
    *slot = Some(CompanionSession { host, port });
    info!(
        "nativeOpenConnection: target stashed (real dial wires in 7d-2)"
    );
    jni::sys::JNI_TRUE
}

#[no_mangle]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativeSendVideoChunk(
    _env: JNIEnv,
    _class: JClass,
    _chunk: jni::objects::JByteArray,
    _pts_us: jlong,
) -> jboolean {
    // Step 7d-2 forwards `chunk` over the `StreamKind::Video` QUIC
    // bidi stream. For now: no-op success so the Kotlin capture loop
    // can be wired without crashing.
    jni::sys::JNI_TRUE
}

#[no_mangle]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativePollInputMessage(
    _env: JNIEnv,
    _class: JClass,
) -> jni::sys::jbyteArray {
    // Returns a postcard-encoded `proto::InputMessage` from the host's
    // input stream, or null if none queued. AccessibilityService
    // polls this on a worker thread and replays gestures. Step 7d-2
    // wires the actual recv loop.
    std::ptr::null_mut()
}

#[no_mangle]
pub extern "system" fn Java_org_gameros_ansync_NativeBridge_nativeClose(
    _env: JNIEnv,
    _class: JClass,
) {
    let mut slot = session_slot().lock().expect("session mutex poisoned");
    if let Some(_session) = slot.take() {
        info!("nativeClose: session torn down");
    } else {
        warn!("nativeClose: no session to close");
    }
}
