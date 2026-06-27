//! PipeWire virtual source / sink backend (no subprocess).
//!
//! Each `create_sink(label)` call spawns a dedicated worker thread
//! hosting its own `MainLoop` + `Stream`. The stream is a Playback
//! stream (`Direction::Output`) — to PipeWire it appears as an audio
//! source that other apps (Discord, OBS, browsers) can capture from
//! under the description `<label> (Ansync)`. PCM written via
//! `AudioSink::write` is fed into a ring buffer; the stream's
//! `process` callback drains the ring into the next ready buffer.
//!
//! `create_source(label)` mirrors the shape with Direction::Input +
//! `STREAM_CAPTURE_SINK=true` so the stream records from the default
//! sink monitor — i.e., whatever the host is hearing. Capture
//! callbacks push PCM into a ring; `AudioSource::read` awaits it.
//!
//! Threading model:
//!   * One worker thread per stream owns its `MainLoop` + `Stream`.
//!   * PCM crosses the thread boundary through `Arc<PcmRing>`
//!     (`std::sync::Mutex<VecDeque<Bytes>>` + `tokio::sync::Notify`).
//!   * On Drop we quit the loop via `MainLoop::quit()` invoked from
//!     the worker thread itself through a thread-safe shutdown flag.
//!
//! Why one thread per stream instead of a shared `ThreadLoop`: the
//! per-peer node count is tiny (one or two streams per paired device)
//! and dedicated threads remove a layer of locking + simplify the
//! teardown story. Each stream lifecycle is independent, so a crashed
//! peer's stream doesn't affect the others.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use pipewire as pw;
use pw::spa;
use pw::spa::pod::Pod;
use tokio::sync::Notify;
use tracing::{info, warn};

use crate::{
    AudioBackend, AudioBackendKind, AudioError, AudioFormat, AudioSink, AudioSource, BoxedSink,
    BoxedSource, SampleFormat,
};

/// Cap on queue depth so a slow consumer can't grow memory without
/// bound — at 20 ms / 3 840 B frames, 64 entries is ~1.28 s buffered.
/// Way more than enough for jitter; oldest entries are dropped past
/// this watermark.
const RING_CAPACITY: usize = 64;

pub struct PipewireBackend;

impl PipewireBackend {
    pub fn new() -> Result<Self, AudioError> {
        // Probe by creating a MainLoop + Context + connecting. If
        // PipeWire isn't running this fails fast; we drop everything
        // and report unavailable so the selector falls through.
        pw::init();
        let probe = std::thread::Builder::new()
            .name("ansync-pw-probe".into())
            .spawn(|| -> Result<(), pw::Error> {
                let ml = pw::main_loop::MainLoop::new(None)?;
                let ctx = pw::context::Context::new(&ml)?;
                let _core = ctx.connect(None)?;
                Ok(())
            })
            .map_err(AudioError::Io)?;
        match probe.join() {
            Ok(Ok(())) => Ok(Self),
            Ok(Err(e)) => {
                warn!(error = %e, "PipeWire probe connect failed");
                Err(AudioError::BackendUnavailable)
            }
            Err(_) => {
                warn!("PipeWire probe thread panicked");
                Err(AudioError::BackendUnavailable)
            }
        }
    }
}

#[async_trait]
impl AudioBackend for PipewireBackend {
    async fn create_source(
        &self,
        name: &str,
        format: AudioFormat,
    ) -> Result<BoxedSource, AudioError> {
        let src = PwCaptureSource::open(name, format)?;
        Ok(Box::new(src))
    }

    async fn create_sink(&self, name: &str, format: AudioFormat) -> Result<BoxedSink, AudioError> {
        let sink = PwVirtualSink::open(name, format)?;
        Ok(Box::new(sink))
    }

    fn kind(&self) -> AudioBackendKind {
        AudioBackendKind::Pipewire
    }
}

/// Shared PCM ring buffer between the worker thread (PipeWire process
/// callback) and async-land (`AudioSink::write` / `AudioSource::read`).
struct PcmRing {
    queue: Mutex<VecDeque<Bytes>>,
    notify: Notify,
    /// Set by the worker thread when the PipeWire stream errors out
    /// or the loop is quit; readers exit with `BackendUnavailable` so
    /// the daemon can tear down the route cleanly.
    closed: AtomicBool,
}

impl PcmRing {
    fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::with_capacity(RING_CAPACITY)),
            notify: Notify::new(),
            closed: AtomicBool::new(false),
        }
    }

    fn push(&self, bytes: Bytes) {
        let mut q = self.queue.lock().expect("pcm ring poisoned");
        if q.len() >= RING_CAPACITY {
            // Drop the oldest frame — a stale buffer is worse than
            // dropping one, and the upstream will catch up on the
            // next callback.
            q.pop_front();
        }
        q.push_back(bytes);
        self.notify.notify_one();
    }

    fn try_pop(&self) -> Option<Bytes> {
        self.queue.lock().expect("pcm ring poisoned").pop_front()
    }

    async fn pop(&self) -> Option<Bytes> {
        loop {
            if self.closed.load(Ordering::Acquire) {
                return None;
            }
            if let Some(b) = self.try_pop() {
                return Some(b);
            }
            self.notify.notified().await;
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
}

fn spa_format(fmt: SampleFormat) -> spa::param::audio::AudioFormat {
    match fmt {
        SampleFormat::S16Le => spa::param::audio::AudioFormat::S16LE,
        SampleFormat::F32Le => spa::param::audio::AudioFormat::F32LE,
    }
}

fn bytes_per_frame(fmt: SampleFormat, channels: u8) -> usize {
    let sample_bytes = match fmt {
        SampleFormat::S16Le => 2,
        SampleFormat::F32Le => 4,
    };
    sample_bytes * channels as usize
}

/// Convert a friendly label into a safe PipeWire `node.name`
/// (alphanumeric + `_`). Apps see the pretty version via
/// `node.description`; this stays stable for graph matching.
fn safe_node_name(label: &str) -> String {
    let stem: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("ansync_{stem}")
}

/// Builds the audio format SPA Pod that pins the stream to the
/// requested rate / channels / sample format. PipeWire treats this as
/// `EnumFormat` so the server gets exactly one option to accept.
fn audio_format_pod(
    format: AudioFormat,
) -> Result<Vec<u8>, AudioError> {
    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa_format(format.format));
    audio_info.set_rate(format.sample_rate);
    audio_info.set_channels(format.channels as u32);
    let obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let bytes = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .map_err(|e| AudioError::Io(std::io::Error::other(e.to_string())))?
    .0
    .into_inner();
    Ok(bytes)
}

/// Virtual source published to PipeWire — apps that record from
/// `<label> (Ansync)` see the PCM we write into the ring.
pub struct PwVirtualSink {
    label: String,
    ring: Arc<PcmRing>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl PwVirtualSink {
    fn open(label: &str, format: AudioFormat) -> Result<Self, AudioError> {
        let ring = Arc::new(PcmRing::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        let label_owned = label.to_string();
        let pcm_bytes_per_frame = bytes_per_frame(format.format, format.channels);
        let node_name = safe_node_name(label);
        let description = format!("{label} (Ansync)");

        let ring_for_thread = ring.clone();
        let shutdown_for_thread = shutdown.clone();
        let thread = thread::Builder::new()
            .name(format!("ansync-pw-sink-{label}"))
            .spawn(move || {
                let _ = run_virtual_sink(
                    label_owned,
                    node_name,
                    description,
                    format,
                    pcm_bytes_per_frame,
                    ring_for_thread,
                    shutdown_for_thread,
                );
            })
            .map_err(AudioError::Io)?;
        Ok(Self {
            label: label.to_string(),
            ring,
            shutdown,
            thread: Some(thread),
        })
    }
}

fn run_virtual_sink(
    label: String,
    node_name: String,
    description: String,
    format: AudioFormat,
    bytes_per_frame: usize,
    ring: Arc<PcmRing>,
    shutdown: Arc<AtomicBool>,
) -> Result<(), pw::Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoop::new(None)?;
    let context = pw::context::Context::new(&mainloop)?;
    let core = context.connect(None)?;

    // Magic prop combo to make a Playback stream appear in app
    // device pickers as a virtual audio source (= virtual mic):
    //   * `media.class = Audio/Source/Virtual` registers the node as
    //     a Source, not a Stream/Output/Audio (which apps ignore for
    //     mic pickers).
    //   * `node.virtual = true` tells wireplumber not to expect a
    //     hardware device backing it.
    //   * `node.always-process = true` keeps the `process` callback
    //     firing even when no consumer is connected. Without this,
    //     PipeWire is lazy: it stops pulling data from us until an
    //     app opens the mic — so the ring buffer fills up and the
    //     first packet a consumer reads is stale by seconds.
    //   * `audio.position = [FL,FR]` pins the stereo layout so apps
    //     don't see "unknown" channels.
    // Discord, OBS, Firefox / Chromium all honor these props through
    // the standard PipeWire/Pulse compat layer.
    let props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::MEDIA_ROLE => "Communication",
        *pw::keys::MEDIA_CLASS => "Audio/Source/Virtual",
        *pw::keys::NODE_NAME => node_name.clone(),
        *pw::keys::NODE_DESCRIPTION => description.clone(),
        *pw::keys::NODE_VIRTUAL => "true",
        *pw::keys::NODE_ALWAYS_PROCESS => "true",
        // Block PipeWire's idle-suspend logic — `state=suspended` keeps
        // the process callback from firing, so the ring fills up
        // until apps actively connect. With this off + always-process,
        // PipeWire keeps polling us so audio is fresh when a consumer
        // arrives.
        *pw::keys::NODE_SUSPEND_ON_IDLE => "false",
        "audio.position" => "FL,FR",
        *pw::keys::APP_NAME => "ansync",
    };

    let stream = pw::stream::Stream::new(&core, &label, props)?;
    let ring_cb = ring.clone();
    let label_state = label.clone();

    let _listener = stream
        .add_local_listener_with_user_data::<()>(())
        .state_changed(move |_, _, old, new| {
            info!(label = %label_state, ?old, ?new, "pipewire virtual sink state");
        })
        .process(move |stream, _| {
            let mut buffer = match stream.dequeue_buffer() {
                Some(b) => b,
                None => return,
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data = &mut datas[0];
            let Some(slot) = data.data() else {
                return;
            };
            let capacity = slot.len();
            let mut written = 0usize;
            // Pull as many ring frames as fit; pad with silence on
            // shortfall to avoid skipping (PipeWire treats short
            // chunks as glitches).
            while written < capacity {
                let chunk = match ring_cb.try_pop() {
                    Some(c) => c,
                    None => break,
                };
                let take = chunk.len().min(capacity - written);
                slot[written..written + take].copy_from_slice(&chunk[..take]);
                written += take;
                if take < chunk.len() {
                    // Push back the leftover; this happens when a
                    // ring frame straddles the PipeWire buffer
                    // boundary.
                    let rest = chunk.slice(take..);
                    ring_cb.push(rest);
                }
            }
            if written < capacity {
                slot[written..].fill(0);
            }
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.size_mut() = capacity as u32;
            *chunk.stride_mut() = bytes_per_frame as i32;
        })
        .register()?;

    let pod_bytes = audio_format_pod(format).map_err(|_| pw::Error::CreationFailed)?;
    let mut params = [Pod::from_bytes(&pod_bytes).ok_or(pw::Error::CreationFailed)?];

    stream.connect(
        spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    // Virtual sources stay Paused by default — there's no consumer
    // when no app has the mic open yet. `set_active(true)` flips the
    // node into Streaming so the process callback fires immediately
    // (so the PCM ring drains and pavucontrol's level meter moves).
    if let Err(e) = stream.set_active(true) {
        warn!(label, error = %e, "pipewire stream.set_active failed");
    }

    info!(label, node_name, "pipewire virtual sink up");

    // Watch the shutdown flag in 50 ms ticks — gives Drop a clean
    // path without poking the loop from outside.
    let shutdown_for_timer = shutdown.clone();
    let mainloop_for_timer = mainloop.clone();
    let timer = mainloop
        .loop_()
        .add_timer(move |_| {
            if shutdown_for_timer.load(Ordering::Acquire) {
                mainloop_for_timer.quit();
            }
        });
    timer
        .update_timer(
            Some(std::time::Duration::from_millis(50)),
            Some(std::time::Duration::from_millis(50)),
        )
        .into_result()
        .map_err(|_| pw::Error::CreationFailed)?;

    mainloop.run();
    ring.close();
    info!(label, "pipewire virtual sink torn down");
    Ok(())
}

#[async_trait]
impl AudioSink for PwVirtualSink {
    async fn write(&mut self, samples: Bytes) -> Result<(), AudioError> {
        if self.ring.closed.load(Ordering::Acquire) {
            return Err(AudioError::BackendUnavailable);
        }
        self.ring.push(samples);
        Ok(())
    }
}

impl Drop for PwVirtualSink {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.ring.close();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        info!(label = %self.label, "PwVirtualSink dropped");
    }
}

/// Capture stream from the default sink monitor — records system
/// audio so the daemon can forward it to the peer's speaker.
pub struct PwCaptureSource {
    label: String,
    ring: Arc<PcmRing>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl PwCaptureSource {
    fn open(label: &str, format: AudioFormat) -> Result<Self, AudioError> {
        let ring = Arc::new(PcmRing::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        let label_owned = label.to_string();
        let node_name = safe_node_name(label);
        let description = format!("{label} (Ansync Capture)");

        let ring_for_thread = ring.clone();
        let shutdown_for_thread = shutdown.clone();
        let thread = thread::Builder::new()
            .name(format!("ansync-pw-src-{label}"))
            .spawn(move || {
                let _ = run_capture_source(
                    label_owned,
                    node_name,
                    description,
                    format,
                    ring_for_thread,
                    shutdown_for_thread,
                );
            })
            .map_err(AudioError::Io)?;
        Ok(Self {
            label: label.to_string(),
            ring,
            shutdown,
            thread: Some(thread),
        })
    }
}

fn run_capture_source(
    label: String,
    node_name: String,
    description: String,
    format: AudioFormat,
    ring: Arc<PcmRing>,
    shutdown: Arc<AtomicBool>,
) -> Result<(), pw::Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoop::new(None)?;
    let context = pw::context::Context::new(&mainloop)?;
    let core = context.connect(None)?;

    let props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::NODE_NAME => node_name.clone(),
        *pw::keys::NODE_DESCRIPTION => description.clone(),
        *pw::keys::APP_NAME => "ansync",
        // Record from sink monitors instead of physical mics — we
        // want "what is the user hearing", not "what is the user
        // saying into the mic".
        *pw::keys::STREAM_CAPTURE_SINK => "true",
    };

    let stream = pw::stream::Stream::new(&core, &label, props)?;
    let ring_cb = ring.clone();

    let _listener = stream
        .add_local_listener_with_user_data::<()>(())
        .process(move |stream, _| {
            let mut buffer = match stream.dequeue_buffer() {
                Some(b) => b,
                None => return,
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data = &mut datas[0];
            let size = data.chunk().size() as usize;
            if size == 0 {
                return;
            }
            if let Some(slot) = data.data() {
                let take = size.min(slot.len());
                let mut buf = BytesMut::with_capacity(take);
                buf.extend_from_slice(&slot[..take]);
                ring_cb.push(buf.freeze());
            }
        })
        .register()?;

    let pod_bytes = audio_format_pod(format).map_err(|_| pw::Error::CreationFailed)?;
    let mut params = [Pod::from_bytes(&pod_bytes).ok_or(pw::Error::CreationFailed)?];

    stream.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    info!(label, node_name, "pipewire capture source up");

    let shutdown_for_timer = shutdown.clone();
    let mainloop_for_timer = mainloop.clone();
    let timer = mainloop
        .loop_()
        .add_timer(move |_| {
            if shutdown_for_timer.load(Ordering::Acquire) {
                mainloop_for_timer.quit();
            }
        });
    timer
        .update_timer(
            Some(std::time::Duration::from_millis(50)),
            Some(std::time::Duration::from_millis(50)),
        )
        .into_result()
        .map_err(|_| pw::Error::CreationFailed)?;

    mainloop.run();
    ring.close();
    info!(label, "pipewire capture source torn down");
    Ok(())
}

#[async_trait]
impl AudioSource for PwCaptureSource {
    async fn read(&mut self) -> Result<Bytes, AudioError> {
        match self.ring.pop().await {
            Some(b) => Ok(b),
            None => Err(AudioError::BackendUnavailable),
        }
    }
}

impl Drop for PwCaptureSource {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.ring.close();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        info!(label = %self.label, "PwCaptureSource dropped");
    }
}
