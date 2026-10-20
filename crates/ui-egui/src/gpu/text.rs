//! Vector glyph pages: mixed characters share geometry through a vertex-readable float
//! texture. This uses GLES 3 / WebGL2 features, with no storage buffers or raster glyphs.
use super::*;
use std::collections::{HashMap, VecDeque};
use std::sync::{OnceLock, PoisonError, Weak};

const WIDTH: u32 = 256;
const PAGE_RECORDS: usize = 16_384;
const MAX_GLYPH: usize = 4096;
const STRIDE: usize = 40;

#[derive(Clone, Copy)]
pub(super) struct Glyph {
    offset: u32,
    count: u32,
    lane: usize,
    origin: Vec2,
    extent: f64,
    color: Rgb,
    aci7: bool,
    alpha: u8,
    lw: f32,
}

struct Page {
    _sources: Vec<Arc<DisplayList>>,
    source: Mutex<Option<Vec<[f32; 4]>>>,
    gpu: OnceLock<GpuPage>,
    records: usize,
    retained: usize,
}

struct GpuPage {
    _texture: wgpu::Texture,
    bind: wgpu::BindGroup,
}

pub(super) fn layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("cad_vector_glyphs"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        }],
    })
}

impl Page {
    fn upload(&self, device: &wgpu::Device, queue: &wgpu::Queue) -> &GpuPage {
        self.gpu.get_or_init(|| {
            let records = self.source.lock().unwrap_or_else(PoisonError::into_inner).take().unwrap_or_default();
            let width = WIDTH.min(self.records.next_power_of_two() as u32).max(1);
            let height = self.records.div_ceil(width as usize).max(1) as u32;
            let mut bytes = Vec::with_capacity(width as usize * height as usize * 16);
            for v in records.iter().flatten() {
                bytes.extend_from_slice(&v.to_le_bytes());
            }
            bytes.resize(width as usize * height as usize * 16, 0);
            let texture = device.create_texture_with_data(
                queue,
                &wgpu::TextureDescriptor {
                    label: Some("cad_vector_glyphs"),
                    size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba32Float,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                wgpu::util::TextureDataOrder::LayerMajor,
                &bytes,
            );
            let view = texture.create_view(&Default::default());
            let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("cad_vector_glyphs"),
                layout: &layout(device),
                entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) }],
            });
            GpuPage { _texture: texture, bind }
        })
    }
}

pub(super) struct Cache {
    entries: HashMap<usize, (Weak<DisplayList>, Weak<Page>, Glyph)>,
    pages: VecDeque<Arc<Page>>,
    bytes: usize,
    limit: usize,
}

impl Default for Cache {
    fn default() -> Self {
        Self { entries: HashMap::new(), pages: VecDeque::new(), bytes: 0, limit: 2 << 20 }
    }
}

impl Cache {
    pub fn set_budget(&mut self, bytes: usize) {
        self.limit = bytes;
        while self.bytes > self.limit {
            let Some(old) = self.pages.pop_front() else { break };
            self.bytes = self.bytes.saturating_sub(old.retained);
        }
        self.entries.retain(|_, (_, page, _)| page.strong_count() > 0);
    }
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    fn find(&self, source: &Arc<DisplayList>) -> Option<(Arc<Page>, Glyph)> {
        let (weak, page, glyph) = self.entries.get(&(Arc::as_ptr(source) as usize))?;
        if weak.ptr_eq(&Arc::downgrade(source)) { Some((page.upgrade()?, *glyph)) } else { None }
    }

    pub fn prepare(&mut self, instances: &[cadcraft_render::screen::BlockInstance]) -> Prepared {
        self.entries.retain(|_, (_, page, _)| page.strong_count() > 0);
        let mut counts = HashMap::<usize, usize>::new();
        for i in instances.iter().filter(|i| i.glyph) {
            let key = Arc::as_ptr(&i.template) as usize;
            if counts.len() < 1024 || counts.contains_key(&key) {
                let n = counts.entry(key).or_default();
                *n = n.saturating_add(1);
            }
        }
        let mut prepared = HashMap::new();
        let mut records = Vec::new();
        let mut sources = Vec::new();
        let mut glyphs = Vec::new();
        let finish = |this: &mut Self,
                      records: &mut Vec<[f32; 4]>,
                      sources: &mut Vec<Arc<DisplayList>>,
                      glyphs: &mut Vec<Glyph>,
                      prepared: &mut HashMap<usize, (Arc<Page>, Glyph)>| {
            if sources.is_empty() {
                return;
            }
            let width = (WIDTH as usize).min(records.len().next_power_of_two()).max(1);
            let texture = records.len().div_ceil(width) * width * 16;
            let copies: usize =
                sources.iter().map(|s| counts.get(&(Arc::as_ptr(s) as usize)).copied().unwrap_or(1)).fold(0usize, usize::saturating_add);
            let expanded: usize = sources
                .iter()
                .zip(glyphs.iter())
                .map(|(s, g)| {
                    let count = counts.get(&(Arc::as_ptr(s) as usize)).copied().unwrap_or(1);
                    (g.count as usize).saturating_mul(count).saturating_mul(if g.lane == 0 { 12 } else { 24 })
                })
                .fold(0usize, usize::saturating_add);
            // Rare complex outlines cost more as float texels than ordinary vertices.
            if expanded <= texture.saturating_add(copies.saturating_mul(STRIDE)) {
                records.clear();
                sources.clear();
                glyphs.clear();
                return;
            }
            let retained = texture.saturating_add(sources.iter().map(|s| cadcraft_render::screen::bytes(s)).fold(0usize, usize::saturating_add));
            let page = Arc::new(Page {
                records: records.len(),
                source: Mutex::new(Some(std::mem::take(records))),
                _sources: std::mem::take(sources),
                gpu: OnceLock::new(),
                retained,
            });
            for (source, glyph) in page._sources.iter().zip(glyphs.drain(..)) {
                let key = Arc::as_ptr(source) as usize;
                prepared.insert(key, (page.clone(), glyph));
                if this.entries.len() < 4096 {
                    this.entries.insert(key, (Arc::downgrade(source), Arc::downgrade(&page), glyph));
                }
            }
            if retained <= this.limit {
                while this.bytes.saturating_add(retained) > this.limit || this.pages.len() >= 128 {
                    let Some(old) = this.pages.pop_front() else { break };
                    this.bytes = this.bytes.saturating_sub(old.retained);
                }
                this.bytes = this.bytes.saturating_add(retained);
                this.pages.push_back(page);
            }
        };
        let mut seen = std::collections::HashSet::new();
        for instance in instances.iter().filter(|i| i.glyph) {
            let source = &instance.template;
            let key = Arc::as_ptr(source) as usize;
            if seen.len() >= 1024 || !seen.insert(key) {
                continue;
            }
            if let Some(found) = self.find(source) {
                prepared.insert(key, found);
                continue;
            }
            let Some((mut glyph, data)) = geometry(source) else { continue };
            if records.len().saturating_add(data.len()) > PAGE_RECORDS {
                finish(self, &mut records, &mut sources, &mut glyphs, &mut prepared);
            }
            glyph.offset = records.len() as u32;
            records.extend(data);
            sources.push(source.clone());
            glyphs.push(glyph);
        }
        finish(self, &mut records, &mut sources, &mut glyphs, &mut prepared);
        Prepared(prepared)
    }
}

fn geometry(source: &DisplayList) -> Option<(Glyph, Vec<[f32; 4]>)> {
    let first = source.prims.first()?;
    if !matches!(first.kind, Kind::Tris | Kind::Polyline) {
        return None;
    }
    let origin = source.bounds.center();
    if !origin.is_finite() {
        return None;
    }
    let mut data = Vec::new();
    for p in &source.prims {
        if p.kind != first.kind || p.color != first.color || p.aci7 != first.aci7 || p.alpha() != first.alpha() || p.lw != first.lw {
            return None;
        }
        let points = source.points(p);
        if matches!(p.kind, Kind::Tris) {
            for p in points {
                let [x, y] = rel(*p, origin);
                data.push([x, y, 0.0, 0.0]);
            }
        } else {
            for segment in points.windows(2) {
                if let [a, b] = segment {
                    let [x, y] = rel(*a, origin);
                    let [u, v] = rel(*b, origin);
                    data.push([x, y, u, v]);
                }
            }
        }
        if data.len() > MAX_GLYPH {
            return None;
        }
    }
    if data.is_empty() || data.iter().flatten().any(|v| !v.is_finite()) {
        return None;
    }
    Some((
        Glyph {
            offset: 0,
            count: data.len() as u32,
            lane: if matches!(first.kind, Kind::Tris) { 0 } else { 1 },
            origin,
            extent: source.bounds.width().max(source.bounds.height()),
            color: first.color,
            aci7: first.aci7,
            alpha: first.alpha(),
            lw: first.lw,
        },
        data,
    ))
}

pub(super) struct Prepared(HashMap<usize, (Arc<Page>, Glyph)>);

pub(super) struct InstanceData {
    page: Arc<Page>,
    lane: usize,
    records: u32,
    opaque: bool,
    transforms: Vec<u8>,
    bounds: Vec<Bounds2>,
    actual: u64,
}

impl Prepared {
    pub fn batch(&self, instances: &[cadcraft_render::screen::BlockInstance], origin: Vec2, bg: Rgb, lw: bool) -> Option<(InstanceData, usize)> {
        let first = instances.first()?;
        let (page, glyph) = self.0.get(&(Arc::as_ptr(&first.template) as usize))?;
        let lane = if glyph.lane == 1 && wide_width(glyph.lw, lw).is_some() { 2 } else { glyph.lane };
        let mut data = InstanceData { page: page.clone(), lane, records: 0, opaque: true, transforms: Vec::new(), bounds: Vec::new(), actual: 0 };
        let mut next = first.prims.start;
        for i in instances.iter().take(4096) {
            if !i.glyph || i.prims.start != next {
                break;
            }
            let Some((other, glyph)) = self.0.get(&(Arc::as_ptr(&i.template) as usize)) else { break };
            let next_lane = if glyph.lane == 1 && wide_width(glyph.lw, lw).is_some() { 2 } else { glyph.lane };
            if !Arc::ptr_eq(page, other) || lane != next_lane {
                break;
            }
            let records = data.records.max(glyph.count);
            let actual = data.actual + u64::from(glyph.count);
            // Bound padding work for unlike glyph sizes rather than executing the largest
            // glyph's vertex count for an arbitrarily long run of tiny characters.
            if !data.bounds.is_empty() && u64::from(records) * (data.bounds.len() as u64 + 1) > actual * 3 {
                break;
            }
            let xf = i.transform;
            let p = xf.apply(glyph.origin) - origin;
            let error = (p.x.abs().max(p.y.abs()) + glyph.extent * xf.a.abs().max(xf.b.abs()).max(xf.c.abs()).max(xf.d.abs()))
                * f64::from(f32::EPSILON)
                * 4.0;
            let transform = [xf.a as f32, xf.b as f32, xf.c as f32, xf.d as f32, p.x as f32, p.y as f32];
            if transform.iter().any(|v| !v.is_finite()) || !error.is_finite() || error > i.tolerance.abs() * 0.5 {
                break;
            }
            for v in transform {
                data.transforms.extend_from_slice(&v.to_le_bytes());
            }
            data.transforms.extend_from_slice(&glyph.offset.to_le_bytes());
            data.transforms.extend_from_slice(&glyph.count.to_le_bytes());
            let color = cadcraft_color::display_rgb(glyph.color, glyph.aci7, bg);
            data.transforms.extend_from_slice(&premultiplied(color, glyph.alpha));
            data.transforms.extend_from_slice(&wide_width(glyph.lw, lw).unwrap_or(1.0).to_le_bytes());
            data.records = records;
            data.actual = actual;
            data.opaque &= glyph.alpha == 255;
            data.bounds.push(Bounds2 { min: i.bounds.min - origin, max: i.bounds.max - origin });
            next = i.prims.end;
        }
        let count = data.bounds.len();
        (count > 0).then_some((data, count))
    }
}

pub(super) struct Placements {
    page: Arc<Page>,
    pub bounds: Vec<Bounds2>,
}

impl Placements {
    pub fn bind(&self) -> Option<&wgpu::BindGroup> {
        self.page.gpu.get().map(|p| &p.bind)
    }
}

impl InstanceData {
    #[cfg(test)]
    pub fn counts(&self) -> (usize, usize) {
        (self.bounds.len(), self.page._sources.len())
    }
    pub fn bytes(&self) -> usize {
        self.transforms.capacity() + self.bounds.capacity() * std::mem::size_of::<Bounds2>()
    }
    pub fn upload(&self, device: &wgpu::Device, queue: &wgpu::Queue) -> Uploaded {
        self.page.upload(device, queue);
        let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("cad_glyph_placements"),
            contents: &self.transforms,
            usage: wgpu::BufferUsages::VERTEX,
        });
        let mut uploaded = Uploaded {
            lines: Vec::new(),
            tris: Vec::new(),
            quads: Vec::new(),
            breaks: Vec::new(),
            bytes: self.bytes().saturating_add(self.page.retained),
        };
        let batch = Batch {
            buffer,
            count: self.records * if self.lane == 1 { 2 } else { 1 },
            first: 0,
            bounds: Bounds2::EMPTY,
            opaque: self.opaque,
            placements: None,
            spatial: None,
            packed: None,
            analytic: false,
            glyphs: Some(Arc::new(Placements { page: self.page.clone(), bounds: self.bounds.clone() })),
        };
        match self.lane {
            0 => uploaded.tris.push(batch),
            1 => uploaded.lines.push(batch),
            _ => uploaded.quads.push(batch),
        }
        uploaded
    }
}

pub(super) fn vertex_layout() -> wgpu::VertexBufferLayout<'static> {
    const ATTRS: [wgpu::VertexAttribute; 5] = wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x2, 2 => Uint32x2, 3 => Unorm8x4, 4 => Float32];
    wgpu::VertexBufferLayout { array_stride: STRIDE as u64, step_mode: wgpu::VertexStepMode::Instance, attributes: &ATTRS }
}
