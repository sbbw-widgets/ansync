//! Cable / ADB-bootstrapped pairing flow.
//!
//! The cable window itself is the security guarantee — MITM requires
//! physical access to the USB link, which we treat as the same trust
//! level as plugging in a keyboard. Wire format is the same versioned
//! `Envelope` used everywhere else; payload is `PairingMessage::Bootstrap*`.
//!
//! Step 16: ADB ops go through the pure-Rust [`adb_client`] crate
//! against the host's local `adbd` (still required — `adb_client`
//! speaks the ADB protocol, not the bare USB protocol). All blocking
//! calls run inside `tokio::task::spawn_blocking` so they don't stall
//! the runtime.

#[cfg(feature = "host")]
use std::net::SocketAddr;
#[cfg(feature = "host")]
use std::time::Duration;

#[cfg(feature = "host")]
use adb_client::{ADBDeviceExt, ADBServer, ADBServerDevice};
use ansync_core::{Capabilities, DeviceName, DevicePermissions};
use ansync_crypto::IdentityKeypair;
use ansync_proto::{
    Envelope, Message, PROTOCOL_VERSION, PairingMessage, read_envelope, write_envelope,
};
use tokio::io::{AsyncRead, AsyncWrite};
#[cfg(feature = "host")]
use tokio::net::TcpListener;
#[cfg(feature = "host")]
use tokio::time::timeout;

use crate::PairingError;
use crate::store::StoredPeer;

/// Maximum size of a single pairing envelope. The wire format here is
/// tiny — these messages only carry pubkey + name + cap bits.
const PAIRING_FRAME_MAX: usize = 4 * 1024;

/// Wait at most this long for the companion to connect after the cable
/// reverse has been set up. The companion side requires the user to
/// tap a heads-up notification (Android 14+ Background Activity
/// Launch restriction work-around), so the timeout has to cover
/// human reaction time — 60s is borderline if the user is mid-task.
#[cfg(feature = "host")]
const COMPANION_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Debug, Clone)]
pub struct AdbDevice {
    pub serial: String,
    pub state: String,
}

/// Companion's Android package id. Matched against `pm list packages`
/// to decide whether `pair_host_via_adb` should auto-install the APK
/// before triggering the pairing broadcast.
pub const COMPANION_PACKAGE: &str = "org.gameros.ansync";

/// Runtime ("dangerous") permissions declared by the companion that
/// the host grants automatically post-install via `pm grant`, so the
/// user never has to walk through individual runtime prompts. Each
/// entry corresponds to a `<uses-permission android:name="..."/>` in
/// `android/app/src/main/AndroidManifest.xml`.
///
/// **Maintenance rule** — every time the companion manifest gains a
/// new `dangerous` (a.k.a. runtime) permission, add the fully
/// qualified name to this array. Normal install-time permissions
/// (`INTERNET`, `WAKE_LOCK`, `FOREGROUND_SERVICE_*`, etc.) must NOT
/// be listed: `pm grant` returns a non-zero status on them and
/// pollutes the pair log. Special "appop" permissions
/// (`SYSTEM_ALERT_WINDOW`, `USE_FULL_SCREEN_INTENT`,
/// `REQUEST_IGNORE_BATTERY_OPTIMIZATIONS`) also don't belong here —
/// they need `appops set ... allow` or an explicit user dialog and
/// are surfaced through `SetupNotif` steps on the device.
pub const COMPANION_RUNTIME_PERMS: &[&str] = &[
    "android.permission.POST_NOTIFICATIONS",
    "android.permission.CAMERA",
    "android.permission.RECORD_AUDIO",
];

#[cfg(feature = "host")]
fn server() -> ADBServer {
    ADBServer::default()
}

/// Iterate [`COMPANION_RUNTIME_PERMS`] and run `pm grant` over the
/// adb_client crate (no shell-out to the `adb` CLI binary). Each
/// failure is logged at `warn!` but never aborts the pair flow —
/// the companion falls back to its in-app `SetupNotif` walkthrough
/// for whatever didn't get granted. Idempotent.
#[cfg(feature = "host")]
fn grant_runtime_perms(device: &mut ADBServerDevice) {
    let mut buf = Vec::with_capacity(64);
    for perm in COMPANION_RUNTIME_PERMS {
        buf.clear();
        if let Err(e) =
            device.shell_command(&["pm", "grant", COMPANION_PACKAGE, perm], &mut buf)
        {
            tracing::warn!(
                error = %e,
                perm = perm,
                "pm grant failed; SetupNotif will surface the unmet grant"
            );
        }
    }
}

#[cfg(feature = "host")]
fn pairing_err<E: std::fmt::Display>(ctx: &'static str, e: E) -> PairingError {
    PairingError::Protocol(format!("{ctx}: {e}"))
}

/// Drive the host side of the cable bootstrap over a single duplex
/// stream. Returns a freshly populated `StoredPeer` with default
/// permissions; capabilities are left empty and refreshed on first
/// successful connect over the regular control plane.
pub async fn bootstrap_host<S>(
    stream: &mut S,
    local: &IdentityKeypair,
    local_name: &str,
    lan_endpoints: Vec<(String, u16)>,
) -> Result<StoredPeer, PairingError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tracing::debug!("bootstrap_host: waiting for BootstrapHello");
    let envelope = read_envelope(stream, PAIRING_FRAME_MAX).await?;
    tracing::debug!("bootstrap_host: BootstrapHello received");
    let (peer_pubkey, peer_name) = match envelope.message {
        Message::Pairing(PairingMessage::BootstrapHello { identity_pubkey, name }) => {
            (identity_pubkey, name)
        }
        other => {
            return Err(PairingError::Protocol(format!(
                "expected BootstrapHello, got {other:?}"
            )));
        }
    };

    let ack = Envelope {
        version: PROTOCOL_VERSION,
        message: Message::Pairing(PairingMessage::BootstrapAck {
            identity_pubkey: local.public().as_bytes(),
            name: local_name.to_string(),
            lan_endpoints,
        }),
    };
    write_envelope(stream, &ack).await?;
    // Flush + half-close write so the kernel sends FIN after the Ack.
    // Without this, Tokio's `TcpStream` drop races the kernel's
    // adb-USB forwarder — the companion reads zero bytes (early EOF)
    // before the Ack ever crosses the wire.
    use tokio::io::AsyncWriteExt;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;

    Ok(StoredPeer::new(
        DeviceName(peer_name),
        peer_pubkey,
        Capabilities::empty(),
        DevicePermissions::default(),
    ))
}

/// Drive the companion (device) side of the cable bootstrap. Symmetric
/// to [`bootstrap_host`] — sends Hello, awaits Ack. Useful from tests
/// and from a future host-as-companion CLI mode.
/// Returned to the companion side after a successful cable bootstrap.
/// Wraps the standard [`StoredPeer`] plus the host's LAN endpoints
/// so the companion can persist them for direct-dial fallback.
#[derive(Debug, Clone)]
pub struct CompanionPairResult {
    pub peer: StoredPeer,
    pub lan_endpoints: Vec<(String, u16)>,
}

pub async fn bootstrap_companion<S>(
    stream: &mut S,
    local: &IdentityKeypair,
    local_name: &str,
) -> Result<CompanionPairResult, PairingError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let hello = Envelope {
        version: PROTOCOL_VERSION,
        message: Message::Pairing(PairingMessage::BootstrapHello {
            identity_pubkey: local.public().as_bytes(),
            name: local_name.to_string(),
        }),
    };
    write_envelope(stream, &hello).await?;
    use tokio::io::AsyncWriteExt;
    let _ = stream.flush().await;

    let envelope = read_envelope(stream, PAIRING_FRAME_MAX).await?;
    let (peer_pubkey, peer_name, lan_endpoints) = match envelope.message {
        Message::Pairing(PairingMessage::BootstrapAck { identity_pubkey, name, lan_endpoints }) => {
            (identity_pubkey, name, lan_endpoints)
        }
        other => {
            return Err(PairingError::Protocol(format!(
                "expected BootstrapAck, got {other:?}"
            )));
        }
    };

    Ok(CompanionPairResult {
        peer: StoredPeer::new(
            DeviceName(peer_name),
            peer_pubkey,
            Capabilities::empty(),
            DevicePermissions::default(),
        ),
        lan_endpoints,
    })
}

/// List ADB devices currently in the `device` state. Devices in
/// `unauthorized` or `offline` are filtered out — the user must accept
/// the USB-debugging prompt before they can pair.
#[cfg(feature = "host")]
pub async fn list_adb_devices() -> Result<Vec<AdbDevice>, PairingError> {
    tokio::task::spawn_blocking(|| {
        let mut srv = server();
        let raw = srv
            .devices()
            .map_err(|e| pairing_err("adb devices", e))?;
        Ok(raw
            .into_iter()
            .filter(|d| format!("{}", d.state).contains("device"))
            .map(|d| AdbDevice {
                serial: d.identifier,
                state: format!("{}", d.state),
            })
            .collect())
    })
    .await
    .map_err(|e| pairing_err("spawn_blocking devices", e))?
}

/// Full host-side cable pairing orchestration:
///
/// 1. Bind a TCP listener on `127.0.0.1` (port chosen by the OS).
/// 2. Tell the companion device (via `adb_client`) to reverse-forward
///    the same port back to the host's listener.
/// 3. Block waiting for the companion to dial in (bounded by
///    [`COMPANION_TIMEOUT`]).
/// 4. Drive [`bootstrap_host`] over the resulting stream.
/// 5. Tear the reverse mapping down regardless of outcome.
#[cfg(feature = "host")]
pub async fn pair_host_via_adb(
    serial: &str,
    local: &IdentityKeypair,
    local_name: &str,
    apk: Option<&std::path::Path>,
    lan_endpoints: Vec<(String, u16)>,
) -> Result<StoredPeer, PairingError> {
    if !companion_installed(serial).await? {
        let apk_path = apk.ok_or_else(|| {
            PairingError::Protocol(format!(
                "{COMPANION_PACKAGE} not installed on {serial} and no APK path supplied (use --apk or wire Step 17 auto-fetch)"
            ))
        })?;
        install_companion_apk(serial, apk_path).await?;
    }

    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
    let port = listener.local_addr()?.port();

    add_adb_reverse(serial, port).await?;
    if let Err(e) = trigger_companion_pair(serial, port, local_name).await {
        let _ = remove_adb_reverse(serial, port).await;
        return Err(e);
    }
    let result =
        wait_and_bootstrap(&listener, local, local_name, lan_endpoints).await;
    let _ = remove_adb_reverse(serial, port).await;
    result
}

/// Probe `pm list packages` for the companion. Returns
/// `Ok(true)` if installed, `Ok(false)` if absent.
///
/// adb_client's `shell_v2` transport interleaves stdout/stderr framing
/// bytes with the actual output, so a strict `line == "package:..."`
/// check misses real installs. We match on substring instead — the
/// fully qualified package name is unique enough that false positives
/// are not a realistic concern.
#[cfg(feature = "host")]
pub async fn companion_installed(serial: &str) -> Result<bool, PairingError> {
    let serial = serial.to_string();
    tokio::task::spawn_blocking(move || {
        let mut device = get_device(&serial)?;
        let mut buf = Vec::with_capacity(256);
        device
            .shell_command(
                &["pm", "list", "packages", COMPANION_PACKAGE],
                &mut buf,
            )
            .map_err(|e| pairing_err("pm list packages", e))?;
        let stdout = String::from_utf8_lossy(&buf);
        Ok(stdout.contains(&format!("package:{COMPANION_PACKAGE}")))
    })
    .await
    .map_err(|e| pairing_err("spawn_blocking pm list", e))?
}

/// Install the companion APK on the device. Replaces an existing
/// install if present.
#[cfg(feature = "host")]
pub async fn install_companion_apk(
    serial: &str,
    apk: &std::path::Path,
) -> Result<(), PairingError> {
    if !apk.exists() {
        return Err(PairingError::Protocol(format!(
            "APK not found at {}",
            apk.display()
        )));
    }
    let serial = serial.to_string();
    let apk = apk.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut device = get_device(&serial)?;
        device
            .install(&apk)
            .map_err(|e| pairing_err("adb install", e))?;
        grant_runtime_perms(&mut device);
        Ok(())
    })
    .await
    .map_err(|e| pairing_err("spawn_blocking install", e))?
}

#[cfg(feature = "host")]
async fn wait_and_bootstrap(
    listener: &TcpListener,
    local: &IdentityKeypair,
    local_name: &str,
    lan_endpoints: Vec<(String, u16)>,
) -> Result<StoredPeer, PairingError> {
    tracing::debug!("wait_and_bootstrap: listening for companion TCP connect");
    let accept = timeout(COMPANION_TIMEOUT, listener.accept())
        .await
        .map_err(|_| PairingError::Protocol("companion did not connect in time".into()))??;
    let (mut stream, peer) = accept;
    tracing::debug!("wait_and_bootstrap: companion connected from {peer}");
    bootstrap_host(&mut stream, local, local_name, lan_endpoints).await
}

#[cfg(feature = "host")]
async fn add_adb_reverse(serial: &str, port: u16) -> Result<(), PairingError> {
    // We talk to the local adb server (`127.0.0.1:5037`) directly
    // instead of shelling out to the `adb` binary OR using
    // `adb_client::ADBServerDevice::reverse(...)`.
    //
    // `adb_client` 2.1.19's `reverse()` reads the first OKAY (the
    // server's "request accepted") and returns. The reverse-forward
    // protocol requires a SECOND OKAY from adbd on the device after
    // the listener is actually bound — without it the host closes
    // the TCP connection before adbd finishes installing the
    // listener and the companion's `connect(127.0.0.1, port)`
    // ETIMEDOUTs.
    //
    // Driving the wire protocol manually is a few dozen lines and
    // keeps us free of `adb` CLI shell-outs.
    let serial = serial.to_string();
    tokio::task::spawn_blocking(move || {
        let mut stream = open_adbd()?;
        adb_send_cmd(&mut stream, &format!("host:transport:{serial}"))?;
        adb_send_cmd(&mut stream, &format!("reverse:forward:tcp:{port};tcp:{port}"))?;
        adb_read_status(&mut stream)?;
        Ok(())
    })
    .await
    .map_err(|e| pairing_err("spawn_blocking reverse", e))?
}

#[cfg(feature = "host")]
async fn trigger_companion_pair(
    serial: &str,
    port: u16,
    host_name: &str,
) -> Result<(), PairingError> {
    let serial = serial.to_string();
    let host_name = host_name.to_string();
    tokio::task::spawn_blocking(move || {
        let mut device = get_device(&serial)?;
        let mut buf = Vec::with_capacity(256);
        // Idempotent re-grant: covers (a) the user revoked a perm
        // mid-session and (b) the skip-install-on-version-match path
        // where `install_companion_apk` never ran.
        grant_runtime_perms(&mut device);
        device
            .shell_command(
                &[
                    "am",
                    "broadcast",
                    // Required when the companion has been installed
                    // but never opened (Android keeps the app in
                    // "stopped" state until the user launches it
                    // once, and stopped-state apps silently drop all
                    // broadcasts that don't carry this flag).
                    "--include-stopped-packages",
                    "-a",
                    "org.gameros.ansync.action.PAIR",
                    "-n",
                    "org.gameros.ansync/.PairingReceiver",
                    "--ei",
                    "port",
                    &port.to_string(),
                    "--es",
                    "name",
                    &host_name,
                ],
                &mut buf,
            )
            .map_err(|e| pairing_err("am broadcast PAIR", e))?;
        // `am broadcast` exits 0 even if the receiver is missing; the
        // stdout carries `Broadcast completed: result=0`. We don't try
        // to parse that — `wait_and_bootstrap` will time out if the
        // companion never connects, and that's the canonical error
        // surface.
        Ok(())
    })
    .await
    .map_err(|e| pairing_err("spawn_blocking broadcast", e))?
}

#[cfg(feature = "host")]
async fn remove_adb_reverse(serial: &str, _port: u16) -> Result<(), PairingError> {
    // Best-effort cleanup over the same raw adbd protocol path as
    // `add_adb_reverse`. Failures are swallowed so a hiccup here
    // doesn't shadow a successful pair.
    let serial = serial.to_string();
    let _ = tokio::task::spawn_blocking(move || -> Result<(), PairingError> {
        let mut stream = open_adbd()?;
        adb_send_cmd(&mut stream, &format!("host:transport:{serial}"))?;
        adb_send_cmd(&mut stream, "reverse:killforward-all")?;
        Ok(())
    })
    .await;
    Ok(())
}

/// Open a TCP connection to the local adb server. adbd doesn't
/// expose a Unix socket on Linux — `5037` over loopback is the
/// canonical surface.
#[cfg(feature = "host")]
fn open_adbd() -> Result<std::net::TcpStream, PairingError> {
    let port = std::env::var("ANDROID_ADB_SERVER_PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(5037);
    std::net::TcpStream::connect(("127.0.0.1", port))
        .map_err(|e| pairing_err("connect adb server", e))
}

/// Send a length-prefixed ASCII command and read the immediate
/// OKAY / FAIL response. adb's wire format: `%04x%s` where the
/// hex prefix is the body length.
#[cfg(feature = "host")]
fn adb_send_cmd(
    stream: &mut std::net::TcpStream,
    cmd: &str,
) -> Result<(), PairingError> {
    use std::io::Write;
    let req = format!("{:04x}{}", cmd.len(), cmd);
    stream
        .write_all(req.as_bytes())
        .map_err(|e| pairing_err("adb write", e))?;
    adb_read_status(stream)
}

/// Read a single 4-byte OKAY / FAIL status. On FAIL, read the
/// following hex-length-prefixed error body and surface it.
#[cfg(feature = "host")]
fn adb_read_status(stream: &mut std::net::TcpStream) -> Result<(), PairingError> {
    use std::io::Read;
    let mut buf = [0u8; 4];
    stream
        .read_exact(&mut buf)
        .map_err(|e| pairing_err("adb read status", e))?;
    match &buf {
        b"OKAY" => Ok(()),
        b"FAIL" => {
            let mut len_buf = [0u8; 4];
            let _ = stream.read_exact(&mut len_buf);
            let len = std::str::from_utf8(&len_buf)
                .ok()
                .and_then(|s| u32::from_str_radix(s, 16).ok())
                .unwrap_or(0) as usize;
            let mut msg = vec![0u8; len];
            let _ = stream.read_exact(&mut msg);
            Err(PairingError::Protocol(format!(
                "adb FAIL: {}",
                String::from_utf8_lossy(&msg)
            )))
        }
        other => Err(PairingError::Protocol(format!(
            "adb unexpected response: {other:?}"
        ))),
    }
}

#[cfg(feature = "host")]
fn get_device(serial: &str) -> Result<ADBServerDevice, PairingError> {
    let mut srv = server();
    srv.get_device_by_name(serial)
        .map_err(|e| pairing_err("get_device_by_name", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn bootstrap_roundtrip_over_duplex() {
        let host_id = IdentityKeypair::generate();
        let companion_id = IdentityKeypair::generate();
        let (mut host_stream, mut companion_stream) = duplex(8192);

        let host_pub = host_id.public().as_bytes();
        let companion_pub = companion_id.public().as_bytes();

        let host_id_for_task = host_id.clone();
        let companion_id_for_task = companion_id.clone();

        let host_task = tokio::spawn(async move {
            bootstrap_host(
                &mut host_stream,
                &host_id_for_task,
                "host-test",
                vec![("10.0.0.5".into(), 47000)],
            )
            .await
            .unwrap()
        });
        let companion_task = tokio::spawn(async move {
            bootstrap_companion(
                &mut companion_stream,
                &companion_id_for_task,
                "companion-test",
            )
            .await
            .unwrap()
        });

        let (h_peer, c_result) = tokio::join!(host_task, companion_task);
        let h_peer = h_peer.unwrap();
        let c_result = c_result.unwrap();

        assert_eq!(h_peer.pubkey, companion_pub);
        assert_eq!(h_peer.name.0, "companion-test");
        assert_eq!(c_result.peer.pubkey, host_pub);
        assert_eq!(c_result.peer.name.0, "host-test");
        assert_eq!(c_result.lan_endpoints, vec![("10.0.0.5".to_string(), 47000)]);
    }
}
