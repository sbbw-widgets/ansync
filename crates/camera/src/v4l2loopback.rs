//! v4l2loopback-backed `VirtualCameraSink`.
//!
//! Open path: scan `/dev/video*`, pick the first node that advertises
//! `V4L2_CAP_VIDEO_OUTPUT` (v4l2loopback nodes do) and let the user
//! pin a specific node via [`V4l2LoopbackSink::with_path`].
//!
//! Frame path: raw NV12 / YUV420 bytes written straight to the device
//! fd. v4l2loopback honours plain `write(2)` on output devices, which
//! is far simpler than the mmap / DQBUF dance and avoids juggling
//! kernel-owned buffers for what is effectively a fan-out pipe.
//!
//! Naming: the kernel-level `card_label` is fixed at module load
//! time, so per-call renaming via ioctl isn't reliable across
//! v4l2loopback versions. The Android device name surfaces in our
//! D-Bus property + journald logs instead. The Nix module
//! (`nix/v4l2loopback.nix`) wires `card_label="Ansync"` so generic
//! consumers (browsers, OBS) at least see a stable, recognisable
//! string.

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

use crate::{CameraError, CameraFormat, CameraPixelFormat, VirtualCameraSink};

/// Concrete v4l2loopback sink. Holds the device under a tokio mutex
/// so concurrent `write_frame` callers serialise on the underlying
/// `write(2)` syscall (v4l2 doesn't define byte-level interleaving
/// guarantees).
pub struct V4l2LoopbackSink {
    /// Explicit node path. `None` ⇒ auto-discover at `register` time.
    explicit: Option<PathBuf>,
    inner: Mutex<Option<RegisteredDevice>>,
}

struct RegisteredDevice {
    device: Device,
    path: PathBuf,
    label: String,
}

impl V4l2LoopbackSink {
    /// Auto-discover a free v4l2loopback output node at `register`.
    pub fn new() -> Self {
        Self {
            explicit: None,
            inner: Mutex::new(None),
        }
    }

    /// Pin to a specific `/dev/videoN` node. Useful when the host has
    /// multiple loopback devices and the operator wants a stable
    /// mapping.
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
        let path = match self.explicit.clone() {
            Some(p) => p,
            None => find_free_output_device()?,
        };
        let device = Device::with_path(&path).map_err(|e| {
            tracing::warn!(path = %path.display(), error = %e, "open v4l2 device failed");
            CameraError::Io(e)
        })?;
        let fourcc = match format.pixel_format {
            CameraPixelFormat::Yuyv => FourCC::new(b"YUYV"),
            CameraPixelFormat::Nv12 => FourCC::new(b"NV12"),
            CameraPixelFormat::Mjpeg => FourCC::new(b"MJPG"),
        };
        let fmt = Format::new(format.width, format.height, fourcc);
        Output::set_format(&device, &fmt).map_err(CameraError::Io)?;
        tracing::info!(
            path = %path.display(),
            label = name,
            w = format.width,
            h = format.height,
            fourcc = %fmt.fourcc,
            "v4l2loopback registered"
        );
        *self.inner.lock().await = Some(RegisteredDevice {
            device,
            path,
            label: name.to_string(),
        });
        Ok(())
    }

    async fn unregister(&self) -> Result<(), CameraError> {
        let mut guard = self.inner.lock().await;
        if let Some(reg) = guard.take() {
            tracing::info!(path = %reg.path.display(), label = %reg.label, "v4l2loopback released");
        }
        Ok(())
    }

    async fn write_frame(&self, frame: Bytes) -> Result<(), CameraError> {
        let guard = self.inner.lock().await;
        let reg = guard.as_ref().ok_or(CameraError::BackendUnavailable)?;
        // We don't model frame size mismatches as errors — kernel
        // returns EINVAL via `write(2)` which we surface raw. Capping
        // here would mask companion-side wire bugs.
        let fd = reg.device.handle().fd();
        let written = unsafe {
            libc::write(fd, frame.as_ptr() as *const c_void, frame.len())
        };
        if written < 0 {
            return Err(CameraError::Io(std::io::Error::last_os_error()));
        }
        if (written as usize) != frame.len() {
            tracing::warn!(
                expected = frame.len(),
                wrote = written,
                "short write to v4l2loopback (frame partially submitted)"
            );
        }
        Ok(())
    }
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
