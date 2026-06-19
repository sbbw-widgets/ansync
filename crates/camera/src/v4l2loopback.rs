//! v4l2loopback-backed `VirtualCameraSink`.
//!
//! Two open paths, tried in order:
//!
//! 1. **Dynamic add** via `/dev/v4l2loopback` (preferred). We ioctl
//!    `V4L2LOOPBACK_CTL_ADD` with the Android device name as
//!    `card_label`, then write raw frames to the returned
//!    `/dev/videoN`. On `unregister` we remove the node so the
//!    system stays clean. This gives per-peer naming in browsers /
//!    OBS / Discord pickers — "Pixel 9 (Ansync)" instead of a
//!    generic "Ansync" string.
//!
//! 2. **Static fallback** — scan `/dev/video*` for the first node
//!    advertising `V4L2_CAP_VIDEO_OUTPUT`, with the legacy
//!    `with_path` override pinning a specific node. This kicks in
//!    if `/dev/v4l2loopback` is missing (older module / explicit
//!    static config) and behaves identically to the original
//!    ship-1 path: peer name lives in tracing logs + D-Bus
//!    `Device.Name`, kernel-level card_label stays whatever
//!    modprobe set.
//!
//! Frame path: raw NV12 / YUV420 bytes written straight to the
//! device fd. v4l2loopback honours plain `write(2)` on output
//! devices, which avoids the mmap / DQBUF dance for what is
//! effectively a fan-out pipe.

use std::os::raw::c_void;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Mutex;
use v4l::buffer::Type as BufferType;
use v4l::capability::Flags as CapFlags;
use v4l::device::Device;
use v4l::format::FourCC;
use v4l::video::Output;
use v4l::Format;

use crate::dyn_ctl;
use crate::{CameraError, CameraFormat, CameraPixelFormat, VirtualCameraSink};

/// Concrete v4l2loopback sink. Holds the device under a tokio mutex
/// so concurrent `write_frame` callers serialise on the underlying
/// `write(2)` syscall (v4l2 doesn't define byte-level interleaving
/// guarantees).
pub struct V4l2LoopbackSink {
    /// Explicit node path. `None` ⇒ dynamic add via control device
    /// (preferred) or auto-discover at `register` time.
    explicit: Option<PathBuf>,
    inner: Mutex<Option<RegisteredDevice>>,
}

struct RegisteredDevice {
    device: Device,
    path: PathBuf,
    label: String,
    /// `Some(nr)` when this node was created via the dyn-ctl
    /// interface and ownership lies with us — `unregister` will
    /// REMOVE it. `None` means the node pre-existed (static modprobe
    /// configuration or `with_path` pin) and we leave it alone.
    owned_nr: Option<u32>,
}

impl V4l2LoopbackSink {
    /// Auto-discover a free v4l2loopback output node at `register`.
    /// Prefers the dynamic `/dev/v4l2loopback` control device when
    /// available, so each peer gets a per-call card_label.
    pub fn new() -> Self {
        Self {
            explicit: None,
            inner: Mutex::new(None),
        }
    }

    /// Pin to a specific `/dev/videoN` node. Useful when the host has
    /// multiple loopback devices and the operator wants a stable
    /// mapping. Disables the dynamic-add path even if available.
    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        Self {
            explicit: Some(path.into()),
            inner: Mutex::new(None),
        }
    }

    /// Currently registered node path, if any.
    pub async fn device_path(&self) -> Option<PathBuf> {
        self.inner.lock().await.as_ref().map(|d| d.path.clone())
    }
}

impl Default for V4l2LoopbackSink {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl VirtualCameraSink for V4l2LoopbackSink {
    async fn register(&self, name: &str, format: CameraFormat) -> Result<(), CameraError> {
        let label = build_card_label(name);
        let (path, owned_nr) = match self.explicit.clone() {
            Some(p) => (p, None),
            None => acquire_node(&label, format.width, format.height)?,
        };
        let device = Device::with_path(&path).map_err(|e| {
            tracing::warn!(path = %path.display(), error = %e, "open v4l2 device failed");
            // If we just dyn-added the node and can't open it, tear
            // it back out so we don't leak a kernel device on every
            // failed connect attempt.
            if let Some(nr) = owned_nr {
                if let Err(rm) = dyn_ctl::remove(nr) {
                    tracing::warn!(nr, error = %rm, "rollback remove failed");
                }
            }
            CameraError::Io(e)
        })?;
        let fourcc = match format.pixel_format {
            CameraPixelFormat::Yuyv => FourCC::new(b"YUYV"),
            CameraPixelFormat::Nv12 => FourCC::new(b"NV12"),
            CameraPixelFormat::Mjpeg => FourCC::new(b"MJPG"),
        };
        let fmt = Format::new(format.width, format.height, fourcc);
        if let Err(e) = Output::set_format(&device, &fmt) {
            if let Some(nr) = owned_nr {
                if let Err(rm) = dyn_ctl::remove(nr) {
                    tracing::warn!(nr, error = %rm, "rollback remove failed");
                }
            }
            return Err(CameraError::Io(e));
        }
        tracing::info!(
            path = %path.display(),
            label = %label,
            owned = owned_nr.is_some(),
            w = format.width,
            h = format.height,
            fourcc = %fmt.fourcc,
            "v4l2loopback registered"
        );
        *self.inner.lock().await = Some(RegisteredDevice {
            device,
            path,
            label,
            owned_nr,
        });
        Ok(())
    }

    async fn unregister(&self) -> Result<(), CameraError> {
        let mut guard = self.inner.lock().await;
        if let Some(reg) = guard.take() {
            tracing::info!(
                path = %reg.path.display(),
                label = %reg.label,
                owned = reg.owned_nr.is_some(),
                "v4l2loopback released"
            );
            // Drop the kernel handle BEFORE removing the loopback
            // node — REMOVE refuses with EBUSY while any fd points
            // at the device.
            drop(reg.device);
            if let Some(nr) = reg.owned_nr {
                if let Err(e) = dyn_ctl::remove(nr) {
                    tracing::warn!(nr, error = %e, "dyn-ctl remove failed");
                }
            }
        }
        Ok(())
    }

    async fn write_frame(&self, frame: Bytes) -> Result<(), CameraError> {
        let guard = self.inner.lock().await;
        let reg = guard.as_ref().ok_or(CameraError::BackendUnavailable)?;
        // POSIX write(2) is allowed to return a short count even
        // without error; v4l2loopback's per-buffer ringbuffer caps
        // individual writes at the negotiated buffer size, so a
        // frame larger than one ring slot completes across several
        // syscalls. Loop until the whole frame is flushed or the
        // kernel returns a real error. EINTR / EAGAIN retry; any
        // other failure surfaces.
        let fd = reg.device.handle().fd();
        let total = frame.len();
        let mut offset = 0usize;
        while offset < total {
            let n = unsafe {
                libc::write(
                    fd,
                    frame.as_ptr().add(offset) as *const c_void,
                    total - offset,
                )
            };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(libc::EINTR) | Some(libc::EAGAIN) => continue,
                    _ => return Err(CameraError::Io(err)),
                }
            }
            if n == 0 {
                // 0-byte write without error means the kernel cannot
                // make progress on this fd (e.g. no consumer hooked
                // up). Surface as a soft error so the caller can
                // decide; spinning here would burn CPU silently.
                return Err(CameraError::Io(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "v4l2loopback write returned 0",
                )));
            }
            offset += n as usize;
        }
        Ok(())
    }
}

/// Try dynamic add via `/dev/v4l2loopback`; fall back to scanning for
/// the first existing output node if the control device is missing.
fn acquire_node(
    label: &str,
    width: u32,
    height: u32,
) -> Result<(PathBuf, Option<u32>), CameraError> {
    if dyn_ctl::control_available() {
        match dyn_ctl::add(label, width, height) {
            Ok((nr, path)) => {
                tracing::debug!(nr, path = %path.display(), label, "v4l2loopback dyn-add");
                // Give udev a tick to apply the GROUP/MODE rule on
                // the freshly-created node before we open it.
                std::thread::sleep(std::time::Duration::from_millis(80));
                return Ok((path, Some(nr)));
            }
            Err(e) => {
                tracing::warn!(error = %e, "dyn-ctl add failed; falling back to scan");
            }
        }
    } else {
        tracing::debug!("/dev/v4l2loopback absent; using static scan");
    }
    find_free_output_device().map(|p| (p, None))
}

/// Construct the card_label string shown in browser pickers / OBS.
/// Mirrors v4l2loopback-ctl conventions: 31-byte payload with the
/// `(Ansync)` suffix so multiple ansync-managed devices are visibly
/// distinct from unrelated loopbacks.
fn build_card_label(name: &str) -> String {
    let trimmed: String = name.chars().filter(|c| !c.is_control()).collect();
    let base = if trimmed.trim().is_empty() {
        "Ansync".to_string()
    } else {
        format!("{} (Ansync)", trimmed.trim())
    };
    // v4l2loopback enforces 31 visible chars + NUL. Trim at character
    // boundaries to avoid splitting a multi-byte UTF-8 codepoint.
    let mut out = String::with_capacity(31);
    for c in base.chars() {
        if out.len() + c.len_utf8() > 31 {
            break;
        }
        out.push(c);
    }
    out
}

/// Scan `/dev/video*` for the first node advertising
/// `V4L2_CAP_VIDEO_OUTPUT`. Returns `BackendUnavailable` if nothing
/// matches — typically because the kernel module isn't loaded.
fn find_free_output_device() -> Result<PathBuf, CameraError> {
    let dir = std::fs::read_dir("/dev").map_err(CameraError::Io)?;
    let mut candidates: Vec<PathBuf> = dir
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("video"))
                .unwrap_or(false)
        })
        .collect();
    candidates.sort();
    for path in candidates {
        let device = match Device::with_path(&path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let caps = match device.query_caps() {
            Ok(c) => c,
            Err(_) => continue,
        };
        if caps.capabilities.contains(CapFlags::VIDEO_OUTPUT) {
            tracing::debug!(path = %path.display(), card = %caps.card, "found v4l2 output node");
            return Ok(path);
        }
        drop(caps);
        drop(device);
    }
    Err(CameraError::BackendUnavailable)
}

/// Sanity helper for tests + early misconfiguration detection.
pub fn is_video_output_path(path: impl AsRef<Path>) -> bool {
    let Ok(device) = Device::with_path(&path) else {
        return false;
    };
    let Ok(caps) = device.query_caps() else {
        return false;
    };
    caps.capabilities.contains(CapFlags::VIDEO_OUTPUT)
}

/// Buffer type marker re-exported so callers wiring custom mmap paths
/// can pick the right output type. The default `write(2)` path
/// doesn't need it.
pub const OUTPUT_BUFFER_TYPE: BufferType = BufferType::VideoOutput;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_appends_ansync_marker() {
        assert_eq!(build_card_label("Pixel 9"), "Pixel 9 (Ansync)");
    }

    #[test]
    fn label_falls_back_to_ansync_when_blank() {
        assert_eq!(build_card_label(""), "Ansync");
        assert_eq!(build_card_label("   "), "Ansync");
    }

    #[test]
    fn label_caps_at_31_bytes_on_char_boundary() {
        let long = "ANameThatGoesOnAndOnAndOnAndOnAndOn";
        let out = build_card_label(long);
        assert!(out.len() <= 31, "label too long: {} bytes", out.len());
        // Still UTF-8.
        assert!(out.is_char_boundary(out.len()));
    }

    #[test]
    fn label_handles_multibyte_truncation_safely() {
        let multi = "日本語のながーいなまえ漢字";
        let out = build_card_label(multi);
        assert!(out.len() <= 31);
        assert!(out.is_char_boundary(out.len()));
    }
}
