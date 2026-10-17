//! Bounded templates for repeated, finite block geometry, with resolved styles and placements.
use super::{Builder, DisplayList, Options};
use cadcraft_doc::{Drawing, Entity, EntityKind, Insert};
use cadcraft_geom::{Bounds2, Mat3};
use std::sync::Arc;

#[derive(Default)]
pub(super) struct Templates {
    entries: Vec<(cadcraft_doc::Common, Insert, u64, Arc<DisplayList>)>,
    pub bytes: usize,
    limit: usize,
    eligible: std::collections::HashMap<String, Option<bool>>,
}

impl Templates {
    pub fn set_budget(&mut self, bytes: usize) {
        self.limit = bytes.min(8 << 20);
        while self.bytes > self.limit {
            let Some((_, _, _, list)) = self.entries.pop() else { break };
            self.bytes = self.bytes.saturating_sub(super::bytes(&list));
        }
    }

    fn get(&mut self, d: &Drawing, e: &Entity, options: &Options) -> Option<(Arc<DisplayList>, Mat3)> {
        let EntityKind::Insert(ins) = &e.kind else { return None };
        if self.limit == 0 || cadcraft_doc::is_annotative(e) || !e.common.xdata.is_empty() || !single(ins) || !ins.insert.xy().is_finite() {
            return None;
        }
        let mut local = ins.clone();
        local.insert.x = 0.0;
        local.insert.y = 0.0;
        let classify = || {
            let mut budget = 4096;
            eligible(d, &ins.block, 0, &mut budget).then(|| affine_block(d, &ins.block, 0))
        };
        let affine_block = if let Some(known) = self.eligible.get(&ins.block) {
            *known
        } else {
            let known = classify();
            if self.eligible.len() < 256 {
                self.eligible.insert(ins.block.clone(), known);
            }
            known
        }?;
        // Continuous planar geometry can share a template across rotations, reflections
        // and unequal scales. Patterned linetypes keep the baked-transform path: dash lengths
        // have world-space semantics and must not change under GPU placement.
        let affine = continuous(d, &e.common) && affine_block;
        let transform = if affine {
            local.scale.x = 1.0;
            local.scale.y = 1.0;
            local.rotation = 0.0;
            Mat3::translate(ins.insert.xy()).then_before(Mat3::rotate(ins.rotation)).then_before(Mat3::scale(ins.scale.x, ins.scale.y))
        } else {
            Mat3::translate(ins.insert.xy())
        };
        let stretch = if affine { ins.scale.x.abs().max(ins.scale.y.abs()).max(1e-12) } else { 1.0 };
        let stretch = 2f64.powf(stretch.log2().ceil());
        let tolerance = options.tolerance / stretch;
        let band = tolerance.to_bits();
        if let Some((_, _, _, list)) = self.entries.iter().find(|(common, key, tol, _)| *common == e.common && *key == local && *tol == band) {
            return Some((list.clone(), transform));
        }
        let template = Entity { handle: e.handle, common: e.common.clone(), kind: EntityKind::Insert(local.clone()) };
        // Eligible leaves have no view-dependent geometry or draw-order masks.
        let options = Options { tolerance, min_dash: options.min_dash / stretch, view: None, ..options.clone() };
        let list = Arc::new(crate::build_entities(d, [&template], &options));
        if options.cancelled() {
            return None;
        }
        let bytes = super::bytes(&list);
        if self.entries.len() < 128 && bytes <= self.limit.saturating_sub(self.bytes) {
            self.bytes = self.bytes.saturating_add(bytes);
            self.entries.push((e.common.clone(), local, band, list.clone()));
        }
        Some((list, transform))
    }
}

fn single(i: &Insert) -> bool {
    i.cols <= 1 && i.rows <= 1 && i.attribs.is_empty() && i.scale.x.is_finite() && i.scale.y.is_finite() && i.rotation.is_finite()
}

fn eligible(d: &Drawing, name: &str, depth: usize, budget: &mut usize) -> bool {
    if depth >= cadcraft_doc::MAX_BLOCK_DEPTH {
        return false;
    }
    let Some(block) = d.block(name) else { return false };
    for e in block.entities.iter() {
        if *budget == 0 || cadcraft_doc::is_annotative(e) {
            return false;
        }
        *budget -= 1;
        match &e.kind {
            EntityKind::Line(_)
            | EntityKind::Circle(_)
            | EntityKind::Arc(_)
            | EntityKind::Ellipse(_)
            | EntityKind::Spline(_)
            | EntityKind::LwPolyline(_)
            | EntityKind::Solid(_)
            | EntityKind::Trace(_)
            | EntityKind::Text(_)
            | EntityKind::MText(_)
            | EntityKind::AttDef(_) => {}
            EntityKind::Insert(i) if single(i) && eligible(d, &i.block, depth + 1, budget) => {}
            _ => return false,
        }
    }
    true
}

/// True when handled (including a fully offscreen instance); false requests the full builder.
pub(super) fn draw(
    templates: &std::sync::Mutex<Templates>,
    b: &mut Builder<'_>,
    d: &Drawing,
    e: &Entity,
    instances: &mut Vec<super::BlockInstance>,
) -> bool {
    let EntityKind::Insert(_) = &e.kind else { return false };
    if d.is_hidden_object(e.handle) || b.opts.cancelled() {
        return true;
    }
    let Some((list, transform)) = templates.lock().ok().and_then(|mut cache| cache.get(d, e, b.opts)) else { return false };
    if !b.expand_by(list.expanded) {
        return false;
    }
    let bounds = if list.bounds.is_empty() { Bounds2::EMPTY } else { Bounds2::from_points(list.bounds.corners().map(|p| transform.apply(p))) };
    let lw = list.prims.iter().map(|p| f64::from(p.lw.max(0.0))).fold(0.0, f64::max);
    let magnitude = bounds.min.x.abs().max(bounds.max.x.abs()).max(bounds.min.y.abs()).max(bounds.max.y.abs());
    let margin = b.opts.tolerance.abs() * (lw * 8.0 + 16.0) + magnitude * f64::EPSILON * 128.0;
    if let Some(view) = b.opts.view.filter(|v| !v.is_empty() && v.min.is_finite() && v.max.is_finite())
        && !bounds.is_empty()
        && bounds.min.is_finite()
        && bounds.max.is_finite()
        && margin.is_finite()
        && !bounds.expand(margin).intersects(&view)
    {
        return true;
    }
    let first = b.list.prims.len();
    let (verts, tris) = (b.list.verts.len() as u32, b.list.tris.len() as u32);
    b.list.prims.extend(list.prims.iter().copied().map(|mut p| {
        p.handle = e.handle;
        p.start += if matches!(p.kind, crate::Kind::Tris | crate::Kind::Mask) { tris } else { verts };
        p
    }));
    b.list.verts.extend(list.verts.iter().map(|p| transform.apply(*p)));
    b.list.tris.extend(list.tris.iter().map(|p| transform.apply(*p)));
    b.list.bounds = b.list.bounds.union(&bounds);
    instances.push(super::BlockInstance { template: list, transform, prims: first..b.list.prims.len(), bounds, tolerance: b.opts.tolerance });
    true
}

fn continuous(d: &Drawing, common: &cadcraft_doc::Common) -> bool {
    let lt = common.linetype.as_str();
    lt.eq_ignore_ascii_case("Continuous")
        || lt.eq_ignore_ascii_case("ByBlock")
        || (lt.eq_ignore_ascii_case("ByLayer") && d.layers.iter().all(|l| l.linetype.eq_ignore_ascii_case("Continuous")))
}

fn affine_block(d: &Drawing, name: &str, depth: usize) -> bool {
    if depth >= cadcraft_doc::MAX_BLOCK_DEPTH {
        return false;
    }
    let Some(block) = d.block(name) else { return false };
    block.entities.iter().all(|e| {
        continuous(d, &e.common)
            && match &e.kind {
                EntityKind::Insert(i) => affine_block(d, &i.block, depth + 1),
                _ => true,
            }
    })
}
