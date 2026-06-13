//! Step 6 screen-mirror window — egui + wgpu presenter.
//!
//! `ansyncd --play-file foo.h264` brings up this window and feeds the
//! decoder from a local Annex-B recording. Once Step 7 lands real
//! peer streams, the same `LatestFrame` slot will be driven by the
//! QUIC video stream rather than [`ansync_video::feed::AnnexBFile`].
//!
//! Frames travel as `ColorImage` through egui's texture manager,
//! which uploads to a `wgpu::Texture` underneath (eframe is configured
//! with `Renderer::Wgpu`). For Step 6 the colour-space conversion
//! (NV12 / BGRA → RGBA) runs on the CPU; a shader-side NV12 sampler
//! is a Step-11-ish optimisation when audio drives presentation
//! pacing and frame budget gets tighter.

use std::sync::{Arc, Mutex};

use ansync_video::{
    DecodedFrame, HostDecoder, PixelFormat, VideoCodec, VideoDecoder, VideoError,
    feed::AnnexBFile, local_decoder_caps,
};
use tracing::{error, info, warn};

pub type LatestFrame = Arc<Mutex<Option<DecodedFrame>>>;

/// Block the calling thread on `eframe::run_native`. The decoder loop
/// is already running on the tokio runtime when this fires — we only
/// need a shared slot to pull from.
pub fn run(shared: LatestFrame) -> Result<(), Box<dyn std::error::Error>> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([960.0, 540.0])
            .with_title("ansync mirror"),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native(
        "ansync mirror",
        native_options,
        Box::new(move |_cc| Ok(Box::new(MirrorApp::new(shared)))),
    )
    .map_err(|e| format!("eframe: {e}").into())
}

struct MirrorApp {
    shared: LatestFrame,
    texture: Option<egui::TextureHandle>,
    last_size: Option<(u32, u32)>,
}

impl MirrorApp {
    fn new(shared: LatestFrame) -> Self {
        Self {
            shared,
            texture: None,
            last_size: None,
        }
    }
}

impl eframe::App for MirrorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let taken = {
            let mut slot = self.shared.lock().ok();
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
                    ui.add(egui::Image::from_texture(tex).fit_to_exact_size(size));
                });
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label("waiting for first decoded frame…");
                });
            }
        });
        ctx.request_repaint_after(std::time::Duration::from_millis(16));
    }
}

/// Convert a [`DecodedFrame`] to an `egui::ColorImage` in RGBA8. NV12
/// → RGBA uses BT.601 limited range (Android `MediaCodec`'s default
/// output). BGRA / RGBA passthrough handles channel order in-place.
fn to_color_image(frame: &DecodedFrame) -> egui::ColorImage {
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
    // Y plane: h rows of `stride` bytes. UV plane: h/2 rows of `stride`
    // bytes (interleaved Cb Cr Cb Cr ...). The plane offset is at
    // `stride * h`.
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
    // BT.601 limited range, integer math (Q8). Output clamped to
    // 0..=255 — the negative values come from headroom/footroom (Y in
    // 16..=235) so a small under/overshoot is expected.
    let y = (y - 16).max(0);
    let r = ((298 * y + 409 * cr + 128) >> 8).clamp(0, 255) as u8;
    let g = ((298 * y - 100 * cb - 208 * cr + 128) >> 8).clamp(0, 255) as u8;
    let b = ((298 * y + 516 * cb + 128) >> 8).clamp(0, 255) as u8;
    egui::Color32::from_rgb(r, g, b)
}

/// Decode loop driving [`HostDecoder`] from an Annex-B file. Pushes
/// each successfully-decoded frame into `shared`. Sleeps between
/// packets to roughly emulate 30 fps; real wall-clock pacing waits
/// for Step 8 when QUIC delivery sets the cadence.
pub async fn run_play_file_loop(
    path: std::path::PathBuf,
    shared: LatestFrame,
) -> Result<(), VideoError> {
    let mut file = AnnexBFile::open(&path).await?;
    let codec = if file.is_h265() {
        VideoCodec::H265
    } else {
        VideoCodec::H264
    };
    let caps = local_decoder_caps();
    if !caps.can_decode.contains(&codec) {
        return Err(VideoError::DecoderUnavailable(format!(
            "local host cannot decode {codec:?} (caps: {:?})",
            caps.can_decode
        )));
    }
    // Initial dimension hint is overridden by SPS on first IDR — pick
    // a common 1080p as a reasonable default for surface pool sizing.
    let mut decoder = HostDecoder::configure(codec, 1920, 1080)?;
    info!(?codec, path = %path.display(), "decoder configured");
    let mut frame_period = tokio::time::interval(std::time::Duration::from_millis(33));
    loop {
        let Some(packet) = file.next_packet().await? else {
            info!("end of Annex-B stream");
            return Ok(());
        };
        if let Err(e) = decoder.feed(packet.data).await {
            warn!(error = %e, "feed failed; continuing");
            continue;
        }
        if let Some(frame) = decoder.take().await? {
            if let Ok(mut slot) = shared.lock() {
                *slot = Some(frame);
            }
        }
        frame_period.tick().await;
    }
}

pub fn spawn_play_file(
    runtime: &tokio::runtime::Runtime,
    path: std::path::PathBuf,
    shared: LatestFrame,
) {
    runtime.spawn(async move {
        if let Err(e) = run_play_file_loop(path, shared).await {
            error!(error = %e, "play-file decode loop exited with error");
        }
    });
}
