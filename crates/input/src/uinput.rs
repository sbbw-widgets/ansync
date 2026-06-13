//! Linux `uinput` backend for the host's virtual input devices.
//!
//! Each concrete builder ([`Keyboard`], [`Mouse`], [`Touchscreen`],
//! [`Stylus`], [`Gamepad`]) opens `/dev/uinput`, advertises the
//! capability bits the kernel needs to expose the device under
//! `/dev/input/event*`, and then accepts [`crate::InputEvent`]
//! messages forwarded from the Android peer.
//!
//! The handles are *blocking* file descriptors. uinput is a fast
//! local syscall — sub-microsecond per event on a quiet host — so
//! wrapping the writes in a tokio worker buys nothing over the
//! latency cost of the syscall itself. The trait surface stays
//! `async` because the rest of the daemon is `async`.

use std::fs::{File, OpenOptions};

use async_trait::async_trait;
use input_linux::{
    AbsoluteAxis, AbsoluteInfo, AbsoluteInfoSetup, EventKind, EventTime, InputId, Key, KeyState,
    SynchronizeKind, UInputHandle,
    sys::{BUS_VIRTUAL, input_event},
};
use tracing::debug;

use crate::{InputError, InputEvent, InputKind, VirtualInputDevice};

/// PID assigned to "generic / open source" vendor at <https://pid.codes>.
/// We pair it with a per-device product id so userspace tools can
/// tell `Ansync Keyboard` from `Ansync Stylus` in `udevadm`.
const VENDOR_PID_CODES: u16 = 0x1209;

const PRODUCT_KEYBOARD: u16 = 0xA000;
const PRODUCT_MOUSE: u16 = 0xA001;
const PRODUCT_TOUCHSCREEN: u16 = 0xA002;
const PRODUCT_STYLUS: u16 = 0xA003;
const PRODUCT_GAMEPAD: u16 = 0xA004;
const VERSION: u16 = 0x0001;

/// Maximum coordinate emitted for any absolute axis whose range we
/// pick ourselves. 32 767 is the upper bound used by most evdev
/// devices that report 16-bit logical coordinates; downstream
/// compositors scale to the display.
const ABS_MAX: i32 = 32_767;

/// Maximum simultaneous fingers we advertise on the multi-touch
/// surface. Matches the trait surface in `crate::InputEvent::TouchSlot`
/// (8-bit slot id) and is generous for actual Android hardware.
const MT_SLOTS: i32 = 10;

fn open_uinput() -> Result<UInputHandle<File>, InputError> {
    let file = OpenOptions::new()
        .write(true)
        .open("/dev/uinput")
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::PermissionDenied => InputError::PermissionDenied,
            _ => InputError::Io(e),
        })?;
    Ok(UInputHandle::new(file))
}

fn make_id(product: u16) -> InputId {
    InputId {
        bustype: BUS_VIRTUAL,
        vendor: VENDOR_PID_CODES,
        product,
        version: VERSION,
    }
}

fn raw(kind: EventKind, code: u16, value: i32) -> input_event {
    let event = input_linux::InputEvent {
        time: EventTime::new(0, 0),
        kind,
        code,
        value,
    };
    event.as_raw().clone()
}

fn syn_report() -> input_event {
    raw(EventKind::Synchronize, SynchronizeKind::Report as u16, 0)
}

fn write_events(handle: &UInputHandle<File>, events: &[input_event]) -> Result<(), InputError> {
    handle.write(events)?;
    Ok(())
}

fn abs(axis: AbsoluteAxis, min: i32, max: i32) -> AbsoluteInfoSetup {
    AbsoluteInfoSetup {
        axis,
        info: AbsoluteInfo {
            value: 0,
            minimum: min,
            maximum: max,
            fuzz: 0,
            flat: 0,
            resolution: 0,
        },
    }
}

// ── Keyboard ──────────────────────────────────────────────────────

pub struct Keyboard {
    handle: Option<UInputHandle<File>>,
}

impl Keyboard {
    pub fn new() -> Self {
        Self { handle: None }
    }
}

impl Default for Keyboard {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl VirtualInputDevice for Keyboard {
    fn kind(&self) -> InputKind {
        InputKind::Keyboard
    }

    async fn create(&mut self, name: &str) -> Result<(), InputError> {
        let handle = open_uinput()?;
        handle.set_evbit(EventKind::Key)?;
        handle.set_evbit(EventKind::Synchronize)?;
        for key in Key::iter() {
            if key.is_key() {
                handle.set_keybit(key)?;
            }
        }
        let full_name = format!("{name} Keyboard");
        handle.create(&make_id(PRODUCT_KEYBOARD), full_name.as_bytes(), 0, &[])?;
        debug!(name = %full_name, "uinput keyboard created");
        self.handle = Some(handle);
        Ok(())
    }

    async fn send(&mut self, event: InputEvent) -> Result<(), InputError> {
        let h = self.handle.as_ref().ok_or(InputError::BackendUnavailable)?;
        match event {
            InputEvent::Key { keycode, pressed } => {
                let value = if pressed {
                    KeyState::PRESSED.value
                } else {
                    KeyState::RELEASED.value
                };
                write_events(
                    h,
                    &[
                        raw(EventKind::Key, keycode as u16, value),
                        syn_report(),
                    ],
                )
            }
            InputEvent::Sync => write_events(h, &[syn_report()]),
            _ => Ok(()),
        }
    }

    async fn destroy(&mut self) -> Result<(), InputError> {
        if let Some(h) = self.handle.take() {
            h.dev_destroy()?;
        }
        Ok(())
    }
}

// ── Mouse ─────────────────────────────────────────────────────────

pub struct Mouse {
    handle: Option<UInputHandle<File>>,
}

impl Mouse {
    pub fn new() -> Self {
        Self { handle: None }
    }
}

impl Default for Mouse {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl VirtualInputDevice for Mouse {
    fn kind(&self) -> InputKind {
        InputKind::Mouse
    }

    async fn create(&mut self, name: &str) -> Result<(), InputError> {
        let handle = open_uinput()?;
        handle.set_evbit(EventKind::Key)?;
        handle.set_evbit(EventKind::Relative)?;
        handle.set_evbit(EventKind::Synchronize)?;
        for button in [
            Key::ButtonLeft,
            Key::ButtonRight,
            Key::ButtonMiddle,
            Key::ButtonSide,
            Key::ButtonExtra,
            Key::ButtonForward,
            Key::ButtonBack,
        ] {
            handle.set_keybit(button)?;
        }
        for axis in [
            input_linux::RelativeAxis::X,
            input_linux::RelativeAxis::Y,
            input_linux::RelativeAxis::Wheel,
            input_linux::RelativeAxis::HorizontalWheel,
        ] {
            handle.set_relbit(axis)?;
        }
        let full_name = format!("{name} Mouse");
        handle.create(&make_id(PRODUCT_MOUSE), full_name.as_bytes(), 0, &[])?;
        debug!(name = %full_name, "uinput mouse created");
        self.handle = Some(handle);
        Ok(())
    }

    async fn send(&mut self, event: InputEvent) -> Result<(), InputError> {
        let h = self.handle.as_ref().ok_or(InputError::BackendUnavailable)?;
        match event {
            InputEvent::MouseRel { dx, dy } => write_events(
                h,
                &[
                    raw(EventKind::Relative, input_linux::RelativeAxis::X as u16, dx),
                    raw(EventKind::Relative, input_linux::RelativeAxis::Y as u16, dy),
                    syn_report(),
                ],
            ),
            InputEvent::MouseButton { button, pressed } => {
                let key = match button {
                    1 => Key::ButtonLeft,
                    2 => Key::ButtonRight,
                    3 => Key::ButtonMiddle,
                    4 => Key::ButtonSide,
                    5 => Key::ButtonExtra,
                    _ => return Ok(()),
                };
                let value = if pressed {
                    KeyState::PRESSED.value
                } else {
                    KeyState::RELEASED.value
                };
                write_events(
                    h,
                    &[raw(EventKind::Key, key as u16, value), syn_report()],
                )
            }
            InputEvent::MouseWheel { dx, dy } => write_events(
                h,
                &[
                    raw(
                        EventKind::Relative,
                        input_linux::RelativeAxis::HorizontalWheel as u16,
                        dx,
                    ),
                    raw(
                        EventKind::Relative,
                        input_linux::RelativeAxis::Wheel as u16,
                        dy,
                    ),
                    syn_report(),
                ],
            ),
            InputEvent::Sync => write_events(h, &[syn_report()]),
            _ => Ok(()),
        }
    }

    async fn destroy(&mut self) -> Result<(), InputError> {
        if let Some(h) = self.handle.take() {
            h.dev_destroy()?;
        }
        Ok(())
    }
}

// ── Touchscreen (multi-touch type B) ──────────────────────────────

pub struct Touchscreen {
    handle: Option<UInputHandle<File>>,
    active_slot: i32,
}

impl Touchscreen {
    pub fn new() -> Self {
        Self {
            handle: None,
            active_slot: -1,
        }
    }
}

impl Default for Touchscreen {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl VirtualInputDevice for Touchscreen {
    fn kind(&self) -> InputKind {
        InputKind::Touchscreen
    }

    async fn create(&mut self, name: &str) -> Result<(), InputError> {
        let handle = open_uinput()?;
        handle.set_evbit(EventKind::Key)?;
        handle.set_evbit(EventKind::Absolute)?;
        handle.set_evbit(EventKind::Synchronize)?;
        handle.set_keybit(Key::ButtonTouch)?;
        let abs_setup = [
            abs(AbsoluteAxis::X, 0, ABS_MAX),
            abs(AbsoluteAxis::Y, 0, ABS_MAX),
            abs(AbsoluteAxis::Pressure, 0, 255),
            abs(AbsoluteAxis::MultitouchSlot, 0, MT_SLOTS - 1),
            abs(AbsoluteAxis::MultitouchTrackingId, 0, 0xFFFF),
            abs(AbsoluteAxis::MultitouchPositionX, 0, ABS_MAX),
            abs(AbsoluteAxis::MultitouchPositionY, 0, ABS_MAX),
            abs(AbsoluteAxis::MultitouchPressure, 0, 255),
        ];
        let full_name = format!("{name} Touchscreen");
        handle.create(
            &make_id(PRODUCT_TOUCHSCREEN),
            full_name.as_bytes(),
            0,
            &abs_setup,
        )?;
        debug!(name = %full_name, "uinput touchscreen created");
        self.handle = Some(handle);
        Ok(())
    }

    async fn send(&mut self, event: InputEvent) -> Result<(), InputError> {
        let h = self.handle.as_ref().ok_or(InputError::BackendUnavailable)?;
        match event {
            InputEvent::TouchSlot {
                slot,
                x,
                y,
                pressure,
                tracking_id,
            } => {
                let slot_i = slot as i32;
                let mut events = Vec::with_capacity(7);
                if self.active_slot != slot_i {
                    events.push(raw(
                        EventKind::Absolute,
                        AbsoluteAxis::MultitouchSlot as u16,
                        slot_i,
                    ));
                    self.active_slot = slot_i;
                }
                events.push(raw(
                    EventKind::Absolute,
                    AbsoluteAxis::MultitouchTrackingId as u16,
                    tracking_id,
                ));
                if tracking_id >= 0 {
                    events.push(raw(
                        EventKind::Absolute,
                        AbsoluteAxis::MultitouchPositionX as u16,
                        x,
                    ));
                    events.push(raw(
                        EventKind::Absolute,
                        AbsoluteAxis::MultitouchPositionY as u16,
                        y,
                    ));
                    events.push(raw(
                        EventKind::Absolute,
                        AbsoluteAxis::MultitouchPressure as u16,
                        pressure as i32,
                    ));
                    events.push(raw(EventKind::Key, Key::ButtonTouch as u16, 1));
                } else {
                    // tracking_id = -1 → lift. Emit BTN_TOUCH up if
                    // this was the last active slot. Higher layers
                    // track that — single-finger heuristic for now.
                    events.push(raw(EventKind::Key, Key::ButtonTouch as u16, 0));
                }
                events.push(syn_report());
                write_events(h, &events)
            }
            InputEvent::Sync => write_events(h, &[syn_report()]),
            _ => Ok(()),
        }
    }

    async fn destroy(&mut self) -> Result<(), InputError> {
        if let Some(h) = self.handle.take() {
            h.dev_destroy()?;
        }
        Ok(())
    }
}

// ── Stylus (Wacom-style pen) ──────────────────────────────────────

pub struct Stylus {
    handle: Option<UInputHandle<File>>,
}

impl Stylus {
    pub fn new() -> Self {
        Self { handle: None }
    }
}

impl Default for Stylus {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl VirtualInputDevice for Stylus {
    fn kind(&self) -> InputKind {
        InputKind::Stylus
    }

    async fn create(&mut self, name: &str) -> Result<(), InputError> {
        let handle = open_uinput()?;
        handle.set_evbit(EventKind::Key)?;
        handle.set_evbit(EventKind::Absolute)?;
        handle.set_evbit(EventKind::Synchronize)?;
        for button in [
            Key::ButtonToolPen,
            Key::ButtonToolRubber,
            Key::ButtonTouch,
            Key::ButtonStylus,
            Key::ButtonStylus2,
        ] {
            handle.set_keybit(button)?;
        }
        let abs_setup = [
            abs(AbsoluteAxis::X, 0, ABS_MAX),
            abs(AbsoluteAxis::Y, 0, ABS_MAX),
            abs(AbsoluteAxis::Pressure, 0, 8191),
            abs(AbsoluteAxis::TiltX, -90, 90),
            abs(AbsoluteAxis::TiltY, -90, 90),
        ];
        let full_name = format!("{name} Stylus");
        handle.create(
            &make_id(PRODUCT_STYLUS),
            full_name.as_bytes(),
            0,
            &abs_setup,
        )?;
        debug!(name = %full_name, "uinput stylus created");
        self.handle = Some(handle);
        Ok(())
    }

    async fn send(&mut self, event: InputEvent) -> Result<(), InputError> {
        let h = self.handle.as_ref().ok_or(InputError::BackendUnavailable)?;
        match event {
            InputEvent::Stylus {
                x,
                y,
                pressure,
                tilt_x,
                tilt_y,
                btn,
            } => {
                let touch = if pressure > 0 { 1 } else { 0 };
                let stylus_btn = (btn & 0x01) as i32;
                let stylus_btn2 = ((btn >> 1) & 0x01) as i32;
                write_events(
                    h,
                    &[
                        raw(EventKind::Absolute, AbsoluteAxis::X as u16, x),
                        raw(EventKind::Absolute, AbsoluteAxis::Y as u16, y),
                        raw(
                            EventKind::Absolute,
                            AbsoluteAxis::Pressure as u16,
                            pressure as i32,
                        ),
                        raw(EventKind::Absolute, AbsoluteAxis::TiltX as u16, tilt_x as i32),
                        raw(EventKind::Absolute, AbsoluteAxis::TiltY as u16, tilt_y as i32),
                        raw(EventKind::Key, Key::ButtonToolPen as u16, 1),
                        raw(EventKind::Key, Key::ButtonTouch as u16, touch),
                        raw(EventKind::Key, Key::ButtonStylus as u16, stylus_btn),
                        raw(EventKind::Key, Key::ButtonStylus2 as u16, stylus_btn2),
                        syn_report(),
                    ],
                )
            }
            InputEvent::Sync => write_events(h, &[syn_report()]),
            _ => Ok(()),
        }
    }

    async fn destroy(&mut self) -> Result<(), InputError> {
        if let Some(h) = self.handle.take() {
            h.dev_destroy()?;
        }
        Ok(())
    }
}

// ── Gamepad (XInput-like layout) ──────────────────────────────────

pub struct Gamepad {
    handle: Option<UInputHandle<File>>,
}

impl Gamepad {
    pub fn new() -> Self {
        Self { handle: None }
    }
}

impl Default for Gamepad {
    fn default() -> Self {
        Self::new()
    }
}

const GP_BTN_LIST: [Key; 11] = [
    Key::ButtonSouth,
    Key::ButtonEast,
    Key::ButtonNorth,
    Key::ButtonWest,
    Key::ButtonTL,
    Key::ButtonTR,
    Key::ButtonSelect,
    Key::ButtonStart,
    Key::ButtonMode,
    Key::ButtonThumbl,
    Key::ButtonThumbr,
];

#[async_trait]
impl VirtualInputDevice for Gamepad {
    fn kind(&self) -> InputKind {
        InputKind::Gamepad
    }

    async fn create(&mut self, name: &str) -> Result<(), InputError> {
        let handle = open_uinput()?;
        handle.set_evbit(EventKind::Key)?;
        handle.set_evbit(EventKind::Absolute)?;
        handle.set_evbit(EventKind::Synchronize)?;
        for btn in GP_BTN_LIST {
            handle.set_keybit(btn)?;
        }
        let abs_setup = [
            abs(AbsoluteAxis::X, i16::MIN as i32, i16::MAX as i32),
            abs(AbsoluteAxis::Y, i16::MIN as i32, i16::MAX as i32),
            abs(AbsoluteAxis::RX, i16::MIN as i32, i16::MAX as i32),
            abs(AbsoluteAxis::RY, i16::MIN as i32, i16::MAX as i32),
            abs(AbsoluteAxis::Z, 0, 255),
            abs(AbsoluteAxis::RZ, 0, 255),
            abs(AbsoluteAxis::Hat0X, -1, 1),
            abs(AbsoluteAxis::Hat0Y, -1, 1),
        ];
        let full_name = format!("{name} Gamepad");
        handle.create(
            &make_id(PRODUCT_GAMEPAD),
            full_name.as_bytes(),
            0,
            &abs_setup,
        )?;
        debug!(name = %full_name, "uinput gamepad created");
        self.handle = Some(handle);
        Ok(())
    }

    async fn send(&mut self, event: InputEvent) -> Result<(), InputError> {
        let h = self.handle.as_ref().ok_or(InputError::BackendUnavailable)?;
        match event {
            InputEvent::Gamepad {
                buttons,
                lx,
                ly,
                rx,
                ry,
                lt,
                rt,
            } => {
                let mut events = Vec::with_capacity(GP_BTN_LIST.len() + 7);
                for (idx, btn) in GP_BTN_LIST.iter().enumerate() {
                    let pressed = ((buttons >> idx) & 0x1) as i32;
                    events.push(raw(EventKind::Key, *btn as u16, pressed));
                }
                events.extend_from_slice(&[
                    raw(EventKind::Absolute, AbsoluteAxis::X as u16, lx as i32),
                    raw(EventKind::Absolute, AbsoluteAxis::Y as u16, ly as i32),
                    raw(EventKind::Absolute, AbsoluteAxis::RX as u16, rx as i32),
                    raw(EventKind::Absolute, AbsoluteAxis::RY as u16, ry as i32),
                    raw(EventKind::Absolute, AbsoluteAxis::Z as u16, lt as i32),
                    raw(EventKind::Absolute, AbsoluteAxis::RZ as u16, rt as i32),
                    syn_report(),
                ]);
                write_events(h, &events)
            }
            InputEvent::Sync => write_events(h, &[syn_report()]),
            _ => Ok(()),
        }
    }

    async fn destroy(&mut self) -> Result<(), InputError> {
        if let Some(h) = self.handle.take() {
            h.dev_destroy()?;
        }
        Ok(())
    }
}
