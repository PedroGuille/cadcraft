//! Shared block vertex buffers with affine per-instance placements (WebGL2/GLES compatible).
//! Resolved styles are part of the template identity. Instances stay in their original lanes
//! and wipeout groups; only consecutive equal templates are combined into one instanced draw.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

use cadcraft_render::screen::{BlockInstance, Part};

use super::*;

pub struct Cache {
    entries: VecDeque<(Arc<DisplayList>, Rgb, bool, Arc<Geometry>)>,
    bytes: usize,
    limit: usize,
    weak: VecDeque<(std::sync::Weak<DisplayList>, Rgb, bool, std::sync::Weak<Geometry>)>,
}

impl Default for Cache {
    fn default() -> Self {
        Self { entries: VecDeque::new(), bytes: 0, limit: 8 << 20, weak: VecDeque::new() }
    }
}

impl Cache {
    pub fn set_budget(&mut self, bytes: usize) {
        self.limit = bytes.min(8 << 20);
        while self.bytes > self.limit {
            let Some((_, _, _, geometry)) = self.entries.pop_front() else { break };
            self.bytes = self.bytes.saturating_sub(geometry.retained);
        }
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    fn geometry(&mut self, source: &Arc<DisplayList>, bg: Rgb, lw: bool) -> Option<Arc<Geometry>> {
        if let Some(at) = self.entries.iter().position(|(s, color, weight, _)| Arc::ptr_eq(s, source) && *color == bg && *weight == lw) {
            let entry = self.entries.remove(at)?;
            let geometry = entry.3.clone();
            self.entries.push_back(entry);
            return Some(geometry);
        }
        self.weak.retain(|(s, _, _, geometry)| s.strong_count() > 0 && geometry.strong_count() > 0);
        if let Some(geometry) = self.weak.iter().find_map(|(s, color, weight, geometry)| {
            (s.ptr_eq(&Arc::downgrade(source)) && *color == bg && *weight == lw).then(|| geometry.upgrade()).flatten()
        }) {
            return Some(geometry);
        }
        if source.prims.iter().any(|p| matches!(p.kind, Kind::Mask | Kind::Infinite { .. })) {
            return None;
        }
        let origin = source.bounds.center();
        if !origin.is_finite() {
            return None;
        }
        let mut mesh = build_mesh(source, origin, bg, lw);
        // A line template uses explicit quad vertices so its second vertex stream can advance
        // per block, rather than per line segment. Geometry is expanded once, never per copy.
        let quad_bytes = mesh.quads.len().checked_mul(6)?;
        let bytes = mesh.lines.len().checked_add(mesh.tris.len())?.checked_add(quad_bytes)?;
        let retained = bytes.checked_add(cadcraft_render::screen::bytes(source))?;
        if bytes == 0 {
            return None;
        }
        let quads = std::mem::take(&mut mesh.quads);
        mesh.quads.reserve(quad_bytes);
        for segment in quads.as_chunks::<INSTANCE_SIZE>().0 {
            for _ in 0..6 {
                mesh.quads.extend_from_slice(segment);
            }
        }
        let geometry = Arc::new(Geometry {
            extent: source.bounds.width().max(source.bounds.height()),
            origin,
            retained,
            source: Mutex::new(Some(mesh)),
            uploaded: OnceLock::new(),
        });
        if self.weak.len() >= 4096 {
            self.weak.pop_front();
        }
        self.weak.push_back((Arc::downgrade(source), bg, lw, Arc::downgrade(&geometry)));
        if retained > self.limit {
            return Some(geometry);
        }
        while self.bytes.saturating_add(retained) > self.limit || self.entries.len() >= 128 {
            let Some((_, _, _, old)) = self.entries.pop_front() else { break };
            self.bytes = self.bytes.saturating_sub(old.retained);
        }
        self.bytes = self.bytes.saturating_add(retained);
        self.entries.push_back((source.clone(), bg, lw, geometry.clone()));
        Some(geometry)
    }
}

struct Geometry {
    origin: Vec2,
    retained: usize,
    source: Mutex<Option<MeshData>>,
    uploaded: OnceLock<Uploaded>,
    extent: f64,
}

impl Geometry {
    fn upload(&self, device: &wgpu::Device) -> &Uploaded {
        self.uploaded.get_or_init(|| {
            let mesh = self.source.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take().unwrap_or_default();
            let mut uploaded = Uploaded::new(device, &mesh);
            // The quad stream is vertex-rate in a block pipeline. Counts/offsets in the rest
            // of the renderer remain in line segments, as for the ordinary quad stream.
            for batch in &mut uploaded.quads {
                batch.first /= 6;
                batch.count /= 6;
            }
            uploaded
        })
    }
}

pub(super) struct InstanceData {
    geometry: Arc<Geometry>,
    transforms: Vec<u8>,
    bounds: Vec<Bounds2>,
}

impl InstanceData {
    pub(super) fn bytes(&self) -> usize {
        self.transforms.capacity().saturating_add(self.bounds.capacity() * std::mem::size_of::<Bounds2>())
    }

    pub(super) fn upload(&self, device: &wgpu::Device) -> Uploaded {
        let mut data = self.geometry.upload(device).clone();
        let placements = Arc::new(Placements {
            buffer: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("cad_block_placements"),
                contents: &self.transforms,
                usage: wgpu::BufferUsages::VERTEX,
            }),
            bounds: self.bounds.clone(),
            _geometry: self.geometry.clone(),
        });
        for batch in data.tris.iter_mut().chain(&mut data.lines).chain(&mut data.quads) {
            batch.placements = Some(placements.clone());
        }
        data.bytes = data.bytes.saturating_add(self.bytes());
        data
    }
}

pub(super) struct Placements {
    _geometry: Arc<Geometry>,
    pub buffer: wgpu::Buffer,
    pub bounds: Vec<Bounds2>,
}

/// Convert shared CPU geometry directly to shared GPU buffers. Precision-sensitive or
/// over-budget placements expand only while building their fallback upload, never in the scene.
pub fn build_part(part: &Part, origin: Vec2, bg: Rgb, lw: bool, cache: &Mutex<Cache>) -> MeshData {
    if part.instances.is_empty() {
        return build_mesh(&part.list, origin, bg, lw);
    }
    let mut cache = cache.try_lock().ok();
    let mut mesh = MeshData::default();
    let mut pending = DisplayList::default();
    let mut first = 0;
    let mut at = 0;
    while let Some(instance) = part.instances.get(at) {
        append_prims(&mut pending, &part.list, part.list.prims.get(first..instance.at).unwrap_or(&[]), cadcraft_geom::Mat3::IDENTITY, None);
        first = instance.at;
        if instance.glyph
            && !part
                .instances
                .get(at + 1)
                .is_some_and(|next| Arc::ptr_eq(&next.template, &instance.template) && next.prims.start == instance.prims.end)
        {
            append_instance(&mut pending, instance);
            at += 1;
            continue;
        }
        let geometry = cache.as_mut().and_then(|c| c.geometry(&instance.template, bg, lw));
        let Some(geometry) = geometry else {
            append_instance(&mut pending, instance);
            at += 1;
            continue;
        };
        let mut data = InstanceData { geometry, transforms: Vec::new(), bounds: Vec::new() };
        let mut end = at;
        let mut last = instance.prims.start;
        while let Some(next) = part.instances.get(end) {
            if !Arc::ptr_eq(&next.template, &instance.template) || next.prims.start != last || !placement(&mut data, next, origin) {
                break;
            }
            last = next.prims.end;
            end += 1;
        }
        // A single glyph is cheaper in the ordinary upload than as an individual draw call.
        // Block bodies still share buffers when their unique attributes interrupt the run.
        if end == at || (instance.glyph && end.saturating_sub(at) < 2) {
            append_instance(&mut pending, instance);
            at += 1;
            continue;
        }
        flush(&mut pending, &mut mesh, origin, bg, lw);
        mesh.parts.push((0, Arc::new(MeshData { instance: Some(data), ..Default::default() })));
        at = end;
    }
    append_prims(&mut pending, &part.list, part.list.prims.get(first..).unwrap_or(&[]), cadcraft_geom::Mat3::IDENTITY, None);
    flush(&mut pending, &mut mesh, origin, bg, lw);
    mesh
}

fn append_instance(dst: &mut DisplayList, instance: &BlockInstance) {
    append_prims(dst, &instance.template, &instance.template.prims, instance.transform, Some(instance.handle));
}

fn append_prims(
    dst: &mut DisplayList,
    src: &DisplayList,
    prims: &[cadcraft_render::DPrim],
    xf: cadcraft_geom::Mat3,
    handle: Option<cadcraft_doc::Handle>,
) {
    for source in prims {
        let mut p = *source;
        let target = if matches!(p.kind, Kind::Tris | Kind::Mask) { &mut dst.tris } else { &mut dst.verts };
        p.start = target.len() as u32;
        p.handle = handle.unwrap_or(p.handle);
        target.extend(src.points(source).iter().map(|p| xf.apply(*p)));
        dst.prims.push(p);
    }
}

fn flush(pending: &mut DisplayList, mesh: &mut MeshData, origin: Vec2, bg: Rgb, lw: bool) {
    if !pending.prims.is_empty() {
        mesh.parts.push((0, Arc::new(build_mesh(pending, origin, bg, lw))));
        *pending = DisplayList::default();
    }
}

fn placement(data: &mut InstanceData, instance: &BlockInstance, origin: Vec2) -> bool {
    let m = instance.transform;
    let translation = m.apply(data.geometry.origin) - origin;
    let values = [m.a as f32, m.b as f32, m.c as f32, m.d as f32, translation.x as f32, translation.y as f32];
    if values.iter().any(|v| !v.is_finite()) {
        return false;
    }
    let error = (data.geometry.extent * m.a.abs().max(m.b.abs()).max(m.c.abs()).max(m.d.abs()) + translation.x.abs().max(translation.y.abs()))
        * f64::from(f32::EPSILON)
        * 4.0;
    if !(error.is_finite() && instance.tolerance.is_finite()) || error > instance.tolerance.abs() * 0.5 {
        return false;
    }
    for value in values {
        data.transforms.extend_from_slice(&value.to_le_bytes());
    }
    data.bounds.push(Bounds2 { min: instance.bounds.min - origin, max: instance.bounds.max - origin });
    true
}

pub(super) fn layout() -> wgpu::VertexBufferLayout<'static> {
    const ATTRS: [wgpu::VertexAttribute; 2] = wgpu::vertex_attr_array![5 => Float32x4, 6 => Float32x2];
    wgpu::VertexBufferLayout { array_stride: 24, step_mode: wgpu::VertexStepMode::Instance, attributes: &ATTRS }
}
