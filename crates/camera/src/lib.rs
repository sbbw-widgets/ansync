//! Virtual camera sink abstraction.
//!
//! Default backend: v4l2loopback (kernel module). The trait lets a future
//! PipeWire-camera backend slot in without touching consumers. The Linux
//! device name is set to the Android device name so it appears clearly
//! in browser camera pickers, OBS, etc.

use async_trait::async_trait;
use bytes::Bytes;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraPixelFormat {
    Yuyv,
    Nv12,
    Mjpeg,
}

#[derive(Debug, Clone, Copy)]
pub struct CameraFormat {
    pub width: u32,
    pub height: u32,
    pub fps: u8,
    pub pixel_format: CameraPixelFormat,
}

#[derive(Debug, thiserror::Error)]
pub enum CameraError {
    #[error("backend unavailable")]
    BackendUnavailable,
    #[error("device busy")]
    DeviceBusy,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[async_trait]
pub trait VirtualCameraSink: Send + Sync {
    async fn register(&self, name: &str, format: CameraFormat) -> Result<(), CameraError>;
    async fn unregister(&self) -> Result<(), CameraError>;
    async fn write_frame(&self, frame: Bytes) -> Result<(), CameraError>;
}
