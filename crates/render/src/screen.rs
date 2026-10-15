//! Bounded model-space tessellation cache for an interactive canvas.
//!
//! Copy-on-write entity chunks identify unchanged geometry. Plots and layouts continue to use
//! the full builder; a screen list preserves entity order, including wipeout barriers.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use cadcraft_doc::{Drawing, Entity, EntityKind, Space};
use cadcraft_geom::Mat3;

use crate::{Builder, DisplayList, MAX_BLOCK_EXPANSION, Options, entity, top_ctx};

// Additional geometry retained for reuse, apart from the list needed to draw the scene.
const MAX_BYTES: usize = 32 << 20;
static NEXT_PART: AtomicU64 = AtomicU64::new(1);

/// Font preferences are part of the canvas and text geometry cache keys.
pub fn font_revision() -> u64 {
    cadcraft_fonts::ttf::revision()
}

#[derive(Clone)]
pub struct Part {
    /// Identity of the tessellation, independent of its position in drawing order.
    pub id: u64,
    pub list: Arc<DisplayList>,
}

pub struct Built {
    pub list: DisplayList,
    pub parts: Vec<Part>,
    pub rebuilt: usize,
    pub reused: usize,
}

#[derive(Clone)]
pub struct Cache {
    // Retaining the snapshot prevents a freed chunk address from being reused as a cache key.
    drawing: Option<Arc<Drawing>>,
    options: Option<Options>,
    font_revision: u64,
    chunks: HashMap<usize, Part>,
    limit: usize,
}

impl Default for Cache {
    fn default() -> Self {
        Self { drawing: None, options: None, font_revision: 0, chunks: HashMap::new(), limit: MAX_BYTES }
    }
}

impl Cache {
    pub fn bytes(&self) -> usize {
        self.chunks.values().map(|p| bytes(&p.list)).sum()
    }

    /// Retain as many individual chunks as fit; the active drawing is never truncated.
    pub fn set_budget(&mut self, bytes: usize) {
        self.limit = bytes;
        let mut used = 0usize;
        self.chunks.retain(|_, part| {
            let size = crate::screen::bytes(&part.list);
            if size > self.limit.saturating_sub(used) {
                return false;
            }
            used = used.saturating_add(size);
            true
        });
        if self.chunks.is_empty() {
            self.drawing = None;
        }
    }

    pub fn build(&mut self, drawing: &Arc<Drawing>, space: &Space, options: &Options) -> Built {
        let fallback = |this: &mut Self| {
            let limit = this.limit;
            *this = Self { limit, ..Self::default() };
            Built { list: crate::build(drawing, space, options), parts: Vec::new(), rebuilt: 0, reused: 0 }
        };
        if *space != Space::Model || options.plot_style_table.is_some() {
            return fallback(self);
        }
        let options = crate::with_point_view(drawing, space, options);
        let font_revision = cadcraft_fonts::ttf::revision();
        if self.font_revision != font_revision
            || self.options.as_ref().is_none_or(|o| !same_options(o, &options))
            || self.drawing.as_ref().is_none_or(|d| !d.same_render_context(drawing))
        {
            self.chunks.clear();
        }
        let chunks: Vec<_> = drawing.model.chunk_slices().collect();
        let used: usize = chunks.iter().filter_map(|(k, _)| self.chunks.get(k)).map(|p| p.list.expanded).sum();
        // A cached part must not give a new part a fresh block-expansion allowance.
        if used >= MAX_BLOCK_EXPANSION {
            return fallback(self);
        }
        let budget = AtomicUsize::new(used);
        let missing: Vec<_> = chunks.iter().filter(|(k, _)| !self.chunks.contains_key(k)).collect();
        let make = |&&(key, ents): &&(usize, &[Arc<Entity>])| {
            let mut b = Builder {
                list: DisplayList::default(),
                opts: &options,
                plotting: false,
                styles: None,
                named: None,
                shared_budget: Some(&budget),
                transparency: 0,
            };
            for e in ents {
                if visible(drawing, e, &options) {
                    entity(&mut b, &top_ctx(drawing, Mat3::IDENTITY, e.handle, &[], &[]), e);
                }
            }
            (key, Part { id: NEXT_PART.fetch_add(1, Ordering::Relaxed), list: Arc::new(b.list) })
        };
        #[cfg(not(target_arch = "wasm32"))]
        let made: Vec<_> = {
            use rayon::prelude::*;
            missing.par_iter().map(make).collect()
        };
        #[cfg(target_arch = "wasm32")]
        let made: Vec<_> = missing.iter().map(make).collect();
        if budget.load(Ordering::Relaxed) >= MAX_BLOCK_EXPANSION {
            drop(made);
            return fallback(self);
        }
        let rebuilt = made.len();
        let mut made: HashMap<_, _> = made.into_iter().collect();
        let lists: Vec<_> = chunks.iter().filter_map(|(key, _)| made.get(key).or_else(|| self.chunks.get(key))).map(|p| &p.list).collect();
        if lists.len() != chunks.len() {
            return fallback(self);
        }
        let complete = lists.iter().map(|list| bytes(list)).sum::<usize>() <= self.limit;
        let mut list = DisplayList::default();
        list.prims.reserve(lists.iter().map(|p| p.prims.len()).sum());
        list.verts.reserve(lists.iter().map(|p| p.verts.len()).sum());
        list.tris.reserve(lists.iter().map(|p| p.tris.len()).sum());
        drop(lists);
        let mut next = HashMap::new();
        let mut parts = Vec::new();
        let mut retained = 0usize;
        for (key, _) in chunks {
            let Some(part) = made.remove(&key).or_else(|| self.chunks.remove(&key)) else { return fallback(self) };
            append(&mut list, &part.list);
            if complete {
                parts.push(part.clone());
            }
            let size = bytes(&part.list);
            if size <= self.limit.saturating_sub(retained) {
                retained = retained.saturating_add(size);
                next.insert(key, part);
            }
        }
        self.chunks = next;
        self.options = Some(options);
        self.font_revision = font_revision;
        self.drawing = Some(Arc::new((**drawing).clone()));
        Built { reused: drawing.model.chunk_slices().count().saturating_sub(rebuilt), rebuilt, list, parts }
    }
}

fn same_options(a: &Options, b: &Options) -> bool {
    a.tolerance.to_bits() == b.tolerance.to_bits()
        && a.min_dash.to_bits() == b.min_dash.to_bits()
        && a.text == b.text
        && a.fill == b.fill
        && a.transparency == b.transparency
        && a.skip_viewport == b.skip_viewport
        && a.lineweights == b.lineweights
        && a.view_height.to_bits() == b.view_height.to_bits()
        && a.view == b.view
}

/// Only cull entities with conservative geometric bounds. Text, blocks, wide polylines and
/// annotation stay in the list, as do masks: even an off-screen wipeout is a draw-order barrier.
fn visible(d: &Drawing, e: &Entity, o: &Options) -> bool {
    let Some(view) = o.view.filter(|v| !v.is_empty() && v.min.is_finite() && v.max.is_finite()) else { return true };
    if cadcraft_doc::is_annotative(e) {
        return true;
    }
    match &e.kind {
        EntityKind::Line(_) | EntityKind::Circle(_) | EntityKind::Arc(_) | EntityKind::Solid(_) => {}
        EntityKind::LwPolyline(p) if p.const_width == 0.0 && p.vertices.iter().all(|v| v.start_width == 0.0 && v.end_width == 0.0) => {}
        _ => return true,
    }
    let ctx = top_ctx(d, Mat3::IDENTITY, e.handle, &[], &[]);
    let (_, lw, lt, _, _, _) = crate::resolve(&ctx, e, false);
    if lt.is_some_and(|l| l.pattern.iter().any(|p| p.text.is_some() || p.shape.is_some())) {
        return true;
    }
    let bounds = cadcraft_doc::entity_bounds(d, e, 0);
    // Tolerance is half a pixel; include both lineweight implementations and rounding at large
    // coordinates. Invalid bounds/widths always take the full rendering path.
    let numerical = bounds.min.x.abs().max(bounds.max.x.abs()).max(bounds.min.y.abs()).max(bounds.max.y.abs()) * f64::EPSILON * 128.0;
    let margin = o.tolerance.abs() * (f64::from(lw.max(0.0)) * 8.0 + 16.0) + numerical;
    bounds.is_empty() || !bounds.min.is_finite() || !bounds.max.is_finite() || !margin.is_finite() || bounds.expand(margin).intersects(&view)
}

pub fn bytes(list: &DisplayList) -> usize {
    list.prims
        .capacity()
        .saturating_mul(std::mem::size_of::<crate::DPrim>())
        .saturating_add(list.verts.capacity().saturating_mul(std::mem::size_of::<cadcraft_geom::Vec2>()))
        .saturating_add(list.tris.capacity().saturating_mul(std::mem::size_of::<cadcraft_geom::Vec2>()))
}

fn append(dst: &mut DisplayList, src: &DisplayList) {
    let (verts, tris) = (dst.verts.len() as u32, dst.tris.len() as u32);
    dst.prims.extend(src.prims.iter().copied().map(|mut p| {
        p.start += if matches!(p.kind, crate::Kind::Tris | crate::Kind::Mask) { tris } else { verts };
        p
    }));
    dst.verts.extend_from_slice(&src.verts);
    dst.tris.extend_from_slice(&src.tris);
    dst.bounds = dst.bounds.union(&src.bounds);
    dst.expanded = dst.expanded.saturating_add(src.expanded);
}
