//! Audio backend selector — picks PipeWire / snd-aloop / cpal based on
//! the `ANSYNC_AUDIO_BACKEND` env var, with a sensible fallback chain
//! when unset.
//!
//! Why explicit fail on user-pinned mismatches: silent fallback to cpal
//! (because `pipewire` failed to init) makes "ok but why am I not on
//! pipewire?" debug sessions painful. If the user wrote `pipewire`
//! they want pipewire — the caller learns about the failure.
//!
//! For unset env, we try `pipewire → aloop → cpal` in order. The first
//! one that initialises wins. cpal is the universal floor: it talks to
//! the system default device through cpal's ALSA shim, available on
//! any Linux with PipeWire / Pulse / vanilla ALSA.

use std::sync::Arc;

use crate::{AudioBackend, AudioError, SharedAudioBackend};

#[cfg(feature = "aloop-backend")]
use crate::AloopBackend;
#[cfg(feature = "cpal-backend")]
use crate::CpalBackend;
#[cfg(feature = "pipewire-backend")]
use crate::PipewireBackend;

/// Stable enum of every backend kind that may exist in the binary.
/// Used for logging / D-Bus introspection. Variants always present so
/// the rest of the codebase can match exhaustively regardless of which
/// feature flags are on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioBackendKind {
    Pipewire,
    Aloop,
    Cpal,
}

impl AudioBackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pipewire => "pipewire",
            Self::Aloop => "aloop",
            Self::Cpal => "cpal",
        }
    }
}

impl std::fmt::Display for AudioBackendKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

const ENV_VAR: &str = "ANSYNC_AUDIO_BACKEND";

/// Read `ANSYNC_AUDIO_BACKEND` and return the matching backend handle.
/// Errors:
///   * `BackendUnavailable` — selected backend is compiled in but its
///     `new()` failed (no daemon socket, no /dev/snd/aloop, etc.).
///   * `Io` wrapping the unknown-name string when the env var contains
///     a value none of the compiled backends recognise.
pub fn select_audio_backend() -> Result<SharedAudioBackend, AudioError> {
    let pref = std::env::var(ENV_VAR).ok();
    match pref.as_deref() {
        Some("pipewire") => build_pipewire(),
        Some("aloop") => build_aloop(),
        Some("cpal") => build_cpal(),
        Some(other) => Err(AudioError::Io(std::io::Error::other(format!(
            "{ENV_VAR}={other}: unknown audio backend (expected pipewire|aloop|cpal)"
        )))),
        None => auto_detect(),
    }
}

fn auto_detect() -> Result<SharedAudioBackend, AudioError> {
    #[cfg(feature = "pipewire-backend")]
    if let Ok(b) = PipewireBackend::new() {
        tracing::info!("audio backend auto-detect: pipewire");
        return Ok(Arc::new(b));
    }
    #[cfg(feature = "aloop-backend")]
    if let Ok(b) = AloopBackend::new() {
        tracing::info!("audio backend auto-detect: aloop");
        return Ok(Arc::new(b));
    }
    #[cfg(feature = "cpal-backend")]
    {
        tracing::info!("audio backend auto-detect: cpal");
        return Ok(Arc::new(CpalBackend::new()) as Arc<dyn AudioBackend>);
    }
    #[cfg(not(any(
        feature = "pipewire-backend",
        feature = "aloop-backend",
        feature = "cpal-backend",
    )))]
    {
        Err(AudioError::BackendUnavailable)
    }
}

fn build_pipewire() -> Result<SharedAudioBackend, AudioError> {
    #[cfg(feature = "pipewire-backend")]
    {
        Ok(Arc::new(PipewireBackend::new()?))
    }
    #[cfg(not(feature = "pipewire-backend"))]
    {
        Err(AudioError::Io(std::io::Error::other(
            "ANSYNC_AUDIO_BACKEND=pipewire but build lacks `pipewire-backend` feature",
        )))
    }
}

fn build_aloop() -> Result<SharedAudioBackend, AudioError> {
    #[cfg(feature = "aloop-backend")]
    {
        Ok(Arc::new(AloopBackend::new()?))
    }
    #[cfg(not(feature = "aloop-backend"))]
    {
        Err(AudioError::Io(std::io::Error::other(
            "ANSYNC_AUDIO_BACKEND=aloop but build lacks `aloop-backend` feature",
        )))
    }
}

fn build_cpal() -> Result<SharedAudioBackend, AudioError> {
    #[cfg(feature = "cpal-backend")]
    {
        Ok(Arc::new(CpalBackend::new()))
    }
    #[cfg(not(feature = "cpal-backend"))]
    {
        Err(AudioError::Io(std::io::Error::other(
            "ANSYNC_AUDIO_BACKEND=cpal but build lacks `cpal-backend` feature",
        )))
    }
}
