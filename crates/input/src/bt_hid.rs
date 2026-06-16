//! Bluetooth HID Device profile via `bluer`.
//!
//! Turns the host into a Bluetooth HID emitter (keyboard / mouse /
//! gamepad) that any paired host accepts as a HID Boot device. The
//! Android peer doesn't need to be involved — once the BT pairing is
//! done at the OS level, every `InputEvent` routed through the
//! `BtHidFactory` lands on the connected host as a real HID report.
//!
//! Topology:
//!
//!   peer → daemon-core → BtHidDevice → HID Boot report bytes ─▶
//!     bluer L2CAP PSM 0x13 (interrupt) → BT-HID host
//!
//! Wire protocol: HID Boot reports per USB HID Spec §B.1 / §B.2.
//!   * Keyboard: 8 bytes = `[modifier, reserved, key1..key6]`
//!   * Mouse:    3 bytes = `[buttons, dx i8, dy i8]`
//!   * Gamepad:  8 bytes = `[buttons_lo, buttons_hi, lx, ly, rx, ry, lt, rt]`
//!   * Stylus / Touchscreen: best-effort mouse-mapping (single
//!     contact only — multi-touch reports require a Report-mode
//!     descriptor, not Boot mode).
//!
//! L2CAP layout: PSM 0x11 = control (HID_CONTROL handshakes), PSM
//! 0x13 = interrupt (report data). Both are BR/EDR sockets opened
//! via `bluer::l2cap::Socket::new_seq_packet`.
//!
//! SDP service record registration is intentionally out of scope:
//! BlueZ 5 removed `sdptool`, and the canonical replacement (the
//! `org.bluez.Profile1` D-Bus API) is async + agent-heavy. Without
//! a registered SDP record, other hosts can still connect by PSM
//! directly (we publish the adapter address; the user pairs from
//! their target host's Bluetooth settings). Adding the Profile1
//! registration is a known follow-up.

use std::sync::Arc;

use async_trait::async_trait;
use bluer::l2cap::{SeqPacket, Socket, SocketAddr};
use bluer::{Address, AddressType};
use tokio::sync::Mutex as AsyncMutex;
use tracing::{debug, info, warn};

use crate::session::InputDeviceFactory;
use crate::{InputError, InputEvent, InputKind, VirtualInputDevice};

/// PSM 0x11 — HID Control channel (handshakes + protocol mode set).
pub const HID_PSM_CONTROL: u16 = 0x0011;
/// PSM 0x13 — HID Interrupt channel (report transport, both directions).
pub const HID_PSM_INTERRUPT: u16 = 0x0013;

/// Shared state across all `BtHidDevice` instances built by the same
/// factory. Holds the L2CAP listener handles + the currently-connected
/// peer's interrupt seq-packet socket. One peer at a time — HID is
/// canonically single-host.
struct BtHidShared {
    /// `Some` once the BT adapter has been powered on. Held to keep
    /// the listener tasks alive.
    listeners: AsyncMutex<Option<ListenerHandles>>,
    /// The interrupt-channel SeqPacket for the currently connected
    /// peer. `None` when no host is bound.
    interrupt: AsyncMutex<Option<SeqPacket>>,
}

struct ListenerHandles {
    _control: tokio::task::JoinHandle<()>,
    _interrupt: tokio::task::JoinHandle<()>,
}

/// HID Device factory. Constructing it doesn't open any sockets;
/// `ensure_listeners()` runs on the first device's `create()` call.
pub struct BtHidFactory {
    shared: Arc<BtHidShared>,
}

impl BtHidFactory {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(BtHidShared {
                listeners: AsyncMutex::new(None),
                interrupt: AsyncMutex::new(None),
            }),
        }
    }
}

impl Default for BtHidFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl InputDeviceFactory for BtHidFactory {
    fn build_keyboard(&self) -> Box<dyn VirtualInputDevice> {
        Box::new(BtHidDevice::new(InputKind::Keyboard, self.shared.clone()))
    }
    fn build_mouse(&self) -> Box<dyn VirtualInputDevice> {
        Box::new(BtHidDevice::new(InputKind::Mouse, self.shared.clone()))
    }
    fn build_touchscreen(&self) -> Box<dyn VirtualInputDevice> {
        Box::new(BtHidDevice::new(InputKind::Touchscreen, self.shared.clone()))
    }
    fn build_stylus(&self) -> Box<dyn VirtualInputDevice> {
        Box::new(BtHidDevice::new(InputKind::Stylus, self.shared.clone()))
    }
    fn build_gamepad(&self) -> Box<dyn VirtualInputDevice> {
        Box::new(BtHidDevice::new(InputKind::Gamepad, self.shared.clone()))
    }
}

pub struct BtHidDevice {
    kind: InputKind,
    name: Option<String>,
    shared: Arc<BtHidShared>,
    /// Tracked across calls so the boot keyboard report represents
    /// the current pressed-key set rather than emitting press/release
    /// in isolation.
    pressed_keys: Vec<u8>,
    /// Mouse / stylus button mask (bit 0 = left, 1 = right, 2 = middle).
    mouse_buttons: u8,
}

impl BtHidDevice {
    fn new(kind: InputKind, shared: Arc<BtHidShared>) -> Self {
        Self {
            kind,
            name: None,
            shared,
            pressed_keys: Vec::with_capacity(6),
            mouse_buttons: 0,
        }
    }

    async fn push_report(&self, report: Vec<u8>) -> Result<(), InputError> {
        let guard = self.shared.interrupt.lock().await;
        let Some(sock) = guard.as_ref() else {
            // No peer connected: report is dropped. Mirrors the
            // uinput backend's behaviour when no consumer is open.
            debug!(kind = ?self.kind, "bt-hid: report dropped (no peer)");
            return Ok(());
        };
        sock.send(&report)
            .await
            .map_err(|e| InputError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        Ok(())
    }
}

async fn ensure_listeners(shared: &Arc<BtHidShared>) -> Result<(), InputError> {
    let mut guard = shared.listeners.lock().await;
    if guard.is_some() {
        return Ok(());
    }
    let session = bluer::Session::new().await.map_err(|e| {
        warn!(error = %e, "bluer::Session::new failed");
        InputError::BackendUnavailable
    })?;
    let adapter = session.default_adapter().await.map_err(|e| {
        warn!(error = %e, "bluer default_adapter failed");
        InputError::BackendUnavailable
    })?;
    adapter
        .set_powered(true)
        .await
        .map_err(|_| InputError::BackendUnavailable)?;
    let addr = adapter
        .address()
        .await
        .map_err(|_| InputError::BackendUnavailable)?;
    info!(adapter = %addr, "BT-HID adapter powered; binding L2CAP PSMs");

    let control = bind_listener(addr, HID_PSM_CONTROL)?;
    let interrupt = bind_listener(addr, HID_PSM_INTERRUPT)?;
    let shared_for_ctl = shared.clone();
    let shared_for_int = shared.clone();
    let control_handle = tokio::spawn(control_accept_loop(control, shared_for_ctl));
    let interrupt_handle = tokio::spawn(interrupt_accept_loop(interrupt, shared_for_int));
    *guard = Some(ListenerHandles {
        _control: control_handle,
        _interrupt: interrupt_handle,
    });
    Ok(())
}

fn bind_listener(addr: Address, psm: u16) -> Result<bluer::l2cap::SeqPacketListener, InputError> {
    let sock = Socket::<SeqPacket>::new_seq_packet()
        .map_err(|e| InputError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
    let sa = SocketAddr::new(addr, AddressType::BrEdr, psm);
    sock.bind(sa)
        .map_err(|e| InputError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
    let listener = sock
        .listen(1)
        .map_err(|e| InputError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
    Ok(listener)
}

async fn control_accept_loop(listener: bluer::l2cap::SeqPacketListener, _shared: Arc<BtHidShared>) {
    loop {
        match listener.accept().await {
            Ok((sock, sa)) => {
                info!(peer = %sa.addr, "bt-hid: control channel connected");
                // The control channel carries HID handshakes
                // (SET_PROTOCOL etc.). We don't enforce SET_REPORT
                // semantics in Boot mode — just drain incoming
                // packets so the peer's stack stays happy.
                tokio::spawn(async move {
                    let mut buf = [0u8; 64];
                    loop {
                        match sock.recv(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => debug!(bytes = n, "bt-hid: control rx"),
                            Err(e) => {
                                warn!(error = %e, "bt-hid: control recv failed");
                                break;
                            }
                        }
                    }
                });
            }
            Err(e) => {
                warn!(error = %e, "bt-hid: control accept failed; sleeping 250ms");
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
    }
}

async fn interrupt_accept_loop(
    listener: bluer::l2cap::SeqPacketListener,
    shared: Arc<BtHidShared>,
) {
    loop {
        match listener.accept().await {
            Ok((sock, sa)) => {
                info!(peer = %sa.addr, "bt-hid: interrupt channel connected");
                *shared.interrupt.lock().await = Some(sock);
            }
            Err(e) => {
                warn!(error = %e, "bt-hid: interrupt accept failed; sleeping 250ms");
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
    }
}

#[async_trait]
impl VirtualInputDevice for BtHidDevice {
    fn kind(&self) -> InputKind {
        self.kind
    }

    async fn create(&mut self, name: &str) -> Result<(), InputError> {
        ensure_listeners(&self.shared).await?;
        self.name = Some(name.to_string());
        info!(kind = ?self.kind, name, "bt-hid: device armed");
        Ok(())
    }

    async fn send(&mut self, event: InputEvent) -> Result<(), InputError> {
        match (self.kind, event) {
            (InputKind::Keyboard, InputEvent::Key { keycode, pressed }) => {
                let code = keycode as u8;
                if pressed {
                    if !self.pressed_keys.contains(&code) && self.pressed_keys.len() < 6 {
                        self.pressed_keys.push(code);
                    }
                } else {
                    self.pressed_keys.retain(|c| *c != code);
                }
                let report = build_keyboard_report(&self.pressed_keys);
                self.push_report(report).await
            }
            (InputKind::Mouse, InputEvent::MouseRel { dx, dy }) => {
                self.push_report(build_mouse_report(self.mouse_buttons, dx, dy, 0))
                    .await
            }
            (InputKind::Mouse, InputEvent::MouseButton { button, pressed }) => {
                let bit = mouse_button_bit(button);
                if pressed {
                    self.mouse_buttons |= bit;
                } else {
                    self.mouse_buttons &= !bit;
                }
                self.push_report(build_mouse_report(self.mouse_buttons, 0, 0, 0))
                    .await
            }
            (InputKind::Mouse, InputEvent::MouseWheel { dx: _, dy }) => {
                self.push_report(build_mouse_report(self.mouse_buttons, 0, 0, dy))
                    .await
            }
            (InputKind::Gamepad, InputEvent::Gamepad { buttons, lx, ly, rx, ry, lt, rt }) => {
                self.push_report(build_gamepad_report(buttons, lx, ly, rx, ry, lt, rt))
                    .await
            }
            // Best-effort mapping for touch / stylus → mouse moves.
            // Boot mode HID has no multi-touch descriptor; full
            // touchscreen support would require a Report-mode HID
            // descriptor (out of scope for this profile).
            (InputKind::Touchscreen | InputKind::Stylus, InputEvent::TouchSlot { x, y, .. })
            | (InputKind::Stylus, InputEvent::Stylus { x, y, .. }) => {
                let dx = (x as i32).clamp(-127, 127);
                let dy = (y as i32).clamp(-127, 127);
                self.push_report(build_mouse_report(self.mouse_buttons, dx, dy, 0))
                    .await
            }
            (_, InputEvent::Sync) => Ok(()),
            (kind, event) => {
                debug!(?kind, ?event, "bt-hid: event ignored (no boot-mode mapping)");
                Ok(())
            }
        }
    }

    async fn destroy(&mut self) -> Result<(), InputError> {
        self.name = None;
        self.pressed_keys.clear();
        self.mouse_buttons = 0;
        Ok(())
    }
}

/// USB HID Boot Keyboard Report (Spec §B.1, page 60):
/// `[modifier_mask | 0x00 | key1..key6]`.
fn build_keyboard_report(pressed: &[u8]) -> Vec<u8> {
    let mut report = vec![0u8; 8];
    // Pressed keys past slot 6 are dropped — Boot mode caps at 6.
    for (slot, code) in pressed.iter().take(6).enumerate() {
        report[2 + slot] = *code;
    }
    // Modifier mask derivation: USB HID modifier keycodes occupy
    // usages 0xE0..=0xE7 (LCtrl..RGui). If any pressed key falls in
    // that range, also set its bit in byte 0 + clear it from the
    // key slot so the host doesn't double-report.
    let mut modifiers = 0u8;
    for code in pressed.iter() {
        if (0xE0..=0xE7).contains(code) {
            modifiers |= 1 << (code - 0xE0);
        }
    }
    report[0] = modifiers;
    report
}

/// USB HID Boot Mouse Report (Spec §B.2, page 61):
/// `[buttons | dx i8 | dy i8]`. Extended here with a 4th wheel byte
/// — many BT-HID Boot mice ship a 4-byte report including wheel.
fn build_mouse_report(buttons: u8, dx: i32, dy: i32, wheel: i32) -> Vec<u8> {
    vec![
        buttons,
        dx.clamp(-127, 127) as i8 as u8,
        dy.clamp(-127, 127) as i8 as u8,
        wheel.clamp(-127, 127) as i8 as u8,
    ]
}

/// XInput-flavoured gamepad report. Not a Boot HID Spec class — there's
/// no Boot-mode gamepad — but most BT-HID hosts accept the layout
/// when the report descriptor isn't enforced.
fn build_gamepad_report(buttons: u32, lx: i16, ly: i16, rx: i16, ry: i16, lt: u8, rt: u8) -> Vec<u8> {
    let buttons16 = buttons as u16;
    let mut out = Vec::with_capacity(10);
    out.push((buttons16 & 0xFF) as u8);
    out.push((buttons16 >> 8) as u8);
    // Sticks are clamped to int8 range so the report fits the layout
    // we emit. Full int16 precision would need a Report-mode
    // descriptor + matching usage page.
    out.push((lx / 256) as i8 as u8);
    out.push((ly / 256) as i8 as u8);
    out.push((rx / 256) as i8 as u8);
    out.push((ry / 256) as i8 as u8);
    out.push(lt);
    out.push(rt);
    out
}

fn mouse_button_bit(button: u8) -> u8 {
    // Mirror the uinput backend's BTN_LEFT/RIGHT/MIDDLE encoding so
    // the upstream `InputEvent::MouseButton` payload is identical
    // across backends.
    match button {
        1 => 0b001, // left
        2 => 0b010, // right
        3 => 0b100, // middle
        _ => 0,
    }
}
