//! Bounded, exact hit-test geometry reused across cursor moves.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use cadcraft_doc::{Drawing, Entity, EntityKind};
use cadcraft_geom::{Bounds2, Line, Vec2};
use rstar::{AABB, PointDistance, RTree, RTreeObject};

const MAX_ENTRIES: usize = 32;
const MAX_BYTES: usize = 16 << 20;
const MAX_SEGMENTS: usize = 100_000;

#[derive(Clone)]
struct Edge {
    a: Vec2,
    b: Vec2,
}

impl RTreeObject for Edge {
    type Envelope = AABB<[f64; 2]>;
    fn envelope(&self) -> Self::Envelope {
        let b = Bounds2::new(self.a, self.b);
        AABB::from_corners([b.min.x, b.min.y], [b.max.x, b.max.y])
    }
}

impl PointDistance for Edge {
    fn distance_2(&self, point: &[f64; 2]) -> f64 {
        let d = Line::new(self.a, self.b).dist(Vec2::new(point[0], point[1]));
        d * d
    }
}

struct Geometry {
    edges: RTree<Edge>,
}

struct Entry {
    doc: Arc<Drawing>,
    entity: Arc<Entity>,
    tolerance: u64,
    font_revision: u64,
    geometry: Arc<Geometry>,
    bytes: usize,
}

#[derive(Default)]
struct Cache {
    entries: VecDeque<Entry>,
    bytes: usize,
}

fn cache() -> &'static Mutex<Cache> {
    static C: OnceLock<Mutex<Cache>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(Cache::default()))
}

fn same_context(a: &Drawing, b: &Drawing) -> bool {
    a.same_render_context(b)
        && a.model.same_as(&b.model)
        && a.layouts.len() == b.layouts.len()
        && a.layouts.iter().zip(&b.layouts).all(|(a, b)| a.name == b.name && a.entities.same_as(&b.entities))
}

pub(super) fn distance(d: &Drawing, e: &Entity, p: Vec2, tolerance: f64) -> Option<f64> {
    // A line and a point need no tessellation or allocation at all.
    match &e.kind {
        EntityKind::Line(l) => return Some(Line::new(l.a.xy(), l.b.xy()).dist(p).min(f64::INFINITY)),
        EntityKind::Point(q) => return Some(q.p.xy().dist(p).min(f64::INFINITY)),
        _ => {}
    }
    if !p.is_finite() || p.x.abs().max(p.y.abs()) > 1e100 || !tolerance.is_finite() || tolerance <= 0.0 {
        return None;
    }
    let tolerance = tolerance.to_bits();
    let font_revision = cadcraft_fonts::ttf::revision();
    let geometry = {
        let mut c = cache().lock().unwrap_or_else(PoisonError::into_inner);
        // Keep only one drawing snapshot: a cache budget must not retain a succession of large
        // drawings, or the old chunks of every edit, just because their hit geometry is small.
        if c.entries.back().is_some_and(|x| x.font_revision != font_revision || !same_context(&x.doc, d)) {
            c.entries.clear();
            c.bytes = 0;
        }
        let found = c.entries.iter().rposition(|x| x.tolerance == tolerance && x.entity.as_ref() == e);
        found.and_then(|i| c.entries.remove(i)).map(|entry| {
            let geometry = entry.geometry.clone();
            c.entries.push_back(entry);
            geometry
        })
    };
    let geometry = match geometry {
        Some(g) => g,
        None => {
            let polys = super::hit_polylines(d, e, f64::from_bits(tolerance));
            let count = polys.iter().map(|p| p.len().saturating_sub(1).max(usize::from(p.len() == 1))).try_fold(0usize, usize::checked_add)?;
            if count > MAX_SEGMENTS || polys.iter().flatten().any(|p| !p.is_finite() || p.x.abs().max(p.y.abs()) > 1e100) {
                return None;
            }
            let mut edges = Vec::with_capacity(count);
            for poly in polys {
                if let [point] = poly.as_slice() {
                    edges.push(Edge { a: *point, b: *point });
                } else {
                    edges.extend(poly.windows(2).filter_map(|p| Some(Edge { a: *p.first()?, b: *p.get(1)? })));
                }
            }
            let geometry = Arc::new(Geometry { edges: RTree::bulk_load(edges) });
            // Include room for tree nodes and the retained entity, not just its edge coordinates.
            let bytes = count.saturating_mul(std::mem::size_of::<Edge>() * 4).saturating_add(std::mem::size_of::<Entry>());
            if bytes <= MAX_BYTES
                && let Some(entity) = d.entity(e.handle).filter(|x| x.as_ref() == e).cloned()
            {
                let mut c = cache().lock().unwrap_or_else(PoisonError::into_inner);
                // Another session may have queried a different drawing while this geometry
                // was being built outside the lock.
                if c.entries.back().is_some_and(|x| x.font_revision != font_revision || !same_context(&x.doc, d)) {
                    c.entries.clear();
                    c.bytes = 0;
                }
                while c.entries.len() >= MAX_ENTRIES || c.bytes.saturating_add(bytes) > MAX_BYTES {
                    let Some(old) = c.entries.pop_front() else { break };
                    c.bytes = c.bytes.saturating_sub(old.bytes);
                }
                let doc = c.entries.iter().rev().find(|x| same_context(&x.doc, d)).map(|x| x.doc.clone()).unwrap_or_else(|| Arc::new(d.clone()));
                c.bytes = c.bytes.saturating_add(bytes);
                c.entries.push_back(Entry { doc, entity, tolerance, font_revision, geometry: geometry.clone(), bytes });
            }
            geometry
        }
    };
    Some(geometry.edges.nearest_neighbor(&[p.x, p.y]).map_or(f64::INFINITY, |edge| Line::new(edge.a, edge.b).dist(p)))
}
