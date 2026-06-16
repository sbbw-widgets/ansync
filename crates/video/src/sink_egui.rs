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

use std::sync::{Arc, Mutex};

use ansync_proto::InputMessage;
use eframe::egui_wgpu;
use eframe::wgpu;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

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
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([960.0, 540.0])
            .with_title(title.clone()),
        renderer: eframe::Renderer::Wgpu,
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
            let resources = MirrorResources::new(
                &render_state.device,
                render_state.target_format,
            );
            render_state
                .renderer
                .write()
                .callback_resources
                .insert(resources);
            Ok(Box::new(MirrorApp::new(slot, input_tx)))
        }),
    )
    .map_err(|e| format!("eframe: {e}").into())
}

pub struct MirrorApp {
    slot: FrameSlot,
    last_size: Option<(u32, u32)>,
    input_tx: Option<UnboundedSender<InputMessage>>,
    last_pointer: Option<egui::Pos2>,
}

impl MirrorApp {
    pub fn new(slot: FrameSlot, input_tx: Option<UnboundedSender<InputMessage>>) -> Self {
        Self {
            slot,
            last_size: None,
            input_tx,
            last_pointer: None,
        }
    }
}

impl eframe::App for MirrorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let taken = self.slot.lock().ok().and_then(|mut s| s.take());
        if let Some(f) = &taken {
            let size = (f.width, f.height);
            if self.last_size != Some(size) {
                info!(width = size.0, height = size.1, "first frame uploaded");
                self.last_size = Some(size);
            }
        }
        let cur_dims = self.last_size;

        let mut hit_rect: Option<egui::Rect> = None;
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some((fw, fh)) = cur_dims {
                let available = ui.available_size();
                let aspect = fw as f32 / fh.max(1) as f32;
                let mut size = available;
                if size.x / size.y > aspect {
                    size.x = size.y * aspect;
                } else {
                    size.y = size.x / aspect;
                }
                ui.centered_and_justified(|ui| {
                    let (rect, _resp) =
                        ui.allocate_exact_size(size, egui::Sense::click_and_drag());
                    let cb = egui_wgpu::Callback::new_paint_callback(
                        rect,
                        MirrorCallback { frame: taken },
                    );
                    ui.painter().add(cb);
                    hit_rect = Some(rect);
                });
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
        ctx.request_repaint_after(std::time::Duration::from_millis(16));
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
        if let (Some(frame), Some(res)) = (
            self.frame.as_ref(),
            callback_resources.get_mut::<MirrorResources>(),
        ) {
            res.upload(device, queue, frame);
        }
        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        callback_resources: &egui_wgpu::CallbackResources,
    ) {
        if let Some(res) = callback_resources.get::<MirrorResources>() {
            res.render(render_pass);
        }
    }
}

/// Pipeline + sampler + (lazily-allocated) textures used by the
/// mirror window. Stored in egui_wgpu's `callback_resources` bag so
/// every paint can re-use the same GPU objects.
struct MirrorResources {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    textures: Option<MirrorTextures>,
}

struct MirrorTextures {
    y_tex: wgpu::Texture,
    uv_tex: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
}

impl MirrorResources {
    fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ansync-mirror-yuv"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("ansync-mirror-bgl"),
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
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ansync-mirror-pl"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("ansync-mirror-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
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
        });
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
            pipeline,
            bind_group_layout,
            sampler,
            textures: None,
        }
    }

    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, frame: &DecodedFrame) {
        match frame.format {
            PixelFormat::Nv12 => {}
            other => {
                warn!(format = ?other, "wgpu mirror sink only supports NV12; dropping frame");
                return;
            }
        }
        let w = frame.width;
        let h = frame.height;
        // Recreate textures if size changed (or first frame). The
        // previous wgpu::Texture handles drop here, returning their
        // GPU allocation to wgpu's pool.
        let need_recreate = self
            .textures
            .as_ref()
            .map_or(true, |t| t.width != w || t.height != h);
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
                label: Some("ansync-mirror-bg"),
                layout: &self.bind_group_layout,
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
                y_tex,
                uv_tex,
                bind_group,
                width: w,
                height: h,
            });
            // Suppress unused-binding warnings on the views — they're
            // only needed to build the bind_group; once bound the
            // textures themselves are accessed via the cached
            // handles.
            let _ = (y_view, uv_view);
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
        // wgpu's `Queue::write_texture` handles bytes_per_row padding
        // internally; passing the source stride directly is safe even
        // when it isn't 256-aligned.
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
                texture: &tex.uv_tex,
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

    fn render(&self, rpass: &mut wgpu::RenderPass<'static>) {
        if let Some(t) = &self.textures {
            rpass.set_pipeline(&self.pipeline);
            rpass.set_bind_group(0, &t.bind_group, &[]);
            rpass.draw(0..6, 0..1);
        }
    }
}

const SHADER: &str = r#"
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
    // BT.601 limited-range YUV → RGB.
    let y = (y_raw - 16.0 / 255.0) * (255.0 / 219.0);
    let cb = uv_raw.r - 0.5;
    let cr = uv_raw.g - 0.5;
    let r = y + 1.5748 * cr;
    let g = y - 0.1873 * cb - 0.4681 * cr;
    let b = y + 1.8556 * cb;
    return vec4<f32>(clamp(r, 0.0, 1.0), clamp(g, 0.0, 1.0), clamp(b, 0.0, 1.0), 1.0);
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
    let mut tracking_id: i32 = 0;
    ctx.input(|i| {
        let Some(pos) = i.pointer.hover_pos() else {
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
