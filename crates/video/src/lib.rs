//! Video decode and presentation.
//!
//! Wraps `ferricast-decoder` for HW-accelerated decode (NVDEC / VAAPI /
//! openh264 SW fallback) and produces frames suitable for upload to a
//! wgpu texture. HEVC support is added in Step 5 by extending
//! `ferricast-decoder` (NVENC / VAAPI paths).

use async_trait::async_trait;
use bytes::Bytes;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    H265,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Nv12,
    I420,
    Rgba8,
}

#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub data: Bytes,
    pub pts_us: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum VideoError {
    #[error("decoder unavailable: {0}")]
    DecoderUnavailable(String),
    #[error("decode failed: {0}")]
    Decode(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[async_trait]
pub trait VideoDecoder: Send {
    fn codec(&self) -> VideoCodec;
    async fn feed(&mut self, packet: Bytes) -> Result<(), VideoError>;
    async fn take(&mut self) -> Result<Option<DecodedFrame>, VideoError>;
}

#[async_trait]
pub trait FrameSink: Send {
    async fn present(&mut self, frame: DecodedFrame) -> Result<(), VideoError>;
}
