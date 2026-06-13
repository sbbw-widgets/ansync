//! File transfer + remote filesystem mount.
//!
//! Two surfaces:
//! - explicit transfer (push / pull) via `TransferProgress` reporting,
//!   used by `ansyncctl push` and the D-Bus `SendFile` method;
//! - FUSE-mounted remote filesystem driven by `RemoteFsBackend`, exposed
//!   under `$XDG_RUNTIME_DIR/ansync/mounts/{device-name}/`.

use async_trait::async_trait;
use bytes::Bytes;

pub mod transfer;

pub use transfer::{
    AutoAcceptPolicy, CHUNK_SIZE, InboundDecision, InboundPolicy, OfferSummary, TransferError,
    receive_file, send_file,
};

#[derive(Debug, thiserror::Error)]
pub enum FilesError {
    #[error("not found")]
    NotFound,
    #[error("permission denied")]
    PermissionDenied,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct TransferProgress {
    pub transfer_id: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

#[derive(Debug, Clone)]
pub struct RemoteMeta {
    pub size: u64,
    pub mode: u32,
    pub mtime: u64,
    pub is_dir: bool,
}

#[derive(Debug, Clone)]
pub struct RemoteEntry {
    pub name: String,
    pub meta: RemoteMeta,
}

/// Backend driving FUSE callbacks. Each method maps 1:1 to a FUSE op.
#[async_trait]
pub trait RemoteFsBackend: Send + Sync {
    async fn stat(&self, path: &str) -> Result<RemoteMeta, FilesError>;
    async fn readdir(&self, path: &str) -> Result<Vec<RemoteEntry>, FilesError>;
    async fn open(&self, path: &str, flags: u32) -> Result<u64, FilesError>;
    async fn read(&self, handle: u64, offset: u64, len: u32) -> Result<Bytes, FilesError>;
    async fn write(&self, handle: u64, offset: u64, data: Bytes) -> Result<u32, FilesError>;
    async fn close(&self, handle: u64) -> Result<(), FilesError>;
}
