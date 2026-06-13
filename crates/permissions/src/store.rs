//! Toml-backed [`PermissionsStore`] under `$XDG_CONFIG_HOME/ansync/devices/`.
//!
//! Writes are atomic (tmp + rename); reads return [`DevicePermissions`]
//! defaults when the file is absent so the daemon can treat "never seen"
//! and "explicitly default" identically.

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use ansync_core::{ClipboardPolicy, DeviceId, DevicePermissions, Permission};
use async_trait::async_trait;

use crate::{PermissionsError, PermissionsStore};

pub struct FilePermissionsStore {
    root: PathBuf,
}

impl FilePermissionsStore {
    pub fn open(root: PathBuf) -> Result<Self, PermissionsError> {
        if !root.exists() {
            fs::create_dir_all(&root)?;
            let mut perms = fs::metadata(&root)?.permissions();
            perms.set_mode(0o700);
            let _ = fs::set_permissions(&root, perms);
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, id: &DeviceId) -> PathBuf {
        self.root.join(format!("{id}.toml"))
    }
}

#[async_trait]
impl PermissionsStore for FilePermissionsStore {
    async fn load(&self, id: &DeviceId) -> Result<DevicePermissions, PermissionsError> {
        let path = self.path_for(id);
        match fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).map_err(|e| PermissionsError::TomlDecode(e.to_string())),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(DevicePermissions::default()),
            Err(e) => Err(e.into()),
        }
    }

    async fn save(
        &self,
        id: &DeviceId,
        perms: &DevicePermissions,
    ) -> Result<(), PermissionsError> {
        let serialized =
            toml::to_string_pretty(perms).map_err(|e| PermissionsError::TomlEncode(e.to_string()))?;
        let path = self.path_for(id);
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, serialized)?;
        let mut file_perms = fs::metadata(&tmp)?.permissions();
        file_perms.set_mode(0o600);
        let _ = fs::set_permissions(&tmp, file_perms);
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    async fn delete(&self, id: &DeviceId) -> Result<(), PermissionsError> {
        match fs::remove_file(self.path_for(id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn check(
        &self,
        id: &DeviceId,
        permission: Permission,
    ) -> Result<bool, PermissionsError> {
        let perms = self.load(id).await?;
        Ok(permission_value(&perms, permission))
    }
}

/// Read the boolean projection of a [`Permission`] from a
/// [`DevicePermissions`] snapshot. `ClipboardPolicy` is collapsed to
/// `true` only when the flag is explicitly `Allow` — both `Off` and
/// `Prompt` deny silent transfer.
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
        Permission::FilesMount => perms.files_mount,
        Permission::ClipboardIn => matches!(perms.clipboard_in, ClipboardPolicy::Allow),
        Permission::ClipboardOut => matches!(perms.clipboard_out, ClipboardPolicy::Allow),
        Permission::InputFromDevice => perms.input_from_device,
        Permission::InputToDevice => perms.input_to_device,
        Permission::Notifications => perms.notifications,
        Permission::Sensors => perms.sensors,
    }
}

/// Apply a boolean write coming from the D-Bus surface. For clipboard
/// flags `true` maps to `Allow` and `false` to `Off`; the `Prompt`
/// state has to be set via a dedicated RPC (TODO Step 12).
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
        Permission::FilesMount => perms.files_mount = value,
        Permission::ClipboardIn => {
            perms.clipboard_in = if value { ClipboardPolicy::Allow } else { ClipboardPolicy::Off };
        }
        Permission::ClipboardOut => {
            perms.clipboard_out =
                if value { ClipboardPolicy::Allow } else { ClipboardPolicy::Off };
        }
        Permission::InputFromDevice => perms.input_from_device = value,
        Permission::InputToDevice => perms.input_to_device = value,
        Permission::Notifications => perms.notifications = value,
        Permission::Sensors => perms.sensors = value,
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
        "files_mount" => Permission::FilesMount,
        "clipboard_in" => Permission::ClipboardIn,
        "clipboard_out" => Permission::ClipboardOut,
        "input_from_device" => Permission::InputFromDevice,
        "input_to_device" => Permission::InputToDevice,
        "notifications" => Permission::Notifications,
        "sensors" => Permission::Sensors,
        _ => return None,
    })
}

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
        Permission::FilesMount => "files_mount",
        Permission::ClipboardIn => "clipboard_in",
        Permission::ClipboardOut => "clipboard_out",
        Permission::InputFromDevice => "input_from_device",
        Permission::InputToDevice => "input_to_device",
        Permission::Notifications => "notifications",
        Permission::Sensors => "sensors",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tempdir() -> PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "ansync-perm-test-{}-{}",
            std::process::id(),
            ts
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn defaults_on_missing_file() {
        let dir = tempdir();
        let store = FilePermissionsStore::open(dir.clone()).unwrap();
        let id = DeviceId([0xAA; 16]);
        let perms = store.load(&id).await.unwrap();
        assert!(perms.screen_mirror);
        assert!(!perms.mic);
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn save_load_roundtrip() {
        let dir = tempdir();
        let store = FilePermissionsStore::open(dir.clone()).unwrap();
        let id = DeviceId([0x55; 16]);
        let mut perms = DevicePermissions::default();
        apply_permission(&mut perms, Permission::Mic, true);
        apply_permission(&mut perms, Permission::ClipboardIn, true);
        store.save(&id, &perms).await.unwrap();

        let mode = fs::metadata(store.path_for(&id))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        let loaded = store.load(&id).await.unwrap();
        assert!(loaded.mic);
        assert!(matches!(loaded.clipboard_in, ClipboardPolicy::Allow));

        assert!(store.check(&id, Permission::Mic).await.unwrap());
        assert!(!store.check(&id, Permission::Sensors).await.unwrap());

        let _ = fs::remove_dir_all(&dir);
    }
}
