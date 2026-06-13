//! Shared state owned by the daemon and consumed by every D-Bus
//! interface impl. Kept in the `dbus` crate so the interfaces don't
//! need to depend on `daemon-core` (which would be a cycle).

use std::sync::Arc;

use ansync_crypto::IdentityKeypair;
use ansync_pairing::PeerStore;
use ansync_permissions::PermissionsStore;

pub struct DaemonState {
    pub identity: IdentityKeypair,
    pub device_name: String,
    pub peers: PeerStore,
    pub permissions: Arc<dyn PermissionsStore>,
}

impl DaemonState {
    pub fn new(
        identity: IdentityKeypair,
        device_name: String,
        peers: PeerStore,
        permissions: Arc<dyn PermissionsStore>,
    ) -> Self {
        Self { identity, device_name, peers, permissions }
    }
}
