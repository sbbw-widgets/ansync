//! D-Bus surface published by `ansyncd` under `org.gameros.Ansync1`.
//!
//! Objects:
//! - `/org/gameros/Ansync1/Manager` — list devices, start pairing, forget.
//! - `/org/gameros/Ansync1/Device/{id}` — capability-bound methods, props,
//!   signals.
//! - `/org/gameros/Ansync1/Permissions/{id}` — per-flag get / set.
//! - `/org/gameros/Ansync1/PairingPrompt` — signal-based prompt; the
//!   daemon falls back to a local egui dialog when no subscriber answers
//!   within 1500 ms.
//!
//! Concrete `#[interface]` impls land in Step 4.

pub const SERVICE_NAME: &str = "org.gameros.Ansync1";
pub const PATH_MANAGER: &str = "/org/gameros/Ansync1/Manager";
pub const PATH_PAIRING_PROMPT: &str = "/org/gameros/Ansync1/PairingPrompt";

pub fn path_device(id: &str) -> String {
    format!("/org/gameros/Ansync1/Device/{id}")
}

pub fn path_permissions(id: &str) -> String {
    format!("/org/gameros/Ansync1/Permissions/{id}")
}
