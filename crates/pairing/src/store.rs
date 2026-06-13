//! Persistent store for paired peers.
//!
//! Layout: `{root}/{device_id_hex}.toml`. Each file is a self-contained
//! record — the pubkey is what makes a peer trusted, the rest is cached
//! metadata that may be refreshed on every successful connection.

use std::fmt::Write as _;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ansync_core::{Capabilities, DeviceId, DeviceName, DevicePermissions};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum PeerStoreError {
    #[error("malformed peer file at {0}: {1}")]
    Malformed(PathBuf, String),
    #[error("peer not found: {0}")]
    NotFound(DeviceId),
    #[error("toml encode: {0}")]
    TomlEncode(#[from] toml::ser::Error),
    #[error("toml decode: {0}")]
    TomlDecode(#[from] toml::de::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct StoredPeer {
    pub id: DeviceId,
    pub name: DeviceName,
    pub pubkey: [u8; 32],
    pub capabilities: Capabilities,
    pub permissions: DevicePermissions,
    pub paired_at: u64,
}

impl StoredPeer {
    pub fn new(
        name: DeviceName,
        pubkey: [u8; 32],
        capabilities: Capabilities,
        permissions: DevicePermissions,
    ) -> Self {
        let mut id_bytes = [0u8; 16];
        id_bytes.copy_from_slice(&pubkey[..16]);
        Self {
            id: DeviceId(id_bytes),
            name,
            pubkey,
            capabilities,
            permissions,
            paired_at: now_unix(),
        }
    }
}

pub struct PeerStore {
    root: PathBuf,
}

impl PeerStore {
    /// Open or create the store rooted at `root` (mode 0700 if created).
    pub fn open(root: PathBuf) -> Result<Self, PeerStoreError> {
        if !root.exists() {
            fs::create_dir_all(&root)?;
            let mut perms = fs::metadata(&root)?.permissions();
            perms.set_mode(0o700);
            let _ = fs::set_permissions(&root, perms);
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn put(&self, peer: &StoredPeer) -> Result<(), PeerStoreError> {
        let file: PeerFile = peer.into();
        let toml_str = toml::to_string_pretty(&file)?;
        let path = self.path_for(&peer.id);
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, toml_str)?;
        let mut perms = fs::metadata(&tmp)?.permissions();
        perms.set_mode(0o600);
        let _ = fs::set_permissions(&tmp, perms);
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn get(&self, id: &DeviceId) -> Result<StoredPeer, PeerStoreError> {
        let path = self.path_for(id);
        let bytes = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Err(PeerStoreError::NotFound(id.clone()));
            }
            Err(e) => return Err(e.into()),
        };
        let file: PeerFile = toml::from_str(&bytes)?;
        file.into_stored(&path)
    }

    pub fn remove(&self, id: &DeviceId) -> Result<(), PeerStoreError> {
        let path = self.path_for(id);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => {
                Err(PeerStoreError::NotFound(id.clone()))
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn list(&self) -> Result<Vec<StoredPeer>, PeerStoreError> {
        let mut out = Vec::new();
        let dir = match fs::read_dir(&self.root) {
            Ok(d) => d,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        for entry in dir {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let bytes = fs::read_to_string(&path)?;
            let file: PeerFile = toml::from_str(&bytes)?;
            out.push(file.into_stored(&path)?);
        }
        out.sort_by(|a, b| a.paired_at.cmp(&b.paired_at));
        Ok(out)
    }

    fn path_for(&self, id: &DeviceId) -> PathBuf {
        let name = format!("{id}.toml");
        self.root.join(name)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct PeerFile {
    id: String,
    name: String,
    pubkey: String,
    capabilities: u32,
    paired_at: u64,
    permissions: DevicePermissions,
}

impl From<&StoredPeer> for PeerFile {
    fn from(p: &StoredPeer) -> Self {
        Self {
            id: hex_encode(&p.id.0),
            name: p.name.0.clone(),
            pubkey: hex_encode(&p.pubkey),
            capabilities: p.capabilities.bits(),
            paired_at: p.paired_at,
            permissions: p.permissions,
        }
    }
}

impl PeerFile {
    fn into_stored(self, source: &Path) -> Result<StoredPeer, PeerStoreError> {
        let pubkey = hex_decode_32(&self.pubkey).ok_or_else(|| {
            PeerStoreError::Malformed(source.to_path_buf(), "pubkey must be 64 hex chars".into())
        })?;
        let id_bytes = hex_decode_16(&self.id).ok_or_else(|| {
            PeerStoreError::Malformed(source.to_path_buf(), "id must be 32 hex chars".into())
        })?;
        let capabilities = Capabilities::from_bits(self.capabilities).ok_or_else(|| {
            PeerStoreError::Malformed(
                source.to_path_buf(),
                format!("unknown capability bits {:#x}", self.capabilities),
            )
        })?;
        Ok(StoredPeer {
            id: DeviceId(id_bytes),
            name: DeviceName(self.name),
            pubkey,
            capabilities,
            permissions: self.permissions,
            paired_at: self.paired_at,
        })
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
    decode_into(s, &mut out)?;
    Some(out)
}

fn hex_decode_16(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    decode_into(s, &mut out)?;
    Some(out)
}

fn decode_into(s: &str, out: &mut [u8]) -> Option<()> {
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let h = std::str::from_utf8(chunk).ok()?;
        out[i] = u8::from_str_radix(h, 16).ok()?;
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_remove_list_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "ansync-peerstore-test-{}-{}",
            std::process::id(),
            now_unix()
        ));
        let store = PeerStore::open(dir.clone()).unwrap();

        let mut pubkey = [0u8; 32];
        for (i, b) in pubkey.iter_mut().enumerate() {
            *b = i as u8;
        }
        let peer = StoredPeer::new(
            DeviceName("pixel".into()),
            pubkey,
            Capabilities::SCREEN_MIRROR | Capabilities::FILES,
            DevicePermissions::default(),
        );
        store.put(&peer).unwrap();

        let mode = fs::metadata(store.path_for(&peer.id))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        let back = store.get(&peer.id).unwrap();
        assert_eq!(back.name.0, "pixel");
        assert_eq!(back.pubkey, pubkey);
        assert_eq!(back.capabilities, peer.capabilities);

        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);

        store.remove(&peer.id).unwrap();
        assert!(matches!(
            store.get(&peer.id),
            Err(PeerStoreError::NotFound(_))
        ));

        let _ = fs::remove_dir_all(&dir);
    }
}
