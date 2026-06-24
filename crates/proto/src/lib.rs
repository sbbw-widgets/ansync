//! On-wire message schema for ansync, versioned.
//!
//! Control plane messages are framed length-prefixed `postcard`-encoded
//! `Envelope`s on the dedicated control stream. Media streams (video /
//! audio) carry raw codec packets after an initial `MediaInit` frame.

use ansync_core::{Capabilities, DeviceId, DeviceName, DevicePermissions, Permission};
use serde::{Deserialize, Serialize};

pub mod frame;

pub use frame::{
    FrameError, MAX_FRAME_SIZE, decode_envelope, encode_envelope, encode_message, read_envelope,
    read_frame, read_typed, write_envelope, write_frame, write_typed,
};

pub const PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct Envelope {
    pub version: u16,
    pub message: Message,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Message {
    Hello(Hello),
    Permission(PermissionMessage),
    Control(ControlMessage),
    Pairing(PairingMessage),
    FileTransfer(FileTransferMessage),
    Clipboard(ClipboardMessage),
    Input(InputMessage),
    Notification(NotificationMessage),
    Url(UrlMessage),
    Goodbye { reason: String },
}

/// One-shot "open this URL on the peer" envelope. Carried by a
/// dedicated `StreamKind::Url` stream: opener writes one
/// `Message::Url(UrlMessage)` postcard frame, drops the stream.
///
/// Receiver behaviour is asymmetric on purpose:
///   - Linux host: `xdg-open` the URL directly (the peer is paired
///     hardware the user trusts as much as their own clipboard).
///   - Android companion: post a high-priority notification asking
///     the user whether to open — anything from a compromised peer
///     reaching `ACTION_VIEW` without consent would otherwise let the
///     attacker pop arbitrary intents on the device.
///
/// `Permission::ShareReceive` gates both directions; off → silently
/// drop the message after logging at debug.
#[derive(Debug, Serialize, Deserialize)]
pub struct UrlMessage {
    pub url: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Hello {
    pub device_id: DeviceId,
    pub name: DeviceName,
    pub capabilities: Capabilities,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum PermissionMessage {
    Snapshot(DevicePermissions),
    Request(Permission),
    Denied(Permission),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ControlMessage {
    StartScreen { codec: VideoCodec, max_bitrate_kbps: u32, max_fps: u8 },
    StopScreen,
    StartCamera(CameraConfig),
    StopCamera,
    StartMic,
    StopMic,
    StartAudioRoute { direction: AudioDirection },
    StopAudioRoute,
    /// Host → companion: "I would like to mirror your screen now."
    /// The companion's service decides how to handle the request — if
    /// the user already granted MediaProjection for this session, it
    /// starts capture immediately; otherwise it posts a high-priority
    /// notification asking the user to grant. Either way the host
    /// can't force capture without on-device consent (Android limit).
    RequestScreenCapture,
    /// Host → companion: "Stop mirroring." Stops capture on the
    /// device side without waiting for the host to drop the QUIC
    /// connection. Symmetric to `StopScreen` but on the
    /// control-stream surface.
    StopScreenCapture,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum VideoCodec {
    H264,
    H265,
}

/// Per-call camera capture parameters negotiated host → companion.
/// `camera_id` is an Android `cameraId` string ("0" = primary back,
/// "1" = primary front on most devices). Width/height are the
/// *encoder output* dimensions; the companion may letterbox or
/// downscale the sensor frame to fit, honouring `aspect`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraConfig {
    pub camera_id: String,
    pub width: u32,
    pub height: u32,
    pub fps: u8,
    pub bitrate_kbps: u32,
    pub codec: VideoCodec,
    pub aspect: CameraAspect,
    pub stabilization: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum CameraAspect {
    /// Crop sensor frame to match `width`/`height` exactly.
    Crop,
    /// Letterbox sensor frame inside `width`/`height` keeping
    /// sensor's native AR.
    Letterbox,
    /// Stretch sensor frame to fill output dimensions ignoring AR.
    Stretch,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum AudioDirection {
    HostToDevice,
    DeviceToHost,
    Both,
}

/// PCM format declared on the first frame of every `StreamKind::Audio`
/// stream. After this header, the stream carries raw little-endian
/// S16 samples interleaved (L,R,L,R,...) — no per-frame envelope so
/// the hot path stays a straight `recv → write_sink` copy.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct AudioStreamInit {
    pub sample_rate: u32,
    pub channels: u8,
    pub direction: AudioDirection,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum PairingMessage {
    /// Bootstrap channel announces this side's identity.
    BootstrapHello { identity_pubkey: [u8; 32], name: String },
    /// Peer accepts and shares its identity back. `lan_endpoints` is
    /// the host's best-guess LAN reachability hint set — typically
    /// the (ip, port) pairs the QUIC listener is bound on, picked
    /// from non-loopback interfaces. The companion persists them so
    /// `HostDialer` can fall back to a direct unicast dial when
    /// mDNS multicast is blocked (AP isolation, hotspot subnets,
    /// etc.).
    BootstrapAck {
        identity_pubkey: [u8; 32],
        name: String,
        lan_endpoints: Vec<(String, u16)>,
    },
    /// WiFi-pair MAC confirmation. Companion displays a 6-digit PIN
    /// on screen, user types it on host. Both sides compute
    /// `SHA-256("ansync-pair-v1" || role || host_pubkey || companion_pubkey || pin)`
    /// with `role = b"host"` (host→companion) or `b"companion"`
    /// (companion→host) and exchange `PinConfirm`. Any mismatch
    /// aborts the bootstrap and increments the companion's lockout
    /// counter — 3 strikes close the listener and rotate the PIN.
    PinConfirm { mac: [u8; 32] },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum FileTransferMessage {
    Offer { transfer_id: u64, name: String, size: u64, sha256: [u8; 32] },
    Accept { transfer_id: u64 },
    Reject { transfer_id: u64, reason: String },
    Chunk { transfer_id: u64, offset: u64, data: Vec<u8> },
    Complete { transfer_id: u64 },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ClipboardMessage {
    Text { content: String },
    Blob { mime: String, data: Vec<u8> },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum InputMessage {
    KeyPress { keycode: u32, pressed: bool },
    MouseMove { dx: i32, dy: i32 },
    MouseButton { button: u8, pressed: bool },
    MouseWheel { dx: i32, dy: i32 },
    TouchSlot { slot: u8, x: i32, y: i32, pressure: u16, tracking_id: i32 },
    Stylus { x: i32, y: i32, pressure: u16, tilt_x: i16, tilt_y: i16, btn: u8 },
    Gamepad(GamepadState),
    /// Insert this UTF-8 string at the focused text field on the
    /// peer. Used by the mirror window's keyboard handler for
    /// arbitrary characters that the evdev `KeyPress` path can't
    /// represent (everything past the curated system-key set the
    /// `AccessibilityService` knows about). On Android the companion
    /// realizes this via `AccessibilityNodeInfo.ACTION_SET_TEXT` on
    /// the focused `EditText`.
    Text(String),
    /// Multi-touch slot on the host's *touchpad* device (libinput
    /// classifies as buttonpad/clickpad → tap-to-click, two-finger
    /// scroll, pinch zoom etc. handled natively by the compositor's
    /// libinput config). Same payload shape as `TouchSlot` but routes
    /// to a different uinput node so the companion can choose
    /// between "Mac-style touchpad" (this) and "absolute touchscreen"
    /// (`TouchSlot`) per gesture mode. Added at the end of the enum
    /// to preserve the postcard variant indices of the older entries.
    TouchpadSlot { slot: u8, x: i32, y: i32, pressure: u16, tracking_id: i32 },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GamepadState {
    pub buttons: u32,
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
    pub lt: u8,
    pub rt: u8,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum NotificationMessage {
    Posted { id: u64, app: String, title: String, body: String },
    Removed { id: u64 },
}
