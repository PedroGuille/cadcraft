//! Small continuous curves use one analytic instance instead of a list of segment vertices.
//! Only consecutive runs are combined, so colour order and wipeout barriers remain intact.

use super::*;

const SIZE: usize = 36;
const MIN_RUN: usize = 4;

pub(super) struct Data {
    records: Vec<u8>,
    bounds: Vec<Bounds2>,
}

impl Data {
    pub fn bytes(&self) -> usize {
        self.records.capacity().saturating_add(self.bounds.capacity() * std::mem::size_of::<Bounds2>())
    }

    pub fn upload(&self, device: &wgpu::Device) -> Uploaded {
        let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("cad_analytic_curves"),
            contents: &self.records,
            usage: wgpu::BufferUsages::VERTEX,
        });
        let mut lines = Vec::new();
        // Coarse groups are culled before the instanced draw; keep their source order.
        for (group, bounds) in self.bounds.chunks(64).enumerate() {
            lines.push(Batch {
                buffer: buffer.clone(),
                count: bounds.len() as u32,
                first: (group * 64) as u32,
                bounds: bounds.iter().fold(Bounds2::EMPTY, |a, b| a.union(b)),
                opaque: false,
                placements: None,
                spatial: None,
                glyphs: None,
                packed: None,
                analytic: true,
            });
        }
        Uploaded { lines, tris: Vec::new(), quads: Vec::new(), breaks: Vec::new(), bytes: self.records.len() }
    }
}

fn record(list: &DisplayList, hint: &cadcraft_render::ArcHint, origin: Vec2, bg: Rgb, error: f64) -> Option<([u8; SIZE], Bounds2)> {
    let prim = list.prims.get(hint.prim)?;
    if !hint.start.is_finite() || !hint.sweep.is_finite() || !(0.0..=std::f64::consts::TAU).contains(&hint.sweep) {
        return None;
    }
    let center = hint.center - origin;
    let values = [center.x, center.y, hint.radius];
    let roundoff = values.iter().map(|v| (f64::from(*v as f32) - v).abs()).sum::<f64>()
        + values.iter().map(|v| v.abs()).fold(0.0, f64::max) * f64::from(f32::EPSILON) * 4.0;
    if !error.is_finite() || error <= 0.0 || !roundoff.is_finite() || roundoff > error || prim.kind != Kind::Polyline || prim.lw > 0.0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(SIZE);
    for value in values {
        bytes.extend_from_slice(&(value as f32).to_le_bytes());
    }
    bytes.extend_from_slice(&premultiplied(prim.display_rgb(bg), prim.alpha()));
    for angle in [hint.start, hint.start + hint.sweep] {
        for value in [angle.cos(), angle.sin()] {
            bytes.extend_from_slice(&(value as f32).to_le_bytes());
        }
    }
    let mode: f32 = if hint.sweep >= std::f64::consts::TAU - 1e-9 {
        2.0
    } else if hint.sweep > std::f64::consts::PI {
        1.0
    } else {
        0.0
    };
    bytes.extend_from_slice(&mode.to_le_bytes());
    let bounds = Bounds2::new(center - Vec2::new(hint.radius, hint.radius), center + Vec2::new(hint.radius, hint.radius));
    Some((bytes.try_into().ok()?, bounds))
}

pub(super) fn build(list: &DisplayList, origin: Vec2, bg: Rgb, lw: bool, error: f64) -> MeshData {
    if error <= 0.0 || list.analytic.len() < MIN_RUN {
        return build_mesh(list, origin, bg, lw);
    }
    let mut mesh = MeshData::default();
    let mut first = 0;
    let mut at = 0;
    while let Some(hint) = list.analytic.get(at) {
        let mut data = Data { records: Vec::new(), bounds: Vec::new() };
        let mut end = at;
        let mut next = hint.prim;
        while let Some(hint) = list.analytic.get(end).filter(|h| h.prim == next) {
            let Some((bytes, bounds)) = record(list, hint, origin, bg, error) else { break };
            data.records.extend_from_slice(&bytes);
            data.bounds.push(bounds);
            end += 1;
            next += 1;
        }
        if end.saturating_sub(at) < MIN_RUN {
            at = end.max(at + 1);
            continue;
        }
        if let Some(prims) = list.prims.get(first..hint.prim).filter(|p| !p.is_empty()) {
            mesh.parts.push((0, Arc::new(build_mesh_prims(list, prims, origin, bg, lw))));
        }
        mesh.parts.push((0, Arc::new(MeshData { curves: Some(data), ..Default::default() })));
        first = next;
        at = end;
    }
    if first == 0 {
        return build_mesh(list, origin, bg, lw);
    }
    if let Some(prims) = list.prims.get(first..).filter(|p| !p.is_empty()) {
        mesh.parts.push((0, Arc::new(build_mesh_prims(list, prims, origin, bg, lw))));
    }
    mesh
}

pub(super) fn layout() -> wgpu::VertexBufferLayout<'static> {
    const ATTRS: [wgpu::VertexAttribute; 5] = wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32, 2 => Unorm8x4, 3 => Float32x4, 4 => Float32];
    wgpu::VertexBufferLayout { array_stride: SIZE as u64, step_mode: wgpu::VertexStepMode::Instance, attributes: &ATTRS }
}
