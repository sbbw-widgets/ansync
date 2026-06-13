//! Shared state owned by the daemon and consumed by every D-Bus
//! interface impl. Kept in the `dbus` crate so the interfaces don't
//! need to depend on `daemon-core` (which would be a cycle).

use std::sync::Arc;

use ansync_core::DeviceId;
use ansync_crypto::IdentityKeypair;
use ansync_pairing::PeerStore;
use ansync_permissions::PermissionsStore;
use tokio::sync::mpsc::UnboundedSender;

/// Actions D-Bus interfaces dispatch back into `daemon-core`. Sent on
/// [`DaemonState::actions`]; the daemon spawns an action loop that
/// consumes the receiver and runs the appropriate task (open mirror
/// window, start camera session, etc.).
///
/// The enum sits in the `dbus` crate to avoid a cycle: D-Bus
/// interfaces own the sender; `daemon-core` owns the receiver.
#[derive(Debug, Clone)]
pub enum DaemonAction {
    /// Show the mirror window for `device`. Idempotent — if a window
    /// is already up, the action is a no-op.
    ShowScreen { device: DeviceId },
    /// Close the mirror window for `device`.
    HideScreen { device: DeviceId },
}

pub struct DaemonState {
    pub identity: IdentityKeypair,
    pub device_name: String,
    pub peers: PeerStore,
    pub permissions: Arc<dyn PermissionsStore>,
    /// Set by `daemon-core` before D-Bus interfaces start handling
    /// calls. `None` only during the brief construction window — D-Bus
    /// interfaces panic if they try to send without it wired.
    pub actions: Option<UnboundedSender<DaemonAction>>,
}

impl DaemonState {
    pub fn new(
        identity: IdentityKeypair,
        device_name: String,
        peers: PeerStore,
        permissions: Arc<dyn PermissionsStore>,
    ) -> Self {
        Self {
            identity,
            device_name,
            peers,
            permissions,
            actions: None,
        }
    }

    pub fn with_actions(mut self, tx: UnboundedSender<DaemonAction>) -> Self {
        self.actions = Some(tx);
        self
    }
}
