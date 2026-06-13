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
    FsOp(FsOpMessage),
    Clipboard(ClipboardMessage),
    Input(InputMessage),
    Notification(NotificationMessage),
    Goodbye { reason: String },
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
    /// Peer accepts and shares its identity back.
    BootstrapAck { identity_pubkey: [u8; 32], name: String },
    /// PIN shown on Android, typed on host.
    PinChallenge { pin: [u8; 6] },
    PinResponse { pin: [u8; 6] },
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
pub enum FsOpMessage {
    Stat { path: String },
    StatReply { meta: FsMeta },
    ReadDir { path: String },
    ReadDirReply { entries: Vec<FsEntry> },
    Open { path: String, flags: u32 },
    OpenReply { handle: u64 },
    Read { handle: u64, offset: u64, len: u32 },
    ReadReply { data: Vec<u8> },
    Write { handle: u64, offset: u64, data: Vec<u8> },
    WriteReply { written: u32 },
    Close { handle: u64 },
    Create { path: String, mode: u32 },
    CreateReply { handle: u64 },
    Unlink { path: String },
    Rename { from: String, to: String },
    Truncate { path: String, size: u64 },
    Chmod { path: String, mode: u32 },
    /// Returned by the receiver in place of the matching Reply
    /// variant when an op fails. `code` mirrors `errno` so the FUSE
    /// glue layer can translate cleanly.
    Ok,
    Error { code: i32, message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsMeta {
    pub size: u64,
    pub mode: u32,
    pub mtime: u64,
    pub is_dir: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsEntry {
    pub name: String,
    pub meta: FsMeta,
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
