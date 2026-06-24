//! `PermissionsStore` impl backed by the existing [`PeerStore`].
//!
//! The peer toml in `~/.local/share/ansync/peers/{id}.toml` already
//! carries a `permissions` block (it ships with the [`StoredPeer`]
//! record so the user only has one place to look). This impl wires
//! the trait through to that record — no separate storage tree, no
//! XDG_CONFIG_HOME duplication.
//!
//! Reads / writes are routed through the same atomic
//! tmp-write + rename path the `PeerStore` already uses. We do *not*
//! cache: every `check()` re-reads the toml. That matches the prior
//! `FilePermissionsStore` behavior — the I/O cost (~one syscall +
//! ~1 KB parse) is bounded and lets external edits to the toml take
//! effect without a daemon restart.

use ansync_core::{DeviceId, DevicePermissions, Permission};
use ansync_pairing::PeerStore;
use ansync_permissions::{PermissionsError, PermissionsStore, permission_value};
use async_trait::async_trait;

pub struct PeerStorePermissions {
    peers: PeerStore,
}

impl PeerStorePermissions {
    pub fn new(peers: PeerStore) -> Self {
        Self { peers }
    }
}

#[async_trait]
impl PermissionsStore for PeerStorePermissions {
    async fn load(&self, id: &DeviceId) -> Result<DevicePermissions, PermissionsError> {
        match self.peers.get(id) {
            Ok(peer) => Ok(peer.permissions),
            // No peer record yet — treat the same as "never persisted",
            // i.e. hand back defaults so the daemon can decide what to
            // do (typically: fall through to a deny since no peer can
            // legitimately act before pairing).
            Err(_) => Ok(DevicePermissions::default()),
        }
    }

    async fn save(
        &self,
        id: &DeviceId,
        perms: &DevicePermissions,
    ) -> Result<(), PermissionsError> {
        // Mutate the existing peer record in place; refuse to create a
        // ghost peer entry if pairing hasn't happened yet (the D-Bus
        // permission surface should only ever be called for paired
        // devices).
        let mut peer = self
            .peers
            .get(id)
            .map_err(|e| PermissionsError::TomlDecode(e.to_string()))?;
        peer.permissions = *perms;
        self.peers
            .put(&peer)
            .map_err(|e| PermissionsError::TomlEncode(e.to_string()))?;
        Ok(())
    }

    async fn delete(&self, id: &DeviceId) -> Result<(), PermissionsError> {
        // Removing the permission record means removing the peer —
        // forget() on the D-Bus surface already calls `peers.remove`
        // directly, so we just no-op here to stay idempotent.
        let _ = id;
        Ok(())
    }

    async fn check(
        &self,
        id: &DeviceId,
        permission: Permission,
    ) -> Result<bool, PermissionsError> {
        let perms = self.load(id).await?;
        Ok(permission_value(&perms, permission))
    }
}
