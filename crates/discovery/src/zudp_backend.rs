//! zudp Probe/Beacon discovery backend for [`Discovery`].

use std::net::SocketAddr;

use ansync_core::{Capabilities, DeviceId, DeviceName};
use async_trait::async_trait;
use futures::stream;
use tokio::sync::Mutex;
use zudp::{AdvertiseHandle, DiscoveryConfig};

use crate::{
    AnsyncBeacon, APP_ID, DiscoveredDevice, DeviceStream, Discovery, DiscoveryError,
};

pub struct ZudpDiscovery {
    pubkey: [u8; 32],
    handle: Mutex<Option<AdvertiseHandle<AnsyncBeacon>>>,
}

impl ZudpDiscovery {
    pub fn new(pubkey: [u8; 32]) -> Self {
        Self {
            pubkey,
            handle: Mutex::new(None),
        }
    }

    pub fn device_id(&self) -> DeviceId {
        let mut id = [0u8; 16];
        id.copy_from_slice(&self.pubkey[..16]);
        DeviceId(id)
    }
}

#[async_trait]
impl Discovery for ZudpDiscovery {
    async fn announce(
        &self,
        name: &DeviceName,
        port: u16,
        caps: Capabilities,
    ) -> Result<(), DiscoveryError> {
        let beacon = AnsyncBeacon {
            pubkey: self.pubkey,
            name: name.0.clone(),
            caps: caps.bits(),
        };
        let cfg = DiscoveryConfig::new(APP_ID, port).meta(beacon);
        let handle = zudp::Discovery::advertise(cfg)
            .map_err(|e| DiscoveryError::Backend(e.to_string()))?;
        *self.handle.lock().await = Some(handle);
        Ok(())
    }

    async fn stop_announce(&self) -> Result<(), DiscoveryError> {
        self.handle.lock().await.take();
        Ok(())
    }

    fn browse(&self) -> Result<DeviceStream, DiscoveryError> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<DiscoveredDevice>();
        tokio::spawn(async move {
            let mut scan = match zudp::Discovery::scan_stream::<AnsyncBeacon>(
                DiscoveryConfig::new(APP_ID, 0),
            )
            .await
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(target: "ansync::discovery", "scan_stream failed: {e}");
                    return;
                }
            };
            loop {
                match scan.next().await {
                    Ok(peer) => {
                        if let Some(dev) = beacon_to_device(peer.data_addr, peer.meta) {
                            let _ = tx.send(dev);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(target: "ansync::discovery", "scan error: {e}");
                        break;
                    }
                }
            }
        });
        let out = stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|d| (d, rx))
        });
        Ok(Box::pin(out))
    }
}

fn beacon_to_device(addr: SocketAddr, beacon: AnsyncBeacon) -> Option<DiscoveredDevice> {
    let caps = Capabilities::from_bits(beacon.caps)?;
    Some(DiscoveredDevice {
        id: beacon.device_id(),
        name: DeviceName(beacon.name),
        pubkey: beacon.pubkey,
        addr,
        capabilities: caps,
    })
}
