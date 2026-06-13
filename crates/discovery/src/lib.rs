//! LAN device discovery abstraction.
//!
//! Default backend (Step 3): mDNS via `mdns-sd`, advertised under the
//! service type `_ansync._udp.local.`. The trait lets us slot a relay /
//! NAT-traversal backend in later without touching call sites.

use std::net::SocketAddr;

use ansync_core::{Capabilities, DeviceId, DeviceName};
use async_trait::async_trait;
use futures::Stream;

pub const SERVICE_TYPE: &str = "_ansync._udp.local.";

#[derive(Debug, Clone)]
pub struct DiscoveredDevice {
    pub id: DeviceId,
    pub name: DeviceName,
    pub addr: SocketAddr,
    pub capabilities: Capabilities,
}

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("discovery backend unavailable")]
    BackendUnavailable,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[async_trait]
pub trait Discovery: Send + Sync {
    async fn announce(
        &self,
        name: &DeviceName,
        port: u16,
        caps: Capabilities,
    ) -> Result<(), DiscoveryError>;

    async fn stop_announce(&self) -> Result<(), DiscoveryError>;

    fn browse(&self) -> Box<dyn Stream<Item = DiscoveredDevice> + Send + Unpin>;
}
