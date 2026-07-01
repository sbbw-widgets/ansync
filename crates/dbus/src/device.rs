//! `/org/gameros/Ansync1/Device/{id}` interface.

use std::sync::Arc;

use ansync_core::{Capabilities, DeviceId};
use zbus::interface;

use crate::state::{ConnState, DaemonAction, DaemonState};

#[derive(Clone)]
pub struct Device {
    pub id: DeviceId,
    pub state: Arc<DaemonState>,
}

const CAP_TABLE: &[(Capabilities, &str)] = &[
    (Capabilities::SCREEN_MIRROR, "screen_mirror"),
    (Capabilities::CAMERA_VIDEO, "camera_video"),
    (Capabilities::CAMERA_AUDIO, "camera_audio"),
    (Capabilities::MIC, "mic"),
    (Capabilities::AUDIO_IN, "audio_in"),
    (Capabilities::AUDIO_OUT, "audio_out"),
    (Capabilities::FILES, "files"),
    (Capabilities::CLIPBOARD, "clipboard"),
    (Capabilities::INPUT_FROM_DEV, "input_from_device"),
    (Capabilities::INPUT_TO_DEV, "input_to_device"),
    (Capabilities::NOTIFICATIONS, "notifications"),
    (Capabilities::STYLUS, "stylus"),
    (Capabilities::HEVC, "hevc"),
    (Capabilities::SHARE, "share"),
];

fn capability_names(caps: Capabilities) -> Vec<String> {
    let mut out = Vec::new();
    for (flag, name) in CAP_TABLE {
        if caps.contains(*flag) {
            out.push((*name).to_string());
        }
    }
    out
}

#[interface(name = "org.gameros.Ansync1.Device")]
impl Device {
    #[zbus(property)]
    async fn id(&self) -> String {
        self.id.to_string()
    }

    #[zbus(property)]
    async fn name(&self) -> String {
        self.state
            .peers
            .get(&self.id)
            .ok()
            .map(|p| p.name.0)
            .unwrap_or_default()
    }

    #[zbus(property)]
    async fn state(&self) -> String {
        self.state.conn_state(&self.id).as_str().to_string()
    }

    #[zbus(property)]
    async fn capabilities(&self) -> Vec<String> {
        let caps = self
            .state
            .peers
            .get(&self.id)
            .ok()
            .map(|p| p.capabilities)
            .unwrap_or(Capabilities::empty());
        capability_names(caps)
    }

    #[zbus(property)]
    async fn battery_level(&self) -> u8 {
        0
    }

    #[zbus(property)]
    async fn address(&self) -> String {
        String::new()
    }

    /// Bring up host → device audio sink. The daemon opens a
    /// `StreamKind::Audio` outbound, encodes host PipeWire capture
    /// with Opus, and pushes it to the peer's speaker.
    ///
    /// This is the only host-initiated media stream. Mirror, camera
    /// and mic share are triggered from the phone (QSTile) — the host
    /// only consumes them.
    async fn start_audio_sink(&self) -> zbus::fdo::Result<()> {
        let tx = self.state.actions.as_ref().ok_or_else(|| {
            zbus::fdo::Error::Failed("daemon action channel not wired".into())
        })?;
        tx.send(DaemonAction::StartAudioSink { device: self.id.clone() })
            .map_err(|e| zbus::fdo::Error::Failed(format!("send action: {e}")))?;
        Ok(())
    }

    async fn stop_audio_sink(&self) -> zbus::fdo::Result<()> {
        let tx = self.state.actions.as_ref().ok_or_else(|| {
            zbus::fdo::Error::Failed("daemon action channel not wired".into())
        })?;
        tx.send(DaemonAction::StopAudioSink { device: self.id.clone() })
            .map_err(|e| zbus::fdo::Error::Failed(format!("send action: {e}")))?;
        Ok(())
    }

    async fn sync_clipboard(&self) -> zbus::fdo::Result<()> {
        let tx = self.state.actions.as_ref().ok_or_else(|| {
            zbus::fdo::Error::Failed("daemon action channel not wired".into())
        })?;
        tx.send(DaemonAction::SyncClipboard {
            device: self.id.clone(),
        })
        .map_err(|e| zbus::fdo::Error::Failed(format!("send action: {e}")))?;
        Ok(())
    }

    /// Queue one or more files for delivery to the peer. Paths are
    /// host-side absolute paths the daemon process can read. The
    /// peer's `AutoAcceptPolicy` drops them under its own incoming
    /// directory and surfaces a `FileReceived` signal on its side.
    /// Returns the number of paths the daemon accepted into the
    /// send queue (zero if the peer is offline or the action channel
    /// is wedged).
    async fn send_files(&self, paths: Vec<String>) -> zbus::fdo::Result<u32> {
        let tx = self.state.actions.as_ref().ok_or_else(|| {
            zbus::fdo::Error::Failed("daemon action channel not wired".into())
        })?;
        let paths: Vec<std::path::PathBuf> = paths.into_iter().map(Into::into).collect();
        let count = paths.len() as u32;
        tx.send(crate::state::DaemonAction::SendFiles {
            device: self.id.clone(),
            paths,
        })
        .map_err(|e| zbus::fdo::Error::Failed(format!("send action: {e}")))?;
        Ok(count)
    }

    /// Ask the peer to open a URL. Linux peers `xdg-open` it
    /// directly; Android peers post a high-priority notification so
    /// the user confirms before `Intent.ACTION_VIEW` fires.
    async fn send_url(&self, url: String) -> zbus::fdo::Result<()> {
        let tx = self.state.actions.as_ref().ok_or_else(|| {
            zbus::fdo::Error::Failed("daemon action channel not wired".into())
        })?;
        tx.send(crate::state::DaemonAction::SendUrl {
            device: self.id.clone(),
            url,
        })
        .map_err(|e| zbus::fdo::Error::Failed(format!("send action: {e}")))?;
        Ok(())
    }

    /// Fired once for every inbound file the daemon finished
    /// receiving from this peer. `path` is the absolute host path
    /// of the stored file (typically under
    /// `$XDG_DATA_HOME/ansync/incoming/{peer}/`). Desktop shells +
    /// `ansyncctl` listen here to surface a "received from <peer>"
    /// notification.
    #[zbus(signal)]
    pub async fn file_received(
        ctxt: &zbus::object_server::SignalEmitter<'_>,
        path: &str,
    ) -> zbus::Result<()>;

    /// Per-chunk progress fan-out for an in-flight file transfer.
    /// Direction is `"send"` (host → peer) or `"receive"` (peer →
    /// host); `batch_*` fields cover the multi-file Share sheet case
    /// and are `0` for one-off transfers on the receive side (the
    /// host can't see the sender's batch shape). Throttled by the
    /// emitter to a 1% delta + file boundary so the bus stays calm.
    #[zbus(signal)]
    pub async fn file_transfer_progress(
        ctxt: &zbus::object_server::SignalEmitter<'_>,
        batch_id: u64,
        transfer_id: u64,
        name: &str,
        bytes: u64,
        total: u64,
        batch_files: u32,
        batch_files_done: u32,
        batch_bytes_done: u64,
        batch_total_bytes: u64,
        direction: &str,
    ) -> zbus::Result<()>;

    /// Fired once for every `NotificationListenerService.onNotificationPosted`
    /// the companion forwards. Subscribers (e.g. a desktop notification
    /// daemon bridge) receive `(id, app, title, body)`.
    #[zbus(signal)]
    pub async fn notification_posted(
        ctxt: &zbus::object_server::SignalEmitter<'_>,
        id: u64,
        app: &str,
        title: &str,
        body: &str,
    ) -> zbus::Result<()>;

    /// Fired when the companion's
    /// `NotificationListenerService.onNotificationRemoved` reports the
    /// notification `id` was dismissed.
    #[zbus(signal)]
    pub async fn notification_removed(
        ctxt: &zbus::object_server::SignalEmitter<'_>,
        id: u64,
    ) -> zbus::Result<()>;

    /// Fired on every stream lifecycle transition (camera, microphone,
    /// audio-out, mirror, files-mount, …). `kind` is the lowercase tag
    /// (`"camera"`, `"mic"`, `"audio"`, `"screen"`, `"files"`); `active`
    /// reflects the new state. UIs that drive the stream via D-Bus —
    /// DMS plugin, ansyncctl — listen here so a `gdbus` call from
    /// another process re-syncs the widget without polling.
    #[zbus(signal)]
    pub async fn stream_state_changed(
        ctxt: &zbus::object_server::SignalEmitter<'_>,
        kind: &str,
        active: bool,
    ) -> zbus::Result<()>;
}

impl Device {
    /// Helper for `daemon-core`: flip the connectivity state for one
    /// peer and emit the auto-generated `PropertiesChanged` signal for
    /// the `State` property + the global `Manager.DeviceConnectivityChanged`.
    /// No-op when the new state matches the current cached value.
    pub async fn emit_state_changed(
        conn: &zbus::Connection,
        state: &Arc<DaemonState>,
        device: &DeviceId,
        next: ConnState,
    ) -> zbus::Result<()> {
        let previous = state.set_conn_state(device, next);
        if previous == next {
            return Ok(());
        }
        let path = crate::path_device(device);
        let object_path = zbus::zvariant::ObjectPath::try_from(path.as_str())
            .map_err(|e| zbus::Error::Failure(format!("bad path {path}: {e}")))?;
        // PropertiesChanged for the `State` property. zbus auto-derives
        // a `state_changed` method that emits the spec-compliant
        // signal; generic property watchers refresh from this.
        if let Ok(iface) = conn
            .object_server()
            .interface::<_, Device>(object_path)
            .await
        {
            iface.get().await.state_changed(iface.signal_emitter()).await?;
        }
        // Manager-level fan-out so DMS widgets / ansyncctl can listen on
        // a single object path instead of subscribing per-device.
        let mgr_path = zbus::zvariant::ObjectPath::try_from(crate::PATH_MANAGER)
            .map_err(|e| zbus::Error::Failure(format!("bad manager path: {e}")))?;
        let mgr_emitter = zbus::object_server::SignalEmitter::new(conn, mgr_path)?;
        crate::manager::Manager::device_connectivity_changed(
            &mgr_emitter,
            &device.to_string(),
            next.as_str(),
        )
        .await?;
        Ok(())
    }

    /// Emit `StreamStateChanged(kind, active)` on `Device` for a peer.
    /// Used by `daemon-core` after `handle_start_*` / `handle_stop_*`
    /// succeeds. No-op when the per-device interface isn't registered
    /// (peer never connected → no D-Bus path).
    pub async fn emit_stream_state(
        conn: &zbus::Connection,
        device: &DeviceId,
        kind: &str,
        active: bool,
    ) -> zbus::Result<()> {
        let path = crate::path_device(device);
        let object_path = zbus::zvariant::ObjectPath::try_from(path.as_str())
            .map_err(|e| zbus::Error::Failure(format!("bad path {path}: {e}")))?;
        let Ok(iface) = conn
            .object_server()
            .interface::<_, Device>(object_path)
            .await
        else {
            return Ok(());
        };
        Device::stream_state_changed(iface.signal_emitter(), kind, active).await
    }

    /// Emit `FileTransferProgress(...)` on `Device` for a peer. The
    /// caller is expected to throttle; this just fans the event onto
    /// the bus. No-op when the per-device interface isn't registered.
    #[allow(clippy::too_many_arguments)]
    pub async fn emit_file_transfer_progress(
        conn: &zbus::Connection,
        device: &DeviceId,
        batch_id: u64,
        transfer_id: u64,
        name: &str,
        bytes: u64,
        total: u64,
        batch_files: u32,
        batch_files_done: u32,
        batch_bytes_done: u64,
        batch_total_bytes: u64,
        direction: &str,
    ) -> zbus::Result<()> {
        let dev_path = crate::path_device(device);
        let object_path = zbus::zvariant::ObjectPath::try_from(dev_path.as_str())
            .map_err(|e| zbus::Error::Failure(format!("bad path {dev_path}: {e}")))?;
        let Ok(iface) = conn
            .object_server()
            .interface::<_, Device>(object_path)
            .await
        else {
            return Ok(());
        };
        Device::file_transfer_progress(
            iface.signal_emitter(),
            batch_id,
            transfer_id,
            name,
            bytes,
            total,
            batch_files,
            batch_files_done,
            batch_bytes_done,
            batch_total_bytes,
            direction,
        )
        .await
    }

    /// Emit `FileReceived(path)` on `Device` for a peer. Called by
    /// `daemon-core` after an inbound `Files` stream completes
    /// successfully. No-op when the per-device interface isn't
    /// registered yet (signal would have no subscribers anyway).
    pub async fn emit_file_received(
        conn: &zbus::Connection,
        device: &DeviceId,
        path: &str,
    ) -> zbus::Result<()> {
        let dev_path = crate::path_device(device);
        let object_path = zbus::zvariant::ObjectPath::try_from(dev_path.as_str())
            .map_err(|e| zbus::Error::Failure(format!("bad path {dev_path}: {e}")))?;
        let Ok(iface) = conn
            .object_server()
            .interface::<_, Device>(object_path)
            .await
        else {
            return Ok(());
        };
        Device::file_received(iface.signal_emitter(), path).await
    }
}
