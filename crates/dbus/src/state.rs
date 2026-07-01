//! Shared state owned by the daemon and consumed by every D-Bus
//! interface impl. Kept in the `dbus` crate so the interfaces don't
//! need to depend on `daemon-core` (which would be a cycle).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use ansync_core::DeviceId;
use ansync_crypto::IdentityKeypair;
use ansync_pairing::PeerStore;
use ansync_permissions::PermissionsStore;
use tokio::sync::mpsc::UnboundedSender;

/// Lifecycle of a per-peer connection as surfaced over D-Bus.
///
/// Transitions are linear forward (Disconnected → Pairing →
/// Authenticated → Active) with `Disconnected` re-entered on
/// connection drop. The DMS widget pinta semáforo: gris para
/// Disconnected, amarillo para Pairing/Authenticated, verde para
/// Active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnState {
    #[default]
    Disconnected,
    /// Cable / Wi-Fi pair handshake in progress. Set by `ansyncctl
    /// pair` via the pairing surface — daemon-core only sees post-pair
    /// connections.
    Pairing,
    /// QUIC + Noise handshake complete; peer trusted but no Hello yet.
    Authenticated,
    /// Hello frame received in either direction — peer reachable and
    /// caps are fresh. Streams may or may not be open.
    Active,
}

impl ConnState {
    pub fn as_str(self) -> &'static str {
        match self {
            ConnState::Disconnected => "disconnected",
            ConnState::Pairing => "pairing",
            ConnState::Authenticated => "authenticated",
            ConnState::Active => "active",
        }
    }
}

/// Actions D-Bus interfaces dispatch back into `daemon-core`. Sent on
/// [`DaemonState::actions`]; the daemon spawns an action loop that
/// consumes the receiver and runs the appropriate task.
///
/// Post sender-initiates refactor (2026-07-01) the surface is
/// intentionally minimal: the daemon owns two outbound streams (audio
/// sink + clipboard + share) and everything else — mirror, camera,
/// mic — flows the other way and is triggered by the phone.
///
/// The enum sits in the `dbus` crate to avoid a cycle: D-Bus
/// interfaces own the sender; `daemon-core` owns the receiver.
#[derive(Debug, Clone)]
pub enum DaemonAction {
    /// Bring up the host → device audio sink route. The daemon opens
    /// a `StreamKind::Audio` outbound, encodes host PipeWire capture
    /// with Opus, and pushes it to the peer's speaker. Gated by
    /// `Permission::AudioOut`.
    StartAudioSink { device: DeviceId },
    /// Tear the audio sink route down.
    StopAudioSink { device: DeviceId },
    /// Read the host's Wayland clipboard and push it to `device`
    /// over a fresh `StreamKind::Clipboard`. Gated by
    /// `Permission::ClipboardOut`.
    SyncClipboard { device: DeviceId },
    /// Push one or more files to `device` over fresh
    /// `StreamKind::Files` streams (one stream per path). Reuses
    /// `ansync_files::send_file`. The companion's `AutoAcceptPolicy`
    /// drops the bytes under `incoming/{peer}/`; on Linux the action
    /// loop lands them in `download_dir`.
    SendFiles { device: DeviceId, paths: Vec<PathBuf> },
    /// Open `url` on `device`. One-shot `StreamKind::Url` stream
    /// carrying a postcard `Message::Url(UrlMessage)`. Receiver
    /// behaviour is per-platform (Linux opens directly, Android
    /// prompts) — see `ansync_proto::UrlMessage`.
    SendUrl { device: DeviceId, url: String },
}

pub struct DaemonState {
    pub identity: IdentityKeypair,
    pub device_name: String,
    pub peers: PeerStore,
    pub permissions: Arc<dyn PermissionsStore>,
    /// Set by `daemon-core` before D-Bus interfaces start handling
    /// calls. `None` only during the brief construction window — D-Bus
    /// interfaces panic if they try to send without it wired.
    pub actions: Option<UnboundedSender<DaemonAction>>,
    /// Per-peer live connectivity state. `daemon-core::handle_connection`
    /// flips entries through Authenticated → Active → Disconnected;
    /// `Device.State` reads from here. Missing entries imply
    /// `Disconnected` — saves an explicit `Forget`-time cleanup.
    pub connectivity: Arc<StdMutex<HashMap<DeviceId, ConnState>>>,
    /// LAN endpoints (`(ip, port)`) the QUIC listener is reachable on.
    /// Populated by `daemon-core` at startup by enumerating non-
    /// loopback interfaces × the bound port. Exposed via
    /// `Manager.ListenEndpoints()` so `ansyncctl pair` can embed
    /// them in the cable bootstrap reply — that lets the companion
    /// fall back to a direct unicast dial when the host's mDNS
    /// multicast doesn't reach (Wi-Fi AP isolation, hotspots, etc.).
    pub listen_endpoints: Arc<StdMutex<Vec<(String, u16)>>>,
    /// Paired companions currently advertising
    /// `_ansync-pair._tcp.local.` on the LAN. Populated by the
    /// daemon-core `companion_watcher` task; cleared when the mDNS
    /// announcement disappears. Exposed via
    /// `Manager.ReachableDevices()` and the
    /// `Manager.DeviceReachable` / `DeviceUnreachable` signals so
    /// widgets can paint a presence dot before the companion's
    /// `HostDialer` finishes its dance with the QUIC server.
    pub reachable: Arc<StdMutex<HashMap<DeviceId, std::net::SocketAddr>>>,
}

impl DaemonState {
    pub fn new(
        identity: IdentityKeypair,
        device_name: String,
        peers: PeerStore,
        permissions: Arc<dyn PermissionsStore>,
    ) -> Self {
        Self {
            identity,
            device_name,
            peers,
            permissions,
            actions: None,
            connectivity: Arc::new(StdMutex::new(HashMap::new())),
            listen_endpoints: Arc::new(StdMutex::new(Vec::new())),
            reachable: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    pub fn with_actions(mut self, tx: UnboundedSender<DaemonAction>) -> Self {
        self.actions = Some(tx);
        self
    }

    /// Snapshot the current `ConnState` for `device`. Defaults to
    /// `Disconnected` when the peer has never connected this session.
    pub fn conn_state(&self, device: &DeviceId) -> ConnState {
        self.connectivity
            .lock()
            .ok()
            .and_then(|g| g.get(device).copied())
            .unwrap_or(ConnState::Disconnected)
    }

    /// Atomically write the new state and return the previous one so
    /// the caller can decide whether to emit a D-Bus signal.
    pub fn set_conn_state(&self, device: &DeviceId, next: ConnState) -> ConnState {
        let mut guard = self.connectivity.lock().expect("connectivity poisoned");
        guard.insert(device.clone(), next).unwrap_or_default()
    }
}
