//! A small, session-local neighborhood cache. Exact cursor/base-dependent snaps are recomputed
//! on every query, and tracking's hover timer always runs. Block/dimension leaf pruning remains
//! on the uncached path because it depends on the exact cursor position.
use cadcraft_doc::{Drawing, EntityKind, Handle, Prim, Space};
use cadcraft_geom::{Bounds2, Segment, Vec2};
use std::sync::Arc;

pub(super) struct Gathered {
    pub cands: Vec<(u32, Vec2)>,
    pub prims: Vec<Prim>,
    pub segments: Vec<Vec<Segment>>,
}

pub(crate) struct Cache {
    source: Option<Arc<Drawing>>,
    aperture: u64,
    mode: u32,
    region: Bounds2,
    entries: Vec<(Handle, Bounds2)>,
    selected: Vec<Handle>,
    gathered: Option<Arc<Gathered>>,
    limit: usize,
}

impl Default for Cache {
    fn default() -> Self {
        Self { source: None, aperture: 0, mode: 0, region: Bounds2::EMPTY, entries: Vec::new(), selected: Vec::new(), gathered: None, limit: 2 << 20 }
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
        self.limit = bytes.min(2 << 20);
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
        let valid = self.aperture == aperture.to_bits()
            && self.mode == mode
            && self.source.as_ref().is_some_and(|old| old.model.same_as(&d.model) && old.same_render_context(d));
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
            self.mode = mode;
        }
        let probe = Bounds2::new(cursor, cursor).expand(aperture);
        let mut selected = Vec::new();
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
                return None;
            }
            selected.push(*handle);
        }
        if selected == self.selected
            && let Some(gathered) = &self.gathered
        {
            return Some(gathered.clone());
        }
        let (cands, prims) = super::gather_in(d, space, ix, cursor, aperture, mode, &[])?;
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
        self.gathered = (bytes <= self.limit).then(|| gathered.clone());
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
