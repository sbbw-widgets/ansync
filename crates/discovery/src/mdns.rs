//! mDNS-SD backend for [`Discovery`].
//!
//! Each peer registers a service instance keyed by its 128-bit device id
//! (hex). TXT carries the full 32-byte Ed25519 pubkey, the human-facing
//! name, and the capability bitflags so the browser can populate a
//! `DiscoveredDevice` without ever opening a TCP connection.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::net::SocketAddr;

use ansync_core::{Capabilities, DeviceId, DeviceName};
use async_trait::async_trait;
use futures::stream::{self};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::sync::Mutex;

use crate::{
    DeviceStream, DiscoveredDevice, Discovery, DiscoveryError, SERVICE_TYPE,
    txt::{CAPS, ID, NAME},
};

pub struct MdnsDiscovery {
    pubkey: [u8; 32],
    device_id: DeviceId,
    daemon: ServiceDaemon,
    announced: Mutex<Option<String>>,
}

impl MdnsDiscovery {
    pub fn new(pubkey: [u8; 32]) -> Result<Self, DiscoveryError> {
        let daemon = ServiceDaemon::new().map_err(|e| DiscoveryError::Backend(e.to_string()))?;
        let mut id = [0u8; 16];
        id.copy_from_slice(&pubkey[..16]);
        Ok(Self {
            pubkey,
            device_id: DeviceId(id),
            daemon,
            announced: Mutex::new(None),
        })
    }

    pub fn device_id(&self) -> &DeviceId {
        &self.device_id
    }

    fn instance_name(&self) -> String {
        hex_encode(&self.device_id.0)
    }
}

#[async_trait]
impl Discovery for MdnsDiscovery {
    async fn announce(
        &self,
        name: &DeviceName,
        port: u16,
        caps: Capabilities,
    ) -> Result<(), DiscoveryError> {
        let mut props: HashMap<String, String> = HashMap::new();
        props.insert(ID.to_string(), hex_encode(&self.pubkey));
        props.insert(NAME.to_string(), name.0.clone());
        props.insert(CAPS.to_string(), format!("{:08x}", caps.bits()));

        let instance = self.instance_name();
        let host = format!("{instance}.local.");
        let info = ServiceInfo::new(SERVICE_TYPE, &instance, &host, "", port, props)
            .map_err(|e| DiscoveryError::Backend(e.to_string()))?
            .enable_addr_auto();
        let fullname = info.get_fullname().to_string();

        self.daemon
            .register(info)
            .map_err(|e| DiscoveryError::Backend(e.to_string()))?;
        *self.announced.lock().await = Some(fullname);
        Ok(())
    }

    async fn stop_announce(&self) -> Result<(), DiscoveryError> {
        if let Some(name) = self.announced.lock().await.take() {
            let _ = self.daemon.unregister(&name);
        }
        Ok(())
    }

    fn browse(&self) -> Result<DeviceStream, DiscoveryError> {
        let receiver = self
            .daemon
            .browse(SERVICE_TYPE)
            .map_err(|e| DiscoveryError::Backend(e.to_string()))?;
        let stream = stream::unfold(receiver, |receiver| async move {
            loop {
                match receiver.recv_async().await {
                    Ok(ServiceEvent::ServiceResolved(info)) => {
                        if let Some(dev) = parse_resolved(&info) {
                            return Some((dev, receiver));
                        }
                    }
                    Ok(_) => continue,
                    Err(_) => return None,
                }
            }
        });
        Ok(Box::pin(stream))
    }
}

fn parse_resolved(info: &ServiceInfo) -> Option<DiscoveredDevice> {
    let id_hex = info.get_property_val_str(ID)?;
    let name = info.get_property_val_str(NAME)?;
    let caps_hex = info.get_property_val_str(CAPS)?;

    let pubkey = hex_decode_32(id_hex)?;
    let mut dev_id = [0u8; 16];
    dev_id.copy_from_slice(&pubkey[..16]);

    let caps_bits = u32::from_str_radix(caps_hex, 16).ok()?;
    let capabilities = Capabilities::from_bits(caps_bits)?;

    let port = info.get_port();
    let ip = info.get_addresses().iter().next().copied()?;
    let addr = SocketAddr::new(ip, port);

    Some(DiscoveredDevice {
        id: DeviceId(dev_id),
        name: DeviceName(name.to_string()),
        addr,
        capabilities,
    })
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

fn hex_decode_32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let h = std::str::from_utf8(chunk).ok()?;
        out[i] = u8::from_str_radix(h, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let v = [0u8, 1, 0xfe, 0xff];
        let s = hex_encode(&v);
        assert_eq!(s, "0001feff");
        let mut full = [0u8; 32];
        for (i, b) in full.iter_mut().enumerate() {
            *b = i as u8;
        }
        let s = hex_encode(&full);
        let back = hex_decode_32(&s).expect("decode");
        assert_eq!(back, full);
    }
}
