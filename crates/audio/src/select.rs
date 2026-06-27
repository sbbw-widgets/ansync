//! Audio backend selector — picks PipeWire or cpal based on the
//! `ANSYNC_AUDIO_BACKEND` env var, with a fallback chain when unset.
//!
//! Why explicit fail on user-pinned mismatches: silent fallback to cpal
//! (because `pipewire` failed to init) makes "ok but why am I not on
//! pipewire?" debug sessions painful. If the user wrote `pipewire`
//! they want pipewire — the caller learns about the failure.
//!
//! For unset env, we try `pipewire → cpal`. cpal is the universal
//! floor: it talks to the system default device through cpal's ALSA
//! shim, available on any Linux with PipeWire / Pulse / vanilla ALSA.
//! snd-aloop was considered but dropped — implementing the kernel
//! ALSA PCM ioctl protocol pure-Rust is brittle, and `PipewireBackend`
//! already exposes per-peer virtual mics with the same UX.

use std::sync::Arc;

use crate::{AudioBackend, AudioError, SharedAudioBackend};

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
    Cpal,
}

impl AudioBackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pipewire => "pipewire",
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
///     `new()` failed (no PipeWire daemon, etc.).
///   * `Io` wrapping the unknown-name string when the env var contains
///     a value none of the compiled backends recognise.
pub fn select_audio_backend() -> Result<SharedAudioBackend, AudioError> {
    let pref = std::env::var(ENV_VAR).ok();
    match pref.as_deref() {
        Some("pipewire") => build_pipewire(),
        Some("cpal") => build_cpal(),
        Some(other) => Err(AudioError::Io(std::io::Error::other(format!(
            "{ENV_VAR}={other}: unknown audio backend (expected pipewire|cpal)"
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
    #[cfg(feature = "cpal-backend")]
    {
        tracing::info!("audio backend auto-detect: cpal");
        return Ok(Arc::new(CpalBackend::new()) as Arc<dyn AudioBackend>);
    }
    #[cfg(not(any(feature = "pipewire-backend", feature = "cpal-backend")))]
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
