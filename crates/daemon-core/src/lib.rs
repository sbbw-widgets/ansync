//! Orchestrator shared between `ansyncd` and integration tests.
//!
//! Owns the device registry, permission store, transport, discovery,
//! and per-device session tasks. Exposes a `Daemon` handle the binary
//! uses to wire D-Bus, GUI, and lifecycle. Concrete wiring lands in
//! Step 4.

use std::sync::Arc;

use ansync_core::DeviceId;

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("core: {0}")]
    Core(#[from] ansync_core::Error),
    #[error("startup: {0}")]
    Startup(String),
}

pub struct DaemonConfig {
    pub device_name: String,
}

pub struct Daemon {
    config: DaemonConfig,
}

impl Daemon {
    pub fn new(config: DaemonConfig) -> Self {
        Self { config }
    }

    pub async fn run(self: Arc<Self>) -> Result<(), DaemonError> {
        tracing::info!(device = %self.config.device_name, "ansyncd daemon-core run (skeleton)");
        Ok(())
    }

    pub async fn shutdown(&self, _device: Option<DeviceId>) -> Result<(), DaemonError> {
        Ok(())
    }
}
