//! `/org/gameros/Ansync1/Manager` interface.

use std::sync::Arc;

use ansync_core::DeviceId;
use zbus::interface;

use crate::register_device;
use crate::state::DaemonState;
use crate::util::parse_device_id;

#[derive(Clone)]
pub struct Manager {
    pub state: Arc<DaemonState>,
}

#[interface(name = "org.gameros.Ansync1.Manager")]
impl Manager {
    async fn list_devices(&self) -> Vec<String> {
        self.state
            .peers
            .list()
            .map(|peers| peers.into_iter().map(|p| p.id.to_string()).collect())
            .unwrap_or_default()
    }

    async fn forget_device(&self, id: String) -> zbus::fdo::Result<()> {
        let device_id: DeviceId = parse_device_id(&id)
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs(format!("bad device id: {id}")))?;
        self.state
            .peers
            .remove(&device_id)
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        self.state
            .permissions
            .delete(&device_id)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        Ok(())
    }

    /// Browse the LAN for companions advertising
    /// `_ansync-pair._tcp.local.`. Returns one entry per resolved
    /// pubkey (deduplicated across NIC replies). `seconds = 0` falls
    /// back to a 5 s budget.
    ///
    /// The widget calls this to populate a "devices nearby" list and
    /// then dispatches the user's pick to [`Self::start_pairing`].
    #[zbus(name = "BrowseAvailable")]
    async fn browse_available(
        &self,
        seconds: u32,
    ) -> zbus::fdo::Result<Vec<(String, String, String)>> {
        let secs = if seconds == 0 { 5 } else { seconds as u64 };
        let timeout = std::time::Duration::from_secs(secs);
        let found = ansync_pairing::browse_pair_candidates(timeout)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("mdns browse: {e}")))?;
        Ok(found
            .into_iter()
            .map(|c| (c.name, c.addr.to_string(), hex::encode(c.pubkey)))
            .collect())
    }

    /// Kick off a WiFi-PIN pair against `addr` (`ip:port`). Returns
    /// the object path of a fresh
    /// `org.gameros.Ansync1.PairingSession` — the caller subscribes to
    /// its `PropertiesChanged` for the `State` transition into
    /// `awaiting_pin`, prompts the user, and dispatches the typed PIN
    /// via `SubmitPin`.
    ///
    /// `expected_pubkey_hex` is optional: when non-empty (typically
    /// populated from a prior `BrowseAvailable` result) the worker
    /// rejects the session if the pubkey carried in `BootstrapAck`
    /// does not match. Set to empty string to skip the check (e.g.
    /// `--remote-addr` style manual entry).
    #[zbus(name = "StartPairing")]
    async fn start_pairing(
        &self,
        addr: String,
        expected_pubkey_hex: String,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<zbus::zvariant::ObjectPath<'static>> {
        let socket: std::net::SocketAddr = addr.parse().map_err(|e| {
            zbus::fdo::Error::InvalidArgs(format!("addr must be `ip:port`: {e}"))
        })?;
        let expected_pubkey = if expected_pubkey_hex.is_empty() {
            None
        } else {
            let bytes = hex::decode(&expected_pubkey_hex).map_err(|e| {
                zbus::fdo::Error::InvalidArgs(format!("expected_pubkey_hex: {e}"))
            })?;
            if bytes.len() != 32 {
                return Err(zbus::fdo::Error::InvalidArgs(format!(
                    "expected_pubkey_hex must be 64 hex chars, got {}",
                    bytes.len() * 2
                )));
            }
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&bytes);
            Some(arr)
        };

        let session_id = uuid::Uuid::new_v4().simple().to_string();
        let path = crate::pair::path_pair_session(&session_id);
        let object_path = zbus::zvariant::ObjectPath::try_from(path.as_str())
            .map_err(|e| zbus::fdo::Error::Failed(format!("bad session path: {e}")))?;

        let (snapshot, pin_tx, pin_rx, cancel_tx, cancel_rx) =
            crate::pair::allocate();
        snapshot
            .lock()
            .expect("snapshot poisoned")
            .address = socket.to_string();

        let iface = crate::pair::PairingSessionIface {
            id: session_id.clone(),
            snapshot: snapshot.clone(),
            pin_tx,
            cancel_tx,
        };
        conn.object_server()
            .at(object_path.clone(), iface)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("register session: {e}")))?;

        crate::pair::spawn_session(
            conn.clone(),
            self.state.clone(),
            session_id,
            socket,
            expected_pubkey,
            snapshot,
            pin_rx,
            cancel_rx,
        );

        Ok(object_path.into_owned())
    }

    /// Fired by `daemon-core` whenever a peer transitions through the
    /// connectivity lifecycle (`disconnected | pairing | authenticated
    /// | active`). Subscribers (DMS widget, ansyncctl status) listen
    /// here for a single fan-out path instead of subscribing per
    /// `/Device/{id}`.
    #[zbus(signal)]
    pub async fn device_connectivity_changed(
        ctxt: &zbus::object_server::SignalEmitter<'_>,
        id: &str,
        state: &str,
    ) -> zbus::Result<()>;

    /// Return the (ip, port) pairs the QUIC listener is reachable on
    /// across non-loopback interfaces. `ansyncctl pair` queries this
    /// before kicking the cable bootstrap so the host can hand the
    /// companion a direct-dial fallback (used when Wi-Fi AP isolation
    /// blocks mDNS multicast).
    #[zbus(name = "ListenEndpoints")]
    async fn listen_endpoints(&self) -> Vec<(String, u16)> {
        self.state
            .listen_endpoints
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    /// Re-scan the PeerStore and register Device/Permissions
    /// interfaces for any peer that doesn't already have one. Called
    /// by `ansyncctl pair` immediately after persisting a freshly
    /// paired peer so the new `/org/gameros/Ansync1/Device/{id}` path
    /// becomes addressable without restarting the daemon.
    ///
    /// Returns the list of newly registered device ids.
    #[zbus(name = "RefreshPeers")]
    async fn refresh_peers(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<Vec<String>> {
        let peers = self
            .state
            .peers
            .list()
            .map_err(|e| zbus::fdo::Error::Failed(format!("list peers: {e}")))?;
        let mut added = Vec::new();
        for peer in peers {
            let path = crate::path_device(&peer.id);
            let object_path = match zbus::zvariant::ObjectPath::try_from(path.as_str()) {
                Ok(p) => p,
                Err(e) => {
                    return Err(zbus::fdo::Error::Failed(format!("bad path {path}: {e}")));
                }
            };
            // `object_server().at(...)` errors if already registered;
            // probing first via `interface::<Device>()` avoids the
            // noisy error path and keeps the call idempotent.
            let already = conn
                .object_server()
                .interface::<_, crate::Device>(object_path)
                .await
                .is_ok();
            if already {
                continue;
            }
            register_device(conn, &self.state, peer.id.clone())
                .await
                .map_err(|e| zbus::fdo::Error::Failed(format!("register: {e}")))?;
            added.push(peer.id.to_string());
        }
        Ok(added)
    }
}
