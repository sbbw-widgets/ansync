//! `/org/gameros/Ansync1/Device/{id}` interface.

use std::sync::Arc;

use ansync_core::{Capabilities, DeviceId};
use zbus::interface;

use crate::state::{DaemonAction, DaemonState};

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
    (Capabilities::FILES_MOUNT, "files_mount"),
    (Capabilities::CLIPBOARD, "clipboard"),
    (Capabilities::INPUT_FROM_DEV, "input_from_device"),
    (Capabilities::INPUT_TO_DEV, "input_to_device"),
    (Capabilities::NOTIFICATIONS, "notifications"),
    (Capabilities::SENSORS, "sensors"),
    (Capabilities::STYLUS, "stylus"),
    (Capabilities::HEVC, "hevc"),
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

fn not_yet(name: &str) -> zbus::fdo::Error {
    zbus::fdo::Error::NotSupported(format!("{name} not implemented yet"))
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
        // Real session tracking lands in Step 6; expose the static
        // "paired but never connected" state for now.
        "disconnected".to_string()
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

    async fn show_screen(&self) -> zbus::fdo::Result<()> {
        let tx = self.state.actions.as_ref().ok_or_else(|| {
            zbus::fdo::Error::Failed("daemon action channel not wired".into())
        })?;
        tx.send(DaemonAction::ShowScreen { device: self.id.clone() })
            .map_err(|e| zbus::fdo::Error::Failed(format!("send action: {e}")))?;
        Ok(())
    }

    async fn hide_screen(&self) -> zbus::fdo::Result<()> {
        let tx = self.state.actions.as_ref().ok_or_else(|| {
            zbus::fdo::Error::Failed("daemon action channel not wired".into())
        })?;
        tx.send(DaemonAction::HideScreen { device: self.id.clone() })
            .map_err(|e| zbus::fdo::Error::Failed(format!("send action: {e}")))?;
        Ok(())
    }

    async fn start_camera(&self) -> zbus::fdo::Result<()> {
        Err(not_yet("StartCamera"))
    }

    async fn stop_camera(&self) -> zbus::fdo::Result<()> {
        Err(not_yet("StopCamera"))
    }

    async fn start_microphone(&self) -> zbus::fdo::Result<()> {
        Err(not_yet("StartMicrophone"))
    }

    async fn stop_microphone(&self) -> zbus::fdo::Result<()> {
        Err(not_yet("StopMicrophone"))
    }

    async fn start_audio_route(&self, _direction: String) -> zbus::fdo::Result<()> {
        Err(not_yet("StartAudioRoute"))
    }

    async fn send_file(&self, _path: String) -> zbus::fdo::Result<String> {
        Err(not_yet("SendFile"))
    }

    async fn mount(&self, _mountpoint: String) -> zbus::fdo::Result<()> {
        Err(not_yet("Mount"))
    }

    async fn unmount(&self) -> zbus::fdo::Result<()> {
        Err(not_yet("Unmount"))
    }
}
