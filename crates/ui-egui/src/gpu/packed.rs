//! Quantized display positions within a small upload page. Canonical CAD coordinates and CPU
//! picking stay f64; pages whose additional display error is too large use Float32x2.

use super::*;

pub(super) struct Page {
    pub transform: wgpu::Buffer,
}

pub(super) struct Encoded {
    pub bytes: Vec<u8>,
    pub transform: [f32; 6],
}

pub(super) fn encode(source: &[u8], error: f64) -> Option<Encoded> {
    if source.len() < 48 * VERTEX_SIZE || !source.len().is_multiple_of(VERTEX_SIZE) || !error.is_finite() || error <= 0.0 {
        return None;
    }
    let point = |vertex: &[u8]| {
        let x = f32::from_le_bytes(vertex.get(..4)?.try_into().ok()?);
        let y = f32::from_le_bytes(vertex.get(4..8)?.try_into().ok()?);
        (x.is_finite() && y.is_finite()).then_some(Vec2::new(f64::from(x), f64::from(y)))
    };
    let mut bounds = Bounds2::EMPTY;
    for vertex in source.as_chunks::<VERTEX_SIZE>().0 {
        bounds.add(point(vertex)?);
    }
    let origin = [bounds.min.x as f32, bounds.min.y as f32];
    let scale = [bounds.width() as f32, bounds.height() as f32];
    if origin.iter().chain(&scale).any(|v| !v.is_finite()) {
        return None;
    }
    // Include arithmetic rounding as well as the normalized-integer step.
    let maximum = bounds.min.x.abs().max(bounds.min.y.abs()).max(bounds.max.x.abs()).max(bounds.max.y.abs());
    let rounding = maximum * f64::from(f32::EPSILON) * 4.0;
    let step = f64::from(scale[0].max(scale[1])) / 65535.0;
    if step * 0.5 + rounding > error {
        return None;
    }
    let mut bytes = Vec::with_capacity(source.len() / VERTEX_SIZE * 8);
    for vertex in source.as_chunks::<VERTEX_SIZE>().0 {
        let p = point(vertex)?;
        for (coordinate, origin, scale) in [(p.x, origin[0], scale[0]), (p.y, origin[1], scale[1])] {
            let q = if scale > 0.0 { ((coordinate - f64::from(origin)) / f64::from(scale) * 65535.0).round().clamp(0.0, 65535.0) as u16 } else { 0 };
            let decoded = f32::from(q) / 65535.0 * scale + origin;
            if (f64::from(decoded) - coordinate).abs() + rounding > error {
                return None;
            }
            bytes.extend_from_slice(&q.to_le_bytes());
        }
        bytes.extend_from_slice(vertex.get(8..12)?);
    }
    Some(Encoded { bytes, transform: [scale[0], 0.0, 0.0, scale[1], origin[0], origin[1]] })
}

pub(super) fn layout() -> wgpu::VertexBufferLayout<'static> {
    const ATTRS: [wgpu::VertexAttribute; 2] = wgpu::vertex_attr_array![0 => Unorm16x2, 1 => Unorm8x4];
    wgpu::VertexBufferLayout { array_stride: 8, step_mode: wgpu::VertexStepMode::Vertex, attributes: &ATTRS }
}

impl Encoded {
    pub fn page(&self, device: &wgpu::Device) -> Arc<Page> {
        let bytes: Vec<_> = self.transform.iter().flat_map(|v| v.to_le_bytes()).collect();
        Arc::new(Page {
            transform: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("cad_packed_origin"),
                contents: &bytes,
                usage: wgpu::BufferUsages::VERTEX,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_positions_respect_error_and_preserve_premultiplied_colour() {
        let mut source = Vec::new();
        for i in 0..512 {
            push_vertex(&mut source, [i as f32 * 0.13 - 30.0, (i % 17) as f32 * 1.3], [13, 24, 35, 120]);
        }
        let encoded = encode(&source, 0.01).unwrap();
        assert_eq!(encoded.bytes.len() * 3, source.len() * 2);
        for (original, packed) in source.as_chunks::<12>().0.iter().zip(encoded.bytes.as_chunks::<8>().0) {
            assert_eq!(&original[8..12], &packed[4..8]);
            for (axis, scale, offset) in [(0, encoded.transform[0], encoded.transform[4]), (1, encoded.transform[3], encoded.transform[5])] {
                let q = u16::from_le_bytes(packed[axis * 2..axis * 2 + 2].try_into().unwrap());
                let value = f32::from(q) / 65535.0 * scale + offset;
                let original = f32::from_le_bytes(original[axis * 4..axis * 4 + 4].try_into().unwrap());
                assert!((value - original).abs() <= 0.01);
            }
        }
        assert!(encode(&source, 0.0000001).is_none());
        source[..4].copy_from_slice(&f32::NAN.to_le_bytes());
        assert!(encode(&source, 1.0).is_none());
    }
}
