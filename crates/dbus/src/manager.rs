//! `/org/gameros/Ansync1/Manager` interface.

use std::sync::Arc;

use ansync_core::DeviceId;
use zbus::interface;

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
}
