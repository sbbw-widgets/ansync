//! Virtual input device abstraction.
//!
//! Linux-side impl is `uinput` — kernel evdev devices created on
//! demand, fed events the host receives over the QUIC `Input`
//! stream.

use async_trait::async_trait;

pub mod session;

#[cfg(feature = "uinput")]
pub mod uinput;

pub use session::{InputDeviceFactory, InputSession, SessionError};
#[cfg(feature = "uinput")]
pub use session::UinputFactory;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Keyboard,
    Mouse,
    Touchscreen,
    Stylus,
    Gamepad,
}

#[derive(Debug, Clone)]
pub enum InputEvent {
    Key { keycode: u32, pressed: bool },
    MouseRel { dx: i32, dy: i32 },
    MouseButton { button: u8, pressed: bool },
    MouseWheel { dx: i32, dy: i32 },
    TouchSlot { slot: u8, x: i32, y: i32, pressure: u16, tracking_id: i32 },
    Stylus { x: i32, y: i32, pressure: u16, tilt_x: i16, tilt_y: i16, btn: u8 },
    Gamepad { buttons: u32, lx: i16, ly: i16, rx: i16, ry: i16, lt: u8, rt: u8 },
    Sync,
}

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("backend unavailable")]
    BackendUnavailable,
    #[error("permission denied — uinput needs CAP_SYS_ADMIN or a udev rule")]
    PermissionDenied,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[async_trait]
pub trait VirtualInputDevice: Send + Sync {
    fn kind(&self) -> InputKind;
    async fn create(&mut self, name: &str) -> Result<(), InputError>;
    async fn send(&mut self, event: InputEvent) -> Result<(), InputError>;
    async fn destroy(&mut self) -> Result<(), InputError>;
}
