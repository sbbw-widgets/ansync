//! Clipboard sync backend abstraction.
//!
//! Default: wayland via `wl-clipboard-rs`. X11 follows as a second
//! backend behind a feature flag. Privacy gating lives in `daemon-core`
//! — this crate only handles the local clipboard mechanics.

use async_trait::async_trait;

#[derive(Debug, Clone)]
pub enum ClipboardContent {
    Text(String),
    Blob { mime: String, data: Vec<u8> },
}

#[derive(Debug, thiserror::Error)]
pub enum ClipboardError {
    #[error("backend unavailable")]
    BackendUnavailable,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[async_trait]
pub trait ClipboardBackend: Send + Sync {
    async fn read(&self) -> Result<ClipboardContent, ClipboardError>;
    async fn write(&self, content: ClipboardContent) -> Result<(), ClipboardError>;
}
