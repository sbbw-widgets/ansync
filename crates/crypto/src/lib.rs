//! Cryptographic identity and handshake primitives.
//!
//! Each peer owns a long-term Ed25519 identity stored on disk. Sessions
//! layer on top: rustls authenticates the QUIC channel by pinning the
//! peer's Ed25519 pubkey, and Noise XX provides a second independent set
//! of session keys for media-stream framing.

use std::fs;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use ansync_core::DeviceId;
use ed25519_dalek::{SigningKey, VerifyingKey};

pub mod noise;
pub mod pair_pin;

pub use noise::{NoiseError, NoiseTransport, NoiseXxSession, Role};
pub use pair_pin::{PinRole, generate_pin, pair_pin_confirm, verify_pin_confirm};

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("invalid identity material")]
    InvalidIdentity,
    #[error("identity file size: expected 32 bytes, got {0}")]
    InvalidIdentitySize(usize),
    #[error("noise: {0}")]
    Noise(#[from] NoiseError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Long-term identity persisted to `$XDG_DATA_HOME/ansync/identity.key`
/// as the raw 32-byte Ed25519 seed with mode 0600.
#[derive(Debug, Clone)]
pub struct IdentityKeypair {
    secret: [u8; 32],
}

impl IdentityKeypair {
    pub fn generate() -> Self {
        let signing = SigningKey::generate(&mut rand_core::OsRng);
        Self { secret: signing.to_bytes() }
    }

    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self { secret: seed }
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

    pub fn seed_bytes(&self) -> &[u8; 32] {
        &self.secret
    }

    /// Load the keypair from disk. Fails with `Io(NotFound)` if absent —
    /// callers can branch on that to call `generate` + `save`.
    pub fn load(path: &Path) -> Result<Self, CryptoError> {
        let bytes = fs::read(path)?;
        if bytes.len() != 32 {
            return Err(CryptoError::InvalidIdentitySize(bytes.len()));
        }
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&bytes);
        Ok(Self::from_seed(seed))
    }

    /// Persist the seed to disk with mode 0600. Parent directory is
    /// created at mode 0700 if missing.
    pub fn save(&self, path: &Path) -> Result<(), CryptoError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
            let mut perms = fs::metadata(parent)?.permissions();
            perms.set_mode(0o700);
            let _ = fs::set_permissions(parent, perms);
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(&self.secret)?;
        file.sync_all()?;
        Ok(())
    }

    /// Load if it exists, otherwise generate + persist a new one.
    pub fn load_or_generate(path: &Path) -> Result<Self, CryptoError> {
        match Self::load(path) {
            Ok(kp) => Ok(kp),
            Err(CryptoError::Io(e)) if e.kind() == ErrorKind::NotFound => {
                let kp = Self::generate();
                kp.save(path)?;
                Ok(kp)
            }
            Err(e) => Err(e),
        }
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

    pub fn verifying_key(&self) -> &VerifyingKey {
        &self.0
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

impl std::fmt::Debug for PeerIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PeerIdentity(device_id={})", self.device_id())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempdir();
        let path = dir.join("identity.key");
        let kp = IdentityKeypair::generate();
        kp.save(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let kp2 = IdentityKeypair::load(&path).unwrap();
        assert_eq!(kp.seed_bytes(), kp2.seed_bytes());
        fs::remove_dir_all(&dir).ok();
    }

    fn tempdir() -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "ansync-crypto-test-{}",
            std::process::id()
        ));
        fs::create_dir_all(&base).unwrap();
        base
    }
}
