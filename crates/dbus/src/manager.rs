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

    async fn start_pairing(&self, _method: String) -> zbus::fdo::Result<String> {
        Err(zbus::fdo::Error::NotSupported(
            "StartPairing over D-Bus lands in a later step; use `ansyncctl pair` for now"
                .into(),
        ))
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
