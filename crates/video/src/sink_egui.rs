//! Screen-mirror window backed by `eframe::Renderer::Wgpu`.
//!
//! Producer (daemon-core's Video stream loop) populates a
//! [`FrameSlot`]; the [`MirrorApp`] consumes the slot, converts each
//! `DecodedFrame` to an `egui::ColorImage` (BT.601 limited range for
//! YUV), uploads via the egui texture manager (wgpu underneath) and
//! draws it preserving aspect ratio.
//!
//! Step 9.5 wires this into the daemon's `Device.ShowScreen` D-Bus
//! method; the same slot is shared between the decoder task and the
//! window thread so reconfigures + reconnects don't tear the GUI
//! down.

use std::sync::{Arc, Mutex};

use ansync_proto::InputMessage;
use tokio::sync::mpsc::UnboundedSender;
use tracing::info;

use crate::{DecodedFrame, PixelFormat};

/// Shared "latest decoded frame" slot. Single-slot mailbox: producer
/// overwrites, consumer takes. Live-mirror prefers latency over
/// completeness, so dropping the older frame on overwrite is the
/// right policy.
pub type FrameSlot = Arc<Mutex<Option<DecodedFrame>>>;

pub fn new_slot() -> FrameSlot {
    Arc::new(Mutex::new(None))
}

/// Block the calling thread on `eframe::run_native`. Spawn this on a
/// dedicated thread when the caller wants the daemon to keep running
/// alongside the window — winit on Linux is happy on any thread.
///
/// `input_tx` is `Some` for prod paths where the host wants to push
/// the mirror window's pointer / key events back to the peer; pass
/// `None` for the dev `--play-file` path where there is no peer to
/// drive.
pub fn run(
    title: String,
    slot: FrameSlot,
    input_tx: Option<UnboundedSender<InputMessage>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([960.0, 540.0])
            .with_title(title.clone()),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native(
        &title,
        native_options,
        Box::new(move |_cc| Ok(Box::new(MirrorApp::new(slot, input_tx)))),
    )
    .map_err(|e| format!("eframe: {e}").into())
}

pub struct MirrorApp {
    slot: FrameSlot,
    texture: Option<egui::TextureHandle>,
    last_size: Option<(u32, u32)>,
    input_tx: Option<UnboundedSender<InputMessage>>,
    last_pointer: Option<egui::Pos2>,
}

impl MirrorApp {
    pub fn new(slot: FrameSlot, input_tx: Option<UnboundedSender<InputMessage>>) -> Self {
        Self {
            slot,
            texture: None,
            last_size: None,
            input_tx,
            last_pointer: None,
        }
    }
}

impl eframe::App for MirrorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let taken = {
            let mut slot = self.slot.lock().ok();
            slot.as_mut().and_then(|s| s.take())
        };
        if let Some(decoded) = taken {
            let size = (decoded.width, decoded.height);
            let image = to_color_image(&decoded);
            let handle = ctx.load_texture("ansync-mirror", image, egui::TextureOptions::LINEAR);
            self.texture = Some(handle);
            if self.last_size != Some(size) {
                info!(width = size.0, height = size.1, "first frame uploaded");
                self.last_size = Some(size);
            }
        }
        let frame_dims = self.last_size;
        let mut hit_rect: Option<egui::Rect> = None;
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(tex) = &self.texture {
                let available = ui.available_size();
                let aspect = tex.aspect_ratio();
                let mut size = available;
                if size.x / size.y > aspect {
                    size.x = size.y * aspect;
                } else {
                    size.y = size.x / aspect;
                }
                ui.centered_and_justified(|ui| {
                    let resp =
                        ui.add(egui::Image::from_texture(tex).fit_to_exact_size(size));
                    hit_rect = Some(resp.rect);
                });
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label("waiting for first decoded frame…");
                });
            }
        });
        if let (Some(rect), Some((fw, fh)), Some(tx)) =
            (hit_rect, frame_dims, self.input_tx.as_ref())
        {
            emit_pointer_events(ctx, &mut self.last_pointer, rect, fw, fh, tx);
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(16));
    }
}

/// Map egui pointer state inside `rect` to absolute coordinates in
/// the remote display's coordinate space (`fw × fh`) and emit
/// `InputMessage::TouchSlot` events. Touch events drive
/// `AccessibilityService.dispatchGesture` on the peer (Step 7e).
fn emit_pointer_events(
    ctx: &egui::Context,
    last: &mut Option<egui::Pos2>,
    rect: egui::Rect,
    fw: u32,
    fh: u32,
    tx: &UnboundedSender<InputMessage>,
) {
    let mut tracking_id: i32 = 0;
    ctx.input(|i| {
        let Some(pos) = i.pointer.hover_pos() else {
            // Pointer left the window: emit a synthetic lift on the
            // last known slot so the peer releases the touch.
            if last.take().is_some() {
                let _ = tx.send(InputMessage::TouchSlot {
                    slot: 0,
                    x: 0,
                    y: 0,
                    pressure: 0,
                    tracking_id: -1,
                });
            }
            return;
        };
        if !rect.contains(pos) {
            return;
        }
        let nx = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        let ny = ((pos.y - rect.top()) / rect.height()).clamp(0.0, 1.0);
        let abs_x = (nx * fw as f32) as i32;
        let abs_y = (ny * fh as f32) as i32;
        let pressed = i.pointer.primary_down();
        // Move when changed.
        let changed = last.map(|p| p != pos).unwrap_or(true);
        if changed {
            *last = Some(pos);
            if pressed {
                tracking_id = 1;
                let _ = tx.send(InputMessage::TouchSlot {
                    slot: 0,
                    x: abs_x,
                    y: abs_y,
                    pressure: 255,
                    tracking_id,
                });
            }
        }
        // Press / release transitions.
        if i.pointer.primary_pressed() {
            let _ = tx.send(InputMessage::TouchSlot {
                slot: 0,
                x: abs_x,
                y: abs_y,
                pressure: 255,
                tracking_id: 1,
            });
        }
        if i.pointer.primary_released() {
            let _ = tx.send(InputMessage::TouchSlot {
                slot: 0,
                x: abs_x,
                y: abs_y,
                pressure: 0,
                tracking_id: -1,
            });
        }
    });
}

/// Convert a [`DecodedFrame`] to an `egui::ColorImage` in RGBA8.
pub fn to_color_image(frame: &DecodedFrame) -> egui::ColorImage {
    let w = frame.width as usize;
    let h = frame.height as usize;
    let stride = frame.stride.max(1) as usize;
    let mut pixels = vec![egui::Color32::BLACK; w * h];
    match frame.format {
        PixelFormat::Rgba8 => copy_rgba(&frame.data, stride, w, h, &mut pixels),
        PixelFormat::Bgra8 => copy_bgra(&frame.data, stride, w, h, &mut pixels),
        PixelFormat::Nv12 => convert_nv12(&frame.data, stride, w, h, &mut pixels),
        PixelFormat::I420 => convert_i420(&frame.data, stride, w, h, &mut pixels),
    }
    egui::ColorImage {
        size: [w, h],
        pixels,
    }
}

fn copy_rgba(src: &[u8], stride: usize, w: usize, h: usize, dst: &mut [egui::Color32]) {
    for y in 0..h {
        let row = &src[y * stride..y * stride + w * 4];
        for x in 0..w {
            let p = &row[x * 4..x * 4 + 4];
            dst[y * w + x] = egui::Color32::from_rgba_unmultiplied(p[0], p[1], p[2], p[3]);
        }
    }
}

fn copy_bgra(src: &[u8], stride: usize, w: usize, h: usize, dst: &mut [egui::Color32]) {
    for y in 0..h {
        let row = &src[y * stride..y * stride + w * 4];
        for x in 0..w {
            let p = &row[x * 4..x * 4 + 4];
            dst[y * w + x] = egui::Color32::from_rgba_unmultiplied(p[2], p[1], p[0], p[3]);
        }
    }
}

fn convert_nv12(src: &[u8], stride: usize, w: usize, h: usize, dst: &mut [egui::Color32]) {
    let y_plane = &src[..stride * h];
    let uv_plane = &src[stride * h..stride * h + stride * (h / 2)];
    for y in 0..h {
        for x in 0..w {
            let yy = y_plane[y * stride + x] as i32;
            let uv_row = (y / 2) * stride;
            let uv_x = (x / 2) * 2;
            let cb = uv_plane[uv_row + uv_x] as i32 - 128;
            let cr = uv_plane[uv_row + uv_x + 1] as i32 - 128;
            dst[y * w + x] = yuv_to_rgba(yy, cb, cr);
        }
    }
}

fn convert_i420(src: &[u8], stride: usize, w: usize, h: usize, dst: &mut [egui::Color32]) {
    let half_stride = stride / 2;
    let y_plane = &src[..stride * h];
    let u_off = stride * h;
    let v_off = u_off + half_stride * (h / 2);
    let u_plane = &src[u_off..u_off + half_stride * (h / 2)];
    let v_plane = &src[v_off..v_off + half_stride * (h / 2)];
    for y in 0..h {
        for x in 0..w {
            let yy = y_plane[y * stride + x] as i32;
            let cb = u_plane[(y / 2) * half_stride + x / 2] as i32 - 128;
            let cr = v_plane[(y / 2) * half_stride + x / 2] as i32 - 128;
            dst[y * w + x] = yuv_to_rgba(yy, cb, cr);
        }
    }
}

#[inline]
fn yuv_to_rgba(y: i32, cb: i32, cr: i32) -> egui::Color32 {
    let y = (y - 16).max(0);
    let r = ((298 * y + 409 * cr + 128) >> 8).clamp(0, 255) as u8;
    let g = ((298 * y - 100 * cb - 208 * cr + 128) >> 8).clamp(0, 255) as u8;
    let b = ((298 * y + 516 * cb + 128) >> 8).clamp(0, 255) as u8;
    egui::Color32::from_rgb(r, g, b)
}
