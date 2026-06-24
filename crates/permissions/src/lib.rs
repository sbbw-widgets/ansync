//! Per-device permission trait surface and helpers.
//!
//! The on-disk storage is owned by the daemon (`daemon-core::perms_backend`)
//! and lives next to the rest of the per-peer state in
//! `~/.local/share/ansync/peers/{id}.toml`. The daemon must check the
//! relevant flag before *every* capability-bound action; downstream
//! crates surface `Error::PermissionDenied(Permission)` when the check
//! fails.

use ansync_core::{DeviceId, DevicePermissions, Permission};
use async_trait::async_trait;

pub mod store;

pub use store::{apply_permission, parse_permission, permission_name, permission_value};

#[derive(Debug, thiserror::Error)]
pub enum PermissionsError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml decode: {0}")]
    TomlDecode(String),
    #[error("toml encode: {0}")]
    TomlEncode(String),
}

#[async_trait]
pub trait PermissionsStore: Send + Sync {
    async fn load(&self, id: &DeviceId) -> Result<DevicePermissions, PermissionsError>;
    async fn save(
        &self,
        id: &DeviceId,
        perms: &DevicePermissions,
    ) -> Result<(), PermissionsError>;
    async fn delete(&self, id: &DeviceId) -> Result<(), PermissionsError>;
    async fn check(
        &self,
        id: &DeviceId,
        permission: Permission,
    ) -> Result<bool, PermissionsError>;
}
