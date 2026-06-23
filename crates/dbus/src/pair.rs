//! WiFi-PIN pairing surface on D-Bus.
//!
//! Path layout:
//!   * `/org/gameros/Ansync1/Manager` gains `BrowseAvailable` +
//!     `StartPairing` (defined in [`crate::manager`]).
//!   * Each in-flight session lives at
//!     `/org/gameros/Ansync1/Pair/{uuid}` exporting
//!     [`PairingSessionIface`].
//!
//! Lifecycle: the Manager spawns a worker per session that owns the
//! TCP socket, drives the host-side bootstrap, and updates a shared
//! [`PairSessionSnapshot`] through transition points. The widget reads
//! `State` / `HostName` properties + `PropertiesChanged` to render
//! UX; once `State` flips to `awaiting_pin`, the user types the PIN on
//! whatever surface they prefer (CLI, GTK dialog, DMS overlay) and the
//! UX calls [`PairingSessionIface::submit_pin`]. The worker computes
//! the MAC, exchanges it, and signals `Completed` / `Failed`. Sessions
//! are auto-removed from the object server 60 s after a terminal
//! state, so a slow client still has time to read the final state.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use ansync_core::{Capabilities, DeviceName, DevicePermissions};
use ansync_crypto::{PinRole, pair_pin_confirm, verify_pin_confirm};
use ansync_pairing::StoredPeer;
use ansync_proto::{
    Envelope, Message, PROTOCOL_VERSION, PairingMessage, read_envelope, write_envelope,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tracing::{info, warn};
use zbus::Connection;
use zbus::interface;
use zbus::object_server::SignalEmitter;

use crate::register_device;
use crate::state::DaemonState;

/// Maximum size of any pairing envelope. Pair messages carry only
/// pubkey + name + MAC; 4 KiB is two orders of magnitude over what
/// they actually need.
const PAIRING_FRAME_MAX: usize = 4 * 1024;

/// How long the worker waits for [`PairingSessionIface::submit_pin`]
/// after publishing `awaiting_pin` before giving up. Five minutes
/// covers the realistic UX of "user picks up the phone, walks back
/// to the desk, types the PIN".
const PIN_WAIT_TIMEOUT: Duration = Duration::from_secs(300);

/// How long the session stays addressable on D-Bus after reaching a
/// terminal state. Clients that subscribe just-too-late still get one
/// `Get` call to read the final state.
const SESSION_LINGER: Duration = Duration::from_secs(60);

/// Build the canonical path for a session id.
pub fn path_pair_session(id: &str) -> String {
    format!("/org/gameros/Ansync1/Pair/{id}")
}

/// Snapshot of the session's externally-visible state. Mutated by the
/// worker, read by the D-Bus interface's property accessors.
#[derive(Debug, Clone)]
pub struct PairSessionSnapshot {
    pub state: PairState,
    pub host_name: String,
    pub host_pubkey_hex: String,
    pub address: String,
    pub error: String,
}

impl PairSessionSnapshot {
    fn new(address: String) -> Self {
        Self {
            state: PairState::Dialing,
            host_name: String::new(),
            host_pubkey_hex: String::new(),
            address,
            error: String::new(),
        }
    }
}

/// Lifecycle of a single pairing session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PairState {
    /// `PairAuto` is probing ADB / browsing mDNS to pick a transport.
    #[default]
    Discovering,
    /// TCP connect + Hello + Ack round in flight (Wi-Fi), or APK
    /// install + reverse-forward + companion broadcast (cable).
    Dialing,
    /// Ack received, companion identity known. Waiting for the user
    /// to type the PIN currently displayed on the device. Wi-Fi only.
    AwaitingPin,
    /// PIN submitted; computing MAC and exchanging with the peer.
    /// Wi-Fi only.
    Verifying,
    /// Pair complete; peer persisted; Device path registered.
    Ok,
    /// Terminal failure; see [`PairSessionSnapshot::error`].
    Failed,
}

impl PairState {
    pub fn as_str(self) -> &'static str {
        match self {
            PairState::Discovering => "discovering",
            PairState::Dialing => "dialing",
            PairState::AwaitingPin => "awaiting_pin",
            PairState::Verifying => "verifying",
            PairState::Ok => "ok",
            PairState::Failed => "failed",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, PairState::Ok | PairState::Failed)
    }
}

/// D-Bus interface backing `/org/gameros/Ansync1/Pair/{id}`.
pub struct PairingSessionIface {
    pub id: String,
    pub snapshot: Arc<StdMutex<PairSessionSnapshot>>,
    pub pin_tx: UnboundedSender<[u8; 6]>,
    pub cancel_tx: UnboundedSender<()>,
}

#[interface(name = "org.gameros.Ansync1.PairingSession")]
impl PairingSessionIface {
    #[zbus(property)]
    fn state(&self) -> String {
        self.snapshot
            .lock()
            .map(|g| g.state.as_str().to_string())
            .unwrap_or_else(|_| "failed".to_string())
    }

    #[zbus(property)]
    fn host_name(&self) -> String {
        self.snapshot
            .lock()
            .map(|g| g.host_name.clone())
            .unwrap_or_default()
    }

    #[zbus(property, name = "HostPubkeyHex")]
    fn host_pubkey_hex(&self) -> String {
        self.snapshot
            .lock()
            .map(|g| g.host_pubkey_hex.clone())
            .unwrap_or_default()
    }

    #[zbus(property)]
    fn address(&self) -> String {
        self.snapshot
            .lock()
            .map(|g| g.address.clone())
            .unwrap_or_default()
    }

    #[zbus(property)]
    fn error(&self) -> String {
        self.snapshot
            .lock()
            .map(|g| g.error.clone())
            .unwrap_or_default()
    }

    /// Submit the 6-digit PIN the user read off the device. Any
    /// non-digit characters are stripped before validation, so
    /// `"123 456"` and `"1-2-3-4-5-6"` both work. Returns `InvalidArgs`
    /// when the digit count is not exactly six.
    async fn submit_pin(&self, pin: String) -> zbus::fdo::Result<()> {
        let digits: Vec<u8> = pin
            .chars()
            .filter(|c| c.is_ascii_digit())
            .map(|c| c as u8)
            .collect();
        if digits.len() != 6 {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "expected 6 digits, got {}",
                digits.len()
            )));
        }
        let mut arr = [0u8; 6];
        arr.copy_from_slice(&digits);
        self.pin_tx.send(arr).map_err(|_| {
            zbus::fdo::Error::Failed("pairing session already terminated".into())
        })?;
        Ok(())
    }

    /// Abort the in-flight session. After `Cancel()` returns, the
    /// state property settles to `failed` with error `"cancelled"` and
    /// the session lingers on D-Bus for [`SESSION_LINGER`] before
    /// auto-removal.
    async fn cancel(&self) -> zbus::fdo::Result<()> {
        let _ = self.cancel_tx.send(());
        Ok(())
    }

    /// Emitted exactly once on successful pair. `device_id` is the
    /// hex of the peer's first 16 pubkey bytes — the same id used by
    /// `Device/{id}` and `Permissions/{id}` paths.
    #[zbus(signal)]
    pub async fn completed(
        emitter: &SignalEmitter<'_>,
        device_id: &str,
        name: &str,
    ) -> zbus::Result<()>;

    /// Emitted exactly once on failure (cancel, timeout, PIN mismatch,
    /// network error, …). `reason` is human-readable.
    #[zbus(signal)]
    pub async fn failed(emitter: &SignalEmitter<'_>, reason: &str) -> zbus::Result<()>;
}

/// Spawn the worker for a brand-new session. Caller has already
/// registered [`PairingSessionIface`] at [`path_pair_session`] on the
/// connection's object server.
pub fn spawn_session(
    conn: Connection,
    state: Arc<DaemonState>,
    session_id: String,
    addr: std::net::SocketAddr,
    expected_pubkey: Option<[u8; 32]>,
    snapshot: Arc<StdMutex<PairSessionSnapshot>>,
    pin_rx: UnboundedReceiver<[u8; 6]>,
    cancel_rx: UnboundedReceiver<()>,
) {
    tokio::spawn(drive_session(
        conn,
        state,
        session_id,
        addr,
        expected_pubkey,
        snapshot,
        pin_rx,
        cancel_rx,
    ));
}

/// Allocate the per-session channels + shared snapshot. Caller wires
/// the interface, registers it on the object server, then calls
/// [`spawn_session`].
pub fn allocate() -> (
    Arc<StdMutex<PairSessionSnapshot>>,
    UnboundedSender<[u8; 6]>,
    UnboundedReceiver<[u8; 6]>,
    UnboundedSender<()>,
    UnboundedReceiver<()>,
) {
    let snapshot = Arc::new(StdMutex::new(PairSessionSnapshot::new(String::new())));
    let (pin_tx, pin_rx) = unbounded_channel();
    let (cancel_tx, cancel_rx) = unbounded_channel();
    (snapshot, pin_tx, pin_rx, cancel_tx, cancel_rx)
}

async fn drive_session(
    conn: Connection,
    state: Arc<DaemonState>,
    session_id: String,
    addr: std::net::SocketAddr,
    expected_pubkey: Option<[u8; 32]>,
    snapshot: Arc<StdMutex<PairSessionSnapshot>>,
    mut pin_rx: UnboundedReceiver<[u8; 6]>,
    mut cancel_rx: UnboundedReceiver<()>,
) {
    let path = path_pair_session(&session_id);

    // 1. Dial.
    let stream = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::net::TcpStream::connect(addr),
    )
    .await;
    let mut stream = match stream {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            finalize_failed(&conn, &path, &snapshot, format!("connect: {e}")).await;
            return;
        }
        Err(_) => {
            finalize_failed(&conn, &path, &snapshot, "connect timeout".into()).await;
            return;
        }
    };

    // 2. Send BootstrapHello.
    let host_pk = state.identity.public().as_bytes();
    let hello = Envelope {
        version: PROTOCOL_VERSION,
        message: Message::Pairing(PairingMessage::BootstrapHello {
            identity_pubkey: host_pk,
            name: state.device_name.clone(),
        }),
    };
    if let Err(e) = write_envelope(&mut stream, &hello).await {
        finalize_failed(&conn, &path, &snapshot, format!("send hello: {e}")).await;
        return;
    }

    // 3. Read BootstrapAck.
    let envelope = match read_envelope(&mut stream, PAIRING_FRAME_MAX).await {
        Ok(env) => env,
        Err(e) => {
            finalize_failed(&conn, &path, &snapshot, format!("read ack: {e}")).await;
            return;
        }
    };
    let (companion_pk, companion_name) = match envelope.message {
        Message::Pairing(PairingMessage::BootstrapAck { identity_pubkey, name, .. }) => {
            (identity_pubkey, name)
        }
        other => {
            finalize_failed(
                &conn,
                &path,
                &snapshot,
                format!("unexpected: {other:?}"),
            )
            .await;
            return;
        }
    };

    // mDNS-bound pubkey check. Protects against a different host
    // hijacking the mDNS record between browse + dial. Failure here
    // is treated identically to a wrong-PIN MITM — abort, surface to
    // the widget, do not persist.
    if let Some(expected) = expected_pubkey {
        if expected != companion_pk {
            finalize_failed(
                &conn,
                &path,
                &snapshot,
                "mDNS pubkey did not match handshake pubkey — possible MITM".into(),
            )
            .await;
            return;
        }
    }

    {
        let mut g = snapshot.lock().expect("snapshot poisoned");
        g.state = PairState::AwaitingPin;
        g.host_name = companion_name.clone();
        g.host_pubkey_hex = hex::encode(companion_pk);
    }
    emit_property_changes(
        &conn,
        &path,
        &["State", "HostName", "HostPubkeyHex"],
    )
    .await;

    // 4. Wait for the user to submit the PIN (or cancel).
    let pin = tokio::select! {
        biased;
        _ = cancel_rx.recv() => {
            finalize_failed(&conn, &path, &snapshot, "cancelled".into()).await;
            return;
        }
        pin = pin_rx.recv() => match pin {
            Some(p) => p,
            None => {
                finalize_failed(&conn, &path, &snapshot, "pin channel closed".into()).await;
                return;
            }
        },
        _ = tokio::time::sleep(PIN_WAIT_TIMEOUT) => {
            finalize_failed(&conn, &path, &snapshot, "pin entry timeout".into()).await;
            return;
        }
    };

    snapshot.lock().expect("snapshot poisoned").state = PairState::Verifying;
    emit_property_changes(&conn, &path, &["State"]).await;

    // 5. Compute + exchange MAC.
    let host_mac = pair_pin_confirm(&pin, PinRole::Host, &host_pk, &companion_pk);
    let send_mac = Envelope {
        version: PROTOCOL_VERSION,
        message: Message::Pairing(PairingMessage::PinConfirm { mac: host_mac }),
    };
    if let Err(e) = write_envelope(&mut stream, &send_mac).await {
        finalize_failed(&conn, &path, &snapshot, format!("send mac: {e}")).await;
        return;
    }
    let envelope = match read_envelope(&mut stream, PAIRING_FRAME_MAX).await {
        Ok(env) => env,
        Err(e) => {
            finalize_failed(&conn, &path, &snapshot, format!("read companion mac: {e}"))
                .await;
            return;
        }
    };
    let companion_mac = match envelope.message {
        Message::Pairing(PairingMessage::PinConfirm { mac }) => mac,
        other => {
            finalize_failed(
                &conn,
                &path,
                &snapshot,
                format!("unexpected: {other:?}"),
            )
            .await;
            return;
        }
    };
    if !verify_pin_confirm(
        &companion_mac,
        &pin,
        PinRole::Companion,
        &host_pk,
        &companion_pk,
    ) {
        finalize_failed(
            &conn,
            &path,
            &snapshot,
            "pin mismatch (wrong code or MITM)".into(),
        )
        .await;
        return;
    }

    use tokio::io::AsyncWriteExt;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;

    // 6. Persist.
    let peer = StoredPeer::new(
        DeviceName(companion_name.clone()),
        companion_pk,
        Capabilities::empty(),
        DevicePermissions::default(),
    );
    let device_id = peer.id.to_string();
    if let Err(e) = state.peers.put(&peer) {
        finalize_failed(&conn, &path, &snapshot, format!("persist: {e}")).await;
        return;
    }

    // 7. Auto-register Device + Permissions paths so subscribers can
    // immediately address the new peer without poking RefreshPeers.
    if let Err(e) = register_device(&conn, &state, peer.id.clone()).await {
        warn!(error = %e, "register_device after pair failed");
    }

    snapshot.lock().expect("snapshot poisoned").state = PairState::Ok;
    emit_property_changes(&conn, &path, &["State"]).await;

    // 8. Completed signal.
    let signal_path = match zbus::zvariant::ObjectPath::try_from(path.as_str()) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, %path, "build SignalEmitter path failed");
            finalize_linger(conn, path).await;
            return;
        }
    };
    if let Ok(emitter) = SignalEmitter::new(&conn, signal_path) {
        let _ = PairingSessionIface::completed(&emitter, &device_id, &companion_name).await;
        // Fan-out for widgets that subscribe per-device on the Manager.
        if let Ok(mgr_emitter) =
            SignalEmitter::new(&conn, crate::PATH_MANAGER)
        {
            let _ = crate::manager::Manager::device_connectivity_changed(
                &mgr_emitter,
                &device_id,
                "pairing",
            )
            .await;
        }
    }

    info!(%device_id, name = %companion_name, "pair session completed");
    finalize_linger(conn, path).await;
}

async fn finalize_failed(
    conn: &Connection,
    path: &str,
    snapshot: &Arc<StdMutex<PairSessionSnapshot>>,
    reason: String,
) {
    {
        let mut g = snapshot.lock().expect("snapshot poisoned");
        g.state = PairState::Failed;
        g.error = reason.clone();
    }
    emit_property_changes(conn, path, &["State", "Error"]).await;
    if let Ok(p) = zbus::zvariant::ObjectPath::try_from(path) {
        if let Ok(emitter) = SignalEmitter::new(conn, p) {
            let _ = PairingSessionIface::failed(&emitter, &reason).await;
        }
    }
    warn!(%path, %reason, "pair session failed");
    finalize_linger(conn.clone(), path.to_string()).await;
}

async fn finalize_linger(conn: Connection, path: String) {
    tokio::spawn(async move {
        tokio::time::sleep(SESSION_LINGER).await;
        if let Err(e) = conn
            .object_server()
            .remove::<PairingSessionIface, _>(path.clone())
            .await
        {
            warn!(%path, error = %e, "auto-remove of stale pair session failed");
        }
    });
}

/// Spawn the worker for a cable / ADB-bootstrapped pair. Caller has
/// already registered [`PairingSessionIface`] at
/// [`path_pair_session`].
///
/// `serial = None` lets the worker auto-pick when exactly one device is
/// attached (errors otherwise). `apk_override = None` triggers the
/// release-fetch + install fallback when the companion is missing or
/// version-mismatched.
pub fn spawn_cable_session(
    conn: Connection,
    state: Arc<DaemonState>,
    session_id: String,
    serial: Option<String>,
    apk_override: Option<std::path::PathBuf>,
    snapshot: Arc<StdMutex<PairSessionSnapshot>>,
    pin_rx: UnboundedReceiver<[u8; 6]>,
    cancel_rx: UnboundedReceiver<()>,
) {
    tokio::spawn(drive_cable_session(
        conn,
        state,
        session_id,
        serial,
        apk_override,
        snapshot,
        cancel_rx,
        pin_rx,
    ));
}

/// Spawn the auto-dispatching worker. Probes ADB; if a single device
/// is attached, runs the cable flow. Otherwise browses mDNS for
/// `discover_seconds` (defaults to 5) — exactly one pair-ready
/// companion auto-routes through the Wi-Fi PIN flow, anything else
/// terminates with `failed`.
pub fn spawn_auto_session(
    conn: Connection,
    state: Arc<DaemonState>,
    session_id: String,
    discover_seconds: u32,
    apk_override: Option<std::path::PathBuf>,
    snapshot: Arc<StdMutex<PairSessionSnapshot>>,
    pin_rx: UnboundedReceiver<[u8; 6]>,
    cancel_rx: UnboundedReceiver<()>,
) {
    tokio::spawn(drive_auto_session(
        conn,
        state,
        session_id,
        discover_seconds,
        apk_override,
        snapshot,
        pin_rx,
        cancel_rx,
    ));
}

async fn drive_cable_session(
    conn: Connection,
    state: Arc<DaemonState>,
    session_id: String,
    serial_hint: Option<String>,
    apk_override: Option<std::path::PathBuf>,
    snapshot: Arc<StdMutex<PairSessionSnapshot>>,
    mut cancel_rx: UnboundedReceiver<()>,
    _pin_rx: UnboundedReceiver<[u8; 6]>,
) {
    let path = path_pair_session(&session_id);

    let serial = match resolve_serial(serial_hint).await {
        Ok(s) => s,
        Err(e) => return finalize_failed(&conn, &path, &snapshot, e).await,
    };
    {
        let mut g = snapshot.lock().expect("snapshot poisoned");
        g.state = PairState::Dialing;
        g.address = format!("cable://{serial}");
    }
    emit_property_changes(&conn, &path, &["State", "Address"]).await;

    let apk_path = match resolve_apk(&serial, apk_override).await {
        Ok(p) => p,
        Err(e) => return finalize_failed(&conn, &path, &snapshot, e).await,
    };

    let local_name = state.device_name.clone();
    let lan_endpoints = state
        .listen_endpoints
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default();

    let result = tokio::select! {
        biased;
        _ = cancel_rx.recv() => {
            return finalize_failed(&conn, &path, &snapshot, "cancelled".into()).await;
        }
        r = ansync_pairing::pair_host_via_adb(
            &serial,
            &state.identity,
            &local_name,
            apk_path.as_deref(),
            lan_endpoints,
        ) => r,
    };

    let peer = match result {
        Ok(p) => p,
        Err(e) => {
            return finalize_failed(
                &conn,
                &path,
                &snapshot,
                format!("cable bootstrap: {e}"),
            )
            .await;
        }
    };

    complete_pair(&conn, &state, &snapshot, &path, peer).await;
}

async fn drive_auto_session(
    conn: Connection,
    state: Arc<DaemonState>,
    session_id: String,
    discover_seconds: u32,
    apk_override: Option<std::path::PathBuf>,
    snapshot: Arc<StdMutex<PairSessionSnapshot>>,
    pin_rx: UnboundedReceiver<[u8; 6]>,
    cancel_rx: UnboundedReceiver<()>,
) {
    let path = path_pair_session(&session_id);
    {
        let mut g = snapshot.lock().expect("snapshot poisoned");
        g.state = PairState::Discovering;
    }
    emit_property_changes(&conn, &path, &["State"]).await;

    let adb = ansync_pairing::list_adb_devices().await.unwrap_or_default();
    if !adb.is_empty() {
        drive_cable_session(
            conn,
            state,
            session_id,
            None,
            apk_override,
            snapshot,
            cancel_rx,
            pin_rx,
        )
        .await;
        return;
    }

    let secs = if discover_seconds == 0 {
        5
    } else {
        discover_seconds as u64
    };
    let cands = match ansync_pairing::browse_pair_candidates(Duration::from_secs(secs))
        .await
    {
        Ok(v) => v,
        Err(e) => {
            return finalize_failed(&conn, &path, &snapshot, format!("mdns browse: {e}"))
                .await;
        }
    };
    let picked = match cands.len() {
        0 => {
            return finalize_failed(
                &conn,
                &path,
                &snapshot,
                "no ADB device and no pair-ready companion on the LAN".into(),
            )
            .await;
        }
        1 => cands.into_iter().next().expect("len==1"),
        n => {
            return finalize_failed(
                &conn,
                &path,
                &snapshot,
                format!(
                    "{n} pair-ready devices on the LAN — call BrowseAvailable + StartPairing to pick one"
                ),
            )
            .await;
        }
    };

    {
        let mut g = snapshot.lock().expect("snapshot poisoned");
        g.state = PairState::Dialing;
        g.address = picked.addr.to_string();
    }
    emit_property_changes(&conn, &path, &["State", "Address"]).await;

    drive_session(
        conn,
        state,
        session_id,
        picked.addr,
        Some(picked.pubkey),
        snapshot,
        pin_rx,
        cancel_rx,
    )
    .await;
}

/// Auto-pick a serial from the local ADB server. `None` hint = require
/// exactly one device. Empty string is normalised to `None` by the
/// D-Bus surface before this is called.
async fn resolve_serial(hint: Option<String>) -> Result<String, String> {
    if let Some(s) = hint.filter(|s| !s.is_empty()) {
        return Ok(s);
    }
    let devices = ansync_pairing::list_adb_devices()
        .await
        .map_err(|e| format!("adb list: {e}"))?;
    match devices.len() {
        0 => Err("no ADB devices attached".into()),
        1 => Ok(devices.into_iter().next().expect("len==1").serial),
        n => Err(format!(
            "{n} ADB devices attached — pass an explicit serial"
        )),
    }
}

/// Locate the companion APK to install, or `None` when the already-
/// installed `versionName` matches `expected_version_bare()` (in which
/// case the pair broadcast just re-wakes the running service).
///
/// Order: explicit override → `ANSYNC_COMPANION_APK` env →
/// `/usr/share/ansync/companion.apk` → release fetch.
async fn resolve_apk(
    serial: &str,
    override_path: Option<std::path::PathBuf>,
) -> Result<Option<std::path::PathBuf>, String> {
    use std::path::PathBuf;
    if let Some(p) = override_path {
        if !p.exists() {
            return Err(format!("APK override not found: {}", p.display()));
        }
        return Ok(Some(p));
    }
    if let Some(env) = std::env::var_os("ANSYNC_COMPANION_APK") {
        let p = PathBuf::from(env);
        if p.exists() {
            return Ok(Some(p));
        }
    }
    let std_path = PathBuf::from("/usr/share/ansync/companion.apk");
    if std_path.exists() {
        return Ok(Some(std_path));
    }
    let expected = ansync_pairing::expected_version_bare();
    let installed = ansync_pairing::query_installed_version(
        serial,
        ansync_pairing::COMPANION_PACKAGE,
    )
    .await
    .unwrap_or(None);
    if installed
        .as_deref()
        .map(|v| v.trim().eq_ignore_ascii_case(expected))
        .unwrap_or(false)
    {
        return Ok(None);
    }
    match ansync_pairing::fetch_companion(expected).await {
        Ok(f) => Ok(Some(f.path)),
        Err(e) => Err(format!("APK fetch failed for {expected}: {e}")),
    }
}

/// Shared completion path between the cable + Wi-Fi workers: persist,
/// register the Device path, flip the session to `ok`, fire the
/// `Completed` + `DeviceConnectivityChanged("pairing")` signals,
/// schedule auto-removal.
async fn complete_pair(
    conn: &Connection,
    state: &Arc<DaemonState>,
    snapshot: &Arc<StdMutex<PairSessionSnapshot>>,
    path: &str,
    peer: StoredPeer,
) {
    let device_id = peer.id.to_string();
    let companion_name = peer.name.0.clone();

    if let Err(e) = state.peers.put(&peer) {
        finalize_failed(conn, path, snapshot, format!("persist: {e}")).await;
        return;
    }
    if let Err(e) = register_device(conn, state, peer.id.clone()).await {
        warn!(error = %e, "register_device after pair failed");
    }

    snapshot.lock().expect("snapshot poisoned").state = PairState::Ok;
    emit_property_changes(conn, path, &["State"]).await;

    let signal_path = match zbus::zvariant::ObjectPath::try_from(path) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, %path, "build SignalEmitter path failed");
            finalize_linger(conn.clone(), path.to_string()).await;
            return;
        }
    };
    if let Ok(emitter) = SignalEmitter::new(conn, signal_path) {
        let _ =
            PairingSessionIface::completed(&emitter, &device_id, &companion_name).await;
        if let Ok(mgr_emitter) = SignalEmitter::new(conn, crate::PATH_MANAGER) {
            let _ = crate::manager::Manager::device_connectivity_changed(
                &mgr_emitter,
                &device_id,
                "pairing",
            )
            .await;
        }
    }
    info!(%device_id, name = %companion_name, "pair session completed");
    finalize_linger(conn.clone(), path.to_string()).await;
}

async fn emit_property_changes(conn: &Connection, path: &str, names: &[&str]) {
    let object_path = match zbus::zvariant::ObjectPath::try_from(path) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, %path, "emit_property_changes: bad path");
            return;
        }
    };
    let iface_ref = match conn
        .object_server()
        .interface::<_, PairingSessionIface>(object_path)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, %path, "emit_property_changes: interface not registered");
            return;
        }
    };
    let emitter = iface_ref.signal_emitter();
    let guard = iface_ref.get().await;
    for name in names {
        match *name {
            "State" => {
                let _ = guard.state_changed(emitter).await;
            }
            "HostName" => {
                let _ = guard.host_name_changed(emitter).await;
            }
            "HostPubkeyHex" => {
                let _ = guard.host_pubkey_hex_changed(emitter).await;
            }
            "Address" => {
                let _ = guard.address_changed(emitter).await;
            }
            "Error" => {
                let _ = guard.error_changed(emitter).await;
            }
            other => warn!(%other, "emit_property_changes: unknown property"),
        }
    }
}
