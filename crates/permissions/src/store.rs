//! Permission helpers — projection/mutation/parsing utilities for
//! [`DevicePermissions`].
//!
//! The on-disk store itself lives in the daemon (backed by the
//! `PeerStore`'s toml; see `daemon-core::perms_backend`) — this
//! module is intentionally storage-free so it can be linked into
//! both the host daemon and the companion JNI bridge.

use ansync_core::{DevicePermissions, Permission};

/// Read the boolean projection of a [`Permission`] from a
/// [`DevicePermissions`] snapshot.
pub fn permission_value(perms: &DevicePermissions, permission: Permission) -> bool {
    match permission {
        Permission::ScreenMirror => perms.screen_mirror,
        Permission::CameraVideo => perms.camera_video,
        Permission::CameraAudio => perms.camera_audio,
        Permission::Mic => perms.mic,
        Permission::AudioIn => perms.audio_in,
        Permission::AudioOut => perms.audio_out,
        Permission::FilesSend => perms.files_send,
        Permission::FilesReceive => perms.files_receive,
        Permission::ClipboardIn => perms.clipboard_in,
        Permission::ClipboardOut => perms.clipboard_out,
        Permission::InputFromDevice => perms.input_from_device,
        Permission::InputToDevice => perms.input_to_device,
        Permission::Notifications => perms.notifications,
        Permission::ShareReceive => perms.share_receive,
    }
}

/// Apply a boolean write coming from the D-Bus surface.
pub fn apply_permission(perms: &mut DevicePermissions, permission: Permission, value: bool) {
    match permission {
        Permission::ScreenMirror => perms.screen_mirror = value,
        Permission::CameraVideo => perms.camera_video = value,
        Permission::CameraAudio => perms.camera_audio = value,
        Permission::Mic => perms.mic = value,
        Permission::AudioIn => perms.audio_in = value,
        Permission::AudioOut => perms.audio_out = value,
        Permission::FilesSend => perms.files_send = value,
        Permission::FilesReceive => perms.files_receive = value,
        Permission::ClipboardIn => perms.clipboard_in = value,
        Permission::ClipboardOut => perms.clipboard_out = value,
        Permission::InputFromDevice => perms.input_from_device = value,
        Permission::InputToDevice => perms.input_to_device = value,
        Permission::Notifications => perms.notifications = value,
        Permission::ShareReceive => perms.share_receive = value,
    }
}

/// Parse a D-Bus permission key string into a [`Permission`] enum.
pub fn parse_permission(name: &str) -> Option<Permission> {
    Some(match name {
        "screen_mirror" => Permission::ScreenMirror,
        "camera_video" => Permission::CameraVideo,
        "camera_audio" => Permission::CameraAudio,
        "mic" => Permission::Mic,
        "audio_in" => Permission::AudioIn,
        "audio_out" => Permission::AudioOut,
        "files_send" => Permission::FilesSend,
        "files_receive" => Permission::FilesReceive,
        "clipboard_in" => Permission::ClipboardIn,
        "clipboard_out" => Permission::ClipboardOut,
        "input_from_device" => Permission::InputFromDevice,
        "input_to_device" => Permission::InputToDevice,
        "notifications" => Permission::Notifications,
        "share_receive" => Permission::ShareReceive,
        _ => return None,
    })
}

/// Stable string name for a [`Permission`] — used as the toml key on
/// disk and the D-Bus property suffix.
pub fn permission_name(permission: Permission) -> &'static str {
    match permission {
        Permission::ScreenMirror => "screen_mirror",
        Permission::CameraVideo => "camera_video",
        Permission::CameraAudio => "camera_audio",
        Permission::Mic => "mic",
        Permission::AudioIn => "audio_in",
        Permission::AudioOut => "audio_out",
        Permission::FilesSend => "files_send",
        Permission::FilesReceive => "files_receive",
        Permission::ClipboardIn => "clipboard_in",
        Permission::ClipboardOut => "clipboard_out",
        Permission::InputFromDevice => "input_from_device",
        Permission::InputToDevice => "input_to_device",
        Permission::Notifications => "notifications",
        Permission::ShareReceive => "share_receive",
    }
}
