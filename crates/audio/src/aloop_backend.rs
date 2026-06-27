//! `snd-aloop` kernel loopback backend.
//!
//! Same idea as v4l2loopback but for audio: a kernel module exposes
//! pairs of `hw:Loopback,playback,N` ↔ `hw:Loopback,capture,N` ALSA
//! devices. Anything writing to the playback side appears on the
//! capture side as a usable mic — so `Discord` / `OBS` see "Loopback
//! PCM" devices they can select.
//!
//! Step 18b stub: returns `BackendUnavailable` from `new()` so
//! auto-detect falls through. Real impl lands in 18d (open
//! `/dev/snd/pcmCxDxp` for write, `/dev/snd/pcmCyDyc` for read, set
//! params via SNDCTL ioctls).

use async_trait::async_trait;

use crate::{
    AudioBackend, AudioBackendKind, AudioError, AudioFormat, BoxedSink, BoxedSource,
};

pub struct AloopBackend {
    _marker: (),
}

impl AloopBackend {
    pub fn new() -> Result<Self, AudioError> {
        Err(AudioError::BackendUnavailable)
    }
}

#[async_trait]
impl AudioBackend for AloopBackend {
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
        AudioBackendKind::Aloop
    }
}
