//! Filled font glyphs: bounded font-unit meshes shared across placements and zoom rebuilds.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock, Weak};

use cadcraft_fonts::{TextGlyph, ttf};
use cadcraft_geom::{Bounds2, Mat3, Vec2};
use lyon_tessellation::geometry_builder::{BuffersBuilder, VertexBuffers};
use lyon_tessellation::math::point;
use lyon_tessellation::path::Path;
use lyon_tessellation::{FillOptions, FillTessellator, FillVertex};

const MAX_POINTS: usize = 16_384;
const MAX_ENTRIES: usize = 8192;
const MAX_CACHE_BYTES: usize = 64 << 20;
const MAX_SIMPLIFY_STEPS: usize = 1_000_000;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Detail {
    Full,
    /// A power-of-two tolerance in font units, rounded toward finer detail.
    Reduced(i16),
    /// An em occupies at most two pixels in every direction: show the ink's extent.
    Box,
}

type Key = (usize, usize, u32, u16, Detail);

pub(crate) struct Mesh {
    _face: Option<ttf::Face>,
    contours: Arc<[Vec<Vec2>]>,
    pub triangles: Vec<Vec2>,
    stroke_mask: Option<u64>,
    templates: Mutex<Vec<(cadcraft_color::Rgb, bool, u8, u32, Weak<crate::DisplayList>)>>,
}

impl Mesh {
    pub(crate) fn bytes(&self) -> usize {
        self.contours
            .iter()
            .map(Vec::capacity)
            .sum::<usize>()
            .saturating_add(self.triangles.capacity())
            .saturating_mul(std::mem::size_of::<Vec2>())
            .saturating_add(self.contours.len().saturating_mul(std::mem::size_of::<Vec<Vec2>>()))
            .saturating_add(std::mem::size_of::<Self>())
    }

    pub(super) fn template(&self, color: cadcraft_color::Rgb, aci7: bool, transparency: u8, lw: f32) -> Arc<crate::DisplayList> {
        let mut templates = self.templates.lock().unwrap_or_else(PoisonError::into_inner);
        templates.retain(|(_, _, _, _, weak)| weak.strong_count() > 0);
        if let Some(list) = templates
            .iter()
            .find_map(|(c, a, t, w, weak)| (*c == color && *a == aci7 && *t == transparency && *w == lw.to_bits()).then(|| weak.upgrade()).flatten())
        {
            return list;
        }
        let mut list =
            crate::DisplayList { tris: self.triangles.clone(), bounds: Bounds2::from_points(self.triangles.iter().copied()), ..Default::default() };
        if !list.tris.is_empty() {
            list.prims.push(crate::DPrim {
                handle: cadcraft_doc::Handle(0),
                color,
                aci7,
                lw: 0.0,
                transparency,
                kind: crate::Kind::Tris,
                start: 0,
                len: list.tris.len() as u32,
            });
        }
        if let Some(mask) = self.stroke_mask {
            for (i, points) in self.contours.iter().enumerate() {
                let collapsed = mask & (1u64.checked_shl(i as u32).unwrap_or(0)) != 0;
                let start = list.verts.len() as u32;
                if collapsed {
                    list.verts.push(Bounds2::from_points(points.iter().copied()).center());
                } else {
                    list.verts.extend_from_slice(points);
                }
                let len = list.verts.len() as u32 - start;
                list.prims.push(crate::DPrim {
                    handle: cadcraft_doc::Handle(0),
                    color,
                    aci7,
                    lw,
                    transparency,
                    kind: if collapsed { crate::Kind::Point } else { crate::Kind::Polyline },
                    start,
                    len,
                });
            }
            list.bounds = Bounds2::from_points(list.verts.iter().copied());
        }
        let list = Arc::new(list);
        if templates.len() >= 16 {
            templates.remove(0);
        }
        templates.push((color, aci7, transparency, lw.to_bits(), Arc::downgrade(&list)));
        list
    }
}

struct Entry {
    // Keep the bytes alive: registering a replacement font must never reuse this key's address.
    mesh: Arc<Mesh>,
    used: AtomicBool,
    bytes: usize,
}

struct Cache {
    entries: HashMap<Key, Entry>,
    bytes: usize,
    clock: VecDeque<Key>,
    limit: usize,
    weak: HashMap<Key, Weak<Mesh>>,
}

impl Default for Cache {
    fn default() -> Self {
        Self { entries: HashMap::new(), bytes: 0, clock: VecDeque::new(), limit: MAX_CACHE_BYTES, weak: HashMap::new() }
    }
}

impl Cache {
    fn trim(&mut self, additional: usize) {
        let mut second_chances = self.clock.len();
        while self.entries.len() >= MAX_ENTRIES || self.bytes.saturating_add(additional) > self.limit {
            let Some(old) = self.clock.pop_front() else { break };
            if second_chances > 0 && self.entries.get(&old).is_some_and(|entry| entry.used.swap(false, Ordering::Relaxed)) {
                second_chances -= 1;
                self.clock.push_back(old);
                continue;
            }
            if let Some(entry) = self.entries.remove(&old) {
                self.bytes = self.bytes.saturating_sub(entry.bytes);
            }
        }
    }
}

/// Extra reusable glyph geometry; live templates referenced by a drawing remain valid.
pub fn set_cache_budget(bytes: usize) {
    let mut c = cache().write().unwrap_or_else(PoisonError::into_inner);
    let bytes = bytes.min(MAX_CACHE_BYTES);
    cadcraft_fonts::ttf::set_glyph_cache_budget(bytes / 3);
    c.limit = bytes - bytes / 3;
    c.trim(0);
}

pub fn trim_cache() {
    cadcraft_fonts::ttf::trim_glyph_cache();
    let mut c = cache().write().unwrap_or_else(PoisonError::into_inner);
    c.entries.clear();
    c.clock.clear();
    c.bytes = 0;
    c.weak.retain(|_, mesh| mesh.strong_count() > 0);
}

/// A read-write lock: display lists are built on all cores and nearly every lookup is a hit.
fn cache() -> &'static RwLock<Cache> {
    static CACHE: OnceLock<RwLock<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(Cache::default()))
}

/// World-space placement of font units, including the block/viewport transform.
pub(super) fn placement(g: &TextGlyph, xf: Mat3) -> Mat3 {
    xf.then_before(Mat3 { a: g.x.x / 1000.0, b: g.x.y / 1000.0, c: g.y.x / 1000.0, d: g.y.y / 1000.0, e: g.origin.x, f: g.origin.y })
}

fn detail(xf: Mat3, pixel: f64) -> Detail {
    // The Frobenius norm bounds the largest stretch, including shear, mirroring and unequal
    // block scales. A font-unit error below pixel / stretch is below a pixel in every direction.
    let stretch = xf.a.hypot(xf.b).hypot(xf.c.hypot(xf.d));
    if !(pixel.is_finite() && pixel > 0.0 && stretch.is_finite() && stretch > 0.0) {
        return Detail::Full;
    }
    if stretch * 1000.0 <= pixel * 2.0 {
        return Detail::Box;
    }
    let tolerance = pixel * 0.25 / stretch;
    if !tolerance.is_finite() || tolerance < 0.0625 { Detail::Full } else { Detail::Reduced(tolerance.log2().floor().clamp(-4.0, 16.0) as i16) }
}

pub(super) fn mesh(g: &TextGlyph, xf: Mat3, pixel: f64) -> Arc<Mesh> {
    let detail = detail(xf, pixel);
    let key = (g.face.bytes.as_ptr() as usize, g.face.bytes.len(), g.face.index, g.gid, detail);
    if let Some(entry) = cache().read().unwrap_or_else(PoisonError::into_inner).entries.get(&key) {
        entry.used.store(true, Ordering::Relaxed);
        return entry.mesh.clone();
    }
    if let Some(mesh) = cache().read().unwrap_or_else(PoisonError::into_inner).weak.get(&key).and_then(Weak::upgrade) {
        return mesh;
    }
    let contours = ttf::glyph_contours(&g.face, g.gid);
    let points = contours.iter().map(Vec::len).try_fold(0usize, usize::checked_add);
    // Font data is untrusted; don't hand unbounded or invalid paths to the tessellator.
    if !points.is_some_and(|n| n <= MAX_POINTS) || contours.iter().flatten().any(|p| !p.is_finite() || p.x.abs().max(p.y.abs()) > 1.0e6) {
        return Arc::new(Mesh {
            _face: Some(g.face.clone()),
            contours: Arc::default(),
            triangles: Vec::new(),
            stroke_mask: None,
            templates: Mutex::default(),
        });
    }
    let contours = match detail {
        Detail::Full => contours,
        Detail::Reduced(band) => contours.iter().map(|c| simplify(c, 2f64.powi(i32::from(band)))).collect::<Vec<_>>().into(),
        Detail::Box => {
            let bb = Bounds2::from_points(contours.iter().flatten().copied());
            if bb.is_empty() {
                Arc::default()
            } else {
                vec![vec![bb.min, Vec2::new(bb.max.x, bb.min.y), bb.max, Vec2::new(bb.min.x, bb.max.y), bb.min]].into()
            }
        }
    };
    let mesh =
        Arc::new(Mesh { _face: Some(g.face.clone()), triangles: triangulate(&contours), contours, stroke_mask: None, templates: Mutex::default() });
    retain(key, mesh)
}

fn retain(key: Key, mesh: Arc<Mesh>) -> Arc<Mesh> {
    let points = mesh.contours.iter().map(Vec::capacity).sum::<usize>().saturating_add(mesh.triangles.capacity());
    let bytes = points
        .saturating_mul(std::mem::size_of::<Vec2>())
        .saturating_add(mesh.contours.len().saturating_mul(std::mem::size_of::<Vec<Vec2>>()))
        .saturating_add(std::mem::size_of::<Entry>() + std::mem::size_of::<Key>() + std::mem::size_of::<Mesh>())
        .saturating_add(16 * std::mem::size_of::<(cadcraft_color::Rgb, bool, u8, u32, Weak<crate::DisplayList>)>());
    let mut c = cache().write().unwrap_or_else(PoisonError::into_inner);
    if let Some(existing) = c.entries.get(&key) {
        existing.used.store(true, Ordering::Relaxed);
        return existing.mesh.clone();
    }
    if let Some(existing) = c.weak.get(&key).and_then(Weak::upgrade) {
        return existing;
    }
    let weak_limit = (c.limit / 1024).clamp(128, MAX_ENTRIES);
    if c.weak.len() >= weak_limit {
        c.weak.retain(|_, mesh| mesh.strong_count() > 0);
    }
    if c.weak.len() < weak_limit {
        c.weak.insert(key, Arc::downgrade(&mesh));
    }
    if bytes <= c.limit {
        c.trim(bytes);
        c.bytes = c.bytes.saturating_add(bytes);
        c.clock.push_back(key);
        c.entries.insert(key, Entry { mesh: mesh.clone(), used: AtomicBool::new(true), bytes });
    }

    mesh
}

/// Drafting-font templates share the same bounded cache and weak live-scene lookup as outlines.
pub(super) fn stroke(g: &cadcraft_fonts::StrokeGlyph, mask: u64) -> Option<Arc<Mesh>> {
    if g.spec.len() > 1024 {
        return None;
    }
    // Face allocations are non-null; a zero first component reserves a disjoint namespace
    // for static built-in font specifications, including the selected screen-detail mask.
    let key = (0, g.spec.as_ptr() as usize, (mask >> 16) as u32, mask as u16, Detail::Full);
    if let Some(entry) = cache().read().unwrap_or_else(PoisonError::into_inner).entries.get(&key) {
        entry.used.store(true, Ordering::Relaxed);
        return Some(entry.mesh.clone());
    }
    if let Some(mesh) = cache().read().unwrap_or_else(PoisonError::into_inner).weak.get(&key).and_then(Weak::upgrade) {
        return Some(mesh);
    }
    let contours = g.geometry();
    if contours.len() > 48 {
        return None;
    }
    Some(retain(
        key,
        Arc::new(Mesh { _face: None, contours: contours.into(), triangles: Vec::new(), stroke_mask: Some(mask), templates: Mutex::default() }),
    ))
}

/// Iterative Douglas-Peucker on a closed contour. Never collapse a hole to a line or recurse
/// on font data; preserve the original if the work budget is exhausted.
fn simplify(contour: &[Vec2], tolerance: f64) -> Vec<Vec2> {
    if contour.len() < 5 {
        return contour.to_vec();
    }
    let mut points = contour.to_vec();
    if let (Some(&first), Some(&last)) = (points.first(), points.last())
        && first != last
    {
        points.push(first);
    }
    let last = points.len().saturating_sub(1);
    let mut keep = vec![false; points.len()];
    if let Some(k) = keep.first_mut() {
        *k = true;
    }
    if let Some(k) = keep.last_mut() {
        *k = true;
    }
    let mut pending = vec![(0usize, last)];
    let mut steps = 0usize;
    while let Some((start, end)) = pending.pop() {
        let (Some(&a), Some(&b)) = (points.get(start), points.get(end)) else { continue };
        let ab = b - a;
        let len2 = ab.len2();
        let mut furthest = None;
        let mut distance = tolerance;
        for (i, &p) in points.iter().enumerate().take(end).skip(start + 1) {
            steps += 1;
            if steps > MAX_SIMPLIFY_STEPS {
                return contour.to_vec();
            }
            let t = if len2 > 0.0 { ((p - a).dot(ab) / len2).clamp(0.0, 1.0) } else { 0.0 };
            let d = p.dist(a + ab * t);
            if d > distance {
                distance = d;
                furthest = Some(i);
            }
        }
        if let Some(i) = furthest {
            if let Some(k) = keep.get_mut(i) {
                *k = true;
            }
            pending.extend([(start, i), (i, end)]);
        }
    }
    let result: Vec<_> = points.into_iter().zip(keep).filter_map(|(p, keep)| keep.then_some(p)).collect();
    if result.len() >= 4 { result } else { contour.to_vec() }
}

/// Monotone tessellation with even-odd holes and intersections. Work in font units so f32
/// precision is independent of drawing coordinates; the placement stays f64. The existing
/// slab filler remains the fallback when tessellation fails (including its u16 vertex limit).
fn triangulate(contours: &[Vec<Vec2>]) -> Vec<Vec2> {
    let mut path = Path::builder();
    for contour in contours {
        let Some(&first) = contour.first().filter(|_| contour.len() >= 3) else { continue };
        path.begin(point(first.x as f32, first.y as f32));
        for p in contour.iter().skip(1) {
            path.line_to(point(p.x as f32, p.y as f32));
        }
        path.end(true);
    }
    let mut buffers: VertexBuffers<Vec2, u16> = VertexBuffers::new();
    let result = FillTessellator::new().tessellate_path(
        &path.build(),
        &FillOptions::even_odd(),
        &mut BuffersBuilder::new(&mut buffers, |v: FillVertex<'_>| {
            let p = v.position();
            Vec2::new(f64::from(p.x), f64::from(p.y))
        }),
    );
    if result.is_err() {
        return crate::fill::triangulate_evenodd(contours);
    }
    buffers.indices.into_iter().filter_map(|i| buffers.vertices.get(usize::from(i)).copied()).collect()
}
