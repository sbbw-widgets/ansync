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
    AbsoluteAxis, AbsoluteInfo, AbsoluteInfoSetup, EventKind, EventTime, InputId, InputProperty,
    Key, KeyState, SynchronizeKind, UInputHandle,
    sys::{BUS_USB, BUS_VIRTUAL, input_event},
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
const PRODUCT_TOUCHPAD: u16 = 0xA005;
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
    make_id_bus(product, BUS_VIRTUAL)
}

fn make_id_bus(product: u16, bus: u16) -> InputId {
    InputId {
        bustype: bus,
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
    abs_res(axis, min, max, 0)
}

/// Same as [`abs`] but with a non-zero `resolution`. Required by
/// libinput's tablet pipeline on ABS_X / ABS_Y: a `resolution == 0`
/// logs `"missing tablet capabilities: resolution. Ignoring this
/// device."` and the tablet is dropped entirely — no tablet-tool
/// events ever reach the compositor.
///
/// Units:
/// - Spatial axes (X/Y): units per *millimetre*.
/// - Tilt axes: units per *radian*.
/// - Pressure: not required by libinput; leave at 0.
fn abs_res(axis: AbsoluteAxis, min: i32, max: i32, resolution: i32) -> AbsoluteInfoSetup {
    AbsoluteInfoSetup {
        axis,
        info: AbsoluteInfo {
            value: 0,
            minimum: min,
            maximum: max,
            fuzz: 0,
            flat: 0,
            resolution,
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
    /// Accumulated high-resolution wheel ticks (REL_WHEEL_HI_RES /
    /// REL_HWHEEL_HI_RES units; 120 hi-res = 1 traditional notch).
    /// We emit a traditional REL_WHEEL / REL_HWHEEL step every time
    /// the accumulator crosses ±120 so legacy consumers that ignore
    /// the hi-res axis still get notched scrolling.
    wheel_accum_y: i32,
    wheel_accum_x: i32,
}

impl Mouse {
    pub fn new() -> Self {
        Self {
            handle: None,
            wheel_accum_y: 0,
            wheel_accum_x: 0,
        }
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
            input_linux::RelativeAxis::WheelHiRes,
            input_linux::RelativeAxis::HorizontalWheelHiRes,
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
            InputEvent::MouseWheel { dx, dy } => {
                // Wire semantics: `dx` / `dy` are high-resolution
                // wheel ticks where 120 hi-res = 1 legacy notch.
                // Hi-res axes drive trackpad-style smooth pixel
                // scrolling in libinput / GNOME / KDE; the legacy
                // notch axes are still emitted for clients that
                // ignore the hi-res ones (older X11 stacks, raw
                // evdev consumers).
                self.wheel_accum_x = self.wheel_accum_x.saturating_add(dx);
                self.wheel_accum_y = self.wheel_accum_y.saturating_add(dy);
                let notch_x = self.wheel_accum_x / 120;
                let notch_y = self.wheel_accum_y / 120;
                self.wheel_accum_x -= notch_x * 120;
                self.wheel_accum_y -= notch_y * 120;
                let mut events = Vec::with_capacity(5);
                events.push(raw(
                    EventKind::Relative,
                    input_linux::RelativeAxis::HorizontalWheelHiRes as u16,
                    dx,
                ));
                events.push(raw(
                    EventKind::Relative,
                    input_linux::RelativeAxis::WheelHiRes as u16,
                    dy,
                ));
                if notch_x != 0 {
                    events.push(raw(
                        EventKind::Relative,
                        input_linux::RelativeAxis::HorizontalWheel as u16,
                        notch_x,
                    ));
                }
                if notch_y != 0 {
                    events.push(raw(
                        EventKind::Relative,
                        input_linux::RelativeAxis::Wheel as u16,
                        notch_y,
                    ));
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
        // Touchscreens are direct-mapped (1:1 with a display). Without
        // `INPUT_PROP_DIRECT`, libinput skips the touchscreen pipeline
        // and falls back to a generic absolute pointer — gestures and
        // multi-touch don't reach the compositor.
        handle.set_propbit(InputProperty::Direct)?;
        // Same rationale as the Stylus block: libinput's touch / MT-B
        // pipeline expects a non-zero resolution on the position
        // axes so it can convert between device units and the output
        // it maps to. 100 units/mm matches the Stylus device.
        let abs_setup = [
            abs_res(AbsoluteAxis::X, 0, ABS_MAX, 100),
            abs_res(AbsoluteAxis::Y, 0, ABS_MAX, 100),
            abs(AbsoluteAxis::Pressure, 0, 255),
            abs(AbsoluteAxis::MultitouchSlot, 0, MT_SLOTS - 1),
            abs(AbsoluteAxis::MultitouchTrackingId, 0, 0xFFFF),
            abs_res(AbsoluteAxis::MultitouchPositionX, 0, ABS_MAX, 100),
            abs_res(AbsoluteAxis::MultitouchPositionY, 0, ABS_MAX, 100),
            abs(AbsoluteAxis::MultitouchPressure, 0, 255),
        ];
        let full_name = format!("{name} Touchscreen");
        // `BUS_USB` (not `BUS_VIRTUAL`) so libinput's input classifier
        // recognises this as a real touchscreen — virtual-bus devices
        // are filtered out of the touch / tablet pipelines.
        handle.create(
            &make_id_bus(PRODUCT_TOUCHSCREEN, BUS_USB),
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

// ── Touchpad (Mac-style clickpad) ────────────────────────────────
//
// Same multi-touch type B protocol as `Touchscreen`, but the axis
// properties + KEY caps are tuned so libinput classifies the device
// as a *clickpad* (`INPUT_PROP_POINTER + INPUT_PROP_BUTTONPAD`).
// That unlocks the full libinput touchpad UX surface from the
// compositor's input config: tap-to-click, two-finger scroll,
// pinch-to-zoom, palm rejection, drag-lock, natural scrolling, etc.
//
// All gesture detection runs on the host (libinput → compositor) so
// the companion only has to forward raw multi-touch slots — the
// Kotlin `Gesture` state machine collapses to "every pointer in the
// MotionEvent becomes a `TouchpadSlot` packet" and Linux apps treat
// the Android tablet like a Magic Trackpad.

pub struct Touchpad {
    handle: Option<UInputHandle<File>>,
    active_slot: i32,
}

impl Touchpad {
    pub fn new() -> Self {
        Self {
            handle: None,
            active_slot: -1,
        }
    }
}

impl Default for Touchpad {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl VirtualInputDevice for Touchpad {
    fn kind(&self) -> InputKind {
        InputKind::Touchpad
    }

    async fn create(&mut self, name: &str) -> Result<(), InputError> {
        let handle = open_uinput()?;
        handle.set_evbit(EventKind::Key)?;
        handle.set_evbit(EventKind::Absolute)?;
        handle.set_evbit(EventKind::Synchronize)?;
        // Clickpad layout: only `BTN_LEFT` is advertised on the kernel
        // device — `INPUT_PROP_BUTTONPAD` tells libinput the whole
        // surface IS the single physical button, and libinput
        // synthesises BTN_RIGHT / BTN_MIDDLE itself from tap-button-map
        // or software-button regions. Advertising BTN_RIGHT/MIDDLE
        // here would trigger libinput's "clickpad advertising right
        // button" kernel-bug warning and may disable some heuristics.
        //
        // The BTN_TOOL_<N>TAP family lets libinput count contact
        // fingers (1/2/3-finger taps → L/R/M via tap-button-map).
        for button in [
            Key::ButtonLeft,
            Key::ButtonTouch,
            Key::ButtonToolFinger,
            Key::ButtonToolDoubleTap,
            Key::ButtonToolTripleTap,
            Key::ButtonToolQuadtap,
            Key::ButtonToolQuintTap,
        ] {
            handle.set_keybit(button)?;
        }
        // Indirect pointer + clickpad. libinput rejects touchpads
        // without these props (falls back to "generic touchscreen").
        handle.set_propbit(InputProperty::Pointer)?;
        handle.set_propbit(InputProperty::ButtonPad)?;
        // Resolution = 500 units/mm gives a reported device size of
        // ~65 mm × 65 mm (close to a small MacBook trackpad). The
        // previous 100 units/mm advertised a 328 mm "touchpad", which
        // makes every per-frame finger delta look like a 20-30 mm
        // jump in libinput's world — `kernel bug: Touch jump detected
        // and discarded` fires and libinput drops the events. Higher
        // resolution shrinks the mm-per-ABS-unit ratio so realistic
        // finger speeds stay below libinput's jump heuristic
        // (currently ~20 mm/event).
        let abs_setup = [
            abs_res(AbsoluteAxis::X, 0, ABS_MAX, 500),
            abs_res(AbsoluteAxis::Y, 0, ABS_MAX, 500),
            abs(AbsoluteAxis::Pressure, 0, 255),
            abs(AbsoluteAxis::MultitouchSlot, 0, MT_SLOTS - 1),
            abs(AbsoluteAxis::MultitouchTrackingId, 0, 0xFFFF),
            abs_res(AbsoluteAxis::MultitouchPositionX, 0, ABS_MAX, 500),
            abs_res(AbsoluteAxis::MultitouchPositionY, 0, ABS_MAX, 500),
            abs(AbsoluteAxis::MultitouchPressure, 0, 255),
        ];
        let full_name = format!("{name} Touchpad");
        handle.create(
            &make_id_bus(PRODUCT_TOUCHPAD, BUS_USB),
            full_name.as_bytes(),
            0,
            &abs_setup,
        )?;
        debug!(name = %full_name, "uinput touchpad created");
        self.handle = Some(handle);
        Ok(())
    }

    async fn send(&mut self, event: InputEvent) -> Result<(), InputError> {
        let h = self.handle.as_ref().ok_or(InputError::BackendUnavailable)?;
        match event {
            InputEvent::TouchpadSlot {
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
        // `INPUT_PROP_POINTER` marks this as an indirect graphics
        // tablet (Wacom Intuos-style): libinput moves the system
        // cursor instead of trying to map it 1:1 to a specific
        // display via libwacom. Without this prop the device shows
        // up under evdev but never reaches the compositor's pointer.
        handle.set_propbit(InputProperty::Pointer)?;
        // Resolution units chosen to mimic a Wacom Intuos-class
        // graphics tablet so libinput's tablet pipeline accepts us:
        //   - X/Y at 100 units/mm → 32767 range maps to ~327 mm of
        //     virtual tablet surface (libinput uses the ratio, not
        //     the absolute extent, since `INPUT_PROP_POINTER` makes
        //     this an indirect tablet that drives the system cursor).
        //   - Tilt at 57 units/radian (= 1 unit/degree) so our
        //     -90..90 degree report converts to libinput's radian
        //     convention with the right magnitude.
        // Pressure intentionally has no resolution — libinput treats
        // it as a unitless 0..1 ratio.
        let abs_setup = [
            abs_res(AbsoluteAxis::X, 0, ABS_MAX, 100),
            abs_res(AbsoluteAxis::Y, 0, ABS_MAX, 100),
            abs(AbsoluteAxis::Pressure, 0, 8191),
            abs_res(AbsoluteAxis::TiltX, -90, 90, 57),
            abs_res(AbsoluteAxis::TiltY, -90, 90, 57),
        ];
        let full_name = format!("{name} Stylus");
        // `BUS_USB` instead of `BUS_VIRTUAL` so libinput's tablet
        // classifier (and libwacom's fallback heuristics) actually
        // pick this device up — virtual-bus tablets get rejected.
        handle.create(
            &make_id_bus(PRODUCT_STYLUS, BUS_USB),
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
                // `btn` byte layout (companion → host):
                //   bit 0 : ButtonStylus  (BARREL primary)
                //   bit 1 : ButtonStylus2 (BARREL secondary)
                //   bit 2 : eraser tool active (BTN_TOOL_RUBBER)
                //   bit 7 : in-proximity (1 = pen near surface, 0 = lifted)
                //   bits 3-6 : reserved
                // When bit 7 is 0 the pen is out of proximity — release
                // both tool bits so libinput finishes the stroke and
                // the cursor doesn't appear stuck.
                let stylus_btn = (btn & 0x01) as i32;
                let stylus_btn2 = ((btn >> 1) & 0x01) as i32;
                let eraser = (btn & 0x04) != 0;
                let in_prox = (btn & 0x80) != 0;
                let touch = if in_prox && pressure > 0 { 1 } else { 0 };
                let tool_pen = if in_prox && !eraser { 1 } else { 0 };
                let tool_rub = if in_prox && eraser { 1 } else { 0 };
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
                        raw(EventKind::Key, Key::ButtonToolPen as u16, tool_pen),
                        raw(EventKind::Key, Key::ButtonToolRubber as u16, tool_rub),
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
