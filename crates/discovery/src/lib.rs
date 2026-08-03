//! LAN device discovery abstraction.
//!
//! Backend: zudp Probe/Beacon frames. The `Discovery` trait lets us slot
//! a relay / NAT-traversal backend in later without touching call sites.

use std::net::SocketAddr;
use std::pin::Pin;

use ansync_core::{Capabilities, DeviceId, DeviceName};
use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};

pub mod zudp_backend;
pub use zudp_backend::ZudpDiscovery;

/// App-ID string used for all ansync Probe/Beacon frames.
pub const APP_ID: &str = "ansync";

/// Metadata broadcast in a Beacon frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnsyncBeacon {
    /// Full Ed25519 public key (32 bytes). First 16 bytes → DeviceId.
    pub pubkey: [u8; 32],
    pub name: String,
    /// Raw `Capabilities` bitflags.
    pub caps: u32,
}

impl AnsyncBeacon {
    pub fn device_id(&self) -> DeviceId {
        let mut id = [0u8; 16];
        id.copy_from_slice(&self.pubkey[..16]);
        DeviceId(id)
    }
}

#[derive(Debug, Clone)]
pub struct DiscoveredDevice {
    pub id: DeviceId,
    pub name: DeviceName,
    pub addr: SocketAddr,
    pub capabilities: Capabilities,
    /// Full Ed25519 public key (32 bytes). Hex-encode to match against the
    /// paired host's `PREF_HOST_PUBKEY_HEX` stored during cable pairing.
    pub pubkey: [u8; 32],
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
