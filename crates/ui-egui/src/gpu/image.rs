//! A bounded image of the drawing, below all live cursor/selection/command overlays.
//! Navigation refreshes geometry at 30 Hz and at reduced resolution. Between those refreshes
//! the previous image is reprojected to the current view, without delaying input or the pen.

use std::hash::{Hash, Hasher};

use super::*;

pub struct CachedCanvasCallback {
    pub canvas: CanvasCallback,
    pub parts: Arc<[cadcraft_render::screen::DamagePart]>,
    pub style: u64,
    pub clip: egui::Rect,
    pub navigation: bool,
    pub reduced_resolution: bool,
    pub navigation_fps: u32,
    pub time: f64,
    pub context: egui::Context,
}

pub(super) struct SceneImage {
    renderer: Box<Resources>,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform: wgpu::Buffer,
    pipeline: wgpu::RenderPipeline,
    clear: wgpu::RenderPipeline,
    scratch: Option<wgpu::Texture>,
    parts: Arc<[cadcraft_render::screen::DamagePart]>,
    image: Option<Texture>,
    source: Option<View>,
    key: u64,
    used: u64,
    rendered: f64,
    #[cfg(test)]
    dirty_tiles: usize,
}

struct Texture {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    bind: wgpu::BindGroup,
    size: [u32; 2],
}

#[derive(Clone, Copy)]
struct View {
    mesh: u64,
    offset: [f32; 2],
    scale: f32,
    center: [f32; 2],
    screen: [u32; 2],
    ppp: f32,
    clip: egui::Rect,
    navigation: bool,
    style: u64,
}

impl CachedCanvasCallback {
    fn key(&self, screen: &egui_wgpu::ScreenDescriptor) -> u64 {
        let mut key = std::collections::hash_map::DefaultHasher::new();
        self.canvas.key.hash(&mut key);
        self.navigation.hash(&mut key);
        self.reduced_resolution.hash(&mut key);
        self.navigation_fps.hash(&mut key);
        screen.size_in_pixels.hash(&mut key);
        for value in self.canvas.offset.into_iter().chain([self.canvas.scale]).chain(self.canvas.center).chain(self.canvas.xform).chain([
            self.canvas.dash,
            screen.pixels_per_point,
            self.clip.min.x,
            self.clip.min.y,
            self.clip.max.x,
            self.clip.max.y,
        ]) {
            value.to_bits().hash(&mut key);
        }
        key.finish()
    }
}

impl SceneImage {
    fn new(device: &wgpu::Device, target: GpuTarget) -> Self {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("cad_scene_image"),
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
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                    count: None,
                },
            ],
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("cad_scene_image"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("cad_scene_image_view"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let module = device
            .create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("cad_scene_image"), source: wgpu::ShaderSource::Wgsl(SHADER.into()) });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("cad_scene_image"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("cad_scene_image"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState { module: &module, entry_point: Some("vs_image"), buffers: &[], compilation_options: Default::default() },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_image"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target.format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: Default::default(),
            multisample: Default::default(),
            depth_stencil: target.stencil.map(|format| wgpu::DepthStencilState {
                format,
                depth_write_enabled: None,
                depth_compare: None,
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multiview_mask: None,
            cache: None,
        });
        let clear_module = device
            .create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("cad_tile_clear"), source: wgpu::ShaderSource::Wgsl(CLEAR.into()) });
        let clear_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("cad_tile_clear"),
            bind_group_layouts: &[],
            immediate_size: 0,
        });
        let clear = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("cad_tile_clear"),
            layout: Some(&clear_layout),
            vertex: wgpu::VertexState { module: &clear_module, entry_point: Some("vs_clear"), buffers: &[], compilation_options: Default::default() },
            fragment: Some(wgpu::FragmentState {
                module: &clear_module,
                entry_point: Some("fs_clear"),
                targets: &[Some(wgpu::ColorTargetState { format: target.format, blend: None, write_mask: wgpu::ColorWrites::ALL })],
                compilation_options: Default::default(),
            }),
            primitive: Default::default(),
            multisample: Default::default(),
            depth_stencil: None,
            multiview_mask: None,
            cache: None,
        });
        Self {
            renderer: Box::new(Resources::new(device, GpuTarget { stencil: None, ..target })),
            layout,
            sampler,
            uniform,
            pipeline,
            clear,
            scratch: None,
            parts: Arc::default(),
            image: None,
            source: None,
            key: 0,
            used: 0,
            rendered: f64::NEG_INFINITY,
            #[cfg(test)]
            dirty_tiles: 0,
        }
    }

    pub(super) fn bytes(&self) -> usize {
        self.image
            .as_ref()
            .map_or(0, |image| image.size[0] as usize * image.size[1] as usize * 4)
            .saturating_add(self.scratch.as_ref().map_or(0, |s| s.width() as usize * s.height() as usize * 4))
    }

    fn prepare(
        &mut self,
        callback: &CachedCanvasCallback,
        resources: &Resources,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        screen: &egui_wgpu::ScreenDescriptor,
        encoder: &mut wgpu::CommandEncoder,
    ) -> bool {
        #[cfg(test)]
        {
            self.dirty_tiles = 0;
        }
        let view = &callback.canvas;
        if view.layer != LAYER_DRAWING || view.stencil != Stencil::Off || view.xform != IDENTITY_XFORM || view.scale <= 0.0 || !view.scale.is_finite()
        {
            return false;
        }
        let Some(layer) = resources.layers.get(view.layer).filter(|l| l.key == Some(view.key)) else { return false };
        let [sw, sh] = screen.size_in_pixels;
        let full = u64::from(sw).saturating_mul(u64::from(sh)).saturating_mul(4);
        if full == 0 {
            return false;
        }
        let reduced = callback.navigation && callback.reduced_resolution;
        let ratio = if reduced { 0.65f64.min((resources.cache_limit as f64 / full as f64).sqrt() * 0.99) } else { 1.0 };
        if ratio < 0.35 || !screen.pixels_per_point.is_finite() || screen.pixels_per_point <= 0.0 {
            return false;
        }
        let size = [(f64::from(sw) * ratio).round().max(1.0) as u32, (f64::from(sh) * ratio).round().max(1.0) as u32];
        let limit = device.limits().max_texture_dimension_2d;
        if size.iter().any(|n| *n > limit) || u64::from(size[0]) * u64::from(size[1]) * 4 > resources.cache_limit as u64 {
            return false;
        }
        let key = callback.key(screen);
        let source = View {
            mesh: view.key,
            offset: view.offset,
            scale: view.scale,
            center: view.center,
            screen: screen.size_in_pixels,
            ppp: screen.pixels_per_point,
            clip: callback.clip,
            navigation: callback.navigation,
            style: callback.style,
        };
        let moved = self.key != key;
        let reusable = self.image.as_ref().is_some_and(|image| image.size == size);
        let interval = 1.0 / f64::from(callback.navigation_fps.clamp(15, 60));
        let throttle = moved
            && reusable
            && callback.navigation
            && self.source.is_some_and(|old| {
                old.mesh == source.mesh
                    && old.style == source.style
                    && old.screen == source.screen
                    && old.ppp == source.ppp
                    && old.clip == source.clip
                    && old.navigation
            })
            && callback.time >= self.rendered
            && callback.time - self.rendered < interval;
        if throttle {
            callback.context.request_repaint_after(std::time::Duration::from_secs_f64((interval - (callback.time - self.rendered)).max(0.001)));
        } else if moved || !reusable {
            if !reusable {
                self.scratch = None;
                let texture = device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("cad_scene_image"),
                    size: wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: resources.target.format,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                        | wgpu::TextureUsages::TEXTURE_BINDING
                        | wgpu::TextureUsages::COPY_SRC
                        | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                let texture_view = texture.create_view(&Default::default());
                let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("cad_scene_image"),
                    layout: &self.layout,
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&texture_view) },
                        wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                        wgpu::BindGroupEntry { binding: 2, resource: self.uniform.as_entire_binding() },
                    ],
                });
                self.image = Some(Texture { _texture: texture, view: texture_view, bind, size });
            }
            let sx = size[0] as f32 / sw as f32 * screen.pixels_per_point;
            let sy = size[1] as f32 / sh as f32 * screen.pixels_per_point;
            let pixel_clip = egui::Rect::from_min_max(
                egui::pos2((callback.clip.min.x * sx).floor().max(0.0), (callback.clip.min.y * sy).floor().max(0.0)),
                egui::pos2((callback.clip.max.x * sx).ceil().min(size[0] as f32), (callback.clip.max.y * sy).ceil().min(size[1] as f32)),
            );
            let plan = reusable.then_some(self.source).flatten().and_then(|old| {
                if old.screen != source.screen
                    || old.ppp != source.ppp
                    || old.clip != source.clip
                    || old.style != source.style
                    || old.scale != source.scale
                    || old.navigation != source.navigation
                {
                    return None;
                }
                let delta = [
                    (source.center[0] - old.center[0] + (source.offset[0] - old.offset[0]) * source.scale) * sx,
                    (source.center[1] - old.center[1] - (source.offset[1] - old.offset[1]) * source.scale) * sy,
                ];
                if delta.iter().any(|v| !v.is_finite() || (v - v.round()).abs() > 0.001 || v.abs() > i32::MAX as f32) {
                    return None;
                }
                let shift = delta.map(|v| v.round() as i32);
                let damage = if old.mesh == source.mesh { Vec::new() } else { tiles::damage(&self.parts, &callback.parts)? };
                Some((shift, damage))
            });
            let mut partial = plan.is_some();
            let mut shift = [0, 0];
            let mut damage = Vec::new();
            if let Some((delta, changed)) = plan {
                shift = delta;
                damage = changed
                    .into_iter()
                    .map(|bounds| {
                        let points = bounds.corners().map(|p| {
                            egui::pos2(
                                (source.center[0] + (p.x as f32 + source.offset[0]) * source.scale) * sx,
                                (source.center[1] - (p.y as f32 + source.offset[1]) * source.scale) * sy,
                            )
                        });
                        egui::Rect::from_points(&points).expand(32.0 * sx.max(sy))
                    })
                    .collect();
                if damage.iter().any(|r| !r.is_finite()) {
                    partial = false;
                }
            }
            if partial && shift != [0, 0] {
                let scratch_bytes = u64::from(tiles::EDGE.min(size[0])) * u64::from(tiles::EDGE.min(size[1])) * 4;
                if u64::from(size[0]) * u64::from(size[1]) * 4 + scratch_bytes > resources.cache_limit as u64 {
                    partial = false;
                    self.scratch = None;
                } else {
                    let scratch = self.scratch.get_or_insert_with(|| {
                        device.create_texture(&wgpu::TextureDescriptor {
                            label: Some("cad_scroll_tile"),
                            size: wgpu::Extent3d { width: tiles::EDGE.min(size[0]), height: tiles::EDGE.min(size[1]), depth_or_array_layers: 1 },
                            mip_level_count: 1,
                            sample_count: 1,
                            dimension: wgpu::TextureDimension::D2,
                            format: resources.target.format,
                            usage: wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST,
                            view_formats: &[],
                        })
                    });
                    if let Some(image) = &self.image {
                        for region in tiles::copies(size, shift) {
                            let extent = wgpu::Extent3d { width: region.size[0], height: region.size[1], depth_or_array_layers: 1 };
                            encoder.copy_texture_to_texture(copy_info(&image._texture, region.from), copy_info(scratch, [0, 0]), extent);
                            encoder.copy_texture_to_texture(copy_info(scratch, [0, 0]), copy_info(&image._texture, region.to), extent);
                        }
                    }
                }
            }
            let valid =
                if partial { pixel_clip.intersect(pixel_clip.translate(egui::vec2(shift[0] as f32, shift[1] as f32))) } else { egui::Rect::NOTHING };
            let scissors = tiles::dirty(size, pixel_clip, &damage, valid);
            #[cfg(test)]
            {
                self.dirty_tiles = scissors.len();
            }
            let Some(image) = &self.image else { return false };
            if !scissors.is_empty() {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("cad_dirty_tiles"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &image.view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: if partial { wgpu::LoadOp::Load } else { wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT) },
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
                if partial {
                    for [x, y, width, height] in scissors {
                        pass.set_scissor_rect(x, y, width, height);
                        pass.set_viewport(0.0, 0.0, size[0] as f32, size[1] as f32, 0.0, 1.0);
                        pass.set_pipeline(&self.clear);
                        pass.draw(0..3, 0..1);
                        let clip = egui::Rect::from_min_max(
                            egui::pos2(x as f32 / sx, y as f32 / sy),
                            egui::pos2((x + width) as f32 / sx, (y + height) as f32 / sy),
                        );
                        self.renderer.paint_layer(view, layer, &mut pass, size, clip);
                    }
                } else if pixel_clip.is_positive() {
                    pass.set_scissor_rect(pixel_clip.min.x as u32, pixel_clip.min.y as u32, pixel_clip.width() as u32, pixel_clip.height() as u32);
                    self.renderer.paint_layer(view, layer, &mut pass, size, callback.clip);
                }
            }
            self.parts = callback.parts.clone();
            self.source = Some(source);
            self.key = key;
            self.rendered = callback.time;
        }
        let Some(old) = self.source else { return false };
        let k = source.scale / old.scale;
        let shift = [
            source.center[0] - old.center[0] * k + (source.offset[0] - old.offset[0]) * source.scale,
            source.center[1] - old.center[1] * k - (source.offset[1] - old.offset[1]) * source.scale,
        ];
        let transform = [k, k, shift[0] * source.ppp / sw as f32, shift[1] * source.ppp / sh as f32];
        let mut bytes = [0u8; 16];
        for (chunk, value) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(transform) {
            chunk.copy_from_slice(&value.to_le_bytes());
        }
        queue.write_buffer(&self.uniform, 0, &bytes);
        self.used = key;
        true
    }
}

impl egui_wgpu::CallbackTrait for CachedCanvasCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        screen: &egui_wgpu::ScreenDescriptor,
        encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let commands = self.canvas.prepare(device, queue, screen, encoder, resources);
        let Some(shared) = resources.get_mut::<Resources>() else { return commands };
        // A full-resolution cache is optional. Under pressure, rendering directly always
        // remains sharp and exact; a cache never forces a texture above the session allowance.
        if shared.cache_limit == 0 {
            shared.image = None;
            return commands;
        }
        let pixels = u64::from(screen.size_in_pixels[0]) * u64::from(screen.size_in_pixels[1]);
        let reduced = self.navigation && self.reduced_resolution;
        if (!reduced && pixels.saturating_mul(4) > shared.cache_limit as u64)
            || (reduced && pixels as f64 * 4.0 * 0.35 * 0.35 > shared.cache_limit as f64)
            || self.canvas.layer != LAYER_DRAWING
            || self.canvas.stencil != Stencil::Off
        {
            shared.image = None;
            return commands;
        }
        let mut image = shared.image.take().unwrap_or_else(|| SceneImage::new(device, shared.target));
        if image.prepare(self, shared, device, queue, screen, encoder) {
            shared.image = Some(image);
        }
        shared.trim_retained();
        commands
    }

    fn paint(&self, info: egui::PaintCallbackInfo, pass: &mut wgpu::RenderPass<'static>, resources: &egui_wgpu::CallbackResources) {
        let Some(shared) = resources.get::<Resources>() else { return };
        let screen = egui_wgpu::ScreenDescriptor { size_in_pixels: info.screen_size_px, pixels_per_point: info.pixels_per_point };
        if let Some(image) = shared.image.as_ref().filter(|image| image.used == self.key(&screen))
            && let Some(texture) = &image.image
        {
            let [w, h] = info.screen_size_px;
            if w > 0 && h > 0 {
                pass.set_viewport(0.0, 0.0, w as f32, h as f32, 0.0, 1.0);
                pass.set_pipeline(&image.pipeline);
                pass.set_bind_group(0, &texture.bind, &[]);
                pass.draw(0..6, 0..1);
            }
            return;
        }
        self.canvas.paint(info, pass, resources);
    }
}

pub(super) const SHADER: &str = r#"
@group(0) @binding(0) var drawing: texture_2d<f32>;
@group(0) @binding(1) var filtering: sampler;
@group(0) @binding(2) var<uniform> transform: vec4<f32>;
struct VOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex fn vs_image(@builtin(vertex_index) index: u32) -> VOut {
    let x = select(0.0, 1.0, index == 1u || index == 4u || index == 5u);
    let y = select(0.0, 1.0, index == 2u || index == 3u || index == 5u);
    let uv = vec2<f32>(x, y);
    let p = uv * transform.xy + transform.zw;
    var out: VOut;
    out.pos = vec4<f32>(p.x * 2.0 - 1.0, 1.0 - p.y * 2.0, 0.0, 1.0);
    out.uv = uv;
    return out;
}
@fragment fn fs_image(v: VOut) -> @location(0) vec4<f32> { return textureSample(drawing, filtering, v.uv); }
"#;

pub(super) const CLEAR: &str = r#"
@vertex fn vs_clear(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    return vec4<f32>(select(-1.0, 3.0, i == 1u), select(-1.0, 3.0, i == 2u), 0.0, 1.0);
}
@fragment fn fs_clear() -> @location(0) vec4<f32> { return vec4<f32>(0.0); }
"#;

fn copy_info(texture: &wgpu::Texture, xy: [u32; 2]) -> wgpu::TexelCopyTextureInfo<'_> {
    wgpu::TexelCopyTextureInfo { texture, mip_level: 0, origin: wgpu::Origin3d { x: xy[0], y: xy[1], z: 0 }, aspect: wgpu::TextureAspect::All }
}

#[cfg(test)]
mod tests;
