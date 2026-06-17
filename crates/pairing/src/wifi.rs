//! WiFi-PIN pairing flow.
//!
//! Threat model: companion and host share the same LAN; an attacker on
//! the same LAN may try to MITM the TCP socket. The companion-displayed
//! PIN binds the bootstrap transcript to a human-verified shared secret
//! — both sides exchange a SHA-256 MAC over `("ansync-pair-v1" || role
//! || host_pk || companion_pk || pin)` (see [`ansync_crypto::pair_pin`]).
//! A guess against the 6-digit PIN succeeds with probability 10⁻⁶ per
//! attempt; the companion-side listener enforces a 3-strike lockout
//! before rotating the PIN.
//!
//! Wire layout (TCP, length-prefixed postcard envelopes, same framing
//! as [`crate::cable`]):
//!
//! ```text
//!   host       →  BootstrapHello{host_pk, host_name}        →  companion
//!   companion  →  (generate PIN, post heads-up notif on Android)
//!   companion  →  BootstrapAck{companion_pk, companion_name, lan_endpoints=[]} →  host
//!   host       →  PinConfirm{mac_h}                         →  companion
//!   companion  →  PinConfirm{mac_c}                         →  host
//!   (TCP FIN)
//! ```
//!
//! The two-phase split (`read_pair_hello` → `respond_pair_pin`) lets
//! the companion process the inbound Hello, generate a PIN, surface
//! it on the OS notification shade, and only then drive the Ack +
//! MAC exchange. Phase boundaries are clean enough that the companion
//! can loop on `BadPin` by re-running `respond_pair_pin` on a fresh
//! TCP connection without leaking PIN-derived state.

use ansync_core::{Capabilities, DeviceName, DevicePermissions};
use ansync_crypto::{IdentityKeypair, PinRole, pair_pin_confirm, verify_pin_confirm};
use ansync_proto::{
    Envelope, Message, PROTOCOL_VERSION, PairingMessage, read_envelope, write_envelope,
};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::PairingError;
use crate::store::StoredPeer;

const PAIRING_FRAME_MAX: usize = 4 * 1024;

/// mDNS service type the companion advertises while a pair listener is
/// running inside its foreground service. Hosts browse this subtype
/// during `ansyncctl pair` (no flag) to find pair-ready devices on the
/// LAN; companion never stops advertising — re-pair is always allowed.
pub const PAIR_MDNS_SERVICE_TYPE: &str = "_ansync-pair._tcp.local.";

/// TXT key the companion fills with the lowercase hex of its Ed25519
/// pubkey so the host can dedupe between mDNS replies before pairing.
pub const PAIR_MDNS_TXT_PUBKEY: &str = "id";
/// TXT key holding the companion's display name (`Build.MANUFACTURER
/// Build.MODEL`).
pub const PAIR_MDNS_TXT_NAME: &str = "name";

/// Drive the host side of the WiFi-PIN handshake over an already-
/// connected duplex stream (typically a [`tokio::net::TcpStream`] from
/// `pair_host_via_wifi`, but generic so the protocol can be exercised
/// over `tokio::io::duplex` in tests).
pub async fn bootstrap_host_wifi<S>(
    stream: &mut S,
    local: &IdentityKeypair,
    local_name: &str,
    pin: &[u8; 6],
) -> Result<StoredPeer, PairingError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let host_pk = local.public().as_bytes();
    let hello = Envelope {
        version: PROTOCOL_VERSION,
        message: Message::Pairing(PairingMessage::BootstrapHello {
            identity_pubkey: host_pk,
            name: local_name.to_string(),
        }),
    };
    write_envelope(stream, &hello).await?;

    let envelope = read_envelope(stream, PAIRING_FRAME_MAX).await?;
    let (companion_pk, companion_name) = match envelope.message {
        Message::Pairing(PairingMessage::BootstrapAck { identity_pubkey, name, .. }) => {
            (identity_pubkey, name)
        }
        other => {
            return Err(PairingError::Protocol(format!(
                "expected BootstrapAck, got {other:?}"
            )));
        }
    };

    let host_mac = pair_pin_confirm(pin, PinRole::Host, &host_pk, &companion_pk);
    let send_mac = Envelope {
        version: PROTOCOL_VERSION,
        message: Message::Pairing(PairingMessage::PinConfirm { mac: host_mac }),
    };
    write_envelope(stream, &send_mac).await?;

    let envelope = read_envelope(stream, PAIRING_FRAME_MAX).await?;
    let companion_mac = match envelope.message {
        Message::Pairing(PairingMessage::PinConfirm { mac }) => mac,
        other => {
            return Err(PairingError::Protocol(format!(
                "expected PinConfirm, got {other:?}"
            )));
        }
    };
    if !verify_pin_confirm(
        &companion_mac,
        pin,
        PinRole::Companion,
        &host_pk,
        &companion_pk,
    ) {
        return Err(PairingError::Protocol(
            "companion PIN mac mismatch — wrong PIN or MITM on the LAN".into(),
        ));
    }

    use tokio::io::AsyncWriteExt;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;

    Ok(StoredPeer::new(
        DeviceName(companion_name),
        companion_pk,
        Capabilities::empty(),
        DevicePermissions::default(),
    ))
}

/// Phase 1 of the companion-side bootstrap: read the host's
/// [`PairingMessage::BootstrapHello`]. Companion uses the returned
/// `(host_pk, host_name)` to (a) compute the local PIN seed and (b)
/// populate the OS heads-up notification before continuing.
pub async fn read_pair_hello<S>(stream: &mut S) -> Result<([u8; 32], String), PairingError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let envelope = read_envelope(stream, PAIRING_FRAME_MAX).await?;
    match envelope.message {
        Message::Pairing(PairingMessage::BootstrapHello { identity_pubkey, name }) => {
            Ok((identity_pubkey, name))
        }
        other => Err(PairingError::Protocol(format!(
            "expected BootstrapHello, got {other:?}"
        ))),
    }
}

/// Outcome of a single companion-side bootstrap attempt. The companion
/// activity loops on `BadPin` (up to the 3-strike lockout) and exits on
/// `Ok` or `Err`.
#[derive(Debug)]
pub enum CompanionWifiOutcome {
    Ok(StoredPeer),
    /// PIN MAC from the host did not match what we computed locally.
    /// The companion service should keep the listener open, post a
    /// "wrong PIN" notif, and accept the next connection until the
    /// lockout counter trips.
    BadPin,
}

/// Phase 2 of the companion-side bootstrap: send Ack, read the host's
/// `PinConfirm`, verify the MAC under the visible PIN, send our own
/// `PinConfirm` back, and return the [`StoredPeer`] for persistence.
pub async fn respond_pair_pin<S>(
    stream: &mut S,
    local: &IdentityKeypair,
    local_name: &str,
    host_pk: &[u8; 32],
    host_name: &str,
    pin: &[u8; 6],
) -> Result<CompanionWifiOutcome, PairingError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let companion_pk = local.public().as_bytes();
    let ack = Envelope {
        version: PROTOCOL_VERSION,
        message: Message::Pairing(PairingMessage::BootstrapAck {
            identity_pubkey: companion_pk,
            name: local_name.to_string(),
            lan_endpoints: Vec::new(),
        }),
    };
    write_envelope(stream, &ack).await?;

    let envelope = read_envelope(stream, PAIRING_FRAME_MAX).await?;
    let host_mac = match envelope.message {
        Message::Pairing(PairingMessage::PinConfirm { mac }) => mac,
        other => {
            return Err(PairingError::Protocol(format!(
                "expected PinConfirm, got {other:?}"
            )));
        }
    };
    if !verify_pin_confirm(&host_mac, pin, PinRole::Host, host_pk, &companion_pk) {
        return Ok(CompanionWifiOutcome::BadPin);
    }

    let companion_mac = pair_pin_confirm(pin, PinRole::Companion, host_pk, &companion_pk);
    let reply = Envelope {
        version: PROTOCOL_VERSION,
        message: Message::Pairing(PairingMessage::PinConfirm { mac: companion_mac }),
    };
    write_envelope(stream, &reply).await?;

    use tokio::io::AsyncWriteExt;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;

    Ok(CompanionWifiOutcome::Ok(StoredPeer::new(
        DeviceName(host_name.to_string()),
        *host_pk,
        Capabilities::empty(),
        DevicePermissions::default(),
    )))
}

/// Convenience wrapper exposing the single-shot companion bootstrap
/// for tests. Production companion calls [`read_pair_hello`] /
/// [`respond_pair_pin`] separately so it can surface the PIN through
/// the OS notif system in between.
pub async fn bootstrap_companion_wifi<S>(
    stream: &mut S,
    local: &IdentityKeypair,
    local_name: &str,
    pin: &[u8; 6],
) -> Result<CompanionWifiOutcome, PairingError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (host_pk, host_name) = read_pair_hello(stream).await?;
    respond_pair_pin(stream, local, local_name, &host_pk, &host_name, pin).await
}

/// Convenience wrapper for the desktop / CLI: dial `addr` over TCP and
/// drive [`bootstrap_host_wifi`]. Returns the persisted-ready
/// [`StoredPeer`] (caller writes it into the `PeerStore`).
#[cfg(feature = "host")]
pub async fn pair_host_via_wifi(
    addr: std::net::SocketAddr,
    pin: &[u8; 6],
    identity: &IdentityKeypair,
    local_name: &str,
) -> Result<StoredPeer, PairingError> {
    use tokio::net::TcpStream;
    use tokio::time::{Duration, timeout};
    let mut stream = timeout(Duration::from_secs(10), TcpStream::connect(addr))
        .await
        .map_err(|_| PairingError::Protocol(format!("connect to {addr} timed out")))??;
    bootstrap_host_wifi(&mut stream, identity, local_name, pin).await
}

/// Pair-ready companion discovered over mDNS.
#[cfg(feature = "host")]
#[derive(Debug, Clone)]
pub struct PairCandidate {
    pub addr: std::net::SocketAddr,
    pub pubkey: [u8; 32],
    pub name: String,
}

/// Long-lived presence event from [`watch_pair_candidates`]. Resolves
/// to a continuously-updated picture of "which paired companions are
/// on the LAN right now" — daemon uses it to emit
/// `Manager.DeviceReachable` signals so widgets can paint green / red
/// without waiting for the companion to actually dial in.
#[cfg(feature = "host")]
#[derive(Debug, Clone)]
pub enum PairWatchEvent {
    /// A companion advertised its pair-ready listener.
    Resolved(PairCandidate),
    /// A previously-resolved companion stopped advertising — typically
    /// because the device dropped off Wi-Fi or the service was torn
    /// down. The `instance` is the mDNS instance name (typically
    /// `ansync-<Build.MODEL>`).
    Removed(String),
}

/// Open a long-lived mDNS browser for [`PAIR_MDNS_SERVICE_TYPE`]
/// returning a tokio `UnboundedReceiver` of [`PairWatchEvent`]. The
/// returned [`mdns_sd::ServiceDaemon`] handle keeps the browser alive;
/// dropping it shuts the browser down.
#[cfg(feature = "host")]
pub fn watch_pair_candidates() -> Result<
    (
        mdns_sd::ServiceDaemon,
        tokio::sync::mpsc::UnboundedReceiver<PairWatchEvent>,
    ),
    PairingError,
> {
    use mdns_sd::{ServiceDaemon, ServiceEvent};

    let daemon = ServiceDaemon::new()
        .map_err(|e| PairingError::Protocol(format!("mdns daemon: {e}")))?;
    let receiver = daemon
        .browse(PAIR_MDNS_SERVICE_TYPE)
        .map_err(|e| PairingError::Protocol(format!("mdns browse: {e}")))?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok(event) = receiver.recv_async().await {
            match event {
                ServiceEvent::ServiceResolved(info) => {
                    let Some(pubkey_hex) = info.get_property_val_str(PAIR_MDNS_TXT_PUBKEY) else {
                        continue;
                    };
                    let Some(name) = info.get_property_val_str(PAIR_MDNS_TXT_NAME) else {
                        continue;
                    };
                    let Some(pubkey) = parse_pubkey_hex(pubkey_hex) else {
                        continue;
                    };
                    let port = info.get_port();
                    let Some(&ip) = info.get_addresses().iter().next() else {
                        continue;
                    };
                    let addr = std::net::SocketAddr::new(ip, port);
                    if tx
                        .send(PairWatchEvent::Resolved(PairCandidate {
                            addr,
                            pubkey,
                            name: name.to_string(),
                        }))
                        .is_err()
                    {
                        break;
                    }
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    if tx.send(PairWatchEvent::Removed(fullname)).is_err() {
                        break;
                    }
                }
                _ => continue,
            }
        }
    });
    Ok((daemon, rx))
}

/// Browse the LAN for companions advertising [`PAIR_MDNS_SERVICE_TYPE`].
/// Blocks (cooperatively) for `timeout` and returns one entry per
/// resolved pubkey (deduped — companions answer on every NIC).
///
/// This is a "fire and collect" helper for `ansyncctl pair` — it owns
/// the mdns-sd daemon for the duration of the call so callers don't
/// have to deal with backpressure or stream cancellation.
#[cfg(feature = "host")]
pub async fn browse_pair_candidates(
    timeout: std::time::Duration,
) -> Result<Vec<PairCandidate>, PairingError> {
    use std::collections::HashMap;
    use mdns_sd::{ServiceDaemon, ServiceEvent};

    let daemon = ServiceDaemon::new()
        .map_err(|e| PairingError::Protocol(format!("mdns daemon: {e}")))?;
    let receiver = daemon
        .browse(PAIR_MDNS_SERVICE_TYPE)
        .map_err(|e| PairingError::Protocol(format!("mdns browse: {e}")))?;

    let mut found: HashMap<[u8; 32], PairCandidate> = HashMap::new();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = match deadline.checked_duration_since(tokio::time::Instant::now()) {
            Some(r) => r,
            None => break,
        };
        let event = match tokio::time::timeout(remaining, receiver.recv_async()).await {
            Ok(Ok(ev)) => ev,
            Ok(Err(_)) => break,
            Err(_) => break,
        };
        if let ServiceEvent::ServiceResolved(info) = event {
            let Some(pubkey_hex) = info.get_property_val_str(PAIR_MDNS_TXT_PUBKEY) else {
                continue;
            };
            let Some(name) = info.get_property_val_str(PAIR_MDNS_TXT_NAME) else {
                continue;
            };
            let Some(pubkey) = parse_pubkey_hex(pubkey_hex) else {
                continue;
            };
            let port = info.get_port();
            // mDNS replies carry every NIC's address. Pick the one
            // the kernel can actually connect to: IPv4 first (simpler
            // routing), then IPv6 global. IPv6 link-local needs a
            // scope id that `SocketAddr` doesn't carry, so a
            // `connect` against fe80:: returns EINVAL; APIPA
            // (169.254/16) means DHCP never assigned a real lease and
            // the peer isn't actually reachable.
            let Some(ip) = info
                .get_addresses()
                .iter()
                .copied()
                .filter(|ip| !is_unreachable_pair_addr(*ip))
                .min_by_key(|ip| pair_addr_rank(*ip))
            else {
                continue;
            };
            let addr = std::net::SocketAddr::new(ip, port);
            // Dedup by pubkey but only overwrite when the new
            // candidate has a strictly better rank — otherwise a
            // late link-local reply from the same peer would clobber
            // a usable IPv4 we already picked.
            let new_rank = pair_addr_rank(addr.ip());
            match found.get(&pubkey) {
                Some(existing) if pair_addr_rank(existing.addr.ip()) <= new_rank => {}
                _ => {
                    found.insert(
                        pubkey,
                        PairCandidate {
                            addr,
                            pubkey,
                            name: name.to_string(),
                        },
                    );
                }
            }
        }
    }
    let _ = daemon.shutdown();
    Ok(found.into_values().collect())
}

#[cfg(feature = "host")]
fn is_unreachable_pair_addr(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_link_local() || v4.is_unspecified() || v4.is_loopback(),
        std::net::IpAddr::V6(v6) => {
            // fe80::/10 — link-local, needs scope id we can't carry.
            // ::1 + unspecified — pointless to dial.
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

#[cfg(feature = "host")]
fn pair_addr_rank(ip: std::net::IpAddr) -> u8 {
    match ip {
        std::net::IpAddr::V4(_) => 0,
        std::net::IpAddr::V6(_) => 1,
    }
}

#[cfg(feature = "host")]
fn parse_pubkey_hex(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let utf8 = std::str::from_utf8(chunk).ok()?;
        out[i] = u8::from_str_radix(utf8, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn wifi_roundtrip_correct_pin() {
        let host_id = IdentityKeypair::generate();
        let companion_id = IdentityKeypair::generate();
        let (mut host_s, mut companion_s) = duplex(8192);
        let pin = *b"424242";

        let host_pub = host_id.public().as_bytes();
        let companion_pub = companion_id.public().as_bytes();

        let h = tokio::spawn(async move {
            bootstrap_host_wifi(&mut host_s, &host_id, "host-test", &pin)
                .await
                .unwrap()
        });
        let c = tokio::spawn(async move {
            bootstrap_companion_wifi(&mut companion_s, &companion_id, "companion-test", &pin)
                .await
                .unwrap()
        });
        let (host_peer, companion_out) = tokio::join!(h, c);
        let host_peer = host_peer.unwrap();
        let companion_out = companion_out.unwrap();
        match companion_out {
            CompanionWifiOutcome::Ok(p) => {
                assert_eq!(p.pubkey, host_pub);
                assert_eq!(p.name.0, "host-test");
            }
            CompanionWifiOutcome::BadPin => panic!("matching PIN rejected"),
        }
        assert_eq!(host_peer.pubkey, companion_pub);
        assert_eq!(host_peer.name.0, "companion-test");
    }

    #[tokio::test]
    async fn wifi_wrong_pin_rejected_on_companion() {
        let host_id = IdentityKeypair::generate();
        let companion_id = IdentityKeypair::generate();
        let (mut host_s, mut companion_s) = duplex(8192);

        let h = tokio::spawn(async move {
            bootstrap_host_wifi(&mut host_s, &host_id, "host-test", b"111111").await
        });
        let c = tokio::spawn(async move {
            bootstrap_companion_wifi(
                &mut companion_s,
                &companion_id,
                "companion-test",
                b"222222",
            )
            .await
        });
        let (host_res, companion_res) = tokio::join!(h, c);
        let companion_res = companion_res.unwrap();
        assert!(matches!(companion_res, Ok(CompanionWifiOutcome::BadPin)));
        let host_res = host_res.unwrap();
        assert!(host_res.is_err(), "host should fail when companion drops");
    }

    #[tokio::test]
    async fn two_phase_split_matches_single_shot() {
        let host_id = IdentityKeypair::generate();
        let companion_id = IdentityKeypair::generate();
        let (mut host_s, mut companion_s) = duplex(8192);
        let pin = *b"999999";

        let h = tokio::spawn(async move {
            bootstrap_host_wifi(&mut host_s, &host_id, "host-test", &pin)
                .await
                .unwrap()
        });
        let c = tokio::spawn(async move {
            let (host_pk, host_name) = read_pair_hello(&mut companion_s).await.unwrap();
            respond_pair_pin(
                &mut companion_s,
                &companion_id,
                "companion-test",
                &host_pk,
                &host_name,
                &pin,
            )
            .await
            .unwrap()
        });
        let (host_peer, companion_out) = tokio::join!(h, c);
        let host_peer = host_peer.unwrap();
        assert!(matches!(companion_out.unwrap(), CompanionWifiOutcome::Ok(_)));
        assert_eq!(host_peer.name.0, "companion-test");
    }
}
