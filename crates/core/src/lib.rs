//! Shared core types used across every ansync crate.

use std::fmt;

use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("permission denied: {0:?}")]
    PermissionDenied(Permission),

    #[error("device not found: {0}")]
    DeviceNotFound(DeviceId),

    #[error("not paired with {0}")]
    NotPaired(DeviceId),

    #[error("transport: {0}")]
    Transport(String),

    #[error("protocol: {0}")]
    Protocol(String),

    #[error("crypto: {0}")]
    Crypto(String),

    #[error("backend unavailable: {0}")]
    BackendUnavailable(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Stable device identity derived from the peer's Ed25519 public key
/// fingerprint. 128-bit prefix is enough for collision-free routing on
/// a single LAN and keeps D-Bus object paths short.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub [u8; 16]);

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId({self})")
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Human-readable name advertised by the peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceName(pub String);

impl fmt::Display for DeviceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

bitflags::bitflags! {
    /// Capabilities advertised by a peer during the handshake. Both sides
    /// must support a capability for the corresponding feature to negotiate.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Capabilities: u32 {
        const SCREEN_MIRROR    = 1 << 0;
        const CAMERA_VIDEO     = 1 << 1;
        const CAMERA_AUDIO     = 1 << 2;
        const MIC              = 1 << 3;
        const AUDIO_IN         = 1 << 4;
        const AUDIO_OUT        = 1 << 5;
        const FILES            = 1 << 6;
        const FILES_MOUNT      = 1 << 7;
        const CLIPBOARD        = 1 << 8;
        const INPUT_FROM_DEV   = 1 << 9;
        const INPUT_TO_DEV     = 1 << 10;
        const NOTIFICATIONS    = 1 << 11;
        const SENSORS          = 1 << 12;
        const STYLUS           = 1 << 13;
        const HEVC             = 1 << 14;
    }
}

/// Persistable per-device permission set. Defaults are tuned for a safe
/// first connection: mirror + file send/recv on, hardware access off,
/// clipboard requires a prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevicePermissions {
    pub screen_mirror: bool,
    pub camera_video: bool,
    pub camera_audio: bool,
    pub mic: bool,
    pub audio_in: bool,
    pub audio_out: bool,
    pub files_send: bool,
    pub files_receive: bool,
    pub files_mount: bool,
    pub clipboard_in: ClipboardPolicy,
    pub clipboard_out: ClipboardPolicy,
    pub input_from_device: bool,
    pub input_to_device: bool,
    pub notifications: bool,
    pub sensors: bool,
}

impl Default for DevicePermissions {
    fn default() -> Self {
        Self {
            screen_mirror: true,
            camera_video: false,
            camera_audio: false,
            mic: false,
            audio_in: false,
            audio_out: false,
            files_send: true,
            files_receive: true,
            files_mount: false,
            clipboard_in: ClipboardPolicy::Prompt,
            clipboard_out: ClipboardPolicy::Prompt,
            input_from_device: false,
            input_to_device: false,
            notifications: true,
            sensors: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardPolicy {
    Off,
    Prompt,
    Allow,
}

/// Permission keys used for error reporting and D-Bus surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Permission {
    ScreenMirror,
    CameraVideo,
    CameraAudio,
    Mic,
    AudioIn,
    AudioOut,
    FilesSend,
    FilesReceive,
    FilesMount,
    ClipboardIn,
    ClipboardOut,
    InputFromDevice,
    InputToDevice,
    Notifications,
    Sensors,
}
