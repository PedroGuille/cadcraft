//! GPU canvas: draws the drawing's display list with an `egui_wgpu` paint callback.
//!
//! The display list is converted once per (document, revision, tessellation band, background,
//! lineweight display, vertex origin) into vertex data ([`MeshData`]) and uploaded to wgpu
//! buffers; each frame only a small view uniform changes. Positions are stored as f32 relative to
//! a per-build origin so large world coordinates keep their precision; at deep zoom the origin
//! follows the view (see [`choose_origin`]).
//!
//! Batches: filled triangles (TriangleList), 1-px hairlines (LineList), and instanced screen-space
//! quads for wide lineweights and point markers. Fills are drawn first, then lines on top. A
//! wipeout starts a new draw-order group (see [`MeshData::breaks`]): its background-coloured mask
//! is drawn after everything before it and under everything after it. The shader only uses
//! vertex buffers and one uniform buffer, so it runs on WebGPU and WebGL2.
//!
//! Infinite lines (xline/ray) and all overlays stay egui shapes (see `canvas.rs`), and the
//! canvas falls back to the CPU path when no wgpu render state was handed to the app.

use std::sync::{Arc, Mutex};

use cadcraft_color::Rgb;
use cadcraft_geom::{Bounds2, Vec2};
use cadcraft_render::{DisplayList, Kind};
use egui_wgpu::wgpu;
use egui_wgpu::wgpu::util::DeviceExt;

pub mod blocks;
mod image;
mod tiles;
pub use image::CachedCanvasCallback;

/// Line/triangle vertex: position (2 × f32, relative to the origin) + sRGB colour (4 × u8,
/// premultiplied by its alpha: transparent objects blend with what is under them).
const VERTEX_SIZE: usize = 12;
/// Wide-line / point instance: a, b (2 × f32 each) + colour (4 × u8) + width in points (f32) +
/// length along its polyline before `a`, in world units (f32; dashing).
const INSTANCE_SIZE: usize = 28;
/// Vertices (or instances) per GPU buffer; divisible by 2 and 3 so no primitive straddles two.
const CHUNK: usize = 4_194_300;
/// Point markers are 1.5 × 1.5 points (as on the CPU path).
const POINT_SIZE: f32 = 1.5;

/// How the native window presents frames (`NativeOptions::wgpu_options.surface`).
///
/// The canvas hides the OS pointer and draws its own crosshair, so every frame the swapchain holds
/// back is visible as cursor lag. With FIFO (vsync) wgpu asks for one queued frame, but Vulkan
/// clamps that to the driver's minimum swapchain size, which is three images on X11 (two frames
/// queued behind the one on screen; Windows DX12 honours one). `low_latency` (the
/// app passes it on Linux/BSD) prefers a mode that never queues: `AutoNoVsync` picks Immediate,
/// then Mailbox, then falls back to FIFO, so it is valid on every surface; under a compositor the
/// compositor still syncs to the display. `vsync` is the `CADCRAFT_VSYNC` override: `1`/`on`
/// forces FIFO (tear-free without a compositor), `0`/`off` forces the low-latency mode.
pub fn surface_config(vsync: Option<&str>, low_latency: bool) -> egui_wgpu::SurfaceConfig {
    let no_vsync = match vsync.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        Some("1" | "on" | "true" | "yes") => false,
        Some("0" | "off" | "false" | "no") => true,
        _ => low_latency,
    };
    egui_wgpu::SurfaceConfig {
        present_mode: if no_vsync { wgpu::PresentMode::AutoNoVsync } else { wgpu::PresentMode::AutoVsync },
        desired_maximum_frame_latency: Some(1),
    }
}

/// Frame-rate cap for presenting without vsync. Nothing else paces frames then (on X11; Wayland
/// still throttles to the compositor), so a 1000 Hz mouse made the app render ~900 frames per
/// second and keep a CPU core busy. Waiting out the rest of a short minimum interval adds at most
/// that interval of input age, far less than a vsync queue.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy, Debug)]
pub struct FrameCap {
    min: std::time::Duration,
    next: Option<std::time::Instant>,
}

#[cfg(not(target_arch = "wasm32"))]
impl FrameCap {
    /// At most `max_fps` frames per second.
    pub fn new(max_fps: u32) -> Self {
        FrameCap { min: std::time::Duration::from_secs(1) / max_fps.max(1), next: None }
    }

    /// How long a frame starting at `now` waits; records when the following one may start.
    pub fn wait_at(&mut self, now: std::time::Instant) -> std::time::Duration {
        let wait = self.next.map_or(std::time::Duration::ZERO, |n| n.saturating_duration_since(now));
        self.next = now.checked_add(wait.saturating_add(self.min));
        wait
    }

    /// Block until this frame may start (call once per frame, at its start).
    pub fn wait(&mut self) {
        let wait = self.wait_at(std::time::Instant::now());
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
    }
}

/// The wgpu target the app renders into, recorded by [`install`].
#[derive(Clone, Copy, Debug)]
pub struct GpuTarget {
    pub format: wgpu::TextureFormat,
    /// The stencil buffer format of the render pass, when it has one (the native window asks
    /// for one): clipped viewports are clipped with it.
    pub stencil: Option<wgpu::TextureFormat>,
}

/// CPU-side vertex data for one display list, waiting to be uploaded.
#[derive(Default)]
pub struct MeshData {
    pub lines: Vec<u8>,
    pub tris: Vec<u8>,
    pub quads: Vec<u8>,
    /// Draw-order breaks, one before each wipeout mask: the fill vertices, line vertices and
    /// quad instances drawn before it. Each group is drawn fills, lines, quads in turn.
    pub breaks: Vec<[usize; 3]>,
    /// Unchanged chunks keep their own vertex buffers. Their lanes are concatenated in list
    /// order; masks still split the complete scene, rather than drawing one chunk at a time.
    pub parts: Vec<(u64, std::sync::Arc<MeshData>)>,
    instance: Option<blocks::InstanceData>,
    /// The vertex buffers already created from this data (see [`MeshData::upload_now`]); the
    /// paint callback then only swaps them in.
    uploaded: Option<Uploaded>,
}

/// GPU buffers made ahead of the frame that draws them.
#[derive(Clone)]
struct Uploaded {
    lines: Vec<Batch>,
    tris: Vec<Batch>,
    quads: Vec<Batch>,
    breaks: Vec<[usize; 3]>,
    bytes: usize,
}

impl MeshData {
    /// Create the vertex buffers now, on the calling thread, and let the CPU copy go. Making a
    /// buffer zero-fills and copies it: about 0.35 s for a million-primitive drawing, which a
    /// background rebuild does off the UI thread (see `canvas::rebuild`).
    pub fn upload_now(&mut self, device: &wgpu::Device) {
        self.uploaded = Some(Uploaded::new(device, self));
        self.lines = Vec::new();
        self.tris = Vec::new();
        self.quads = Vec::new();
        self.parts = Vec::new();
        self.instance = None;
    }

    pub fn line_vertices(&self) -> usize {
        self.lines.len() / VERTEX_SIZE
    }
    pub fn tri_vertices(&self) -> usize {
        self.tris.len() / VERTEX_SIZE
    }
    pub fn quad_instances(&self) -> usize {
        self.quads.len() / INSTANCE_SIZE
    }
    pub fn bytes(&self) -> usize {
        self.lines
            .len()
            .saturating_add(self.tris.len())
            .saturating_add(self.quads.len())
            .saturating_add(self.parts.iter().map(|(_, p)| p.bytes()).sum::<usize>())
            .saturating_add(self.instance.as_ref().map_or(0, blocks::InstanceData::bytes))
    }
    pub fn cpu_bytes(&self) -> usize {
        self.lines
            .capacity()
            .saturating_add(self.tris.capacity())
            .saturating_add(self.quads.capacity())
            .saturating_add(self.breaks.capacity().saturating_mul(std::mem::size_of::<[usize; 3]>()))
            .saturating_add(self.parts.iter().map(|(_, p)| p.cpu_bytes()).sum::<usize>())
            .saturating_add(self.instance.as_ref().map_or(0, blocks::InstanceData::bytes))
    }
}

fn rel(p: Vec2, origin: Vec2) -> [f32; 2] {
    [(p.x - origin.x) as f32, (p.y - origin.y) as f32]
}

fn push_vertex(out: &mut Vec<u8>, p: [f32; 2], c: [u8; 4]) {
    out.extend_from_slice(&p[0].to_le_bytes());
    out.extend_from_slice(&p[1].to_le_bytes());
    out.extend_from_slice(&c);
}

fn push_instance(out: &mut Vec<u8>, a: [f32; 2], b: [f32; 2], c: [u8; 4], width: f32, start: f32) {
    for v in [a[0], a[1], b[0], b[1]] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&c);
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&start.to_le_bytes());
}

/// Lineweight in screen points for a primitive (as on the CPU path), or `None` for a hairline.
pub fn wide_width(lw: f32, lwdisplay: bool) -> Option<f32> {
    if !lwdisplay || lw <= 0.0 {
        return None;
    }
    let w = (lw * 3.78).clamp(1.0, 12.0);
    (w > 1.0).then_some(w)
}

/// `c` with alpha `a`, premultiplied (as the pipelines blend).
fn premultiplied(c: Rgb, a: u8) -> [u8; 4] {
    let k = |v: u8| u8::try_from((u32::from(v) * u32::from(a) + 127) / 255).unwrap_or(v);
    [k(c.0), k(c.1), k(c.2), a]
}

/// Convert a display list into vertex data relative to `origin`, colours resolved against `bg`.
pub fn build_mesh(list: &DisplayList, origin: Vec2, bg: Rgb, lwdisplay: bool) -> MeshData {
    build_mesh_prims(list, &list.prims, origin, bg, lwdisplay)
}

fn build_mesh_prims(list: &DisplayList, prims: &[cadcraft_render::DPrim], origin: Vec2, bg: Rgb, lwdisplay: bool) -> MeshData {
    let mut m = MeshData {
        lines: Vec::with_capacity(list.segment_count().saturating_mul(2 * VERTEX_SIZE)),
        tris: Vec::with_capacity(list.tris.len().saturating_mul(VERTEX_SIZE)),
        quads: Vec::new(),
        breaks: Vec::new(),
        parts: Vec::new(),
        uploaded: None,
        instance: None,
    };
    for prim in prims {
        let col = premultiplied(prim.display_rgb(bg), prim.alpha());
        let pts = list.points(prim);
        match prim.kind {
            Kind::Polyline => match wide_width(prim.lw, lwdisplay) {
                Some(w) => {
                    for s in pts.windows(2) {
                        if let [a, b] = s {
                            push_instance(&mut m.quads, rel(*a, origin), rel(*b, origin), col, w, 0.0);
                        }
                    }
                }
                None => {
                    for s in pts.windows(2) {
                        if let [a, b] = s {
                            push_vertex(&mut m.lines, rel(*a, origin), col);
                            push_vertex(&mut m.lines, rel(*b, origin), col);
                        }
                    }
                }
            },
            Kind::Tris => {
                for t in pts.as_chunks::<3>().0 {
                    for q in t {
                        push_vertex(&mut m.tris, rel(*q, origin), col);
                    }
                }
            }
            Kind::Mask => {
                m.breaks.push([m.tri_vertices(), m.line_vertices(), m.quad_instances()]);
                for t in pts.as_chunks::<3>().0 {
                    for q in t {
                        push_vertex(&mut m.tris, rel(*q, origin), [bg.0, bg.1, bg.2, 255]);
                    }
                }
            }
            Kind::Point => {
                if let Some(q) = pts.first() {
                    let p = rel(*q, origin);
                    push_instance(&mut m.quads, p, p, col, POINT_SIZE, 0.0);
                }
            }
            // Drawn on the CPU each frame (clipped to the view).
            Kind::Infinite { .. } => {}
        }
    }
    m
}

/// Vertex data drawing primitives `prims` of `list` in `color` (the selection highlight), as on
/// the CPU path: lines `width` points wide, and fills filled, or with `dashed` (SELECTIONEFFECT 0;
/// the callback's `dash` dashes the lines) outlined. A wipeout is always outlined, since filling it
/// would hide the drawing; infinite lines are left to the CPU (they are clipped per frame).
pub fn build_highlight_mesh(
    list: &impl cadcraft_render::screen::Geometry,
    prims: &[usize],
    origin: Vec2,
    color: [u8; 4],
    width: f32,
    dashed: bool,
) -> MeshData {
    let mut m = MeshData::default();
    for primitive in prims.iter().filter_map(|&i| list.primitive(i)) {
        let prim = &primitive.prim;
        let pts = primitive.points();
        match prim.kind {
            Kind::Polyline => {
                let mut run = 0.0;
                for s in pts.windows(2) {
                    if let [a, b] = s {
                        push_instance(&mut m.quads, rel(*a, origin), rel(*b, origin), color, width, run as f32);
                        run += a.dist(*b);
                    }
                }
            }
            Kind::Point => {
                if let Some(q) = pts.first() {
                    let p = rel(*q, origin);
                    push_instance(&mut m.quads, p, p, color, 3.0, 0.0);
                }
            }
            Kind::Tris if !dashed => {
                let fill = translucent(color, HIGHLIGHT_FILL_ALPHA);
                for t in pts.as_chunks::<3>().0 {
                    for q in t {
                        push_vertex(&mut m.tris, rel(*q, origin), fill);
                    }
                }
            }
            Kind::Tris | Kind::Mask => {
                for line in primitive.fill_outlines() {
                    let mut run = 0.0;
                    for s in line.windows(2) {
                        if let [a, b] = s {
                            push_instance(&mut m.quads, rel(*a, origin), rel(*b, origin), color, width, run as f32);
                            run += a.dist(*b);
                        }
                    }
                }
            }
            Kind::Infinite { .. } => {}
        }
    }
    m
}

/// The vertex origin for a display list seen at `view_center` / `view_height`: the list's centre
/// normally; at deep zoom (the view much smaller than the drawing) a point near the view, snapped
/// to a coarse grid so panning re-uploads only every few dozen screens.
pub fn choose_origin(bounds: &Bounds2, view_center: Vec2, view_height: f64) -> Vec2 {
    let size = bounds.width().max(bounds.height());
    if bounds.is_empty()
        || !view_center.is_finite()
        || view_height.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater)
        || view_height * 2000.0 >= size
    {
        return if bounds.is_empty() { Vec2::ZERO } else { bounds.center() };
    }
    let step = view_height * 64.0;
    Vec2::new((view_center.x / step).round() * step, (view_center.y / step).round() * step)
}

/// Pending upload shared between the canvas (producer) and the paint callback (consumer).
pub type MeshSlot = Arc<Mutex<Option<MeshData>>>;

/// Per-frame parameters for the paint callback.
pub struct CanvasCallback {
    /// Identifies the mesh in `slot`; the GPU buffers are rebuilt when it changes.
    pub key: u64,
    pub slot: MeshSlot,
    /// (origin − view centre) in world units.
    pub offset: [f32; 2],
    /// Points per world unit.
    pub scale: f32,
    /// Canvas rect centre in points.
    pub center: [f32; 2],
    /// Which buffers to draw: [`LAYER_DRAWING`], [`LAYER_DRAG`], [`LAYER_HIGHLIGHT`] or
    /// [`LAYER_VIEWPORT`].
    pub layer: usize,
    /// Affine transform applied to the origin-relative positions before `offset`: the linear
    /// part (a, b, c, d as in `Mat3`) and the translation. [`IDENTITY_XFORM`] for the drawing.
    pub xform: [f32; 6],
    /// Dash period in points for wide lines (0 = solid): the selection highlight.
    pub dash: f32,
    /// How the draw uses the stencil buffer.
    pub stencil: Stencil,
}

/// How a [`CanvasCallback`] uses the stencil buffer ([`GpuTarget::stencil`]): a clipped
/// viewport's outline is drawn as a triangle fan that flips a stencil bit ([`Stencil::Mask`],
/// even-odd, as the outline clips on the CPU), then its contents draw only where the bit is set
/// ([`Stencil::Inside`]). The stencil is cleared every frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Stencil {
    #[default]
    Off,
    Mask,
    Inside,
}

/// The drawing's display list.
pub const LAYER_DRAWING: usize = 0;
/// The objects a MOVE/COPY/ROTATE/SCALE preview drags, uploaded once per drag.
pub const LAYER_DRAG: usize = 1;
/// The selection highlight, rebuilt only when the selection or the drawing changes.
pub const LAYER_HIGHLIGHT: usize = 2;
/// Model space seen through the active layout viewport, moved to its current view each frame.
pub const LAYER_VIEWPORT: usize = 3;
/// The outline of the active viewport when it is clipped (stencil only, see [`Stencil`]).
pub const LAYER_MASK: usize = 4;

/// Opacity of filled areas in the selection and hover highlight: see-through, so the lines and
/// text on (and under) a highlighted hatch stay visible, as the reference app's dot pattern does.
pub const HIGHLIGHT_FILL_ALPHA: f32 = 0.5;

/// `c` at `alpha` opacity, premultiplied (as the pipelines blend).
fn translucent(c: [u8; 4], alpha: f32) -> [u8; 4] {
    let a = alpha.clamp(0.0, 1.0);
    let m = |v: u8| (f32::from(v) * a).round() as u8;
    [m(c[0]), m(c[1]), m(c[2]), m(c[3])]
}
pub const IDENTITY_XFORM: [f32; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

#[derive(Clone)]
struct Batch {
    buffer: wgpu::Buffer,
    count: u32,
    first: u32,
    bounds: Bounds2,
    opaque: bool,
    placements: Option<Arc<blocks::Placements>>,
}

/// One uploaded mesh with its own view uniform (each callback's transform differs).
struct DrawSpan {
    lane: usize,
    batch: usize,
    range: std::ops::Range<u32>,
}

struct Layer {
    uniform: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    key: Option<u64>,
    lines: Vec<Batch>,
    tris: Vec<Batch>,
    quads: Vec<Batch>,
    breaks: Vec<[usize; 3]>,
    parts: std::collections::VecDeque<(u64, std::sync::Arc<Uploaded>)>,
    part_bytes: usize,
    active_parts: std::collections::HashSet<u64>,
    draws: Vec<DrawSpan>,
}

impl Uploaded {
    fn new(device: &wgpu::Device, m: &MeshData) -> Self {
        if let Some(uploaded) = &m.uploaded {
            return uploaded.clone();
        }
        if let Some(instance) = &m.instance {
            return instance.upload(device);
        }
        if !m.parts.is_empty() {
            let mut result = Self { lines: Vec::new(), tris: Vec::new(), quads: Vec::new(), breaks: Vec::new(), bytes: 0 };
            for (_, part) in &m.parts {
                result.append(Self::new(device, part));
            }
            return result;
        }
        let usage = wgpu::BufferUsages::VERTEX;
        Self {
            lines: upload(device, "cad_lines", &m.lines, VERTEX_SIZE, usage),
            tris: upload(device, "cad_fills", &m.tris, VERTEX_SIZE, usage),
            quads: upload(device, "cad_quads", &m.quads, INSTANCE_SIZE, usage),
            breaks: m.breaks.clone(),
            bytes: m.bytes(),
        }
    }

    fn append(&mut self, part: Self) {
        let offset = [
            self.tris.iter().map(|b| b.count as usize).sum::<usize>(),
            self.lines.iter().map(|b| b.count as usize).sum(),
            self.quads.iter().map(|b| b.count as usize).sum(),
        ];
        self.breaks.extend(part.breaks.iter().map(|b| [offset[0] + b[0], offset[1] + b[1], offset[2] + b[2]]));
        self.lines.extend(part.lines);
        self.tris.extend(part.tris);
        self.quads.extend(part.quads);
        self.bytes = self.bytes.saturating_add(part.bytes);
    }
}

impl Layer {
    fn load(&mut self, device: &wgpu::Device, mesh: MeshData) {
        self.active_parts.clear();
        if mesh.parts.is_empty() {
            let data = Uploaded::new(device, &mesh);
            self.lines = data.lines;
            self.tris = data.tris;
            self.quads = data.quads;
            self.breaks = data.breaks;
            self.plan_draws();
            return;
        }
        self.lines.clear();
        self.tris.clear();
        self.quads.clear();
        self.breaks.clear();
        let mut offset = [0usize; 3];
        for (id, m) in mesh.parts {
            self.active_parts.insert(id);
            let existing = self.parts.iter().position(|(key, _)| *key == id).and_then(|i| self.parts.remove(i));
            let data = if let Some((_, data)) = existing {
                self.part_bytes = self.part_bytes.saturating_sub(data.bytes);
                data
            } else {
                std::sync::Arc::new(Uploaded::new(device, &m))
            };
            self.breaks.extend(data.breaks.iter().map(|b| [offset[0] + b[0], offset[1] + b[1], offset[2] + b[2]]));
            offset[0] += data.tris.iter().map(|b| b.count as usize).sum::<usize>();
            offset[1] += data.lines.iter().map(|b| b.count as usize).sum::<usize>();
            offset[2] += data.quads.iter().map(|b| b.count as usize).sum::<usize>();
            self.lines.extend_from_slice(&data.lines);
            self.tris.extend_from_slice(&data.tris);
            self.quads.extend_from_slice(&data.quads);
            // Active buffers are already required by lines/tris/quads. Retaining their handles
            // costs no additional GPU storage and lets the next edit reuse unchanged chunks.
            self.part_bytes = self.part_bytes.saturating_add(data.bytes);
            self.parts.push_back((id, data));
            while self.parts.len() > 4096 {
                let Some((_, old)) = self.parts.pop_front() else { break };
                self.part_bytes = self.part_bytes.saturating_sub(old.bytes);
            }
        }
        self.plan_draws();
    }

    // Wipeouts define ordering barriers. Resolve each group's batch ranges once per upload,
    // rather than allocating and rescanning every batch on each cursor-only frame.
    fn plan_draws(&mut self) {
        self.draws.clear();
        let lanes = [&self.tris, &self.lines, &self.quads];
        let all = lanes.map(|bs| bs.iter().map(|b| b.count as usize).sum::<usize>());
        let mut from = [0; 3];
        let mut cursor = [0usize; 3];
        let mut offset = [0usize; 3];
        for to in self.breaks.iter().copied().chain(std::iter::once(all)) {
            for (lane, batches) in lanes.iter().enumerate() {
                let mut at = from[lane];
                while at < to[lane] {
                    let Some(batch) = batches.get(cursor[lane]) else { break };
                    let end = offset[lane].saturating_add(batch.count as usize);
                    let stop = end.min(to[lane]);
                    if at < stop {
                        self.draws.push(DrawSpan { lane, batch: cursor[lane], range: (at - offset[lane]) as u32..(stop - offset[lane]) as u32 });
                    }
                    if end <= to[lane] {
                        offset[lane] = end;
                        cursor[lane] += 1;
                    }
                    at = stop;
                }
            }
            from = to;
        }
    }
}

struct Resources {
    target: GpuTarget,
    image: Option<image::SceneImage>,
    line_pipeline: wgpu::RenderPipeline,
    tri_pipeline: wgpu::RenderPipeline,
    quad_pipeline: wgpu::RenderPipeline,
    block_pipeline: [wgpu::RenderPipeline; 3],
    opaque_pipeline: [wgpu::RenderPipeline; 6],
    /// With a stencil buffer: the pipelines of [`Stencil::Mask`] and [`Stencil::Inside`].
    clipped: Option<ClipPipelines>,
    linear_out: bool,
    layers: Vec<Layer>,
    cache_limit: usize,
}

struct ClipPipelines {
    /// Flips the stencil bit under each triangle; draws no colour.
    mask: wgpu::RenderPipeline,
    line: wgpu::RenderPipeline,
    tri: wgpu::RenderPipeline,
    quad: wgpu::RenderPipeline,
}

const UNIFORM_SIZE: u64 = 80;

const SHADER: &str = r#"
struct View {
    a: vec4<f32>, // offset x, offset y (world), scale (device px per unit), 1 = linear output
    b: vec4<f32>, // canvas centre x, y (device px), screen w, h (device px)
    c: vec4<f32>, // pixels per point, dash period in points (0 = solid)
    d: vec4<f32>, // transform linear part: a, b, c, d (x' = a x + c y, y' = b x + d y)
    e: vec4<f32>, // transform translation x, y (world, origin-relative)
};
@group(0) @binding(0) var<uniform> view: View;

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
    // Device pixels along a wide line from its start (dashing).
    @location(1) along: f32,
};

fn to_px(p: vec2<f32>) -> vec2<f32> {
    let q = vec2<f32>(view.d.x * p.x + view.d.z * p.y, view.d.y * p.x + view.d.w * p.y) + view.e.xy;
    let w = (q + view.a.xy) * view.a.z;
    return vec2<f32>(view.b.x + w.x, view.b.y - w.y);
}

fn px_to_clip(px: vec2<f32>) -> vec4<f32> {
    return vec4<f32>(px.x / view.b.z * 2.0 - 1.0, 1.0 - px.y / view.b.w * 2.0, 0.0, 1.0);
}

// `c` is premultiplied sRGB; a linear target gets it linearised, still premultiplied.
fn out_color(c: vec4<f32>) -> vec4<f32> {
    if (view.a.w > 0.5) {
        let s = c.rgb / max(c.a, 0.0001);
        let lo = s / 12.92;
        let hi = pow((s + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
        return vec4<f32>(select(hi, lo, s <= vec3<f32>(0.04045)) * c.a, c.a);
    }
    return c;
}

@vertex
fn vs_line(@location(0) pos: vec2<f32>, @location(1) color: vec4<f32>) -> VOut {
    var o: VOut;
    o.pos = px_to_clip(to_px(pos));
    o.color = out_color(color);
    o.along = 0.0;
    return o;
}

fn wide_quad(
    vi: u32,
    a: vec2<f32>,
    b: vec2<f32>,
    color: vec4<f32>,
    width: f32,
    start: f32,
) -> VOut {
    let pa = to_px(a);
    let pb = to_px(b);
    let d = pb - pa;
    let len = length(d);
    var dir = vec2<f32>(1.0, 0.0);
    if (len > 0.0001) {
        dir = d / len;
    }
    let n = vec2<f32>(-dir.y, dir.x);
    // Width arrives in points.
    let hw = 0.5 * width * view.c.x;
    let t = select(0.0, 1.0, vi == 1u || vi == 4u || vi == 5u);
    let s = select(-1.0, 1.0, vi == 2u || vi == 3u || vi == 5u);
    let along = mix(pa - dir * hw, pb + dir * hw, t);
    var o: VOut;
    o.pos = px_to_clip(along + n * hw * s);
    o.color = out_color(color);
    o.along = start * view.a.z + mix(-hw, len + hw, t);
    return o;
}

@vertex
fn vs_quad(@builtin(vertex_index) vi: u32, @location(0) a: vec2<f32>,
    @location(1) b: vec2<f32>, @location(2) color: vec4<f32>,
    @location(3) width: f32, @location(4) start: f32) -> VOut {
    return wide_quad(vi, a, b, color, width, start);
}

fn placed(p: vec2<f32>, m: vec4<f32>, t: vec2<f32>) -> vec2<f32> {
    return vec2<f32>(m.x * p.x + m.z * p.y, m.y * p.x + m.w * p.y) + t;
}

@vertex
fn vs_block_line(@location(0) pos: vec2<f32>, @location(1) color: vec4<f32>,
    @location(5) m: vec4<f32>, @location(6) t: vec2<f32>) -> VOut {
    var o: VOut;
    o.pos = px_to_clip(to_px(placed(pos, m, t)));
    o.color = out_color(color);
    o.along = 0.0;
    return o;
}

@vertex
fn vs_block_quad(@builtin(vertex_index) vi: u32, @location(0) a: vec2<f32>,
    @location(1) b: vec2<f32>, @location(2) color: vec4<f32>,
    @location(3) width: f32, @location(4) start: f32,
    @location(5) m: vec4<f32>, @location(6) t: vec2<f32>) -> VOut {
    return wide_quad(vi % 6u, placed(a, m, t), placed(b, m, t), color, width, start);
}

@fragment
fn fs_main(v: VOut) -> @location(0) vec4<f32> {
    if (view.c.y > 0.0) {
        // 6 on, 4 off (as the CPU selection highlight's dashes).
        if (fract(v.along / (view.c.y * view.c.x)) > 0.6) {
            discard;
        }
    }
    return v.color;
}
"#;

fn vertex_layout() -> wgpu::VertexBufferLayout<'static> {
    const ATTRS: [wgpu::VertexAttribute; 2] = wgpu::vertex_attr_array![0 => Float32x2, 1 => Unorm8x4];
    wgpu::VertexBufferLayout { array_stride: VERTEX_SIZE as u64, step_mode: wgpu::VertexStepMode::Vertex, attributes: &ATTRS }
}

fn instance_layout() -> wgpu::VertexBufferLayout<'static> {
    const ATTRS: [wgpu::VertexAttribute; 5] = wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Unorm8x4, 3 => Float32, 4 => Float32];
    wgpu::VertexBufferLayout { array_stride: INSTANCE_SIZE as u64, step_mode: wgpu::VertexStepMode::Instance, attributes: &ATTRS }
}

fn block_quad_layout() -> wgpu::VertexBufferLayout<'static> {
    let mut layout = instance_layout();
    layout.step_mode = wgpu::VertexStepMode::Vertex;
    layout
}

impl Resources {
    fn new(device: &wgpu::Device, target: GpuTarget) -> Self {
        let module =
            device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("cad_canvas"), source: wgpu::ShaderSource::Wgsl(SHADER.into()) });
        let entry = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
            count: None,
        };
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("cad_canvas"), entries: &[entry(0)] });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("cad_canvas"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let layer = || {
            let uniform = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("cad_canvas_view"),
                size: UNIFORM_SIZE,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("cad_canvas"),
                layout: &bgl,
                entries: &[wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() }],
            });
            Layer {
                uniform,
                bind_group,
                key: None,
                lines: Vec::new(),
                tris: Vec::new(),
                quads: Vec::new(),
                breaks: Vec::new(),
                parts: std::collections::VecDeque::new(),
                part_bytes: 0,
                active_parts: Default::default(),
                draws: Vec::new(),
            }
        };
        // A render pass with a stencil buffer needs every pipeline to name its format; only the
        // clipped-viewport pipelines read or write it.
        let face = |compare, pass_op| wgpu::StencilFaceState {
            compare,
            fail_op: wgpu::StencilOperation::Keep,
            depth_fail_op: wgpu::StencilOperation::Keep,
            pass_op,
        };
        let stencil_state = |s: Stencil| {
            target.stencil.map(|format| {
                let stencil = match s {
                    Stencil::Off => wgpu::StencilState::default(),
                    Stencil::Mask => {
                        let f = face(wgpu::CompareFunction::Always, wgpu::StencilOperation::Invert);
                        wgpu::StencilState { front: f, back: f, read_mask: 1, write_mask: 1 }
                    }
                    Stencil::Inside => {
                        let f = face(wgpu::CompareFunction::Equal, wgpu::StencilOperation::Keep);
                        wgpu::StencilState { front: f, back: f, read_mask: 1, write_mask: 0 }
                    }
                };
                wgpu::DepthStencilState { format, depth_write_enabled: None, depth_compare: None, stencil, bias: wgpu::DepthBiasState::default() }
            })
        };
        let pipeline =
            |label: &str, vs: &str, topology: wgpu::PrimitiveTopology, buffer: wgpu::VertexBufferLayout<'static>, s: Stencil, opaque: bool| {
                device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(label),
                    layout: Some(&layout),
                    vertex: wgpu::VertexState {
                        module: &module,
                        entry_point: Some(vs),
                        buffers: &if vs.starts_with("vs_block") { vec![Some(buffer), Some(blocks::layout())] } else { vec![Some(buffer)] },
                        compilation_options: Default::default(),
                    },
                    primitive: wgpu::PrimitiveState { topology, ..Default::default() },
                    depth_stencil: stencil_state(s),
                    multisample: wgpu::MultisampleState { count: 1, mask: !0, alpha_to_coverage_enabled: false },
                    fragment: Some(wgpu::FragmentState {
                        module: &module,
                        entry_point: Some("fs_main"),
                        targets: &[Some(wgpu::ColorTargetState {
                            format: target.format,
                            blend: (!opaque).then_some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                            write_mask: if s == Stencil::Mask { wgpu::ColorWrites::empty() } else { wgpu::ColorWrites::ALL },
                        })],
                        compilation_options: Default::default(),
                    }),
                    multiview_mask: None,
                    cache: None,
                })
            };
        use wgpu::PrimitiveTopology::{LineList, TriangleList};
        Resources {
            target,
            image: None,
            line_pipeline: pipeline("cad_lines", "vs_line", LineList, vertex_layout(), Stencil::Off, false),
            tri_pipeline: pipeline("cad_fills", "vs_line", TriangleList, vertex_layout(), Stencil::Off, false),
            quad_pipeline: pipeline("cad_quads", "vs_quad", TriangleList, instance_layout(), Stencil::Off, false),
            block_pipeline: [
                pipeline("cad_block_fills", "vs_block_line", TriangleList, vertex_layout(), Stencil::Off, false),
                pipeline("cad_block_lines", "vs_block_line", LineList, vertex_layout(), Stencil::Off, false),
                pipeline("cad_block_quads", "vs_block_quad", TriangleList, block_quad_layout(), Stencil::Off, false),
            ],
            opaque_pipeline: [
                pipeline("cad_opaque_fills", "vs_line", TriangleList, vertex_layout(), Stencil::Off, true),
                pipeline("cad_opaque_lines", "vs_line", LineList, vertex_layout(), Stencil::Off, true),
                pipeline("cad_opaque_quads", "vs_quad", TriangleList, instance_layout(), Stencil::Off, true),
                pipeline("cad_opaque_block_fills", "vs_block_line", TriangleList, vertex_layout(), Stencil::Off, true),
                pipeline("cad_opaque_block_lines", "vs_block_line", LineList, vertex_layout(), Stencil::Off, true),
                pipeline("cad_opaque_block_quads", "vs_block_quad", TriangleList, block_quad_layout(), Stencil::Off, true),
            ],
            clipped: target.stencil.map(|_| ClipPipelines {
                mask: pipeline("cad_clip_mask", "vs_line", TriangleList, vertex_layout(), Stencil::Mask, false),
                line: pipeline("cad_clip_lines", "vs_line", LineList, vertex_layout(), Stencil::Inside, false),
                tri: pipeline("cad_clip_fills", "vs_line", TriangleList, vertex_layout(), Stencil::Inside, false),
                quad: pipeline("cad_clip_quads", "vs_quad", TriangleList, instance_layout(), Stencil::Inside, false),
            }),
            linear_out: target.format.is_srgb(),
            layers: (0..=LAYER_MASK).map(|_| layer()).collect(),
            cache_limit: 64 << 20,
        }
    }
}

fn upload(device: &wgpu::Device, label: &str, bytes: &[u8], stride: usize, usage: wgpu::BufferUsages) -> Vec<Batch> {
    let mut batches = Vec::new();
    for chunk in bytes.chunks(CHUNK * stride).filter(|c| c.len() >= stride) {
        let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some(label), contents: chunk, usage });
        // Small conservative ranges can be rejected before submitting vertices, rather than
        // relying on scissoring. Offsets share one buffer, with no extra GPU allocation.
        for (index, region) in chunk.chunks(384 * stride).enumerate() {
            let mut bounds = Bounds2::EMPTY;
            let mut opaque = true;
            let mut finite = true;
            for vertex in region.chunks_exact(stride) {
                for offset in if stride == INSTANCE_SIZE { &[0, 8][..] } else { &[0][..] } {
                    if let Some(pair) = vertex.get(*offset..*offset + 8) {
                        let mut values = pair.as_chunks::<4>().0.iter().map(|v| f32::from_le_bytes(*v));
                        let point = Vec2::new(f64::from(values.next().unwrap_or(f32::NAN)), f64::from(values.next().unwrap_or(f32::NAN)));
                        finite &= point.is_finite();
                        bounds.add(point);
                    }
                }
                opaque &= vertex.get(if stride == INSTANCE_SIZE { 19 } else { 11 }) == Some(&255);
            }
            batches.push(Batch {
                buffer: buffer.clone(),
                first: (index * 384) as u32,
                count: (region.len() / stride) as u32,
                bounds: if finite { bounds } else { Bounds2::EMPTY },
                opaque,
                placements: None,
            });
        }
    }
    batches
}

/// GPU trouble wgpu reported: uncaptured errors (validation, out of memory) and the device being
/// lost (a driver reset or a hung GPU). wgpu's default handler panics on the first uncaptured
/// error, closing the app with any unsaved work (#602); the handlers [`install`] registers record
/// them here instead, and the app autosaves and tells the user (see `CadApp::gpu_fault_tick`).
#[derive(Debug, Default)]
pub struct GpuFaults {
    state: Mutex<FaultState>,
}

#[derive(Debug, Default)]
struct FaultState {
    errors: u64,
    errors_seen: u64,
    lost: bool,
    lost_seen: bool,
    last: String,
}

/// What [`GpuFaults::take`] reports.
#[derive(Clone, Debug, PartialEq)]
pub struct GpuFault {
    /// Uncaptured errors so far (an invalid resource can fail again every frame).
    pub errors: u64,
    /// The device was lost: nothing more can be drawn until the app restarts.
    pub lost: bool,
    /// The latest error or device-lost message.
    pub message: String,
}

impl GpuFaults {
    /// Errors logged in full before only every 1000th is (a repeating error would flood the log).
    const LOGGED: u64 = 10;

    pub fn error(&self, message: String) {
        let Ok(mut s) = self.state.lock() else { return };
        s.errors += 1;
        if s.errors <= Self::LOGGED || s.errors % 1000 == 0 {
            log::error!("wgpu error #{}: {message}", s.errors);
        }
        s.last = message;
    }

    pub fn device_lost(&self, reason: wgpu::DeviceLostReason, message: String) {
        // `Destroyed` is the device being dropped on purpose (the app closing), not a failure.
        if reason == wgpu::DeviceLostReason::Destroyed {
            return;
        }
        log::error!("wgpu device lost ({reason:?}): {message}");
        let Ok(mut s) = self.state.lock() else { return };
        s.lost = true;
        s.last = message;
    }

    /// Faults reported since the last call, if any.
    pub fn take(&self) -> Option<GpuFault> {
        let Ok(mut s) = self.state.lock() else { return None };
        if s.errors == s.errors_seen && s.lost == s.lost_seen {
            return None;
        }
        s.errors_seen = s.errors;
        s.lost_seen = s.lost;
        Some(GpuFault { errors: s.errors, lost: s.lost, message: s.last.clone() })
    }
}

/// Create the canvas pipelines in the renderer's callback resources, and report wgpu errors and
/// device loss to `faults` instead of panicking. Call once with the app's render state (eframe
/// `CreationContext::wgpu_render_state`) and the format of its render pass's stencil buffer,
/// if it has one (eframe `NativeOptions::stencil_buffer`).
pub fn install(rs: &egui_wgpu::RenderState, faults: &Arc<GpuFaults>, stencil: Option<wgpu::TextureFormat>) -> GpuTarget {
    let f = Arc::clone(faults);
    rs.device.on_uncaptured_error(Arc::new(move |e: wgpu::Error| f.error(e.to_string())));
    let f = Arc::clone(faults);
    rs.device.set_device_lost_callback(move |reason, message| f.device_lost(reason, message));
    let target = GpuTarget { format: rs.target_format, stencil: stencil.filter(|s| s.has_stencil_aspect()) };
    let res = Resources::new(&rs.device, target);
    rs.renderer.write().callback_resources.insert(res);
    log::info!("gpu canvas: target {:?}, stencil {:?}", rs.target_format, target.stencil);
    target
}

impl Layer {
    fn retained_bytes(&self) -> usize {
        self.parts.iter().filter(|(id, _)| !self.active_parts.contains(id)).map(|(_, p)| p.bytes).sum()
    }
}

impl Resources {
    fn trim_retained(&mut self) {
        let mut bytes: usize = self.layers.iter().map(Layer::retained_bytes).sum();
        let limit = self.cache_limit.saturating_sub(self.image.as_ref().map_or(0, image::SceneImage::bytes));
        while bytes > limit {
            let Some(layer) = self.layers.iter_mut().max_by_key(|l| l.retained_bytes()) else { break };
            let Some(at) = layer.parts.iter().position(|(id, _)| !layer.active_parts.contains(id)) else { break };
            let Some((_, old)) = layer.parts.remove(at) else { break };
            layer.part_bytes = layer.part_bytes.saturating_sub(old.bytes);
            bytes = bytes.saturating_sub(old.bytes);
        }
    }
}

/// One allowance shared by every retained GPU layer, apart from buffers needed for this frame.
pub fn set_cache_budget(rs: &egui_wgpu::RenderState, mib: u32) {
    if let Some(resources) = rs.renderer.write().callback_resources.get_mut::<Resources>() {
        resources.cache_limit = (mib.min(64) as usize) << 20;
        if resources.image.as_ref().is_some_and(|image| image.bytes() > resources.cache_limit) {
            resources.image = None;
        }
        resources.trim_retained();
    }
}

pub fn trim_cache(rs: &egui_wgpu::RenderState) {
    if let Some(resources) = rs.renderer.write().callback_resources.get_mut::<Resources>() {
        resources.image = None;
        for layer in &mut resources.layers {
            layer.key = None;
            layer.lines.clear();
            layer.tris.clear();
            layer.quads.clear();
            layer.breaks.clear();
            layer.draws.clear();
            layer.parts.clear();
            layer.active_parts.clear();
            layer.part_bytes = 0;
        }
    }
}

impl egui_wgpu::CallbackTrait for CanvasCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        screen: &egui_wgpu::ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(res) = resources.get_mut::<Resources>() else { return Vec::new() };
        let linear_out = res.linear_out;
        let Some(layer) = res.layers.get_mut(self.layer) else { return Vec::new() };
        let mut changed = false;
        if layer.key != Some(self.key) {
            let pending = self.slot.lock().ok().and_then(|mut s| s.take());
            if let Some(m) = pending {
                layer.load(device, m);
                layer.key = Some(self.key);
                changed = true;
            }
        }
        if changed {
            res.trim_retained();
        }
        let Some(res) = res.layers.get(self.layer) else { return Vec::new() };
        let ppp = screen.pixels_per_point;
        let [w, h] = screen.size_in_pixels;
        let a = [self.offset[0], self.offset[1], self.scale * ppp, if linear_out { 1.0 } else { 0.0 }];
        let b = [self.center[0] * ppp, self.center[1] * ppp, w.max(1) as f32, h.max(1) as f32];
        let c = [ppp, self.dash, 0.0, 0.0];
        let [ma, mb, mc, md, tx, ty] = self.xform;
        let d = [ma, mb, mc, md];
        let e = [tx, ty, 0.0, 0.0];
        let mut bytes = [0u8; UNIFORM_SIZE as usize];
        for (dst, value) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(a.iter().chain(&b).chain(&c).chain(&d).chain(&e)) {
            dst.copy_from_slice(&value.to_le_bytes());
        }
        queue.write_buffer(&res.uniform, 0, &bytes);
        Vec::new()
    }

    fn paint(&self, info: egui::PaintCallbackInfo, pass: &mut wgpu::RenderPass<'static>, resources: &egui_wgpu::CallbackResources) {
        let Some(shared) = resources.get::<Resources>() else { return };
        let Some(res) = shared.layers.get(self.layer) else { return };
        if res.key != Some(self.key) {
            return;
        }
        shared.paint_layer(self, res, pass, info.screen_size_px, info.clip_rect);
    }
}

impl CanvasCallback {
    fn visible(&self, rect: egui::Rect) -> Option<Bounds2> {
        use cadcraft_geom::Mat3;
        let scale = f64::from(self.scale);
        if !(scale.is_finite() && scale > 0.0) {
            return None;
        }
        let [a, b, c, d, e, f] = self.xform.map(f64::from);
        let inverse = Mat3 { a, b, c, d, e, f }.inverse()?;
        let bounds = Bounds2::from_points([rect.left_top(), rect.right_top(), rect.left_bottom(), rect.right_bottom()].map(|p| {
            inverse.apply(Vec2::new(
                f64::from(p.x - self.center[0]) / scale - f64::from(self.offset[0]),
                f64::from(self.center[1] - p.y) / scale - f64::from(self.offset[1]),
            ))
        }));
        let magnitude = bounds.min.x.abs().max(bounds.min.y.abs()).max(bounds.max.x.abs()).max(bounds.max.y.abs());
        // Include wide strokes, f32 placement rounding and affine overlay transforms.
        let stretch = inverse.a.hypot(inverse.b).hypot(inverse.c.hypot(inverse.d)).max(1.0);
        let margin = 32.0 / scale * stretch + magnitude * f64::from(f32::EPSILON) * 16.0;
        (bounds.min.is_finite() && bounds.max.is_finite() && margin.is_finite()).then(|| bounds.expand(margin))
    }
}

impl Resources {
    fn paint_layer(&self, callback: &CanvasCallback, res: &Layer, pass: &mut wgpu::RenderPass<'_>, screen: [u32; 2], clip: egui::Rect) {
        let [sw, sh] = screen;
        if sw == 0 || sh == 0 {
            return;
        }
        pass.set_viewport(0.0, 0.0, sw as f32, sh as f32, 0.0, 1.0);
        pass.set_bind_group(0, &res.bind_group, &[]);
        let (tri, line, quad) = match (callback.stencil, &self.clipped) {
            (Stencil::Off, _) => (&self.tri_pipeline, &self.line_pipeline, &self.quad_pipeline),
            (Stencil::Mask, Some(c)) => (&c.mask, &c.mask, &c.mask),
            (Stencil::Inside, Some(c)) => (&c.tri, &c.line, &c.quad),
            (_, None) => return,
        };
        if callback.stencil != Stencil::Off {
            pass.set_stencil_reference(1);
        }
        let mask = callback.stencil == Stencil::Mask;
        let lanes = [(tri, &res.tris), (line, &res.lines), (quad, &res.quads)];
        let visible = callback.visible(clip);
        let intersects = |bounds: Bounds2| bounds.is_empty() || visible.is_none_or(|view| bounds.intersects(&view));
        let mut previous = None;
        let mut draws = res.draws.iter().peekable();
        while let Some(draw) = draws.next() {
            if mask && draw.lane != 0 {
                continue;
            }
            let Some((normal, batches)) = lanes.get(draw.lane) else { continue };
            let Some(batch) = batches.get(draw.batch) else { continue };
            let instanced = batch.placements.is_some();
            if !instanced && !intersects(batch.bounds) {
                continue;
            }
            let opaque = batch.opaque && callback.stencil == Stencil::Off;
            let state = (draw.lane, instanced, opaque);
            let pipeline = if opaque {
                self.opaque_pipeline.get(draw.lane + if instanced { 3 } else { 0 })
            } else if instanced && callback.stencil == Stencil::Off {
                self.block_pipeline.get(draw.lane)
            } else if !instanced {
                Some(*normal)
            } else {
                None
            };
            let Some(pipeline) = pipeline else { continue };
            if previous != Some(state) {
                pass.set_pipeline(pipeline);
                previous = Some(state);
            }
            pass.set_vertex_buffer(0, batch.buffer.slice(..));
            let mut range = batch.first + draw.range.start..batch.first + draw.range.end;
            // Joining adjacent visible ranges recovers one draw for a fully visible buffer.
            // A rejected range or a different material/placement remains an ordering boundary.
            while let Some(next) = draws.peek().filter(|next| next.lane == draw.lane) {
                let Some(other) = batches.get(next.batch) else { break };
                let same_instances = match (&batch.placements, &other.placements) {
                    (None, None) => true,
                    (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                    _ => false,
                };
                if other.buffer != batch.buffer
                    || other.opaque != batch.opaque
                    || !same_instances
                    || range.end != other.first + next.range.start
                    || (!instanced && !intersects(other.bounds))
                {
                    break;
                }
                range.end = other.first + next.range.end;
                draws.next();
            }
            if let Some(placements) = &batch.placements {
                pass.set_vertex_buffer(1, placements.buffer.slice(..));
                let mut first = None;
                for (index, bounds) in placements.bounds.iter().enumerate().chain(std::iter::once((placements.bounds.len(), &Bounds2::EMPTY))) {
                    let show = index < placements.bounds.len() && intersects(*bounds);
                    if show && first.is_none() {
                        first = Some(index as u32);
                    }
                    if !show && let Some(start) = first.take() {
                        let vertices = if draw.lane == 2 { range.start * 6..range.end * 6 } else { range.clone() };
                        pass.draw(vertices, start..index as u32);
                    }
                }
            } else if draw.lane == 2 {
                pass.draw(0..6, range);
            } else {
                pass.draw(range, 0..1);
            }
        }
    }
}

/// For batches holding `counts` vertices (or instances) each: the batch indices and the ranges
/// within them that cover `range` of the batches laid end to end.
#[cfg(test)]
fn batch_ranges(counts: impl IntoIterator<Item = u32>, range: std::ops::Range<usize>) -> Vec<(usize, std::ops::Range<u32>)> {
    let mut out = Vec::new();
    let mut off = 0usize;
    for (j, n) in counts.into_iter().enumerate() {
        let n = n as usize;
        let (lo, hi) = (range.start.max(off), range.end.min(off + n));
        if lo < hi {
            out.push((j, (lo - off) as u32..(hi - off) as u32));
        }
        off += n;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use cadcraft_doc::Handle;
    use cadcraft_render::DPrim;

    #[test]
    fn surface_config_prefers_low_latency_where_asked_and_honours_override() {
        use wgpu::PresentMode::{AutoNoVsync, AutoVsync};
        assert_eq!(surface_config(None, true).present_mode, AutoNoVsync);
        assert_eq!(surface_config(None, false).present_mode, AutoVsync);
        for on in ["1", "on", " TRUE ", "yes"] {
            assert_eq!(surface_config(Some(on), true).present_mode, AutoVsync, "{on}");
        }
        for off in ["0", "Off", "false", "no"] {
            assert_eq!(surface_config(Some(off), false).present_mode, AutoNoVsync, "{off}");
        }
        assert_eq!(surface_config(Some("maybe"), true).present_mode, AutoNoVsync);
        assert_eq!(surface_config(Some(""), false).present_mode, AutoVsync);
        assert_eq!(surface_config(None, true).desired_maximum_frame_latency, Some(1));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn frame_cap_waits_out_the_minimum_interval_only() {
        use std::time::{Duration, Instant};
        let mut cap = FrameCap::new(250); // 4 ms
        let t0 = Instant::now();
        assert_eq!(cap.wait_at(t0), Duration::ZERO, "first frame never waits");
        assert_eq!(cap.wait_at(t0 + Duration::from_millis(1)), Duration::from_millis(3));
        // That frame started at 4 ms; the next one 10 ms later is long past the interval.
        assert_eq!(cap.wait_at(t0 + Duration::from_millis(14)), Duration::ZERO);
        assert_eq!(cap.wait_at(t0 + Duration::from_millis(18)), Duration::ZERO);
        assert_eq!(FrameCap::new(0).min, Duration::from_secs(1));
    }

    fn list() -> DisplayList {
        let mut l = DisplayList {
            verts: vec![Vec2::new(1e6, 1e6), Vec2::new(1e6 + 1.0, 1e6), Vec2::new(1e6 + 1.0, 1e6 + 1.0), Vec2::new(5.0, 5.0)],
            tris: vec![Vec2::new(0.0, 0.0), Vec2::new(1.0, 0.0), Vec2::new(0.0, 1.0)],
            ..Default::default()
        };
        let p = |kind, start, len, lw| DPrim { handle: Handle(1), color: Rgb(255, 255, 255), aci7: true, lw, transparency: 0, kind, start, len };
        l.prims =
            vec![p(Kind::Polyline, 0, 3, 0.0), p(Kind::Tris, 0, 3, 0.0), p(Kind::Point, 3, 1, 0.0), p(Kind::Infinite { ray: false }, 0, 2, 0.0)];
        for v in l.verts.clone() {
            l.bounds.add(v);
        }
        l
    }

    #[test]
    fn highlight_dashes_run_on_along_each_polyline() {
        let l = list();
        // The 3-point polyline (two unit segments) and the point marker.
        let m = build_highlight_mesh(&l, &[0, 2], Vec2::new(1e6, 1e6), [1, 2, 3, 255], 1.5, true);
        assert_eq!(m.quad_instances(), 3);
        let start = |i: usize| {
            let o = i * INSTANCE_SIZE + 24;
            f32::from_le_bytes([m.quads[o], m.quads[o + 1], m.quads[o + 2], m.quads[o + 3]])
        };
        assert_eq!([start(0), start(1), start(2)], [0.0, 1.0, 0.0]);
        assert!(m.lines.is_empty() && m.tris.is_empty());
    }

    #[test]
    fn highlight_fills_fills_and_outlines_wipeouts() {
        let mut l = list();
        // The filled triangle (0,0) (1,0) (0,1) is filled in the highlight colour...
        let m = build_highlight_mesh(&l, &[1], Vec2::ZERO, [1, 2, 3, 255], 1.5, false);
        assert_eq!((m.tri_vertices(), m.quad_instances()), (3, 0));
        assert_eq!(&m.tris[8..12], &[1, 1, 2, 128], "in the highlight colour, see-through (premultiplied)");
        // ...or, dashed (SELECTIONEFFECT 0), outlined by its three edges, the dash running on.
        let m = build_highlight_mesh(&l, &[1], Vec2::ZERO, [1, 2, 3, 255], 1.5, true);
        assert_eq!((m.tri_vertices(), m.quad_instances()), (0, 3));
        let start = |i: usize| {
            let o = i * INSTANCE_SIZE + 24;
            f32::from_le_bytes([m.quads[o], m.quads[o + 1], m.quads[o + 2], m.quads[o + 3]])
        };
        let mut runs = [start(0), start(1), start(2)];
        runs.sort_by(f32::total_cmp);
        assert!(runs[0] == 0.0 && runs[1] > 0.9 && runs[2] > runs[1] + 0.9, "{runs:?}");
        // A wipeout would hide the drawing if filled: outlined either way.
        if let Some(p) = l.prims.get_mut(1) {
            p.kind = Kind::Mask;
        }
        let m = build_highlight_mesh(&l, &[1], Vec2::ZERO, [1, 2, 3, 255], 1.5, false);
        assert_eq!((m.tri_vertices(), m.quad_instances()), (0, 3));
    }

    #[test]
    fn mesh_counts_and_precision() {
        let l = list();
        let origin = Vec2::new(1e6, 1e6);
        let m = build_mesh(&l, origin, Rgb(255, 255, 255), false);
        assert_eq!(m.line_vertices(), 4, "two segments");
        assert_eq!(m.tri_vertices(), 3);
        assert_eq!(m.quad_instances(), 1, "the point marker");
        // First vertex is stored relative to the origin, so it's exactly zero.
        assert_eq!(&m.lines[0..8], &[0u8; 8]);
        // Colour 7 on a white background inverts to black (display_rgb).
        assert_eq!(&m.lines[8..12], &[0, 0, 0, 255]);
        let x = f32::from_le_bytes([m.lines[12], m.lines[13], m.lines[14], m.lines[15]]);
        assert_eq!(x, 1.0);
    }

    #[test]
    fn transparent_objects_get_premultiplied_alpha() {
        let mut l = list();
        for p in &mut l.prims {
            p.transparency = 50;
        }
        // Colour 7 on a black background is white: half of it, premultiplied.
        let m = build_mesh(&l, Vec2::ZERO, Rgb(0, 0, 0), false);
        assert_eq!(&m.lines[8..12], &[128, 128, 128, 128]);
        assert_eq!(&m.tris[8..12], &[128, 128, 128, 128]);
        // A wipeout's mask stays opaque.
        if let Some(p) = l.prims.get_mut(1) {
            p.kind = Kind::Mask;
        }
        let m = build_mesh(&l, Vec2::ZERO, Rgb(10, 20, 30), false);
        assert_eq!(&m.tris[8..12], &[10, 20, 30, 255]);
        assert_eq!(premultiplied(Rgb(255, 0, 0), 255), [255, 0, 0, 255], "opaque colours are unchanged");
    }

    #[test]
    fn lineweights_become_quads() {
        let mut l = list();
        if let Some(p) = l.prims.first_mut() {
            p.lw = 0.7;
        }
        let m = build_mesh(&l, Vec2::ZERO, Rgb(0, 0, 0), true);
        assert_eq!(m.line_vertices(), 0);
        assert_eq!(m.quad_instances(), 3);
        assert_eq!(wide_width(0.25, true), None, "thin lineweights stay hairlines");
        assert_eq!(wide_width(0.7, false), None);
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn shader_is_valid_for_webgpu_and_webgl2() {
        for (shader, entries) in [
            (
                SHADER,
                vec![
                    (naga::ShaderStage::Vertex, "vs_line"),
                    (naga::ShaderStage::Vertex, "vs_quad"),
                    (naga::ShaderStage::Vertex, "vs_block_line"),
                    (naga::ShaderStage::Vertex, "vs_block_quad"),
                    (naga::ShaderStage::Fragment, "fs_main"),
                ],
            ),
            (image::SHADER, vec![(naga::ShaderStage::Vertex, "vs_image"), (naga::ShaderStage::Fragment, "fs_image")]),
            (image::CLEAR, vec![(naga::ShaderStage::Vertex, "vs_clear"), (naga::ShaderStage::Fragment, "fs_clear")]),
        ] {
            let module = naga::front::wgsl::parse_str(shader).unwrap();
            let info =
                naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::empty()).validate(&module).unwrap();
            for (stage, entry) in entries {
                let options = naga::back::glsl::Options { version: naga::back::glsl::Version::new_gles(300), ..Default::default() };
                let pipeline = naga::back::glsl::PipelineOptions { shader_stage: stage, entry_point: entry.into(), multiview: None };
                let mut out = String::new();
                let mut w =
                    naga::back::glsl::Writer::new(&mut out, &module, &info, &options, &pipeline, naga::proc::BoundsCheckPolicies::default()).unwrap();
                w.write().unwrap();
                assert!(out.contains("#version 300 es"), "{entry}");
            }
        }
    }

    #[test]
    fn origin_follows_deep_zoom() {
        let b = Bounds2::new(Vec2::new(0.0, 0.0), Vec2::new(1e6, 1e6));
        assert_eq!(choose_origin(&b, Vec2::new(10.0, 10.0), 1e5), b.center());
        let o = choose_origin(&b, Vec2::new(123_456.7, 654_321.2), 1.0);
        assert!((o.x - 123_456.7).abs() <= 32.0 && (o.y - 654_321.2).abs() <= 32.0);
        assert_eq!(choose_origin(&Bounds2::EMPTY, Vec2::new(1.0, 1.0), 1.0), Vec2::ZERO);
        assert_eq!(choose_origin(&b, Vec2::new(f64::NAN, 0.0), 1.0), b.center());
    }

    #[test]
    fn wipeout_masks_start_a_draw_order_group() {
        let mut l = list();
        if let Some(p) = l.prims.get_mut(1) {
            p.kind = Kind::Mask;
        }
        let bg = Rgb(10, 20, 30);
        let m = build_mesh(&l, Vec2::ZERO, bg, false);
        // The polyline's two segments come before the mask; the point marker after it.
        assert_eq!(m.breaks, vec![[0, 4, 0]]);
        assert_eq!(m.tri_vertices(), 3);
        assert_eq!(&m.tris[8..12], &[10, 20, 30, 255], "the mask takes the background colour");
        assert_eq!(m.quad_instances(), 1);
    }

    #[test]
    fn batch_ranges_cover_a_range_across_batches() {
        assert_eq!(batch_ranges([4, 4, 4], 2..9), vec![(0, 2..4), (1, 0..4), (2, 0..1)]);
        assert_eq!(batch_ranges([4, 4], 4..8), vec![(1, 0..4)]);
        assert!(batch_ranges([4, 4], 3..3).is_empty());
        assert!(batch_ranges([], 0..5).is_empty());
    }
}
