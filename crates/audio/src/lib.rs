//! Virtual audio source / sink abstraction.
//!
//! Backends ordered by preference (override via `ANSYNC_AUDIO_BACKEND`):
//!   * `pipewire` — creates a native PipeWire virtual source/sink per
//!     paired device, labelled `<peer> (Ansync)`. Default on NixOS /
//!     modern desktops.
//!   * `aloop` — kernel-level `snd-aloop` loopback. Same UX as
//!     v4l2loopback but for audio. Fallback when PipeWire isn't on
//!     the box.
//!   * `cpal` — talks to the system default device via cpal's ALSA
//!     shim. Portable but doesn't create virtual nodes (mic forwarding
//!     plays through the host's existing default output).
//!
//! The trait is dyn-friendly (`Box<dyn AudioBackend>`) so the
//! daemon picks one at init and the rest of the codebase doesn't care
//! which is live.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

#[cfg(feature = "cpal-backend")]
pub mod cpal_backend;

#[cfg(feature = "cpal-backend")]
pub use cpal_backend::{CpalBackend, CpalSink, CpalSource};

#[cfg(feature = "opus")]
pub mod opus_codec;

#[cfg(feature = "opus")]
pub use opus_codec::{
    AUDIO_BITRATE_BPS, OPUS_CHANNELS, OPUS_FRAME_SAMPLES, OPUS_SAMPLE_RATE, OpusDecoderWrap,
    OpusEncoderWrap, VOIP_BITRATE_BPS,
};

#[cfg(feature = "pipewire-backend")]
pub mod pipewire_backend;

#[cfg(feature = "pipewire-backend")]
pub use pipewire_backend::PipewireBackend;

#[cfg(feature = "aloop-backend")]
pub mod aloop_backend;

#[cfg(feature = "aloop-backend")]
pub use aloop_backend::AloopBackend;

pub mod select;
pub use select::{AudioBackendKind, select_audio_backend};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    S16Le,
    F32Le,
}

#[derive(Debug, Clone, Copy)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u8,
    pub format: SampleFormat,
}

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("backend unavailable")]
    BackendUnavailable,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Erased source / sink pair returned by every backend. Boxed because
/// the daemon stores `Arc<dyn AudioBackend>` and we can't carry GATs
/// through `dyn` — and the slight allocation cost is negligible
/// against the audio frame budget (one box per peer, not per packet).
pub type BoxedSource = Box<dyn AudioSource + Send>;
pub type BoxedSink = Box<dyn AudioSink + Send>;

#[async_trait]
pub trait AudioBackend: Send + Sync {
    async fn create_source(&self, name: &str, format: AudioFormat)
        -> Result<BoxedSource, AudioError>;

    async fn create_sink(&self, name: &str, format: AudioFormat)
        -> Result<BoxedSink, AudioError>;

    /// Human-readable name for logs / D-Bus introspection. Cheap to
    /// hand back — each backend hardcodes a string literal.
    fn kind(&self) -> AudioBackendKind;
}

#[async_trait]
pub trait AudioSource: Send {
    async fn read(&mut self) -> Result<Bytes, AudioError>;
}

#[async_trait]
pub trait AudioSink: Send {
    async fn write(&mut self, samples: Bytes) -> Result<(), AudioError>;
}

/// Convenience shared backend handle. Cloned by every audio entry that
/// needs to allocate a source / sink.
pub type SharedAudioBackend = Arc<dyn AudioBackend>;
