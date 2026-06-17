//! Pairing flows that establish a long-term trust between host and Android
//! device.
//!
//! Initial path: cable-based bootstrap over ADB — most secure default
//! because the cable window makes MITM impossible. Wi-Fi fallback uses a
//! PIN displayed on Android and verified through Noise XX. BT-HID is a
//! secondary, input-only flow for keyboard / stylus sharing.

use ansync_crypto::PeerIdentity;
use async_trait::async_trait;

pub mod cable;
#[cfg(feature = "host")]
pub mod release;
pub mod store;
pub mod wifi;

pub use cable::{AdbDevice, bootstrap_companion, bootstrap_host, COMPANION_PACKAGE};
#[cfg(feature = "host")]
pub use cable::{companion_installed, install_companion_apk, list_adb_devices, pair_host_via_adb};
#[cfg(feature = "host")]
pub use release::{
    expected_version, expected_version_bare, fetch_companion, fetch_latest_companion,
    query_installed_version, FetchedApk,
};
pub use store::{PeerStore, PeerStoreError, StoredPeer};
pub use wifi::{
    bootstrap_companion_wifi, bootstrap_host_wifi, read_pair_hello, respond_pair_pin,
    CompanionWifiOutcome, PAIR_MDNS_SERVICE_TYPE, PAIR_MDNS_TXT_NAME, PAIR_MDNS_TXT_PUBKEY,
};
#[cfg(feature = "host")]
pub use wifi::{
    browse_pair_candidates, pair_host_via_wifi, watch_pair_candidates, PairCandidate,
    PairWatchEvent,
};

#[derive(Debug, thiserror::Error)]
pub enum PairingError {
    #[error("cancelled by user")]
    Cancelled,
    #[error("rejected by peer")]
    Rejected,
    #[error("crypto: {0}")]
    Crypto(#[from] ansync_crypto::CryptoError),
    #[error("store: {0}")]
    Store(#[from] PeerStoreError),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("frame: {0}")]
    Frame(#[from] ansync_proto::FrameError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Copy)]
pub enum PairingMethod {
    /// USB cable bootstrap via ADB. Most secure default.
    Cable,
    /// Wi-Fi bootstrap with a PIN displayed on Android.
    WifiPin,
    /// Bluetooth HID Device pairing for keyboard / stylus sharing.
    BluetoothHid,
}

#[async_trait]
pub trait PairingChannel: Send + Sync {
    fn method(&self) -> PairingMethod;
    async fn run(&mut self) -> Result<PeerIdentity, PairingError>;
}
