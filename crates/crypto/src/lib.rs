//! Cryptographic identity and handshake primitives.
//!
//! Each peer owns a long-term Ed25519 identity. Sessions are negotiated via
//! Noise XX over the QUIC control stream, producing symmetric keys for
//! authenticated framing on media streams.

use ansync_core::DeviceId;
use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("invalid identity material")]
    InvalidIdentity,
    #[error("handshake failed: {0}")]
    Handshake(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Long-term identity stored on disk at `$XDG_DATA_HOME/ansync/identity.key`.
#[derive(Debug, Serialize, Deserialize)]
pub struct IdentityKeypair {
    secret: [u8; 32],
}

impl IdentityKeypair {
    pub fn generate() -> Self {
        let signing = SigningKey::generate(&mut rand_core::OsRng);
        Self { secret: signing.to_bytes() }
    }

    pub fn signing(&self) -> SigningKey {
        SigningKey::from_bytes(&self.secret)
    }

    pub fn public(&self) -> PeerIdentity {
        PeerIdentity(self.signing().verifying_key())
    }

    pub fn device_id(&self) -> DeviceId {
        self.public().device_id()
    }
}

#[derive(Clone)]
pub struct PeerIdentity(pub VerifyingKey);

impl PeerIdentity {
    pub fn from_bytes(bytes: [u8; 32]) -> Result<Self, CryptoError> {
        VerifyingKey::from_bytes(&bytes)
            .map(Self)
            .map_err(|_| CryptoError::InvalidIdentity)
    }

    pub fn as_bytes(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    /// 128-bit fingerprint of the Ed25519 public key. Stable across
    /// reconnects — used as the routing identifier on every D-Bus path.
    pub fn device_id(&self) -> DeviceId {
        let bytes = self.0.to_bytes();
        let mut id = [0u8; 16];
        id.copy_from_slice(&bytes[..16]);
        DeviceId(id)
    }
}

/// Handshake driver. Implementations wrap Noise XX over the long-term
/// Ed25519 identities, producing ephemeral X25519 session keys.
pub trait Handshake {
    fn write_message(&mut self, payload: &[u8], out: &mut [u8]) -> Result<usize, CryptoError>;
    fn read_message(&mut self, msg: &[u8], out: &mut [u8]) -> Result<usize, CryptoError>;
    fn is_complete(&self) -> bool;
    fn peer_identity(&self) -> Option<PeerIdentity>;
}
