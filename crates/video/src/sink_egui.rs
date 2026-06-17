//! Screen-mirror window backed by `eframe::Renderer::Wgpu`.
//!
//! The decoder produces NV12 frames on the GPU (NVDEC) which
//! ferricast hands us as host-side `Bytes`. The naive path
//! (YUV→RGBA on CPU, copy into an `egui::ColorImage`, upload via
//! `Context::load_texture`) allocated **24 MB of host RAM per frame
//! at 1080p60** — an 8 MB BGRA buffer from the decoder, an 8 MB
//! `Vec<Color32>` from the conversion, and 8 MB inside egui's
//! texture queue. At 60 fps and unbounded NVDEC output, the daemon
//! RSS climbed to multiple GB in seconds.
//!
//! This module instead uses an `egui_wgpu::CallbackTrait` to push
//! NV12 straight into two GPU textures (Y as R8Unorm, UV as
//! Rg8Unorm) and runs a tiny YUV→RGB conversion in the fragment
//! shader. The CPU side never owns more than the 3 MB NV12 frame
//! we just decoded, and the egui texture path is bypassed entirely.
//!
//! The eframe app **never** drives its own repaint cadence — there
//! is no `request_repaint_after` poll. The producer (the QUIC video
//! stream loop) calls [`MirrorSlot::store`] each time a new frame
//! lands, which wakes egui via the cached [`egui::Context`]. With no
//! incoming frames the GUI stays idle, which is the difference
//! between ~0 % CPU and ~25 % CPU per peer on the host.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ansync_proto::InputMessage;
use eframe::egui_wgpu;
use eframe::wgpu;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{error, info, warn};

use crate::{DecodedFrame, PixelFormat};

/// Single-shot trace gate. `set()` returns `true` on the first call,
/// `false` on every subsequent call, so a hot path can log one
/// breadcrumb per program run without ever spamming the journal.
static FIRST_PREPARE_LOGGED: AtomicBool = AtomicBool::new(false);
static FIRST_PAINT_LOGGED: AtomicBool = AtomicBool::new(false);
static FIRST_UPLOAD_LOGGED: AtomicBool = AtomicBool::new(false);
static FIRST_LAYOUT_LOGGED: AtomicBool = AtomicBool::new(false);
static FIRST_INPUT_LOGGED: AtomicBool = AtomicBool::new(false);

fn fire_once(flag: &AtomicBool) -> bool {
    !flag.swap(true, Ordering::Relaxed)
}

/// Single-slot "latest decoded frame" mailbox shared between the
/// QUIC video loop (producer) and the eframe paint loop (consumer).
///
/// Producer overwrites; consumer takes. Live-mirror prefers latency
/// over completeness, so dropping the older frame on overwrite is
/// the right policy. After every `store` the slot calls
/// `ctx.request_repaint()` on the cached egui context so the UI
/// wakes from idle without us having to poll on a timer.
pub struct MirrorSlot {
    inner: Mutex<Option<DecodedFrame>>,
    /// Filled in by `MirrorApp` on its first `update`. `Mutex` (not
    /// `OnceLock`) so the slot can be cloned for a second window
    /// later — overwriting the handle is fine, both contexts share
    /// the same wgpu device.
    ctx: Mutex<Option<egui::Context>>,
}

impl MirrorSlot {
    fn new() -> Self {
        Self {
            inner: Mutex::new(None),
            ctx: Mutex::new(None),
        }
    }

    /// Producer entry point. Overwrites the slot with `frame` and
    /// asks egui to repaint. Both operations are cheap — the egui
    /// repaint request coalesces with any pending wake-up.
    pub fn store(&self, frame: DecodedFrame) {
        if let Ok(mut s) = self.inner.lock() {
            *s = Some(frame);
        }
        if let Ok(ctx) = self.ctx.lock() {
            if let Some(ctx) = ctx.as_ref() {
                ctx.request_repaint();
            }
        }
    }

    /// Consumer entry point. Returns the latest decoded frame and
    /// clears the slot. Subsequent paints with no new frame see
    /// `None` and re-render the previously-uploaded texture without
    /// touching CPU memory.
    pub fn take(&self) -> Option<DecodedFrame> {
        self.inner.lock().ok().and_then(|mut s| s.take())
    }

    fn attach_ctx(&self, ctx: egui::Context) {
        if let Ok(mut g) = self.ctx.lock() {
            *g = Some(ctx);
        }
    }
}

pub type FrameSlot = Arc<MirrorSlot>;

pub fn new_slot() -> FrameSlot {
    Arc::new(MirrorSlot::new())
}

/// Block the calling thread on `eframe::run_native`. Spawn this on a
/// dedicated thread when the caller wants the daemon to keep running
/// alongside the window — winit on Linux is happy on any thread.
pub fn run(
    title: String,
    slot: FrameSlot,
    input_tx: Option<UnboundedSender<InputMessage>>,
) -> Result<(), Box<dyn std::error::Error>> {
    // The mirror window is spawned from `daemon-core::action_loop` on
    // a dedicated thread, which winit/eframe normally refuses on
    // Linux (X11 + Wayland event loops insist on the main thread).
    // Both backends ship an `any_thread` opt-in for embedders that
    // genuinely want the per-window thread model — that's us.
    let event_loop_builder: eframe::EventLoopBuilderHook =
        Box::new(|builder: &mut eframe::EventLoopBuilder<_>| {
            #[cfg(target_os = "linux")]
            {
                use winit::platform::wayland::EventLoopBuilderExtWayland;
                use winit::platform::x11::EventLoopBuilderExtX11;
                <eframe::EventLoopBuilder<_> as EventLoopBuilderExtWayland>::with_any_thread(
                    builder, true,
                );
                <eframe::EventLoopBuilder<_> as EventLoopBuilderExtX11>::with_any_thread(
                    builder, true,
                );
            }
        });
    // The Wayland compositor advertises a non-standard present mode
    // (the `Unrecognized present mode 1000361000` warning) that wgpu
    // doesn't know how to drive. Default `AutoVsync` picks the first
    // available mode and silently falls through to a no-op present
    // pipeline — the window keeps showing whatever the surface had
    // the moment before the first real paint. Force `Fifo` so wgpu
    // negotiates a mode it actually supports.
    //
    // We also override `on_surface_error`. egui_wgpu's default
    // SILENTLY skips the frame when `surface.get_current_texture()`
    // returns `Outdated` — which is what NVIDIA/Wayland surfaces
    // emit every time the compositor reconfigures the swapchain. The
    // skipped frames *still* run `prepare()` (so our queue uploads
    // pile up) but never reach `render()` (so paint never runs and
    // nothing presents). Default behavior also doesn't log Outdated,
    // making this look like a pure "paint never fires" bug.
    //
    // Returning `RecreateSurface` rebuilds the swapchain in-place and
    // the next frame submits cleanly.
    let mut wgpu_options = eframe::egui_wgpu::WgpuConfiguration::default();
    wgpu_options.present_mode = eframe::wgpu::PresentMode::Fifo;
    wgpu_options.on_surface_error = std::sync::Arc::new(|err| {
        warn!(error = ?err, "mirror surface error; recreating swapchain");
        eframe::egui_wgpu::SurfaceErrorAction::RecreateSurface
    });
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([960.0, 540.0])
            .with_title(title.clone()),
        renderer: eframe::Renderer::Wgpu,
        wgpu_options,
        event_loop_builder: Some(event_loop_builder),
        ..Default::default()
    };
    eframe::run_native(
        &title,
        native_options,
        Box::new(move |cc| {
            // Install the YUV→RGB pipeline + sampler into the
            // wgpu renderer's callback-resources bag so paint
            // callbacks can pull it back out without re-creating.
            let render_state = cc
                .wgpu_render_state
                .as_ref()
                .ok_or_else(|| "wgpu render state missing".to_string())?;
            info!(
                target_format = ?render_state.target_format,
                "mirror: wgpu render state ready; building pipelines"
            );
            let resources = MirrorResources::new(
                &render_state.device,
                render_state.target_format,
            );
            render_state
                .renderer
                .write()
                .callback_resources
                .insert(resources);
            // Producer wakes egui via this handle; install it before
            // returning so the very first frame doesn't get stranded
            // waiting for an event-driven repaint.
            slot.attach_ctx(cc.egui_ctx.clone());
            Ok(Box::new(MirrorApp::new(slot, input_tx)))
        }),
    )
    .map_err(|e| format!("eframe: {e}").into())
}

pub struct MirrorApp {
    slot: FrameSlot,
    last_size: Option<(u32, u32)>,
    last_format: Option<PixelFormat>,
    input_tx: Option<UnboundedSender<InputMessage>>,
    last_pointer: Option<egui::Pos2>,
}

impl MirrorApp {
    pub fn new(slot: FrameSlot, input_tx: Option<UnboundedSender<InputMessage>>) -> Self {
        Self {
            slot,
            last_size: None,
            last_format: None,
            input_tx,
            last_pointer: None,
        }
    }
}

impl eframe::App for MirrorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let taken = self.slot.take();
        if let Some(f) = &taken {
            let size = (f.width, f.height);
            if self.last_size != Some(size) {
                info!(width = size.0, height = size.1, format = ?f.format, "first frame uploaded");
                self.last_size = Some(size);
            }
            if self.last_format != Some(f.format) {
                self.last_format = Some(f.format);
            }
        }
        let cur_dims = self.last_size;

        let mut hit_rect: Option<egui::Rect> = None;
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some((fw, fh)) = cur_dims {
                // egui_wgpu's renderer skips paint callbacks when the
                // primitive's CLIP RECT (the painter's clip, not the
                // callback rect) has zero pixel area. Our previous
                // builds added the callback via `ui.painter()` —
                // whose clip is the *panel* rect — but the resulting
                // primitive ended up sharing a clip with a previous
                // zero-sized text shape, which killed it. Using
                // `painter_at(rect)` pins the clip exactly to the
                // callback rect so the renderer always honors it.
                let panel = ui.max_rect();
                let aspect = fw as f32 / fh.max(1) as f32;
                let (w, h) = if panel.width() / panel.height() > aspect {
                    (panel.height() * aspect, panel.height())
                } else {
                    (panel.width(), panel.width() / aspect)
                };
                let rect = egui::Rect::from_center_size(panel.center(), egui::vec2(w, h));
                if fire_once(&FIRST_LAYOUT_LOGGED) {
                    info!(
                        panel = ?panel,
                        rect = ?rect,
                        "mirror: first layout (rect goes to paint callback)"
                    );
                }
                // Diagnostic: paint a magenta rect underneath the
                // callback. If the user sees magenta but no video,
                // egui's mesh path renders fine into the rect and the
                // problem is specific to the wgpu paint-callback
                // bridge. If the rect is also missing, the rect is
                // being clipped out entirely upstream. Remove once
                // we're confident the callback path is working.
                let painter = ui.painter_at(rect);
                painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(255, 0, 255));
                let cb = egui_wgpu::Callback::new_paint_callback(
                    rect,
                    MirrorCallback { frame: taken },
                );
                painter.add(cb);
                let _ = ui.allocate_rect(rect, egui::Sense::click_and_drag());
                hit_rect = Some(rect);
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label("waiting for first decoded frame…");
                });
            }
        });
        if let (Some(rect), Some((fw, fh)), Some(tx)) =
            (hit_rect, cur_dims, self.input_tx.as_ref())
        {
            emit_pointer_events(ctx, &mut self.last_pointer, rect, fw, fh, tx);
        }
        // No `request_repaint_after`: the producer wakes us via
        // `MirrorSlot::store` → `ctx.request_repaint()` whenever a
        // new frame is ready. Idle GUI = idle CPU.
    }
}

/// Per-frame wgpu paint callback. Carries the newly-decoded
/// [`DecodedFrame`] (if any) into the GPU upload path; if `frame` is
/// `None` the previous-frame textures are re-rendered without
/// re-uploading.
struct MirrorCallback {
    frame: Option<DecodedFrame>,
}

impl egui_wgpu::CallbackTrait for MirrorCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &egui_wgpu::ScreenDescriptor,
        _egui_encoder: &mut wgpu::CommandEncoder,
        callback_resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        if fire_once(&FIRST_PREPARE_LOGGED) {
            info!(
                has_frame = self.frame.is_some(),
                has_resources = callback_resources.get::<MirrorResources>().is_some(),
                "mirror: first prepare callback"
            );
        }
        match (
            self.frame.as_ref(),
            callback_resources.get_mut::<MirrorResources>(),
        ) {
            (Some(frame), Some(res)) => res.upload(device, queue, frame),
            (Some(_), None) => {
                // Means the eframe creator never installed
                // `MirrorResources` — likely because
                // `cc.wgpu_render_state` was None (eframe fell back
                // to glow or the surface init failed). Log once so the
                // diagnosis is obvious.
                if fire_once(&FIRST_UPLOAD_LOGGED) {
                    error!("mirror prepare: MirrorResources missing from callback_resources; eframe likely on glow backend");
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn paint(
        &self,
        info: egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        callback_resources: &egui_wgpu::CallbackResources,
    ) {
        if fire_once(&FIRST_PAINT_LOGGED) {
            // tracing has been mysteriously skipping this site even
            // when prepare's analogous info!() fires. eprintln bypass
            // the subscriber so we get ground truth on whether paint
            // ever runs.
            eprintln!(
                "MIRROR-PAINT: first paint callback (viewport={:?} clip={:?} has_res={})",
                info.viewport,
                info.clip_rect,
                callback_resources.get::<MirrorResources>().is_some()
            );
            info!(
                has_resources = callback_resources.get::<MirrorResources>().is_some(),
                viewport = ?info.viewport,
                clip = ?info.clip_rect,
                "mirror: first paint callback"
            );
        }
        if let Some(res) = callback_resources.get::<MirrorResources>() {
            res.render(render_pass);
        }
    }
}

/// Which texture / shader combo the last upload installed. Used to
/// pick the right render pipeline at paint time and to know whether
/// a format change forces a full pipeline rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SurfaceKind {
    /// Two planes — Y as R8Unorm, UV as Rg8Unorm. Fragment shader
    /// runs BT.601 limited-range YUV→RGB conversion on sample.
    Nv12,
    /// Single packed plane, R-first byte order. Used by `openh264`
    /// SW fallback and any other RGBA-source decoder.
    Rgba,
    /// Single packed plane, B-first byte order. Same as Rgba but
    /// channels swapped in the fragment shader.
    Bgra,
}

/// Pipelines + sampler + (lazily-allocated) textures used by the
/// mirror window. Stored in egui_wgpu's `callback_resources` bag so
/// every paint can re-use the same GPU objects. Both NV12 and packed
/// pipelines are built up-front so a mid-stream format change (rare
/// but possible if the decoder backend swaps) doesn't require a
/// device-level rebuild.
struct MirrorResources {
    nv12_pipeline: wgpu::RenderPipeline,
    nv12_bgl: wgpu::BindGroupLayout,
    packed_pipeline: wgpu::RenderPipeline,
    packed_bgl: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    textures: Option<MirrorTextures>,
}

struct MirrorTextures {
    kind: SurfaceKind,
    // NV12: two textures. Packed: one in `y_tex`; `uv_tex` is `None`.
    y_tex: wgpu::Texture,
    uv_tex: Option<wgpu::Texture>,
    bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
}

impl MirrorResources {
    fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let nv12_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ansync-mirror-yuv"),
            source: wgpu::ShaderSource::Wgsl(NV12_SHADER.into()),
        });
        let nv12_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ansync-mirror-bgl-nv12"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let nv12_pipeline = build_pipeline(
            device,
            target_format,
            &nv12_shader,
            &nv12_bgl,
            "ansync-mirror-nv12-pipeline",
        );

        let packed_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ansync-mirror-packed"),
            source: wgpu::ShaderSource::Wgsl(PACKED_SHADER.into()),
        });
        let packed_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ansync-mirror-bgl-packed"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let packed_pipeline = build_pipeline(
            device,
            target_format,
            &packed_shader,
            &packed_bgl,
            "ansync-mirror-packed-pipeline",
        );

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("ansync-mirror-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        Self {
            nv12_pipeline,
            nv12_bgl,
            packed_pipeline,
            packed_bgl,
            sampler,
            textures: None,
        }
    }

    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, frame: &DecodedFrame) {
        match frame.format {
            PixelFormat::Nv12 => self.upload_nv12(device, queue, frame),
            // Both packed-RGB formats go through the same Rgba8Unorm
            // texture; the PACKED_SHADER assumes BGRA byte order on
            // upload (matching ferricast's openh264 backend output)
            // and rebuilds true RGB at sample time. If a future
            // decoder emits real RGBA we'd need to split pipelines.
            PixelFormat::Bgra8 => self.upload_packed(device, queue, frame, SurfaceKind::Bgra),
            PixelFormat::Rgba8 => self.upload_packed(device, queue, frame, SurfaceKind::Rgba),
            PixelFormat::I420 => {
                // I420 → NV12 would need a CPU UV-interleave pass; no
                // ferricast backend emits I420 today.
                warn!("mirror sink: I420 frames unsupported; skipping");
            }
        }
    }

    fn upload_nv12(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, frame: &DecodedFrame) {
        let w = frame.width;
        let h = frame.height;
        let need_recreate = match &self.textures {
            Some(t) => t.kind != SurfaceKind::Nv12 || t.width != w || t.height != h,
            None => true,
        };
        if need_recreate {
            let y_tex = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("ansync-mirror-y"),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let uv_tex = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("ansync-mirror-uv"),
                size: wgpu::Extent3d {
                    width: w / 2,
                    height: h / 2,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rg8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let y_view = y_tex.create_view(&wgpu::TextureViewDescriptor::default());
            let uv_view = uv_tex.create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("ansync-mirror-bg-nv12"),
                layout: &self.nv12_bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&y_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&uv_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });
            self.textures = Some(MirrorTextures {
                kind: SurfaceKind::Nv12,
                y_tex,
                uv_tex: Some(uv_tex),
                bind_group,
                width: w,
                height: h,
            });
        }
        let stride = frame.stride.max(w);
        let y_len = (stride as usize) * (h as usize);
        let uv_len = (stride as usize) * (h as usize) / 2;
        if frame.data.len() < y_len + uv_len {
            warn!(
                expected = y_len + uv_len,
                got = frame.data.len(),
                "NV12 frame shorter than expected; skipping"
            );
            return;
        }
        let tex = self.textures.as_ref().expect("textures created above");
        let uv = tex.uv_tex.as_ref().expect("nv12 path always sets uv_tex");
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &tex.y_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &frame.data[..y_len],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: uv,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &frame.data[y_len..y_len + uv_len],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride),
                rows_per_image: Some(h / 2),
            },
            wgpu::Extent3d {
                width: w / 2,
                height: h / 2,
                depth_or_array_layers: 1,
            },
        );
    }

    fn upload_packed(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        frame: &DecodedFrame,
        kind: SurfaceKind,
    ) {
        let w = frame.width;
        let h = frame.height;
        let need_recreate = match &self.textures {
            Some(t) => t.kind != kind || t.width != w || t.height != h,
            None => true,
        };
        if need_recreate {
            let rgba_tex = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("ansync-mirror-packed"),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = rgba_tex.create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("ansync-mirror-bg-packed"),
                layout: &self.packed_bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });
            self.textures = Some(MirrorTextures {
                kind,
                y_tex: rgba_tex,
                uv_tex: None,
                bind_group,
                width: w,
                height: h,
            });
        }
        // Packed formats: one plane, 4 bytes/pixel. ferricast's
        // openh264 path emits BGRA with `stride == width * 4`. We
        // upload directly. The B-vs-R order is resolved in the
        // fragment shader (PACKED_SHADER swaps for Bgra).
        let stride = frame.stride.max(w * 4);
        let needed = (stride as usize) * (h as usize);
        if frame.data.len() < needed {
            warn!(
                expected = needed,
                got = frame.data.len(),
                ?kind,
                "packed frame shorter than expected; skipping"
            );
            return;
        }
        let tex = self.textures.as_ref().expect("textures created above");
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &tex.y_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &frame.data[..needed],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
    }

    fn render(&self, rpass: &mut wgpu::RenderPass<'static>) {
        if let Some(t) = &self.textures {
            let pipeline = match t.kind {
                SurfaceKind::Nv12 => &self.nv12_pipeline,
                SurfaceKind::Bgra | SurfaceKind::Rgba => &self.packed_pipeline,
            };
            rpass.set_pipeline(pipeline);
            rpass.set_bind_group(0, &t.bind_group, &[]);
            rpass.draw(0..6, 0..1);
        }
    }
}

fn build_pipeline(
    device: &wgpu::Device,
    target_format: wgpu::TextureFormat,
    shader: &wgpu::ShaderModule,
    bgl: &wgpu::BindGroupLayout,
    label: &str,
) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[bgl],
        push_constant_ranges: &[],
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format: target_format,
                blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    })
}

const NV12_SHADER: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> VsOut {
    var pos = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 1.0, -1.0),
        vec2<f32>(-1.0,  1.0),
        vec2<f32>(-1.0,  1.0),
        vec2<f32>( 1.0, -1.0),
        vec2<f32>( 1.0,  1.0),
    );
    var uv = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 0.0),
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(1.0, 0.0),
    );
    var out: VsOut;
    out.pos = vec4<f32>(pos[idx], 0.0, 1.0);
    out.uv = uv[idx];
    return out;
}

@group(0) @binding(0) var y_tex: texture_2d<f32>;
@group(0) @binding(1) var uv_tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let y_raw = textureSample(y_tex, samp, in.uv).r;
    let uv_raw = textureSample(uv_tex, samp, in.uv).rg;
    // BT.709 limited-range YUV → RGB. Android MediaCodec H.264
    // encodes 1080p screen capture in BT.709 (HD primaries) with the
    // standard limited range (Y in [16/255, 235/255], C in
    // [16/255, 240/255]). Y scaled by 255/219 to map to [0,1]; Cb/Cr
    // recentred at 128/255 then scaled by 255/224 to map to
    // [-0.5, 0.5]. Skipping the chroma scale (`uv_raw - 0.5` only)
    // leaves chroma under-saturated and gives the "washed out"
    // colour look users see on Wayland targets.
    let y = (y_raw - 16.0 / 255.0) * (255.0 / 219.0);
    let cb = (uv_raw.r - 128.0 / 255.0) * (255.0 / 224.0);
    let cr = (uv_raw.g - 128.0 / 255.0) * (255.0 / 224.0);
    let r = y + 1.5748 * cr;
    let g = y - 0.1873 * cb - 0.4681 * cr;
    let b = y + 1.8556 * cb;
    return vec4<f32>(clamp(r, 0.0, 1.0), clamp(g, 0.0, 1.0), clamp(b, 0.0, 1.0), 1.0);
}
"#;

// Packed shader: the texture is uploaded as Rgba8Unorm. For BGRA
// source data we swap the .r and .b lanes here at sample time, so
// the same pipeline serves both formats without a CPU swap pass.
// Channel choice is baked into the WGSL — we only ever build ONE
// packed pipeline; the BGRA path is unreachable today (NVDEC emits
// NV12, openh264 emits BGRA which we re-tag as RGBA on upload — see
// `upload_packed`'s shader-side `.bgr` swizzle).
const PACKED_SHADER: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> VsOut {
    var pos = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 1.0, -1.0),
        vec2<f32>(-1.0,  1.0),
        vec2<f32>(-1.0,  1.0),
        vec2<f32>( 1.0, -1.0),
        vec2<f32>( 1.0,  1.0),
    );
    var uv = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 0.0),
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(1.0, 0.0),
    );
    var out: VsOut;
    out.pos = vec4<f32>(pos[idx], 0.0, 1.0);
    out.uv = uv[idx];
    return out;
}

@group(0) @binding(0) var packed_tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // ferricast's openh264 backend writes BGRA into the buffer but
    // the wgpu format we upload as is Rgba8Unorm, so what hits the
    // sampler is interpreted with B in the .r lane. Swap back here.
    let c = textureSample(packed_tex, samp, in.uv);
    return vec4<f32>(c.b, c.g, c.r, c.a);
}
"#;

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
    // `interact_pos` carries the cursor location through drags too,
    // not just hover. `hover_pos` returns `None` mid-click on some
    // winit/Wayland combos which silently dropped every drag event.
    ctx.input(|i| {
        let pos = i.pointer.interact_pos().or_else(|| i.pointer.latest_pos());
        let Some(pos) = pos else {
            if last.take().is_some() {
                let _ = send_input(tx, InputMessage::TouchSlot {
                    slot: 0,
                    x: 0,
                    y: 0,
                    pressure: 0,
                    tracking_id: -1,
                });
            }
            return;
        };
        let inside = rect.contains(pos);
        // `primary_pressed` / `primary_released` are true ONLY in the
        // exact update where the edge happened. They override the
        // inside/outside gate so a click-up that lands just outside
        // still emits the release (peer would otherwise stay stuck
        // in a phantom-pressed state).
        let pressed_edge = i.pointer.primary_pressed();
        let released_edge = i.pointer.primary_released();
        let down = i.pointer.primary_down();
        let nx = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        let ny = ((pos.y - rect.top()) / rect.height()).clamp(0.0, 1.0);
        let abs_x = (nx * fw as f32) as i32;
        let abs_y = (ny * fh as f32) as i32;
        if pressed_edge && inside {
            let _ = send_input(tx, InputMessage::TouchSlot {
                slot: 0,
                x: abs_x,
                y: abs_y,
                pressure: 255,
                tracking_id: 1,
            });
            *last = Some(pos);
        } else if released_edge {
            let _ = send_input(tx, InputMessage::TouchSlot {
                slot: 0,
                x: abs_x,
                y: abs_y,
                pressure: 0,
                tracking_id: -1,
            });
            *last = None;
        } else if down && inside {
            let changed = last.map(|p| p != pos).unwrap_or(true);
            if changed {
                let _ = send_input(tx, InputMessage::TouchSlot {
                    slot: 0,
                    x: abs_x,
                    y: abs_y,
                    pressure: 255,
                    tracking_id: 1,
                });
                *last = Some(pos);
            }
        }
    });
}

fn send_input(
    tx: &UnboundedSender<InputMessage>,
    msg: InputMessage,
) -> Result<(), tokio::sync::mpsc::error::SendError<InputMessage>> {
    if fire_once(&FIRST_INPUT_LOGGED) {
        info!(?msg, "mirror: first input event dispatched");
    }
    tx.send(msg)
}
