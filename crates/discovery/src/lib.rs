//! LAN device discovery abstraction.
//!
//! Default backend (Step 3): mDNS via `mdns-sd`, advertised under the
//! service type `_ansync._udp.local.`. The trait lets us slot a relay /
//! NAT-traversal backend in later without touching call sites.

use std::net::SocketAddr;
use std::pin::Pin;

use ansync_core::{Capabilities, DeviceId, DeviceName};
use async_trait::async_trait;
use futures::Stream;

#[cfg(feature = "mdns")]
pub mod mdns;

#[cfg(feature = "mdns")]
pub use mdns::MdnsDiscovery;

pub const SERVICE_TYPE: &str = "_ansync._udp.local.";

/// Keys used in the mDNS TXT record. Listed here so call sites don't
/// hard-code them.
pub mod txt {
    pub const ID: &str = "id";
    pub const NAME: &str = "name";
    pub const CAPS: &str = "caps";
}

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
    #[error("backend: {0}")]
    Backend(String),
    #[error("malformed advertisement: {0}")]
    Malformed(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type DeviceStream = Pin<Box<dyn Stream<Item = DiscoveredDevice> + Send>>;

#[async_trait]
pub trait Discovery: Send + Sync {
    async fn announce(
        &self,
        name: &DeviceName,
        port: u16,
        caps: Capabilities,
    ) -> Result<(), DiscoveryError>;

    async fn stop_announce(&self) -> Result<(), DiscoveryError>;

    fn browse(&self) -> Result<DeviceStream, DiscoveryError>;
}
