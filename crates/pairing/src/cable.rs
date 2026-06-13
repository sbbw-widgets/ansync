//! Cable / ADB-bootstrapped pairing flow.
//!
//! The cable window itself is the security guarantee — MITM requires
//! physical access to the USB link, which we treat as the same trust
//! level as plugging in a keyboard. Wire format is the same versioned
//! `Envelope` used everywhere else; payload is `PairingMessage::Bootstrap*`.

use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;

use ansync_core::{Capabilities, DeviceName, DevicePermissions};
use ansync_crypto::IdentityKeypair;
use ansync_proto::{
    Envelope, Message, PROTOCOL_VERSION, PairingMessage, read_envelope, write_envelope,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::time::timeout;

use crate::PairingError;
use crate::store::StoredPeer;

/// Maximum size of a single pairing envelope. The wire format here is
/// tiny — these messages only carry pubkey + name + cap bits.
const PAIRING_FRAME_MAX: usize = 4 * 1024;

/// Wait at most this long for the companion to connect after the cable
/// reverse has been set up.
const COMPANION_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct AdbDevice {
    pub serial: String,
    pub state: String,
}

/// Drive the host side of the cable bootstrap over a single duplex
/// stream. Returns a freshly populated `StoredPeer` with default
/// permissions; capabilities are left empty and refreshed on first
/// successful connect over the regular control plane.
pub async fn bootstrap_host<S>(
    stream: &mut S,
    local: &IdentityKeypair,
    local_name: &str,
) -> Result<StoredPeer, PairingError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let envelope = read_envelope(stream, PAIRING_FRAME_MAX).await?;
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
        }),
    };
    write_envelope(stream, &ack).await?;

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
pub async fn bootstrap_companion<S>(
    stream: &mut S,
    local: &IdentityKeypair,
    local_name: &str,
) -> Result<StoredPeer, PairingError>
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

    let envelope = read_envelope(stream, PAIRING_FRAME_MAX).await?;
    let (peer_pubkey, peer_name) = match envelope.message {
        Message::Pairing(PairingMessage::BootstrapAck { identity_pubkey, name }) => {
            (identity_pubkey, name)
        }
        other => {
            return Err(PairingError::Protocol(format!(
                "expected BootstrapAck, got {other:?}"
            )));
        }
    };

    Ok(StoredPeer::new(
        DeviceName(peer_name),
        peer_pubkey,
        Capabilities::empty(),
        DevicePermissions::default(),
    ))
}

/// List ADB devices currently in the `device` state. Devices in
/// `unauthorized` or `offline` are filtered out — the user must accept
/// the USB-debugging prompt before they can pair.
pub async fn list_adb_devices() -> Result<Vec<AdbDevice>, PairingError> {
    let output = Command::new("adb")
        .arg("devices")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(PairingError::Protocol(format!(
            "adb devices failed: {err}"
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut devices = Vec::new();
    for line in stdout.lines().skip(1) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let serial = match parts.next() {
            Some(s) => s.to_string(),
            None => continue,
        };
        let state = parts.next().unwrap_or("").to_string();
        if state == "device" {
            devices.push(AdbDevice { serial, state });
        }
    }
    Ok(devices)
}

/// Full host-side cable pairing orchestration:
///
/// 1. Bind a TCP listener on `127.0.0.1` (port chosen by the OS).
/// 2. Configure `adb -s <serial> reverse` to forward the same port on
///    the device back to the host's listener.
/// 3. Block waiting for the companion to dial in (bounded by
///    [`COMPANION_TIMEOUT`]).
/// 4. Drive [`bootstrap_host`] over the resulting stream.
/// 5. Tear the reverse mapping down regardless of outcome.
pub async fn pair_host_via_adb(
    serial: &str,
    local: &IdentityKeypair,
    local_name: &str,
) -> Result<StoredPeer, PairingError> {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
    let port = listener.local_addr()?.port();

    add_adb_reverse(serial, port).await?;
    // Wake the companion via broadcast so the user does not have to
    // open the app manually. The cable is the security guarantee —
    // no user prompt on the device side.
    if let Err(e) = trigger_companion_pair(serial, port, local_name).await {
        let _ = remove_adb_reverse(serial, port).await;
        return Err(e);
    }
    let result = wait_and_bootstrap(&listener, local, local_name).await;
    let _ = remove_adb_reverse(serial, port).await;
    result
}

async fn wait_and_bootstrap(
    listener: &TcpListener,
    local: &IdentityKeypair,
    local_name: &str,
) -> Result<StoredPeer, PairingError> {
    let accept = timeout(COMPANION_TIMEOUT, listener.accept())
        .await
        .map_err(|_| PairingError::Protocol("companion did not connect in time".into()))??;
    let (mut stream, _peer) = accept;
    bootstrap_host(&mut stream, local, local_name).await
}

async fn add_adb_reverse(serial: &str, port: u16) -> Result<(), PairingError> {
    let output = Command::new("adb")
        .args([
            "-s",
            serial,
            "reverse",
            &format!("tcp:{port}"),
            &format!("tcp:{port}"),
        ])
        .output()
        .await?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(PairingError::Protocol(format!(
            "adb reverse failed: {err}"
        )));
    }
    Ok(())
}

async fn trigger_companion_pair(
    serial: &str,
    port: u16,
    host_name: &str,
) -> Result<(), PairingError> {
    let output = Command::new("adb")
        .args([
            "-s",
            serial,
            "shell",
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
            host_name,
        ])
        .output()
        .await?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(PairingError::Protocol(format!(
            "adb shell am broadcast failed: {err}"
        )));
    }
    // `am broadcast` exits 0 even if the receiver is missing; the
    // stdout carries `Broadcast completed: result=0`. We don't try
    // to parse that — the host's `wait_and_bootstrap` will time out
    // if the companion never connects, and that's the canonical
    // error surface.
    Ok(())
}

async fn remove_adb_reverse(serial: &str, port: u16) -> Result<(), PairingError> {
    let output = Command::new("adb")
        .args(["-s", serial, "reverse", "--remove", &format!("tcp:{port}")])
        .output()
        .await?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(PairingError::Protocol(format!(
            "adb reverse --remove failed: {err}"
        )));
    }
    Ok(())
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
            bootstrap_host(&mut host_stream, &host_id_for_task, "host-test")
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

        let (h_peer, c_peer) = tokio::join!(host_task, companion_task);
        let h_peer = h_peer.unwrap();
        let c_peer = c_peer.unwrap();

        assert_eq!(h_peer.pubkey, companion_pub);
        assert_eq!(h_peer.name.0, "companion-test");
        assert_eq!(c_peer.pubkey, host_pub);
        assert_eq!(c_peer.name.0, "host-test");
    }
}
