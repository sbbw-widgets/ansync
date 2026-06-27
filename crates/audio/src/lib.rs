//! Virtual audio source / sink abstraction.
//!
//! Default backend: PipeWire (creates null sinks and loopbacks per
//! paired device). The trait shape allows ALSA or JACK to be added
//! later without changing call sites.

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

#[async_trait]
pub trait AudioBackend: Send + Sync {
    type Source: AudioSource;
    type Sink: AudioSink;

    async fn create_source(
        &self,
        name: &str,
        format: AudioFormat,
    ) -> Result<Self::Source, AudioError>;

    async fn create_sink(
        &self,
        name: &str,
        format: AudioFormat,
    ) -> Result<Self::Sink, AudioError>;
}

#[async_trait]
pub trait AudioSource: Send {
    async fn read(&mut self) -> Result<Bytes, AudioError>;
}

#[async_trait]
pub trait AudioSink: Send {
    async fn write(&mut self, samples: Bytes) -> Result<(), AudioError>;
}
