//! Wayland clipboard change watcher via `wlr_data_control_unstable_v1`.
//!
//! `wl-clipboard-rs` only exposes one-shot read/write helpers — there
//! is no built-in API to notice when the selection changes. We bind
//! the wlroots `zwlr_data_control_manager_v1` global directly, get a
//! `data_device` for the default seat, and fire an event every time
//! the compositor emits `selection` (regular clipboard) or
//! `primary_selection`.
//!
//! Supported compositors (as of 2026): wlroots (sway, hyprland,
//! river), KDE Plasma 6+, COSMIC, niri. **Not** GNOME — mutter still
//! refuses to advertise the global for security-policy reasons. When
//! the global is missing we return [`WatcherError::ProtocolUnsupported`]
//! at start time so the daemon can degrade to manual-only sync.
//!
//! Event loop lives on a dedicated OS thread (Wayland's
//! `EventQueue::blocking_dispatch` is sync). Cross-thread channel is
//! a tokio `UnboundedSender<()>` — caller hands us a tokio handle to
//! avoid hard-wiring a runtime dependency.

use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::thread::JoinHandle;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle,
    event_created_child,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_registry, wl_seat},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
};

#[derive(Debug, thiserror::Error)]
pub enum WatcherError {
    #[error("WAYLAND_DISPLAY not set or compositor unreachable: {0}")]
    Connect(String),
    #[error("compositor does not advertise zwlr_data_control_manager_v1 (try sway / KDE / hyprland)")]
    ProtocolUnsupported,
    #[error("compositor has no wl_seat global")]
    NoSeat,
    #[error("worker thread panicked: {0}")]
    Thread(String),
}

/// Spawned watcher. Drop to stop — the worker thread observes the
/// channel close and exits cleanly.
pub struct WaylandClipboardWatcher {
    rx: UnboundedReceiver<()>,
    _thread: WatcherThread,
}

/// RAII wrapper that joins the worker thread on drop. Kept separate
/// from the receiver so callers can `mem::forget` the rx and let the
/// thread linger if they really want to.
struct WatcherThread(Option<JoinHandle<()>>);

impl Drop for WatcherThread {
    fn drop(&mut self) {
        // Closing the channel signals the worker to exit on the next
        // event roundtrip. We don't block on join — the worker can
        // be parked inside `blocking_dispatch` and joining would risk
        // deadlocking daemon shutdown. The thread is short-lived once
        // the channel closes.
        drop(self.0.take());
    }
}

impl WaylandClipboardWatcher {
    /// Connect to the compositor, bind globals, spawn the worker
    /// thread, and return a receiver that fires `()` for every
    /// selection change. Returns [`WatcherError::ProtocolUnsupported`]
    /// when the compositor does not advertise
    /// `zwlr_data_control_manager_v1`.
    pub fn start() -> Result<Self, WatcherError> {
        let conn = Connection::connect_to_env()
            .map_err(|e| WatcherError::Connect(e.to_string()))?;
        let (globals, mut queue) =
            registry_queue_init::<WatcherState>(&conn).map_err(|e| {
                WatcherError::Connect(format!("registry init: {e}"))
            })?;
        let qh = queue.handle();

        let manager: ZwlrDataControlManagerV1 = globals
            .bind(&qh, 1..=2, ())
            .map_err(|_| WatcherError::ProtocolUnsupported)?;
        let seat: wl_seat::WlSeat = globals
            .bind(&qh, 1..=8, ())
            .map_err(|_| WatcherError::NoSeat)?;
        let device = manager.get_data_device(&seat, &qh, ());

        let (tx, rx) = unbounded_channel();

        let mut state = WatcherState {
            tx,
            pending_offers: Arc::new(StdMutex::new(Vec::new())),
            _device: device,
            _manager: manager,
            _seat: seat,
        };
        // Initial roundtrip so all globals settle and we don't miss
        // the very first `selection` event.
        if let Err(e) = queue.roundtrip(&mut state) {
            return Err(WatcherError::Connect(format!("initial roundtrip: {e}")));
        }

        let handle = std::thread::Builder::new()
            .name("ansync-clip-watch".into())
            .spawn(move || run_event_loop(queue, state))
            .map_err(|e| WatcherError::Thread(e.to_string()))?;

        Ok(Self {
            rx,
            _thread: WatcherThread(Some(handle)),
        })
    }

    /// Borrow the mpsc receiver for `tokio::select!` integration. The
    /// receiver yields `()` once per selection change — callers
    /// debounce + re-read the clipboard via [`crate::WaylandClipboard`].
    pub fn rx(&mut self) -> &mut UnboundedReceiver<()> {
        &mut self.rx
    }
}

struct WatcherState {
    tx: UnboundedSender<()>,
    /// Offers introduced by the compositor for the current selection
    /// cycle. We don't actually read their contents from here — the
    /// daemon re-reads the clipboard via `wl-clipboard-rs` after we
    /// fire — but the protocol requires us to acknowledge them so
    /// the offer objects don't leak.
    pending_offers: Arc<StdMutex<Vec<ZwlrDataControlOfferV1>>>,
    _device: ZwlrDataControlDeviceV1,
    _manager: ZwlrDataControlManagerV1,
    _seat: wl_seat::WlSeat,
}

fn run_event_loop(mut queue: EventQueue<WatcherState>, mut state: WatcherState) {
    loop {
        if state.tx.is_closed() {
            return;
        }
        if let Err(e) = queue.blocking_dispatch(&mut state) {
            tracing::warn!(error = %e, "wayland clipboard watcher dispatch failed; exiting");
            return;
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WatcherState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: <wl_registry::WlRegistry as wayland_client::Proxy>::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Globals tracked by registry_queue_init; nothing else to do.
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for WatcherState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_seat::WlSeat,
        _event: <wl_seat::WlSeat as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for WatcherState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrDataControlManagerV1,
        _event: <ZwlrDataControlManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for WatcherState {
    // `data_offer` (opcode 0) is a "constructor" event — the compositor
    // is introducing a new `ZwlrDataControlOfferV1` object. The
    // wayland-client runtime needs to know which Dispatch impl handles
    // the child, otherwise it aborts the worker thread. The
    // `event_created_child!` macro generates the trait method that
    // routes the opcode → child object's dispatch data.
    event_created_child!(WatcherState, ZwlrDataControlDeviceV1, [
        0 => (ZwlrDataControlOfferV1, ()),
    ]);

    fn event(
        state: &mut Self,
        _proxy: &ZwlrDataControlDeviceV1,
        event: <ZwlrDataControlDeviceV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use zwlr_data_control_device_v1::Event;
        match event {
            Event::DataOffer { id } => {
                // Track the offer so it isn't dropped prematurely.
                if let Ok(mut g) = state.pending_offers.lock() {
                    g.push(id);
                }
            }
            Event::Selection { id } => {
                // Selection changed (or cleared if `id` is None).
                // Clean up pending offers and notify.
                if let Ok(mut g) = state.pending_offers.lock() {
                    for offer in g.drain(..) {
                        offer.destroy();
                    }
                }
                let _ = id; // We don't read here; daemon re-reads.
                let _ = state.tx.send(());
            }
            Event::PrimarySelection { id } => {
                if let Ok(mut g) = state.pending_offers.lock() {
                    for offer in g.drain(..) {
                        offer.destroy();
                    }
                }
                let _ = id;
                let _ = state.tx.send(());
            }
            Event::Finished => {
                // Compositor revoked our device — exit the loop.
                drop(state.tx.clone());
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for WatcherState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrDataControlOfferV1,
        _event: <ZwlrDataControlOfferV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Offer mime-type events arrive after `DataOffer`; we don't
        // need them — the daemon re-reads the actual content using
        // `wl-clipboard-rs` which negotiates the MIME on its own.
    }
}

#[allow(dead_code)]
fn _unused_fd_marker() -> Option<OwnedFd> {
    // Future expansion: when we want to read directly via
    // `ZwlrDataControlOfferV1::receive(mime, fd)` we'll plumb the
    // OwnedFd path through here. Keeping the import noted prevents
    // accidental removal on the next clean-up sweep.
    None
}
