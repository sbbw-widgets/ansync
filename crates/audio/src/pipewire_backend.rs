//! PipeWire virtual source / sink backend.
//!
//! Each paired peer gets its own pair of nodes labelled
//! `<peer> (Ansync Source)` / `<peer> (Ansync Sink)` — apps like
//! Discord, OBS and the browser see them in the device picker exactly
//! the way they see v4l2loopback cameras.
//!
//! Implementation is intentionally stubbed in Step 18b. Real impl
//! lands in 18c via `pw-cli create-node adapter media.class=Audio/...`
//! spawn + raw socket I/O on the resulting node. The stub returns
//! `BackendUnavailable` from `new()` so the auto-detect chain falls
//! through to aloop / cpal during dev.

use async_trait::async_trait;

use crate::{
    AudioBackend, AudioBackendKind, AudioError, AudioFormat, BoxedSink, BoxedSource,
};

pub struct PipewireBackend {
    _marker: (),
}

impl PipewireBackend {
    pub fn new() -> Result<Self, AudioError> {
        // 18c: probe `pw-cli info 0`. If pipewire-pulse / pipewire is
        // up, return Ok. Until then we always fail so auto-detect
        // skips us cleanly.
        Err(AudioError::BackendUnavailable)
    }
}

#[async_trait]
impl AudioBackend for PipewireBackend {
    async fn create_source(
        &self,
        _name: &str,
        _format: AudioFormat,
    ) -> Result<BoxedSource, AudioError> {
        Err(AudioError::BackendUnavailable)
    }

    async fn create_sink(
        &self,
        _name: &str,
        _format: AudioFormat,
    ) -> Result<BoxedSink, AudioError> {
        Err(AudioError::BackendUnavailable)
    }

    fn kind(&self) -> AudioBackendKind {
        AudioBackendKind::Pipewire
    }
}
