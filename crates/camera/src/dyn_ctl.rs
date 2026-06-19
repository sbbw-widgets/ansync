//! Dynamic v4l2loopback device management via `/dev/v4l2loopback`.
//!
//! v4l2loopback's per-device `card_label` is fixed at module load
//! when the legacy `options v4l2loopback ... card_label="..."` path
//! is used. The kernel module ALSO exposes a control character
//! device at `/dev/v4l2loopback` that accepts `ADD` / `REMOVE`
//! ioctls, and the `ADD` ioctl takes a `card_label` per call. That's
//! how `v4l2loopback-ctl add --card-label="Pixel 9"` does it.
//!
//! This module is a thin Rust wrapper over that ioctl interface, so
//! `V4l2LoopbackSink::register` can create a per-peer node labelled
//! with the Android device name (e.g. "Pixel 9 (Ansync)") and remove
//! it on disconnect. Browsers / OBS / Discord see the peer name in
//! the picker without any module reload.
//!
//! Layout pinned to v4l2loopback 0.15.x, which is what nixpkgs
//! currently ships. Earlier versions used a slightly different
//! struct order; we validate via `version()` before trusting the
//! struct layout, and refuse to talk to anything < 0.15 so we don't
//! corrupt the kernel struct silently.

use std::os::fd::AsRawFd;
use std::os::raw::c_void;
use std::path::PathBuf;

use crate::CameraError;

const CTL_PATH: &str = "/dev/v4l2loopback";
const CARD_LABEL_LEN: usize = 32;
const MIN_SUPPORTED_VERSION: u32 = (0 << 16) | (15 << 8);

#[repr(C)]
#[derive(Default)]
struct LoopbackConfig {
    output_nr: i32,
    unused: i32,
    card_label: [u8; CARD_LABEL_LEN],
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
    max_buffers: i32,
    max_openers: i32,
    debug: i32,
    announce_all_caps: i32,
}

// ioctl numbers from v4l2loopback.h (magic '~' = 0x7E, struct size 72).
// _IOR('~', 0, __u32)                                       — VERSION
const V4L2LOOPBACK_CTL_VERSION: u64 = 0x80047E00;
// _IOW('~', 1, struct v4l2_loopback_config)                 — ADD
const V4L2LOOPBACK_CTL_ADD: u64 = 0x40487E01;
// _IOW('~', 2, __u32)                                       — REMOVE
const V4L2LOOPBACK_CTL_REMOVE: u64 = 0x40047E02;

const _: () = assert!(std::mem::size_of::<LoopbackConfig>() == 72);

/// Whether `/dev/v4l2loopback` exists and we can open it.
pub fn control_available() -> bool {
    std::path::Path::new(CTL_PATH).exists()
}

/// Read the loaded module version. Useful for capability gating —
/// callers should refuse to call [`add`] if the running module is
/// older than what we coded the struct layout against.
pub fn version() -> Result<u32, CameraError> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(CTL_PATH)
        .map_err(CameraError::Io)?;
    let mut v: u32 = 0;
    let ret = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            V4L2LOOPBACK_CTL_VERSION,
            &mut v as *mut u32 as *mut c_void,
        )
    };
    if ret < 0 {
        return Err(CameraError::Io(std::io::Error::last_os_error()));
    }
    Ok(v)
}

/// Format the kernel version code (KERNEL_VERSION(maj,min,bug)) into
/// a human string for logs.
pub fn format_version(v: u32) -> String {
    format!("{}.{}.{}", (v >> 16) & 0xff, (v >> 8) & 0xff, v & 0xff)
}

/// Create a new loopback device with the given card label and
/// maximum frame size. Returns `(device_nr, /dev/videoN)`.
///
/// `max_width` / `max_height` are upper bounds — the consumer can
/// still negotiate any lower resolution. We pass the actual encoded
/// resolution as both min/max so v4l2loopback advertises a stable
/// frame size to clients (Chromium picks the listed max otherwise).
pub fn add(label: &str, width: u32, height: u32) -> Result<(u32, PathBuf), CameraError> {
    let ver = version()?;
    if ver < MIN_SUPPORTED_VERSION {
        tracing::warn!(
            running = %format_version(ver),
            need = %format_version(MIN_SUPPORTED_VERSION),
            "v4l2loopback control kABI predates our struct layout; refusing dynamic add"
        );
        return Err(CameraError::BackendUnavailable);
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(CTL_PATH)
        .map_err(CameraError::Io)?;
    let mut cfg = LoopbackConfig::default();
    cfg.output_nr = -1;
    cfg.unused = -1;
    // Truncate the label to 31 bytes so the C-side NUL terminator
    // fits. Going over silently corrupts neighbouring fields on the
    // kernel side, so we hard-cap here.
    let bytes = label.as_bytes();
    let copy_len = bytes.len().min(CARD_LABEL_LEN - 1);
    cfg.card_label[..copy_len].copy_from_slice(&bytes[..copy_len]);
    cfg.min_width = width;
    cfg.max_width = width;
    cfg.min_height = height;
    cfg.max_height = height;
    // Match v4l2loopback-ctl defaults so consumers behave the same.
    cfg.max_buffers = 0;
    cfg.max_openers = 0;
    cfg.debug = 0;
    // 0 ⇒ exclusive_caps=1 ⇒ device shows as pure capture to apps.
    cfg.announce_all_caps = 0;
    let ret = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            V4L2LOOPBACK_CTL_ADD,
            &cfg as *const _ as *const c_void,
        )
    };
    if ret < 0 {
        return Err(CameraError::Io(std::io::Error::last_os_error()));
    }
    let device_nr = ret as u32;
    Ok((device_nr, PathBuf::from(format!("/dev/video{device_nr}"))))
}

/// Remove a previously-added loopback device. Idempotent — a missing
/// device returns success rather than surfacing the kernel's ENODEV.
pub fn remove(device_nr: u32) -> Result<(), CameraError> {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(CTL_PATH)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(CameraError::Io(e)),
    };
    let nr: u32 = device_nr;
    let ret = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            V4L2LOOPBACK_CTL_REMOVE,
            &nr as *const u32 as *const c_void,
        )
    };
    if ret < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ENODEV) {
            return Ok(());
        }
        return Err(CameraError::Io(err));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_is_72_bytes() {
        // Mirrors the static_assert above. Catches anyone bumping a
        // field without rechecking the kernel ABI.
        assert_eq!(std::mem::size_of::<LoopbackConfig>(), 72);
    }

    #[test]
    fn version_formats_kernel_version_macro() {
        let v: u32 = (0 << 16) | (15 << 8) | 3;
        assert_eq!(format_version(v), "0.15.3");
    }
}
