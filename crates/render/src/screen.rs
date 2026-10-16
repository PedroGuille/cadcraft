//! Bounded model-space tessellation cache for an interactive canvas.
//!
//! Copy-on-write entity chunks identify unchanged geometry. Plots and layouts continue to use
//! the full builder; a screen list preserves entity order, including wipeout barriers.

mod blocks;

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

/// An interactive scene keeps immutable chunks instead of copying all vertices after an edit.
/// The scene owns its active geometry regardless of the optional retention budget.
#[derive(Clone, Default)]
pub struct Scene {
    pub parts: Vec<Part>,
    pub bounds: cadcraft_geom::Bounds2,
    pub sheet: Option<crate::Sheet>,
    offsets: Vec<usize>,
    count: usize,
}

/// Read primitives without requiring a contiguous display list. Each primitive stays paired
/// with the list owning its point offsets, including for highlights and infinite lines.
pub trait Geometry {
    fn primitive_count(&self) -> usize;
    fn primitives(&self) -> impl Iterator<Item = (&DisplayList, &crate::DPrim)>;
    fn primitive(&self, index: usize) -> Option<(&DisplayList, &crate::DPrim)>;
    fn bounds(&self) -> &cadcraft_geom::Bounds2;
}

impl Geometry for DisplayList {
    fn primitive_count(&self) -> usize {
        self.prims.len()
    }
    fn primitives(&self) -> impl Iterator<Item = (&DisplayList, &crate::DPrim)> {
        self.prims.iter().map(move |p| (self, p))
    }
    fn primitive(&self, index: usize) -> Option<(&DisplayList, &crate::DPrim)> {
        self.prims.get(index).map(|p| (self, p))
    }
    fn bounds(&self) -> &cadcraft_geom::Bounds2 {
        &self.bounds
    }
}

impl Geometry for Scene {
    fn primitive_count(&self) -> usize {
        self.count
    }
    fn primitives(&self) -> impl Iterator<Item = (&DisplayList, &crate::DPrim)> {
        self.parts.iter().flat_map(|part| part.list.prims.iter().map(|p| (part.list.as_ref(), p)))
    }
    fn primitive(&self, index: usize) -> Option<(&DisplayList, &crate::DPrim)> {
        if index >= self.count {
            return None;
        }
        let at = self.offsets.partition_point(|start| *start <= index).checked_sub(1)?;
        let list = self.parts.get(at)?.list.as_ref();
        list.prims.get(index.checked_sub(*self.offsets.get(at)?)?).map(|p| (list, p))
    }
    fn bounds(&self) -> &cadcraft_geom::Bounds2 {
        &self.bounds
    }
}

impl Scene {
    pub fn from_list(list: DisplayList) -> Self {
        Self::from_parts(vec![Part { id: NEXT_PART.fetch_add(1, Ordering::Relaxed), list: Arc::new(list) }])
    }
    fn from_parts(parts: Vec<Part>) -> Self {
        let mut scene = Self { parts, ..Self::default() };
        for part in &scene.parts {
            scene.offsets.push(scene.count);
            scene.count = scene.count.saturating_add(part.list.prims.len());
            scene.bounds = scene.bounds.union(&part.list.bounds);
            scene.sheet = scene.sheet.or(part.list.sheet);
        }
        scene
    }
    pub fn bytes(&self) -> usize {
        self.parts.iter().map(|p| bytes(&p.list)).sum()
    }
    /// Explicit compatibility/export operation; interactive drawing never flattens the scene.
    pub fn flatten(&self) -> DisplayList {
        if let [part] = self.parts.as_slice() {
            return (*part.list).clone();
        }
        let mut list = DisplayList::default();
        list.prims.reserve(self.count);
        list.verts.reserve(self.parts.iter().map(|p| p.list.verts.len()).sum());
        list.tris.reserve(self.parts.iter().map(|p| p.list.tris.len()).sum());
        for part in &self.parts {
            append(&mut list, &part.list);
        }
        list.sheet = self.sheet;
        list
    }
}

pub struct SceneBuilt {
    pub scene: Scene,
    pub rebuilt: usize,
    pub reused: usize,
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
    // Weak references reuse the active scene even when the retention budget is zero.
    active: HashMap<usize, (u64, std::sync::Weak<DisplayList>)>,
    limit: usize,
    templates: Arc<std::sync::Mutex<blocks::Templates>>,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            drawing: None,
            options: None,
            font_revision: 0,
            chunks: HashMap::new(),
            active: HashMap::new(),
            limit: MAX_BYTES,
            templates: Default::default(),
        }
    }
}

impl Cache {
    pub fn bytes(&self) -> usize {
        // A worker may be tessellating a template: memory accounting must not stall the UI.
        let templates = self.templates.try_lock().map_or((self.limit / 4).min(8 << 20), |c| c.bytes);
        self.chunks.values().map(|p| bytes(&p.list)).sum::<usize>().saturating_add(templates)
    }

    /// Retain as many individual chunks as fit; the active drawing is never truncated.
    pub fn set_budget(&mut self, bytes: usize) {
        self.limit = bytes;
        let retained = if let Ok(mut templates) = self.templates.try_lock() {
            templates.set_budget(bytes / 4);
            Some(templates.bytes)
        } else {
            None
        };
        let mut used = retained.unwrap_or_else(|| {
            // Detach from a busy worker instead of waiting on its template build. Its own
            // cache is budgeted again when the finished scene is installed.
            let mut templates = blocks::Templates::default();
            templates.set_budget(bytes / 4);
            self.templates = Arc::new(std::sync::Mutex::new(templates));
            0
        });
        self.chunks.retain(|_, part| {
            let size = crate::screen::bytes(&part.list);
            if size > self.limit.saturating_sub(used) {
                return false;
            }
            used = used.saturating_add(size);
            true
        });
    }

    /// A bounded work estimate for choosing synchronous edits versus the canvas worker.
    pub fn changed_work(&self, drawing: &Drawing, space: &Space, options: &Options) -> usize {
        if *space != Space::Model {
            return 0;
        }
        let options = crate::with_point_view(drawing, space, options);
        let reusable = self.font_revision == font_revision()
            && self.options.as_ref().is_some_and(|o| same_options(o, &options))
            && self.drawing.as_ref().is_some_and(|d| d.same_render_context(drawing));
        drawing
            .model
            .chunk_slices()
            .filter(|(key, _)| !reusable || !(self.chunks.contains_key(key) || self.active.get(key).is_some_and(|(_, p)| p.strong_count() > 0)))
            .flat_map(|(_, ents)| ents)
            .fold(0usize, |work, e| {
                work.saturating_add(match &e.kind {
                    EntityKind::Hatch(_) => 8192,
                    EntityKind::Insert(_) => 512,
                    EntityKind::MText(_) | EntityKind::Dimension(_) => 128,
                    _ => 1,
                })
            })
    }

    pub fn build(&mut self, drawing: &Arc<Drawing>, space: &Space, options: &Options) -> Built {
        let built = self.build_scene(drawing, space, options);
        let list = built.scene.flatten();
        let parts = if built.scene.bytes() <= self.limit && *space == Space::Model && options.plot_style_table.is_none() {
            built.scene.parts
        } else {
            Vec::new()
        };
        Built { list, parts, rebuilt: built.rebuilt, reused: built.reused }
    }

    pub fn build_scene(&mut self, drawing: &Arc<Drawing>, space: &Space, options: &Options) -> SceneBuilt {
        let fallback = |this: &mut Self| {
            let limit = this.limit;
            *this = Self { limit, ..Self::default() };
            SceneBuilt { scene: Scene::from_list(crate::build(drawing, space, options)), rebuilt: 0, reused: 0 }
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
            self.active.clear();
            self.templates = Default::default();
        }
        if let Ok(mut templates) = self.templates.lock() {
            templates.set_budget(self.limit / 4);
        }
        for (&key, (id, weak)) in &self.active {
            if let Some(list) = weak.upgrade() {
                self.chunks.entry(key).or_insert_with(|| Part { id: *id, list });
            }
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
                if visible(drawing, e, &options) && !blocks::draw(&self.templates, &mut b, drawing, e) {
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
        drop(lists);
        let mut next = HashMap::new();
        let mut parts = Vec::new();
        let mut retained = self.templates.lock().map_or(0, |c| c.bytes);
        self.active.clear();
        for (key, _) in chunks {
            let Some(part) = made.remove(&key).or_else(|| self.chunks.remove(&key)) else { return fallback(self) };
            self.active.insert(key, (part.id, Arc::downgrade(&part.list)));
            parts.push(part.clone());
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
        SceneBuilt { reused: drawing.model.chunk_slices().count().saturating_sub(rebuilt), rebuilt, scene: Scene::from_parts(parts) }
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
