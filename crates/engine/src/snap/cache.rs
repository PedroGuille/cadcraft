//! Bounded neighborhood and compiled block/dimension snap caches. Leaf pruning, intersections,
//! cursor/base-dependent snaps and tracking timers still run for the exact current probe.
use cadcraft_doc::{Drawing, EntityKind, Handle, Prim, Space};
use cadcraft_geom::{Bounds2, Segment, Vec2};
use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    sync::Arc,
};

pub(super) struct Gathered {
    pub cands: Vec<(u32, Vec2)>,
    pub prims: Vec<Prim>,
    pub segments: Vec<Vec<Segment>>,
}

struct Record {
    candidates: Range<usize>,
    primitive: Option<usize>,
    prune: bool,
}

/// Records unpruned geometry once, with the original candidate ordering and pruning rules.
/// This avoids retaining an EntityKind clone or repeating nested traversal, transforms and
/// dimension layout for every hover. Allocation growth is checked while recording leaves.
pub(super) struct Recording {
    records: Vec<Record>,
    heap: usize,
    limit: usize,
    pub full: bool,
}

impl Recording {
    pub fn record(&mut self, candidates: Range<usize>, primitive: Option<usize>, prune: bool, cands: &Vec<(u32, Vec2)>, prims: &Vec<Prim>) {
        if self.full || (candidates.is_empty() && primitive.is_none()) {
            return;
        }
        if let Some(p) = primitive.and_then(|i| prims.get(i)) {
            self.heap = self.heap.saturating_add(prim_bytes(p).saturating_sub(std::mem::size_of::<Prim>()));
        }
        self.records.push(Record { candidates, primitive, prune });
        let bytes = self
            .heap
            .saturating_add(cands.capacity().saturating_mul(std::mem::size_of::<(u32, Vec2)>()))
            .saturating_add(prims.capacity().saturating_mul(std::mem::size_of::<Prim>()))
            .saturating_add(self.records.capacity().saturating_mul(std::mem::size_of::<Record>()));
        self.full = bytes > self.limit;
    }
}

struct Prepared {
    cands: Vec<(u32, Vec2)>,
    prims: Vec<Prim>,
    records: Vec<Record>,
    cost: usize,
    bytes: usize,
    used: u64,
}

impl Prepared {
    fn build(d: &Drawing, e: &cadcraft_doc::Entity, aperture: f64, osmode: u32, limit: usize) -> Option<Self> {
        let mut cands = Vec::new();
        let mut prims = Vec::new();
        let mut recording = Recording { records: Vec::new(), heap: 0, limit, full: false };
        let mut budget = super::MAX_BLOCK_LEAVES;
        let mut sink = super::Sink { osmode, aperture, probe: Bounds2::EMPTY, cands: &mut cands, near: &mut prims, recording: Some(&mut recording) };
        super::gather_entity(d, e, &mut sink, &mut budget, &[]);
        if recording.full {
            return None;
        }
        let bytes = recording
            .heap
            .saturating_add(cands.capacity().saturating_mul(std::mem::size_of::<(u32, Vec2)>()))
            .saturating_add(prims.capacity().saturating_mul(std::mem::size_of::<Prim>()))
            .saturating_add(recording.records.capacity().saturating_mul(std::mem::size_of::<Record>()))
            .saturating_add(std::mem::size_of::<Self>());
        (bytes <= limit).then_some(Self { cands, prims, records: recording.records, cost: super::MAX_BLOCK_LEAVES - budget, bytes, used: 0 })
    }

    fn append(&self, sink: &mut super::Sink<'_>, budget: &mut usize) -> bool {
        if *budget < self.cost {
            return false;
        }
        *budget -= self.cost;
        for record in &self.records {
            let primitive = record.primitive.and_then(|i| self.prims.get(i));
            if record.prune && primitive.is_some_and(|p| !super::prim_near(p, &sink.probe, sink.aperture)) {
                continue;
            }
            if let Some(points) = self.cands.get(record.candidates.clone()) {
                sink.cands.extend_from_slice(points);
            }
            if let Some(p) = primitive {
                sink.near.push(p.clone());
            }
        }
        true
    }
}

pub(crate) struct Cache {
    source: Option<Arc<Drawing>>,
    aperture: u64,
    font_revision: u64,
    mode: u32,
    region: Bounds2,
    entries: Vec<(Handle, Bounds2)>,
    selected: Vec<Handle>,
    gathered: Option<Arc<Gathered>>,
    prepared: HashMap<Handle, Prepared>,
    oversized: HashSet<Handle>,
    prepared_bytes: usize,
    tick: u64,
    limit: usize,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            source: None,
            aperture: 0,
            font_revision: 0,
            mode: 0,
            region: Bounds2::EMPTY,
            entries: Vec::new(),
            selected: Vec::new(),
            gathered: None,
            prepared: HashMap::new(),
            oversized: HashSet::new(),
            prepared_bytes: 0,
            tick: 0,
            limit: 2 << 20,
        }
    }
}

impl Cache {
    pub fn clear(&mut self) {
        *self = Self { limit: self.limit, ..Self::default() };
    }

    pub fn set_budget(&mut self, bytes: usize) {
        if bytes < self.limit {
            self.clear();
        }
        if bytes > self.limit {
            self.oversized.clear();
        }
        self.limit = bytes.min(32 << 20);
    }

    pub(super) fn nearby(
        &mut self,
        d: &Drawing,
        space: &Space,
        ix: &Option<Arc<crate::spatial::SpatialIndex>>,
        cursor: Vec2,
        aperture: f64,
        mode: u32,
    ) -> Option<Arc<Gathered>> {
        if self.limit == 0
            || *space != Space::Model
            || mode == 0
            || mode & super::mode::OFF != 0
            || !cursor.is_finite()
            || !(aperture.is_finite() && aperture > 0.0)
        {
            return None;
        }
        let query = Bounds2::new(cursor, cursor).expand(aperture * 2.0);
        if !super::finite_bounds(&query) {
            return None;
        }
        let font_revision = cadcraft_render::screen::font_revision();
        let valid = self.aperture == aperture.to_bits()
            && self.font_revision == font_revision
            && self.mode == mode
            && self.source.as_ref().is_some_and(|old| old.model.same_as(&d.model) && old.same_render_context(d));
        if !valid {
            self.prepared.clear();
            self.oversized.clear();
            self.prepared_bytes = 0;
        }
        if !valid || !self.region.contains_box(&query) {
            self.gathered = None;
            self.entries.clear();
            self.source = None;
            self.region = query.expand(aperture * 4.0);
            if !super::finite_bounds(&self.region) {
                return None;
            }
            for (e, known) in crate::select::candidates(d, space, ix, &self.region, true, false)? {
                if self.entries.len() >= 8192 || (self.entries.len() + 1) * std::mem::size_of::<(Handle, Bounds2)>() > self.limit {
                    return None;
                }
                self.entries.push((e.handle, crate::select::bounds_of(d, e, known)));
            }
            self.source = Some(Arc::new(d.clone()));
            self.aperture = aperture.to_bits();
            self.font_revision = font_revision;
            self.mode = mode;
        }
        let probe = Bounds2::new(cursor, cursor).expand(aperture);
        let mut selected = Vec::new();
        let mut dynamic = false;
        for (handle, bounds) in &self.entries {
            let e = d.model.get(*handle)?;
            if !d.is_visible(e) {
                continue;
            }
            if !crate::select::is_infinite(e) && !bounds.expand(aperture).intersects(&probe) {
                continue;
            }
            if selected.len() == 2000 {
                break;
            }
            if matches!(e.kind, EntityKind::Insert(_) | EntityKind::Dimension(_)) {
                dynamic = true;
            }
            selected.push(*handle);
        }
        if !dynamic
            && selected == self.selected
            && let Some(gathered) = &self.gathered
        {
            return Some(gathered.clone());
        }
        self.tick = self.tick.wrapping_add(1);
        let mut cands = Vec::new();
        let mut prims = Vec::new();
        let mut budget = super::MAX_BLOCK_LEAVES;
        let mut sink = super::Sink { osmode: mode, aperture, probe, cands: &mut cands, near: &mut prims, recording: None };
        for handle in &selected {
            let e = d.model.get(*handle)?;
            let complex = matches!(e.kind, EntityKind::Insert(_) | EntityKind::Dimension(_));
            if complex && !self.prepared.contains_key(handle) && !self.oversized.contains(handle) {
                let limit = self.limit / 2;
                if let Some(mut prepared) = Prepared::build(d, e, aperture, mode, limit) {
                    while !self.prepared.is_empty() && (self.prepared.len() >= 1024 || self.prepared_bytes.saturating_add(prepared.bytes) > limit) {
                        let oldest = self.prepared.iter().min_by_key(|(_, p)| p.used).map(|(h, _)| *h);
                        if let Some(old) = oldest.and_then(|h| self.prepared.remove(&h)) {
                            self.prepared_bytes = self.prepared_bytes.saturating_sub(old.bytes);
                        }
                    }
                    prepared.used = self.tick;
                    self.prepared_bytes = self.prepared_bytes.saturating_add(prepared.bytes);
                    self.prepared.insert(*handle, prepared);
                } else if self.oversized.len() < 1024 {
                    // Do not repeat an oversized compilation before every exact fallback.
                    self.oversized.insert(*handle);
                }
            }
            if let Some(prepared) = self.prepared.get_mut(handle) {
                prepared.used = self.tick;
                if prepared.append(&mut sink, &mut budget) {
                    continue;
                }
            }
            super::gather_entity(d, e, &mut sink, &mut budget, &[]);
        }
        let segments =
            if mode & (super::mode::INT | super::mode::APP) != 0 { prims.iter().take(200).map(super::prim_segments).collect() } else { Vec::new() };
        let gathered = Arc::new(Gathered { cands, prims, segments });
        let bytes = self
            .entries
            .capacity()
            .saturating_mul(std::mem::size_of::<(Handle, Bounds2)>())
            .saturating_add(gathered.cands.capacity().saturating_mul(std::mem::size_of::<(u32, Vec2)>()))
            .saturating_add(gathered.prims.iter().map(prim_bytes).sum::<usize>())
            .saturating_add(gathered.segments.iter().map(|s| s.capacity().saturating_mul(std::mem::size_of::<Segment>())).sum::<usize>());
        self.selected = selected;
        self.gathered = (!dynamic && bytes.saturating_add(self.prepared_bytes) <= self.limit).then(|| gathered.clone());
        Some(gathered)
    }
}

fn prim_bytes(p: &Prim) -> usize {
    let heap = match p {
        Prim::Fill(v) => v.capacity().saturating_mul(std::mem::size_of::<Vec2>()),
        Prim::Spline(s) => (s.control.capacity() + s.fit.capacity())
            .saturating_mul(std::mem::size_of::<Vec2>())
            .saturating_add((s.knots.capacity() + s.weights.capacity()).saturating_mul(std::mem::size_of::<f64>())),
        _ => 0,
    };
    std::mem::size_of::<Prim>().saturating_add(heap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Session, snap::mode};
    use serde_json::json;

    fn drawing() -> Session {
        let mut s = Session::new();
        let l = s.execute("line", &json!({"points": [[0,0],[20,0],[20,8]]})).unwrap();
        let c = s.execute("circle", &json!({"center": [10,4], "radius": 2})).unwrap();
        s.execute("block", &json!({"name":"B", "base":[0,0], "handles":[l["handles"][0],l["handles"][1],c["handle"]], "keep":"delete"})).unwrap();
        s.execute("insert", &json!({"name":"B", "at":[0,0]})).unwrap();
        s.cmdline("dimlinear 0,0 20,0 10,3").unwrap();
        s
    }

    fn same(cache: &mut Cache, d: &Drawing, at: Vec2, mode: u32) {
        let ix = crate::spatial::index(d, &Space::Model);
        let (candidates, primitives) = super::super::gather(d, &Space::Model, &ix, at, 0.7, mode).unwrap();
        let actual = cache.nearby(d, &Space::Model, &ix, at, 0.7, mode).unwrap();
        assert_eq!(actual.cands, candidates, "candidates at {at:?}");
        assert_eq!(actual.prims, primitives, "primitives at {at:?}");
        for base in [None, Some(Vec2::new(7.0, 12.0))] {
            let expected = super::super::resolve_neighborhood(candidates.clone(), &primitives, None, at, 0.7, mode, base, true, None);
            let hit =
                super::super::resolve_neighborhood(actual.cands.clone(), &actual.prims, Some(&actual.segments), at, 0.7, mode, base, true, None);
            assert_eq!(hit, expected, "resolved snap at {at:?}");
        }
    }

    #[test]
    fn compiled_blocks_and_dimensions_reuse_geometry_with_exact_leaf_pruning() {
        let s = drawing();
        let mut cache = Cache::default();
        cache.set_budget(32 << 20);
        for modes in [mode::END | mode::MID | mode::INS, mode::CEN | mode::QUA | mode::TAN, mode::NOD | mode::PER | mode::NEA, mode::INT | mode::APP]
        {
            for at in [Vec2::new(0.1, 0.1), Vec2::new(10.1, 3.1), Vec2::new(20.1, 0.1), Vec2::new(20.1, 7.9), Vec2::new(10.1, 6.1)] {
                same(&mut cache, s.doc().unwrap(), at, modes);
            }
        }
        assert!(cache.prepared.len() >= 2);
        let identities: Vec<_> = cache.prepared.iter().map(|(h, p)| (*h, p.prims.as_ptr())).collect();
        same(&mut cache, s.doc().unwrap(), Vec2::new(10.2, 6.0), mode::INT | mode::APP);
        for (h, ptr) in identities {
            assert_eq!(cache.prepared[&h].prims.as_ptr(), ptr);
        }
    }

    #[test]
    fn cached_dimension_blocks_edits_and_pressure_match_uncached_snaps() {
        let mut s = drawing();
        let dim = s.doc().unwrap().model.iter().find(|e| matches!(e.kind, EntityKind::Dimension(_))).unwrap().handle;
        s.doc_mut().unwrap().model.modify(dim, |e| {
            if let EntityKind::Dimension(dm) = &mut e.kind {
                dm.block = Some("B".into());
            }
        });
        let mut cache = Cache::default();
        for at in [Vec2::new(0.1, 0.1), Vec2::new(20.1, 7.9)] {
            same(&mut cache, s.doc().unwrap(), at, mode::END | mode::NOD | mode::INT);
        }
        s.doc_mut().unwrap().model.modify(dim, |e| {
            if let EntityKind::Dimension(dm) = &mut e.kind {
                dm.block = None;
                dm.defpt.y = 6.0;
            }
        });
        same(&mut cache, s.doc().unwrap(), Vec2::new(20.1, 6.0), mode::END | mode::NOD | mode::INT);
        cache.set_budget(0);
        assert!(cache.prepared.is_empty() && cache.gathered.is_none());
        cache.set_budget(32 << 20);
        same(&mut cache, s.doc().unwrap(), Vec2::new(20.1, 6.0), mode::END | mode::NOD | mode::INT);
        assert!(cache.prepared_bytes <= cache.limit / 2);
    }

    #[test]
    fn compiled_arrays_preserve_the_shared_nested_traversal_budget() {
        let mut s = drawing();
        let inserts: Vec<_> = s.doc().unwrap().model.iter().filter(|e| matches!(e.kind, EntityKind::Insert(_))).map(|e| e.handle).collect();
        for h in inserts {
            s.doc_mut().unwrap().model.modify(h, |e| {
                if let EntityKind::Insert(i) = &mut e.kind {
                    i.cols = 10_000;
                    i.rows = 2;
                    i.col_spacing = 0.001;
                }
            });
        }
        s.execute("insert", &json!({"name":"B", "at":[0,0]})).unwrap();
        let mut cache = Cache::default();
        cache.set_budget(32 << 20);
        for at in [Vec2::new(0.1, 0.1), Vec2::new(0.2, 0.1)] {
            same(&mut cache, s.doc().unwrap(), at, mode::END);
        }
    }
}
