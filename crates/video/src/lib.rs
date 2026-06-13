//! Video decode and presentation.
//!
//! Wraps `ferricast-decoder` for HW-accelerated decode. The H.264 path
//! auto-selects NVDEC → VA-API → openh264 SW fallback; the H.265 path
//! auto-selects NVDEC → VA-API and refuses to bring up without HW
//! decode (no SW fallback exists for HEVC).
//!
//! Step 6 wires the decode hot path: callers drive the decoder by
//! pushing `Bytes` packets through [`VideoDecoder::feed`] and pull
//! decoded frames out via [`VideoDecoder::take`]. The latest-frame
//! buffer is owned by the decoder instance (no thread-local state),
//! so the producer and consumer can live on different tokio tasks.
//!
//! Presentation lives outside this crate today: `ansyncd` consumes
//! [`DecodedFrame`] and uploads it to a wgpu / egui texture. The
//! [`FrameSink`] trait is defined here so future presenters (e.g. a
//! virtual camera v4l2loopback sink) share the same plumbing.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use ferricast_core::{
    CapturedFrame, Codec as FerricastCodec, DecoderConfig as FerricastDecoderConfig, EncodedFrame,
    PixelFormat as FerricastPixelFormat, VideoDecoder as FerricastVideoDecoder,
};
use ferricast_decoder::{H264Decoder, H265Decoder};
use tracing::{debug, info, warn};

pub mod feed;

/// Codecs the ansync video pipeline ever produces / consumes. Strict
/// subset of `ferricast_core::Codec` — VP8 / VP9 are out of scope
/// because no Android encoder we negotiate against emits them in the
/// configurations ansync sets up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    H264,
    H265,
}

impl VideoCodec {
    fn to_ferricast(self) -> FerricastCodec {
        match self {
            VideoCodec::H264 => FerricastCodec::H264,
            VideoCodec::H265 => FerricastCodec::H265,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 8-bit Y plane (`width × height`) followed by interleaved UV
    /// plane (`width × height/2`). Stride applies to both planes
    /// equally — backends that align the pitch to 256 / 512 bytes
    /// preserve that alignment for the UV plane as well.
    Nv12,
    /// 8-bit Y, U, V planes, each at half-stride for U / V.
    I420,
    /// Packed BGRA8, one byte per channel, B first in memory. This is
    /// what `openh264` and the VA-API H.264 readback emit.
    Bgra8,
    /// Packed RGBA8, R first. Reserved for paths that already speak
    /// RGB order — the decoder facade does not emit this today, but a
    /// future converter or test feeder may.
    Rgba8,
}

/// A single decoded frame, ready for the presentation sink. `data`
/// length is `stride * height` for packed formats and `stride *
/// height * 3 / 2` for NV12 / I420 (Y plane in-stride, UV plane
/// directly after).
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    /// Y-plane stride for NV12 / I420; packed-pixel row stride for
    /// BGRA / RGBA. Always `>= width * bytes_per_pixel_y`.
    pub stride: u32,
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
    #[error("no common video codec between peers")]
    NoCommonCodec,
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

/// Capabilities advertised by one end of the link — the codecs its
/// hardware can encode and decode. Built from the device capability
/// table the daemon negotiates at pairing time. H.265 entries imply
/// the device can do at least Main profile (8-bit 4:2:0); higher
/// profiles are out of scope at negotiation time.
#[derive(Debug, Clone, Default)]
pub struct CodecCapabilities {
    pub can_encode: Vec<VideoCodec>,
    pub can_decode: Vec<VideoCodec>,
}

impl CodecCapabilities {
    pub fn h264_only() -> Self {
        Self {
            can_encode: vec![VideoCodec::H264],
            can_decode: vec![VideoCodec::H264],
        }
    }

    pub fn h264_and_h265() -> Self {
        Self {
            can_encode: vec![VideoCodec::H264, VideoCodec::H265],
            can_decode: vec![VideoCodec::H264, VideoCodec::H265],
        }
    }
}

/// Pick the codec for a screen-mirror stream from device →
/// host. H.265 wins iff:
///
/// 1. The Android side can encode it.
/// 2. The local host can decode it (the [`local_decoder_caps`] probe
///    has confirmed NVDEC or VA-API support).
///
/// Otherwise falls back to H.264 — every modern Android device
/// encodes H.264, and the H.264 facade has openh264 SW fallback so
/// decode never fails to bring up.
pub fn negotiate_codec(
    peer: &CodecCapabilities,
    local: &CodecCapabilities,
) -> Result<VideoCodec, VideoError> {
    if peer.can_encode.contains(&VideoCodec::H265) && local.can_decode.contains(&VideoCodec::H265) {
        return Ok(VideoCodec::H265);
    }
    if peer.can_encode.contains(&VideoCodec::H264) && local.can_decode.contains(&VideoCodec::H264) {
        return Ok(VideoCodec::H264);
    }
    Err(VideoError::NoCommonCodec)
}

/// One-shot probe of which codecs the local host can decode. Result
/// is cached for the process lifetime — probing NVDEC / VA-API costs
/// a CUDA / DRM round-trip, so re-doing it on every negotiation would
/// add latency for no benefit (the answer can't change without a
/// driver swap).
pub fn local_decoder_caps() -> &'static CodecCapabilities {
    use std::sync::OnceLock;
    static CACHE: OnceLock<CodecCapabilities> = OnceLock::new();
    CACHE.get_or_init(|| {
        let mut caps = CodecCapabilities {
            can_decode: vec![VideoCodec::H264],
            can_encode: Vec::new(),
        };
        let mut probe = H265Decoder::new();
        let probe_cfg = FerricastDecoderConfig {
            codec: FerricastCodec::H265,
            width: 1280,
            height: 720,
            pixel_format: FerricastPixelFormat::Nv12,
        };
        match probe.configure(&probe_cfg) {
            Ok(()) => {
                info!("local decoder caps: H.264 + H.265");
                caps.can_decode.push(VideoCodec::H265);
            }
            Err(e) => {
                debug!(error = %e, "H.265 HW decode unavailable; advertising H.264 only");
            }
        }
        caps
    })
}

/// Concrete decoder wrapping the ferricast facade for one chosen codec.
///
/// Either variant satisfies [`VideoDecoder`]; outer code dispatches on
/// the codec but doesn't need to care which HW backend ferricast picked
/// underneath. Frames come back as `CapturedFrame` and are converted to
/// ansync's [`DecodedFrame`] shape on the way out.
///
/// The "latest frame" slot is owned by the instance, not a
/// thread-local, so the decoder can be driven from one tokio task
/// while another task pulls frames out for the sink. Live screen
/// mirror prefers latency over completeness: when the decoder
/// produces frames faster than the sink consumes them, [`feed`]
/// overwrites the slot rather than queueing.
pub struct HostDecoder {
    inner: HostDecoderInner,
    latest: Arc<Mutex<Option<CapturedFrame>>>,
}

enum HostDecoderInner {
    H264(H264Decoder),
    H265(H265Decoder),
}

impl HostDecoder {
    /// Bring up a decoder for the negotiated codec. Configures
    /// `ferricast` with a dimension hint so backends that pre-allocate
    /// surface pools size them correctly on first frame.
    pub fn configure(codec: VideoCodec, width: u32, height: u32) -> Result<Self, VideoError> {
        let cfg = FerricastDecoderConfig {
            codec: codec.to_ferricast(),
            width,
            height,
            pixel_format: FerricastPixelFormat::Nv12,
        };
        let inner = match codec {
            VideoCodec::H264 => {
                let mut d = H264Decoder::new();
                d.configure(&cfg)
                    .map_err(|e| VideoError::DecoderUnavailable(format!("H.264: {e}")))?;
                HostDecoderInner::H264(d)
            }
            VideoCodec::H265 => {
                let mut d = H265Decoder::new();
                d.configure(&cfg)
                    .map_err(|e| VideoError::DecoderUnavailable(format!("H.265: {e}")))?;
                HostDecoderInner::H265(d)
            }
        };
        Ok(Self {
            inner,
            latest: Arc::new(Mutex::new(None)),
        })
    }
}

#[async_trait]
impl VideoDecoder for HostDecoder {
    fn codec(&self) -> VideoCodec {
        match &self.inner {
            HostDecoderInner::H264(_) => VideoCodec::H264,
            HostDecoderInner::H265(_) => VideoCodec::H265,
        }
    }

    async fn feed(&mut self, packet: Bytes) -> Result<(), VideoError> {
        // ferricast decoders consume one EncodedFrame at a time and
        // may return zero or one frame per packet (HW pipelines often
        // buffer the first IDR). The call is synchronous — cros-libva
        // is non-Send and shiguredo's NVDEC consumes a CUDA context
        // bound to its creator thread, so we don't spawn a worker.
        let frame = EncodedFrame {
            codec: self.codec().to_ferricast(),
            data: packet,
            timestamp_us: 0,
            is_keyframe: false,
            duration_us: None,
            pts_dts: (0, 0),
        };
        let outcome = match &mut self.inner {
            HostDecoderInner::H264(d) => d.decode(frame),
            HostDecoderInner::H265(d) => d.decode(frame),
        };
        match outcome {
            Ok(Some(captured)) => {
                if let Ok(mut slot) = self.latest.lock() {
                    *slot = Some(captured);
                }
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(e) => {
                warn!(error = %e, "decode failed");
                Err(VideoError::Decode(e.to_string()))
            }
        }
    }

    async fn take(&mut self) -> Result<Option<DecodedFrame>, VideoError> {
        let captured = match self.latest.lock() {
            Ok(mut slot) => slot.take(),
            Err(_) => None,
        };
        Ok(captured.map(captured_to_decoded))
    }
}

fn captured_to_decoded(frame: CapturedFrame) -> DecodedFrame {
    let timestamp_us = frame.timestamp_us();
    let width = frame.width();
    let height = frame.height();
    let src_format = frame.pixel_format();
    let (data, stride) = match frame.into_cpu() {
        Ok(raw) => (raw.data, raw.stride),
        Err(_) => (Bytes::new(), 0),
    };
    let format = match src_format {
        FerricastPixelFormat::Nv12 => PixelFormat::Nv12,
        FerricastPixelFormat::I420 => PixelFormat::I420,
        FerricastPixelFormat::Bgra => PixelFormat::Bgra8,
        FerricastPixelFormat::Rgba => PixelFormat::Rgba8,
    };
    DecodedFrame {
        width,
        height,
        stride,
        format,
        data,
        pts_us: timestamp_us,
    }
}
