//! Video decode and presentation.
//!
//! Wraps `ferricast-decoder` for HW-accelerated decode. The H.264 path
//! auto-selects NVDEC → VA-API → openh264 SW fallback; the H.265 path
//! auto-selects NVDEC → VA-API and refuses to bring up without HW
//! decode (no SW fallback exists for HEVC).
//!
//! Step 5 wires the codec negotiation + decoder dispatch. Step 6 will
//! plug a wgpu / eframe sink into [`FrameSink::present`].

use std::sync::OnceLock;

use async_trait::async_trait;
use bytes::Bytes;
use ferricast_core::{
    CapturedFrame, Codec as FerricastCodec, DecoderConfig as FerricastDecoderConfig, EncodedFrame,
    PixelFormat as FerricastPixelFormat, VideoDecoder as FerricastVideoDecoder,
};
use ferricast_decoder::{H264Decoder, H265Decoder};
use tracing::{debug, info, warn};

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
    static CACHE: OnceLock<CodecCapabilities> = OnceLock::new();
    CACHE.get_or_init(|| {
        let mut caps = CodecCapabilities {
            // H.264 is always present — `ferricast-decoder` ships an
            // openh264 SW fallback, so the facade can always bring up
            // some backend.
            can_decode: vec![VideoCodec::H264],
            // ansync today only ever decodes inbound video. Encoding
            // is the Android side's job; leave the local "can_encode"
            // empty so [`negotiate_codec`] doesn't accidentally claim
            // we encode.
            can_encode: Vec::new(),
        };
        // Probe H.265. We just try to bring up the facade with a
        // small config and tear it down — successful `configure()`
        // implies at least one HW backend is available.
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
/// ansync's [`DecodedFrame`] shape on the way out — pixel format /
/// stride / pts mapping is straight passthrough today (NVDEC and VA-API
/// both emit NV12; openh264 emits BGRA, mapped to `Rgba8` as the closest
/// match the sink consumes).
pub enum HostDecoder {
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
        match codec {
            VideoCodec::H264 => {
                let mut d = H264Decoder::new();
                d.configure(&cfg)
                    .map_err(|e| VideoError::DecoderUnavailable(format!("H.264: {e}")))?;
                Ok(Self::H264(d))
            }
            VideoCodec::H265 => {
                let mut d = H265Decoder::new();
                d.configure(&cfg)
                    .map_err(|e| VideoError::DecoderUnavailable(format!("H.265: {e}")))?;
                Ok(Self::H265(d))
            }
        }
    }
}

#[async_trait]
impl VideoDecoder for HostDecoder {
    fn codec(&self) -> VideoCodec {
        match self {
            HostDecoder::H264(_) => VideoCodec::H264,
            HostDecoder::H265(_) => VideoCodec::H265,
        }
    }

    async fn feed(&mut self, packet: Bytes) -> Result<(), VideoError> {
        // ferricast decoders consume one EncodedFrame at a time and
        // may return zero or one frame per packet (HW pipelines often
        // buffer the first IDR). We pump synchronously inside a
        // blocking task — `cros-libva` is non-Send and shiguredo's
        // NVDEC consumes a CUDA context that's bound to its creator
        // thread; spinning them off would need a worker thread.
        let frame = EncodedFrame {
            codec: self.codec().to_ferricast(),
            data: packet,
            timestamp_us: 0,
            is_keyframe: false,
            duration_us: None,
            pts_dts: (0, 0),
        };
        let outcome = match self {
            HostDecoder::H264(d) => d.decode(frame),
            HostDecoder::H265(d) => d.decode(frame),
        };
        match outcome {
            Ok(Some(captured)) => {
                // Stash the latest frame on the decoder so the next
                // `take` can return it. Today we only carry the
                // newest decoded frame — pacing is the sink's job, so
                // dropping older queued frames is the right policy
                // for a live mirror.
                cache_latest(captured);
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
        Ok(take_latest().map(captured_to_decoded))
    }
}

// ── Latest-frame buffer ───────────────────────────────────────────
//
// Single-slot, thread-local cache. Live screen mirror prefers latency
// over completeness — when the decoder produces frames faster than the
// sink consumes them, the right policy is to drop everything but the
// freshest. A `RefCell<Option<CapturedFrame>>` keyed thread-local
// gives us O(1) feed / take without contention; the decoder and sink
// run on the same tokio task in the daemon today.

thread_local! {
    static LATEST: std::cell::RefCell<Option<CapturedFrame>> =
        const { std::cell::RefCell::new(None) };
}

fn cache_latest(frame: CapturedFrame) {
    LATEST.with(|cell| *cell.borrow_mut() = Some(frame));
}

fn take_latest() -> Option<CapturedFrame> {
    LATEST.with(|cell| cell.borrow_mut().take())
}

fn captured_to_decoded(frame: CapturedFrame) -> DecodedFrame {
    // Ferricast hands us either a CPU NV12 (NVDEC / openh264 with our
    // conversion) or a GPU surface. The CPU readback path materialises
    // bytes; we forward them with the same width / height / stride
    // semantics. The pts is whatever the source frame carried.
    let timestamp_us = frame.timestamp_us();
    let pixel = match frame.pixel_format() {
        FerricastPixelFormat::Nv12 => PixelFormat::Nv12,
        FerricastPixelFormat::I420 => PixelFormat::I420,
        FerricastPixelFormat::Bgra | FerricastPixelFormat::Rgba => PixelFormat::Rgba8,
    };
    let width = frame.width();
    let height = frame.height();
    let bytes = match frame.into_cpu() {
        Ok(raw) => raw.data,
        Err(_) => Bytes::new(),
    };
    DecodedFrame {
        width,
        height,
        format: pixel,
        data: bytes,
        pts_us: timestamp_us,
    }
}
