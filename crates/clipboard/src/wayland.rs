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
    get_contents, get_mime_types, ClipboardType, Error as PasteError, MimeType as PasteMime, Seat,
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
            // Enumerate all advertised MIMEs and pick the richest the
            // companion can render. Naïvely asking for `PasteMime::Text`
            // makes apps like Firefox / Krita hand us back `text/html`
            // (an `<img>` tag with a base64 URL) when the user really
            // wanted to paste the actual image bytes on the device.
            let mimes = match get_mime_types(ClipboardType::Regular, Seat::Unspecified) {
                Ok(set) => set,
                Err(PasteError::ClipboardEmpty) | Err(PasteError::NoMimeType) => {
                    return Ok(ClipboardContent::Text(String::new()));
                }
                Err(e) => {
                    warn!(error = %e, "wayland mime-type query failed");
                    return Err(ClipboardError::BackendUnavailable);
                }
            };
            let chosen = pick_best_mime(&mimes);
            let chosen = match chosen {
                Some(m) => m,
                None => return Ok(ClipboardContent::Text(String::new())),
            };
            let (mut pipe, mime) = match get_contents(
                ClipboardType::Regular,
                Seat::Unspecified,
                PasteMime::Specific(&chosen),
            ) {
                Ok(v) => v,
                Err(PasteError::ClipboardEmpty) | Err(PasteError::NoMimeType) => {
                    return Ok(ClipboardContent::Text(String::new()));
                }
                Err(e) => {
                    warn!(error = %e, mime = %chosen, "wayland paste failed");
                    return Err(ClipboardError::BackendUnavailable);
                }
            };
            let mut buf = Vec::new();
            pipe.read_to_end(&mut buf).map_err(ClipboardError::Io)?;
            if mime.starts_with("text/") && !mime.starts_with("text/html") {
                let text = String::from_utf8_lossy(&buf).to_string();
                Ok(ClipboardContent::Text(text))
            } else {
                Ok(ClipboardContent::Blob { mime, data: buf })
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

/// Pick the MIME that yields the best paste on the companion. Order:
///   1. concrete `image/*` (png / jpeg / webp / gif / others) so an
///      image copied from a browser or editor lands as an actual
///      bitmap, not the `<img>` HTML wrapper.
///   2. `text/plain` (and its UTF-8 variants) — the most universally
///      pasteable text representation.
///   3. any other `text/*` (often `text/html` or `text/uri-list`).
///   4. anything else verbatim as a blob.
///
/// Returns `None` only when the offer is empty.
fn pick_best_mime(mimes: &std::collections::HashSet<String>) -> Option<String> {
    if mimes.is_empty() {
        return None;
    }
    const IMAGE_PRIORITY: &[&str] = &[
        "image/png",
        "image/jpeg",
        "image/jpg",
        "image/webp",
        "image/gif",
        "image/bmp",
        "image/tiff",
    ];
    for &m in IMAGE_PRIORITY {
        if mimes.contains(m) {
            return Some(m.to_string());
        }
    }
    // Any other image/* not in the explicit list (image/svg+xml, etc.).
    if let Some(any_image) = mimes.iter().find(|m| m.starts_with("image/")) {
        return Some(any_image.clone());
    }
    const TEXT_PRIORITY: &[&str] = &[
        "text/plain;charset=utf-8",
        "text/plain;charset=UTF-8",
        "UTF8_STRING",
        "text/plain",
        "STRING",
    ];
    for &m in TEXT_PRIORITY {
        if mimes.contains(m) {
            return Some(m.to_string());
        }
    }
    // Fall back to the first remaining MIME (HashSet order is
    // non-deterministic but we've already tried every preferred MIME
    // above, so this is genuinely the "everything else" bucket).
    mimes.iter().next().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn images_outrank_html() {
        let mimes = set(&["text/html", "image/png", "text/plain"]);
        assert_eq!(pick_best_mime(&mimes), Some("image/png".to_string()));
    }

    #[test]
    fn jpeg_when_no_png() {
        let mimes = set(&["text/html", "image/jpeg", "text/plain"]);
        assert_eq!(pick_best_mime(&mimes), Some("image/jpeg".to_string()));
    }

    #[test]
    fn plain_text_beats_html() {
        let mimes = set(&["text/html", "text/plain"]);
        assert_eq!(pick_best_mime(&mimes), Some("text/plain".to_string()));
    }

    #[test]
    fn empty_offer_returns_none() {
        assert_eq!(pick_best_mime(&set(&[])), None);
    }
}
