//! `/org/gameros/Ansync1/Permissions/{id}` interface.

use std::sync::Arc;

use ansync_core::{DeviceId, DevicePermissions};
use ansync_permissions::{apply_permission, parse_permission};
use zbus::interface;

use crate::state::DaemonState;

#[derive(Clone)]
pub struct PermissionsIface {
    pub id: DeviceId,
    pub state: Arc<DaemonState>,
}

fn unknown_flag(flag: &str) -> zbus::fdo::Error {
    zbus::fdo::Error::InvalidArgs(format!("unknown permission flag: {flag}"))
}

#[interface(name = "org.gameros.Ansync1.Permissions")]
impl PermissionsIface {
    async fn get(&self, flag: String) -> zbus::fdo::Result<bool> {
        let permission = parse_permission(&flag).ok_or_else(|| unknown_flag(&flag))?;
        self.state
            .permissions
            .check(&self.id, permission)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    async fn set(&self, flag: String, value: bool) -> zbus::fdo::Result<()> {
        let permission = parse_permission(&flag).ok_or_else(|| unknown_flag(&flag))?;
        let mut perms = self
            .state
            .permissions
            .load(&self.id)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        apply_permission(&mut perms, permission, value);
        self.state
            .permissions
            .save(&self.id, &perms)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        Ok(())
    }

    async fn reset(&self) -> zbus::fdo::Result<()> {
        self.state
            .permissions
            .save(&self.id, &DevicePermissions::default())
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        Ok(())
    }
}
