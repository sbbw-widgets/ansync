//! Bluetooth HID Device profile via `bluer`.
//!
//! Scope: scaffold. Wires the trait surface
//! ([`BtHidFactory`] → [`InputDeviceFactory`]) so `daemon-core` can
//! swap a uinput backend for a BT one transparently. The actual HID
//! report transmission is deferred to a follow-up because it needs
//! a full SDP record + L2CAP socket dance that varies across BlueZ
//! versions. For now the devices accept events, derive HID reports
//! locally, and log them — enough to verify routing without
//! claiming a real BT HID profile slot.
//!
//! Topology when fully wired (deferred):
//!
//!   peer → daemon-core → BtHidKeyboard → HID report bytes ─▶
//!     bluer L2CAP control/interrupt channels → host
//!
//! The factory exposes the same five `build_*` methods as
//! `UinputFactory`, so opting into BT-HID only requires a config
//! toggle in `DaemonConfig` (Step 14 hooks).

use async_trait::async_trait;
use tracing::{info, warn};

use crate::{InputError, InputEvent, InputKind, VirtualInputDevice};
use crate::session::InputDeviceFactory;

/// Stub HID factory. Constructing it doesn't open any BT sockets
/// yet; that happens lazily inside each device's `create()`.
pub struct BtHidFactory;

impl InputDeviceFactory for BtHidFactory {
    fn build_keyboard(&self) -> Box<dyn VirtualInputDevice> {
        Box::new(BtHidDevice::new(InputKind::Keyboard))
    }
    fn build_mouse(&self) -> Box<dyn VirtualInputDevice> {
        Box::new(BtHidDevice::new(InputKind::Mouse))
    }
    fn build_touchscreen(&self) -> Box<dyn VirtualInputDevice> {
        Box::new(BtHidDevice::new(InputKind::Touchscreen))
    }
    fn build_stylus(&self) -> Box<dyn VirtualInputDevice> {
        Box::new(BtHidDevice::new(InputKind::Stylus))
    }
    fn build_gamepad(&self) -> Box<dyn VirtualInputDevice> {
        Box::new(BtHidDevice::new(InputKind::Gamepad))
    }
}

pub struct BtHidDevice {
    kind: InputKind,
    name: Option<String>,
}

impl BtHidDevice {
    fn new(kind: InputKind) -> Self {
        Self { kind, name: None }
    }
}

#[async_trait]
impl VirtualInputDevice for BtHidDevice {
    fn kind(&self) -> InputKind {
        self.kind
    }

    async fn create(&mut self, name: &str) -> Result<(), InputError> {
        // Lazy adapter probe — if BlueZ isn't running we surface
        // `BackendUnavailable` so the orchestrator can log + skip.
        let session = bluer::Session::new()
            .await
            .map_err(|e| {
                warn!(error = %e, "bluer::Session::new failed");
                InputError::BackendUnavailable
            })?;
        let adapter = session
            .default_adapter()
            .await
            .map_err(|e| {
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
            .map(|a| a.to_string())
            .unwrap_or_default();
        info!(adapter = %addr, kind = ?self.kind, name,
              "BT-HID device ready (SDP/L2CAP registration deferred)");
        self.name = Some(name.to_string());
        Ok(())
    }

    async fn send(&mut self, event: InputEvent) -> Result<(), InputError> {
        // Until the full L2CAP wiring lands we log the synthesised
        // HID report shape. Keep the level at `debug` so production
        // logs don't fill up.
        tracing::debug!(kind = ?self.kind, ?event, "bt-hid: would emit HID report");
        Ok(())
    }

    async fn destroy(&mut self) -> Result<(), InputError> {
        self.name = None;
        Ok(())
    }
}
