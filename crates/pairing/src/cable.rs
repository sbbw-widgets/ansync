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

#[cfg(feature = "host")]
fn server() -> ADBServer {
    ADBServer::default()
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
    // adb_client 2.1.x ships a `reverse(...)` that sends the right
    // wire bytes but never actually installs the listener on the
    // device's adbd (verified empirically: the `adb reverse --list`
    // mapping shows up host-side but the device never opens a
    // matching `LISTEN` socket, so the companion's `connect("127.0.0.1",
    // port)` ETIMEDOUTs). Until the upstream bug is fixed we shell out
    // to the official `adb` binary — `Step 16` removed adb-stdout
    // *parsing*, not adb-binary usage; reverse has no stdout to parse
    // beyond an exit code so this stays clean.
    let serial = serial.to_string();
    tokio::task::spawn_blocking(move || {
        let output = std::process::Command::new("adb")
            .args(["-s", &serial, "reverse", &format!("tcp:{port}"), &format!("tcp:{port}")])
            .output()
            .map_err(|e| pairing_err("spawn adb reverse", e))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(PairingError::Protocol(format!(
                "adb reverse exited {}: {}",
                output.status, stderr.trim()
            )));
        }
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
        device
            .shell_command(
                &[
                    "am",
                    "broadcast",
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
    // Mirror of `add_adb_reverse`: shell out until adb_client's
    // reverse impl is fixed upstream. Failure here is best-effort —
    // we still return Ok so the pair-success path isn't shadowed by
    // a cleanup hiccup.
    let serial = serial.to_string();
    let _ = tokio::task::spawn_blocking(move || {
        let _ = std::process::Command::new("adb")
            .args(["-s", &serial, "reverse", "--remove-all"])
            .output();
    })
    .await;
    Ok(())
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
