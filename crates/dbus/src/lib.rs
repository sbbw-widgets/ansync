//! D-Bus surface published by `ansyncd` under `org.gameros.Ansync1`.
//!
//! Objects:
//! - `/org/gameros/Ansync1/Manager` — list devices, start pairing, forget.
//! - `/org/gameros/Ansync1/Device/{id}` — capability-bound methods, props,
//!   signals.
//! - `/org/gameros/Ansync1/Permissions/{id}` — per-flag get / set.
//! - `/org/gameros/Ansync1/PairingPrompt` — signal-based prompt; the
//!   daemon falls back to a local egui dialog when no subscriber answers
//!   within 1500 ms (added in a later step).

use std::sync::Arc;

use ansync_core::DeviceId;
use zbus::Connection;

mod device;
mod manager;
mod permissions;
pub mod state;
mod util;

pub use device::Device;
pub use manager::Manager;
pub use permissions::PermissionsIface;
pub use state::{DaemonAction, DaemonState};
pub use util::parse_device_id;

pub const SERVICE_NAME: &str = "org.gameros.Ansync1";
pub const PATH_MANAGER: &str = "/org/gameros/Ansync1/Manager";
pub const PATH_PAIRING_PROMPT: &str = "/org/gameros/Ansync1/PairingPrompt";

pub fn path_device(id: &DeviceId) -> String {
    format!("/org/gameros/Ansync1/Device/{id}")
}

pub fn path_permissions(id: &DeviceId) -> String {
    format!("/org/gameros/Ansync1/Permissions/{id}")
}

#[derive(Debug, thiserror::Error)]
pub enum DbusError {
    #[error("zbus: {0}")]
    Zbus(#[from] zbus::Error),
    #[error("name acquisition: {0}")]
    Name(String),
}

/// Build a session-bus connection, claim the well-known name, and serve
/// the Manager + a Device/Permissions pair for every already-paired peer.
pub async fn serve(state: Arc<DaemonState>) -> Result<Connection, DbusError> {
    let manager = Manager { state: state.clone() };
    let conn = zbus::connection::Builder::session()?
        .name(SERVICE_NAME)?
        .serve_at(PATH_MANAGER, manager)?
        .build()
        .await?;

    let known = state.peers.list().unwrap_or_default();
    for peer in known {
        register_device(&conn, &state, peer.id).await?;
    }

    Ok(conn)
}

/// Attach a Device + Permissions interface pair under the canonical
/// paths. Idempotent for already-registered ids.
pub async fn register_device(
    conn: &Connection,
    state: &Arc<DaemonState>,
    id: DeviceId,
) -> Result<(), DbusError> {
    let device_path = path_device(&id);
    let perms_path = path_permissions(&id);

    let device = Device { id: id.clone(), state: state.clone() };
    let perms_iface = PermissionsIface { id, state: state.clone() };

    conn.object_server().at(device_path, device).await?;
    conn.object_server().at(perms_path, perms_iface).await?;
    Ok(())
}

/// Detach a Device + Permissions interface pair. Used by Manager.Forget
/// once `daemon-core` wires it up.
pub async fn unregister_device(conn: &Connection, id: &DeviceId) -> Result<(), DbusError> {
    let device_path = path_device(id);
    let perms_path = path_permissions(id);
    conn.object_server()
        .remove::<Device, _>(device_path)
        .await?;
    conn.object_server()
        .remove::<PermissionsIface, _>(perms_path)
        .await?;
    Ok(())
}
