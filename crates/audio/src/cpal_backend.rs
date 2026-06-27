//! `cpal` AudioBackend.
//!
//! cpal abstracts over the host's native audio API. On Linux it
//! reaches PipeWire through PipeWire's ALSA shim (installed by
//! `pipewire-alsa` in nixpkgs and most distros) — same wire as if
//! we used `pipewire-rs` directly, minus the FFI dance.
//!
//! Topology:
//!
//!   capture default device ─▶ CpalSource ─▶ Bytes (S16LE) ─▶ QUIC
//!   QUIC ─▶ Bytes (S16LE) ─▶ CpalSink ─▶ playback default device
//!
//! Per-peer routing is done in `daemon-core`: each peer gets one
//! source + one sink, both labelled with the peer's friendly name
//! so the user can re-route them in `pavucontrol` / `helvum`.

use std::sync::Mutex as StdMutex;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::{
    AudioBackend, AudioBackendKind, AudioError, AudioFormat, AudioSink, AudioSource, BoxedSink,
    BoxedSource, SampleFormat,
};

/// Host backend. Cheap to construct; the cpal `Host` handle lives
/// inside each created stream rather than on the backend itself so
/// the backend stays `Send + Sync` without juggling the non-Send
/// cpal types.
pub struct CpalBackend;

impl CpalBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CpalBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AudioBackend for CpalBackend {
    async fn create_source(
        &self,
        name: &str,
        format: AudioFormat,
    ) -> Result<BoxedSource, AudioError> {
        Ok(Box::new(CpalSource::open(name, format)?))
    }

    async fn create_sink(
        &self,
        name: &str,
        format: AudioFormat,
    ) -> Result<BoxedSink, AudioError> {
        Ok(Box::new(CpalSink::open(name, format)?))
    }

    fn kind(&self) -> AudioBackendKind {
        AudioBackendKind::Cpal
    }
}

/// Capture stream. The cpal `Stream` is `!Send`, so we keep it
/// pinned behind a `StdMutex<Option<...>>` that we only touch from
/// the spawning thread (cpal callbacks run in cpal's own worker
/// threads, not ours). The sample channel is the only thing the
/// async side observes.
pub struct CpalSource {
    rx: Mutex<UnboundedReceiver<Bytes>>,
    /// Held purely to keep the stream alive until `CpalSource` is
    /// dropped. Wrapped in `StdMutex<Option<_>>` so we can take it
    /// out on drop without `Send` bound problems.
    _stream: StreamHandle,
}

pub struct CpalSink {
    tx: UnboundedSender<Bytes>,
    _stream: StreamHandle,
}

/// Newtype around the non-`Send` cpal stream. The held field is
/// load-bearing on drop only — the destructor stops the stream and
/// hands the device back to cpal's worker pool. Reads are
/// unnecessary; `dead_code` is silenced on the field rather than
/// the whole struct so a future `pause()` API addition isn't masked.
struct StreamHandle(#[allow(dead_code)] StdMutex<Option<cpal::Stream>>);
// SAFETY: `cpal::Stream` is `!Send` because its callback closures
// are run on cpal's own threads. We never move the held stream
// across threads — the only operation we perform is `drop()`,
// which cpal documents as safe from any thread.
unsafe impl Send for StreamHandle {}
unsafe impl Sync for StreamHandle {}

impl CpalSource {
    fn open(name: &str, format: AudioFormat) -> Result<Self, AudioError> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or(AudioError::BackendUnavailable)?;
        let supported = device
            .default_input_config()
            .map_err(|e| AudioError::Io(std::io::Error::other(e)))?;
        let config = cpal::StreamConfig {
            channels: format.channels as u16,
            sample_rate: cpal::SampleRate(format.sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };
        let (tx, rx) = unbounded_channel::<Bytes>();
        let err_label = name.to_string();
        let sample_format = supported.sample_format();
        let stream = match sample_format {
            cpal::SampleFormat::I16 => device.build_input_stream(
                &config,
                move |data: &[i16], _: &_| {
                    let mut buf = BytesMut::with_capacity(data.len() * 2);
                    for s in data {
                        buf.extend_from_slice(&s.to_le_bytes());
                    }
                    let _ = tx.send(buf.freeze());
                },
                move |e| warn!(label = %err_label, error = %e, "cpal source error"),
                None,
            ),
            cpal::SampleFormat::F32 => match format.format {
                SampleFormat::F32Le => device.build_input_stream(
                    &config,
                    move |data: &[f32], _: &_| {
                        let mut buf = BytesMut::with_capacity(data.len() * 4);
                        for s in data {
                            buf.extend_from_slice(&s.to_le_bytes());
                        }
                        let _ = tx.send(buf.freeze());
                    },
                    move |e| warn!(label = %err_label, error = %e, "cpal source error"),
                    None,
                ),
                SampleFormat::S16Le => device.build_input_stream(
                    &config,
                    move |data: &[f32], _: &_| {
                        let mut buf = BytesMut::with_capacity(data.len() * 2);
                        for &s in data {
                            let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                            buf.extend_from_slice(&v.to_le_bytes());
                        }
                        let _ = tx.send(buf.freeze());
                    },
                    move |e| warn!(label = %err_label, error = %e, "cpal source error"),
                    None,
                ),
            },
            other => {
                return Err(AudioError::Io(std::io::Error::other(format!(
                    "unsupported cpal sample format: {other:?}"
                ))));
            }
        };
        let stream = stream.map_err(|e| AudioError::Io(std::io::Error::other(e)))?;
        stream
            .play()
            .map_err(|e| AudioError::Io(std::io::Error::other(e)))?;
        info!(name, ?format, "cpal source up");
        Ok(Self {
            rx: Mutex::new(rx),
            _stream: StreamHandle(StdMutex::new(Some(stream))),
        })
    }
}

impl CpalSink {
    fn open(name: &str, format: AudioFormat) -> Result<Self, AudioError> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or(AudioError::BackendUnavailable)?;
        let supported = device
            .default_output_config()
            .map_err(|e| AudioError::Io(std::io::Error::other(e)))?;
        let config = cpal::StreamConfig {
            channels: format.channels as u16,
            sample_rate: cpal::SampleRate(format.sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };
        let (tx, mut rx) = unbounded_channel::<Bytes>();
        let err_label = name.to_string();
        let mut pending = BytesMut::new();
        let sample_format = supported.sample_format();
        let stream = match sample_format {
            cpal::SampleFormat::I16 => device.build_output_stream(
                &config,
                move |out: &mut [i16], _: &_| {
                    drain_into(&mut rx, &mut pending);
                    fill_i16(out, &mut pending);
                },
                move |e| warn!(label = %err_label, error = %e, "cpal sink error"),
                None,
            ),
            cpal::SampleFormat::F32 => device.build_output_stream(
                &config,
                move |out: &mut [f32], _: &_| {
                    drain_into(&mut rx, &mut pending);
                    fill_f32_from_s16(out, &mut pending);
                },
                move |e| warn!(label = %err_label, error = %e, "cpal sink error"),
                None,
            ),
            other => {
                return Err(AudioError::Io(std::io::Error::other(format!(
                    "unsupported cpal sample format: {other:?}"
                ))));
            }
        };
        let stream = stream.map_err(|e| AudioError::Io(std::io::Error::other(e)))?;
        stream
            .play()
            .map_err(|e| AudioError::Io(std::io::Error::other(e)))?;
        info!(name, ?format, "cpal sink up");
        Ok(Self {
            tx,
            _stream: StreamHandle(StdMutex::new(Some(stream))),
        })
    }
}

fn drain_into(rx: &mut UnboundedReceiver<Bytes>, pending: &mut BytesMut) {
    while let Ok(chunk) = rx.try_recv() {
        pending.extend_from_slice(&chunk);
    }
}

fn fill_i16(out: &mut [i16], pending: &mut BytesMut) {
    let n = out.len().min(pending.len() / 2);
    for (slot, pair) in out.iter_mut().take(n).zip(pending.chunks(2)) {
        *slot = i16::from_le_bytes([pair[0], pair[1]]);
    }
    let consumed = n * 2;
    let _ = pending.split_to(consumed);
    for slot in out.iter_mut().skip(n) {
        *slot = 0;
    }
}

fn fill_f32_from_s16(out: &mut [f32], pending: &mut BytesMut) {
    let n = out.len().min(pending.len() / 2);
    for (slot, pair) in out.iter_mut().take(n).zip(pending.chunks(2)) {
        let s = i16::from_le_bytes([pair[0], pair[1]]);
        *slot = (s as f32) / 32768.0;
    }
    let consumed = n * 2;
    let _ = pending.split_to(consumed);
    for slot in out.iter_mut().skip(n) {
        *slot = 0.0;
    }
}

#[async_trait]
impl AudioSource for CpalSource {
    async fn read(&mut self) -> Result<Bytes, AudioError> {
        let mut guard = self.rx.lock().await;
        match guard.recv().await {
            Some(b) => Ok(b),
            None => Err(AudioError::BackendUnavailable),
        }
    }
}

#[async_trait]
impl AudioSink for CpalSink {
    async fn write(&mut self, samples: Bytes) -> Result<(), AudioError> {
        self.tx
            .send(samples)
            .map_err(|_| AudioError::BackendUnavailable)
    }
}
