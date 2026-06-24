//! Per-peer input dispatch orchestrator.
//!
//! Owns one of each [`VirtualInputDevice`] kind for a given paired
//! peer and routes incoming `ansync_proto::InputMessage` packets into
//! the matching device. Permission is checked on every dispatch
//! against the `input_from_device` flag so a mid-session revoke from
//! the D-Bus surface stops the next event without tearing the QUIC
//! stream down — the stream just goes quiet from the device's POV.
//!
//! Devices are lazily constructed: the first event of a given kind
//! triggers a `create()`, so a peer that never sends gamepad data
//! never claims a `/dev/input/event*` slot for one.

use std::sync::Arc;

use ansync_core::{DeviceId, DeviceName, Permission};
use ansync_permissions::{PermissionsError, PermissionsStore};
use ansync_proto::InputMessage;
use tracing::warn;

use crate::{InputError, InputEvent, VirtualInputDevice};

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("input backend: {0}")]
    Input(#[from] InputError),
    #[error("permissions: {0}")]
    Permissions(#[from] PermissionsError),
    #[error("permission denied: {0:?}")]
    Denied(Permission),
}

/// Factory for the concrete device implementations the session
/// instantiates lazily. Decouples the orchestrator from any specific
/// backend (uinput today, BT-HID Step 13) — the daemon constructs an
/// `InputDeviceFactory` once per session and hands it over.
pub trait InputDeviceFactory: Send + Sync {
    fn build_keyboard(&self) -> Box<dyn VirtualInputDevice>;
    fn build_mouse(&self) -> Box<dyn VirtualInputDevice>;
    fn build_touchscreen(&self) -> Box<dyn VirtualInputDevice>;
    fn build_touchpad(&self) -> Box<dyn VirtualInputDevice>;
    fn build_stylus(&self) -> Box<dyn VirtualInputDevice>;
    fn build_gamepad(&self) -> Box<dyn VirtualInputDevice>;
}

/// The single uinput factory the daemon uses on Linux. Constructs the
/// concrete [`crate::uinput`] types boxed as trait objects.
#[cfg(feature = "uinput")]
pub struct UinputFactory;

#[cfg(feature = "uinput")]
impl InputDeviceFactory for UinputFactory {
    fn build_keyboard(&self) -> Box<dyn VirtualInputDevice> {
        Box::new(crate::uinput::Keyboard::new())
    }
    fn build_mouse(&self) -> Box<dyn VirtualInputDevice> {
        Box::new(crate::uinput::Mouse::new())
    }
    fn build_touchscreen(&self) -> Box<dyn VirtualInputDevice> {
        Box::new(crate::uinput::Touchscreen::new())
    }
    fn build_touchpad(&self) -> Box<dyn VirtualInputDevice> {
        Box::new(crate::uinput::Touchpad::new())
    }
    fn build_stylus(&self) -> Box<dyn VirtualInputDevice> {
        Box::new(crate::uinput::Stylus::new())
    }
    fn build_gamepad(&self) -> Box<dyn VirtualInputDevice> {
        Box::new(crate::uinput::Gamepad::new())
    }
}

pub struct InputSession {
    peer_id: DeviceId,
    peer_name: DeviceName,
    permissions: Arc<dyn PermissionsStore>,
    factory: Arc<dyn InputDeviceFactory>,
    keyboard: Option<Box<dyn VirtualInputDevice>>,
    mouse: Option<Box<dyn VirtualInputDevice>>,
    touchscreen: Option<Box<dyn VirtualInputDevice>>,
    touchpad: Option<Box<dyn VirtualInputDevice>>,
    stylus: Option<Box<dyn VirtualInputDevice>>,
    gamepad: Option<Box<dyn VirtualInputDevice>>,
}

impl InputSession {
    pub fn new(
        peer_id: DeviceId,
        peer_name: DeviceName,
        permissions: Arc<dyn PermissionsStore>,
        factory: Arc<dyn InputDeviceFactory>,
    ) -> Self {
        Self {
            peer_id,
            peer_name,
            permissions,
            factory,
            keyboard: None,
            mouse: None,
            touchscreen: None,
            touchpad: None,
            stylus: None,
            gamepad: None,
        }
    }

    /// Dispatch one wire-format [`InputMessage`]. Checks
    /// `input_from_device` first; lazily builds the matching uinput
    /// device on first event of that kind; converts to
    /// [`InputEvent`] and sends.
    pub async fn dispatch(&mut self, msg: InputMessage) -> Result<(), SessionError> {
        if !self
            .permissions
            .check(&self.peer_id, Permission::InputFromDevice)
            .await?
        {
            return Err(SessionError::Denied(Permission::InputFromDevice));
        }
        let event = wire_to_event(msg);
        match &event {
            InputEvent::Key { .. } => self.send_via(SessionDevice::Keyboard, event).await,
            InputEvent::MouseRel { .. }
            | InputEvent::MouseButton { .. }
            | InputEvent::MouseWheel { .. } => self.send_via(SessionDevice::Mouse, event).await,
            InputEvent::TouchSlot { .. } => self.send_via(SessionDevice::Touchscreen, event).await,
            InputEvent::TouchpadSlot { .. } => self.send_via(SessionDevice::Touchpad, event).await,
            InputEvent::Stylus { .. } => self.send_via(SessionDevice::Stylus, event).await,
            InputEvent::Gamepad { .. } => self.send_via(SessionDevice::Gamepad, event).await,
            InputEvent::Sync => Ok(()),
        }
    }

    /// Tear every active device down. The daemon calls this on peer
    /// disconnect or on `input_from_device=false`; subsequent
    /// `dispatch` calls will re-create lazily if permission flips
    /// back on.
    pub async fn shutdown(&mut self) {
        for slot in [
            &mut self.keyboard,
            &mut self.mouse,
            &mut self.touchscreen,
            &mut self.touchpad,
            &mut self.stylus,
            &mut self.gamepad,
        ] {
            if let Some(dev) = slot.as_mut() {
                if let Err(e) = dev.destroy().await {
                    warn!(error = %e, "input device destroy failed");
                }
            }
            *slot = None;
        }
    }

    async fn send_via(
        &mut self,
        which: SessionDevice,
        event: InputEvent,
    ) -> Result<(), SessionError> {
        let dev = self.ensure(which).await?;
        dev.send(event).await?;
        Ok(())
    }

    async fn ensure(
        &mut self,
        which: SessionDevice,
    ) -> Result<&mut Box<dyn VirtualInputDevice>, SessionError> {
        let slot = match which {
            SessionDevice::Keyboard => &mut self.keyboard,
            SessionDevice::Mouse => &mut self.mouse,
            SessionDevice::Touchscreen => &mut self.touchscreen,
            SessionDevice::Touchpad => &mut self.touchpad,
            SessionDevice::Stylus => &mut self.stylus,
            SessionDevice::Gamepad => &mut self.gamepad,
        };
        if slot.is_none() {
            let mut dev: Box<dyn VirtualInputDevice> = match which {
                SessionDevice::Keyboard => self.factory.build_keyboard(),
                SessionDevice::Mouse => self.factory.build_mouse(),
                SessionDevice::Touchscreen => self.factory.build_touchscreen(),
                SessionDevice::Touchpad => self.factory.build_touchpad(),
                SessionDevice::Stylus => self.factory.build_stylus(),
                SessionDevice::Gamepad => self.factory.build_gamepad(),
            };
            let name = format!("Ansync {}", self.peer_name);
            dev.create(&name).await?;
            *slot = Some(dev);
        }
        Ok(slot.as_mut().expect("just inserted"))
    }
}

#[derive(Debug, Clone, Copy)]
enum SessionDevice {
    Keyboard,
    Mouse,
    Touchscreen,
    Touchpad,
    Stylus,
    Gamepad,
}

fn wire_to_event(msg: InputMessage) -> InputEvent {
    match msg {
        InputMessage::KeyPress { keycode, pressed } => InputEvent::Key { keycode, pressed },
        InputMessage::MouseMove { dx, dy } => InputEvent::MouseRel { dx, dy },
        InputMessage::MouseButton { button, pressed } => {
            InputEvent::MouseButton { button, pressed }
        }
        InputMessage::MouseWheel { dx, dy } => InputEvent::MouseWheel { dx, dy },
        InputMessage::TouchSlot {
            slot,
            x,
            y,
            pressure,
            tracking_id,
        } => InputEvent::TouchSlot {
            slot,
            x,
            y,
            pressure,
            tracking_id,
        },
        InputMessage::TouchpadSlot {
            slot,
            x,
            y,
            pressure,
            tracking_id,
        } => InputEvent::TouchpadSlot {
            slot,
            x,
            y,
            pressure,
            tracking_id,
        },
        InputMessage::Stylus {
            x,
            y,
            pressure,
            tilt_x,
            tilt_y,
            btn,
        } => InputEvent::Stylus {
            x,
            y,
            pressure,
            tilt_x,
            tilt_y,
            btn,
        },
        InputMessage::Gamepad(state) => InputEvent::Gamepad {
            buttons: state.buttons,
            lx: state.lx,
            ly: state.ly,
            rx: state.rx,
            ry: state.ry,
            lt: state.lt,
            rt: state.rt,
        },
        // Text injection is a host → peer message (Android side
        // realises it via `ACTION_SET_TEXT`); there's no uinput event
        // that would let the host evdev sink consume it, so map to
        // `Sync` as a no-op.
        InputMessage::Text(_) => InputEvent::Sync,
    }
}
