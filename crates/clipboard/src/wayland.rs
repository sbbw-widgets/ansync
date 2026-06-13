//! Wayland-backed clipboard via `wl-clipboard-rs`.
//!
//! Both directions are synchronous in the underlying crate
//! (`copy::Options::copy` / `paste::get_contents`), so the async
//! methods on the trait wrap them in `tokio::task::spawn_blocking`
//! to avoid blocking the runtime worker.

use std::io::Read;

use async_trait::async_trait;
use tracing::warn;
use wl_clipboard_rs::copy::{MimeType as CopyMime, Options, Source};
use wl_clipboard_rs::paste::{
    get_contents, ClipboardType, Error as PasteError, MimeType as PasteMime, Seat,
};

use crate::{ClipboardBackend, ClipboardContent, ClipboardError};

/// Pure Wayland backend. No state — every call goes straight to the
/// compositor.
#[derive(Debug, Default, Clone)]
pub struct WaylandClipboard;

impl WaylandClipboard {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ClipboardBackend for WaylandClipboard {
    async fn read(&self) -> Result<ClipboardContent, ClipboardError> {
        tokio::task::spawn_blocking(|| {
            match get_contents(ClipboardType::Regular, Seat::Unspecified, PasteMime::Text) {
                Ok((mut pipe, mime)) => {
                    let mut buf = Vec::new();
                    pipe.read_to_end(&mut buf)
                        .map_err(ClipboardError::Io)?;
                    if mime.starts_with("text/") {
                        let text = String::from_utf8_lossy(&buf).to_string();
                        Ok(ClipboardContent::Text(text))
                    } else {
                        Ok(ClipboardContent::Blob {
                            mime,
                            data: buf,
                        })
                    }
                }
                Err(PasteError::ClipboardEmpty) | Err(PasteError::NoMimeType) => {
                    Ok(ClipboardContent::Text(String::new()))
                }
                Err(e) => {
                    warn!(error = %e, "wayland paste failed");
                    Err(ClipboardError::BackendUnavailable)
                }
            }
        })
        .await
        .map_err(|e| ClipboardError::Io(std::io::Error::other(e)))?
    }

    async fn write(&self, content: ClipboardContent) -> Result<(), ClipboardError> {
        tokio::task::spawn_blocking(move || {
            let opts = Options::new();
            let (source, mime) = match content {
                ClipboardContent::Text(s) => (
                    Source::Bytes(s.into_bytes().into_boxed_slice()),
                    CopyMime::Text,
                ),
                ClipboardContent::Blob { mime, data } => (
                    Source::Bytes(data.into_boxed_slice()),
                    CopyMime::Specific(mime),
                ),
            };
            opts.copy(source, mime)
                .map_err(|e| {
                    warn!(error = %e, "wayland copy failed");
                    ClipboardError::BackendUnavailable
                })
        })
        .await
        .map_err(|e| ClipboardError::Io(std::io::Error::other(e)))?
    }
}
