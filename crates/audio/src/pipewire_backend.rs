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

/// Byte-level ring cap for the sink path. 48 kHz stereo S16 = 192 KB/s.
/// 512 KiB ≈ 2.66 s — enough headroom for any sane jitter; older bytes
/// get evicted past this watermark.
const SINK_BYTE_CAP: usize = 512 * 1024;

/// Sink prebuffer target: hold playback until this many bytes are
/// queued, then drain freely. 7 680 B = 2 × Opus frame (40 ms) — masks
/// network jitter without adding perceptible latency.
const SINK_PREBUFFER_BYTES: usize = 7_680;

/// Opus frame samples in a single packet. Used to pin PipeWire quantum
/// via `node.latency = "<samples>/<rate>"` so the process callback
/// pulls in exact frame-aligned chunks (no straddling).
const OPUS_FRAME_SAMPLES_PW: u32 = 960;

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

/// Byte-level continuous ring used by the sink path. The PipeWire
/// process callback drains exact `slot.len()` bytes per quantum — no
/// chunk boundaries to straddle, no time scramble. A prebuffer
/// threshold gates the first drain: until the buffer reaches
/// `SINK_PREBUFFER_BYTES`, the callback emits `size = 0` (PipeWire
/// treats that as no-data, no audible click). After priming, drains
/// run continuous; if the buffer ever empties we re-arm the prebuffer
/// gate, hiding underrun glitches as silence.
struct ByteRing {
    buf: Mutex<VecDeque<u8>>,
    closed: AtomicBool,
    primed: AtomicBool,
}

impl ByteRing {
    fn new() -> Self {
        Self {
            buf: Mutex::new(VecDeque::with_capacity(SINK_BYTE_CAP)),
            closed: AtomicBool::new(false),
            primed: AtomicBool::new(false),
        }
    }

    fn extend(&self, bytes: &[u8]) {
        let mut q = self.buf.lock().expect("byte ring poisoned");
        q.extend(bytes.iter().copied());
        // Drop oldest bytes if over cap — keeps memory bounded under a
        // slow consumer. Chops in 4 KiB swings so we don't pay the
        // pop_front cost per byte.
        while q.len() > SINK_BYTE_CAP {
            let drop_n = (q.len() - SINK_BYTE_CAP).min(4096);
            q.drain(..drop_n);
        }
        if !self.primed.load(Ordering::Acquire) && q.len() >= SINK_PREBUFFER_BYTES {
            self.primed.store(true, Ordering::Release);
        }
    }

    /// Drain exactly `slot.len()` bytes if primed and available;
    /// returns bytes actually written (0 when not primed or empty).
    fn drain_into(&self, slot: &mut [u8]) -> usize {
        if !self.primed.load(Ordering::Acquire) {
            return 0;
        }
        let mut q = self.buf.lock().expect("byte ring poisoned");
        let want = slot.len();
        let have = q.len();
        if have == 0 {
            // Re-arm prebuffer gate so the next quantum waits for
            // more data — avoids tiny dribbles that glitch.
            drop(q);
            self.primed.store(false, Ordering::Release);
            return 0;
        }
        let take = want.min(have);
        for (i, b) in q.drain(..take).enumerate() {
            slot[i] = b;
        }
        take
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
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
    ring: Arc<ByteRing>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl PwVirtualSink {
    fn open(label: &str, format: AudioFormat) -> Result<Self, AudioError> {
        let ring = Arc::new(ByteRing::new());
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
    ring: Arc<ByteRing>,
    shutdown: Arc<AtomicBool>,
) -> Result<(), pw::Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoop::new(None)?;
    let context = pw::context::Context::new(&mainloop)?;
    let core = context.connect(None)?;

    // Stage 1: ask the core to materialize a persistent
    // `support.null-audio-sink` node and publish it as `Audio/Sink`.
    //
    // Why not `Audio/Source/Virtual` directly: when the null-sink is
    // published as a pure Source, the adapter only exposes its
    // *output* ports (capture_FL/FR). The session manager then has no
    // input ports to route a Playback stream into, so `target.object`
    // on the feeder gets ignored and wireplumber falls back to the
    // default sink (= sound comes out the user's speakers, virtual
    // mic stays silent).
    //
    // Publishing as `Audio/Sink` instead gives the node real input
    // ports (`playback_FL/FR`) plus an auto-generated monitor source
    // (`<node>.monitor` → capture_FL/FR) that apps record from. The
    // feeder writes into the input ports; the monitor produces the
    // same PCM as a recordable source. Equivalent to
    // `pactl load-module module-null-sink sink_name=…`.
    let null_sink_props = pw::properties::properties! {
        "factory.name" => "support.null-audio-sink",
        "node.name" => node_name.clone(),
        "node.description" => description.clone(),
        "media.class" => "Audio/Sink",
        "audio.position" => "FL,FR",
        "audio.channels" => format!("{}", format.channels),
        "audio.rate" => format!("{}", format.sample_rate),
        // Surface the monitor source so apps can pick it up under a
        // pretty name (`<description> Monitor`) instead of having to
        // toggle "show monitors" in pavucontrol.
        "monitor.channel-volumes" => "true",
        // Drop the node when the proxy goes away — keeps tear-down
        // clean across daemon restarts. Real lifecycle is owned by
        // the worker thread.
        "object.linger" => "false",
    };
    let _null_sink_node: pw::node::Node = core
        .create_object("adapter", &null_sink_props)
        .map_err(|e| {
            warn!(label = %label, error = %e, "create_object null-audio-sink failed");
            pw::Error::CreationFailed
        })?;

    // Roundtrip so the server actually instantiates the node before
    // the feeder stream tries to target it by name.
    {
        use std::cell::Cell;
        use std::rc::Rc;
        let done = Rc::new(Cell::new(false));
        let pending = core.sync(0).map_err(|_| pw::Error::CreationFailed)?;
        let done_cb = done.clone();
        let ml_cb = mainloop.clone();
        let _l = core
            .add_listener_local()
            .done(move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == pending {
                    done_cb.set(true);
                    ml_cb.quit();
                }
            })
            .register();
        while !done.get() {
            mainloop.run();
        }
    }
    info!(label, node_name, "pipewire null-audio-sink registered");

    // Stage 2: open a playback Stream that feeds the null-sink. The
    // sink itself does the heavy lifting (it's a proper node), this
    // stream just shovels PCM into it. `target.object` pins routing
    // so the feeder never lands on the default speakers if no app is
    // capturing yet.
    let latency = format!("{}/{}", OPUS_FRAME_SAMPLES_PW, format.sample_rate);
    let stream_props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::NODE_NAME => format!("{node_name}_feeder"),
        *pw::keys::NODE_DESCRIPTION => format!("{description} feeder"),
        *pw::keys::APP_NAME => "ansync",
        "target.object" => node_name.clone(),
        // Keep emitting silence when the ring is empty so the feeder
        // never goes idle — wireplumber would otherwise re-route the
        // sink to a different source.
        *pw::keys::NODE_ALWAYS_PROCESS => "true",
        // Pin the PipeWire quantum to the Opus frame size (960 / 48000
        // = 20 ms). Without this PipeWire defaults to 1024-sample
        // quanta — every process callback strides past the 960-sample
        // packet boundary, dragging in a partial of the next packet
        // and creating audible buzz at the ~46 Hz boundary rate.
        *pw::keys::NODE_LATENCY => latency.clone(),
    };

    let stream = pw::stream::Stream::new(&core, &label, stream_props)?;
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
            // Continuous byte drain — no chunk straddling. Returns 0
            // until prebuffer primes, then drains exactly `capacity`
            // bytes per quantum as long as data is available.
            let drained = ring_cb.drain_into(slot);
            if drained == 0 {
                // No data: report empty chunk so PipeWire stays idle
                // for this quantum instead of replaying stale buffer
                // contents. ALWAYS_PROCESS keeps the node alive.
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.size_mut() = 0;
                *chunk.stride_mut() = bytes_per_frame as i32;
                return;
            }
            if drained < capacity {
                // Partial drain after priming — zero the tail so we
                // never replay stale memory from the SHM slot. Reports
                // only `drained` bytes to PipeWire so the silence
                // doesn't get treated as PCM.
                slot[drained..].fill(0);
            }
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.size_mut() = drained as u32;
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

    info!(label, node_name, "pipewire virtual mic feeder up");

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
        self.ring.extend(&samples);
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
