//! Bounded templates for repeated, finite block geometry. Templates include resolved styles
//! and the linear transform; only the reference translation and selection handle vary.
use super::{Builder, DisplayList, Options};
use cadcraft_doc::{Drawing, Entity, EntityKind, Insert};
use cadcraft_geom::Bounds2;
use std::sync::Arc;

#[derive(Default)]
pub(super) struct Templates {
    entries: Vec<(cadcraft_doc::Common, Insert, Arc<DisplayList>)>,
    pub bytes: usize,
    limit: usize,
}

impl Templates {
    pub fn set_budget(&mut self, bytes: usize) {
        self.limit = bytes.min(8 << 20);
        while self.bytes > self.limit {
            let Some((_, _, list)) = self.entries.pop() else { break };
            self.bytes = self.bytes.saturating_sub(super::bytes(&list));
        }
    }

    fn get(&mut self, d: &Drawing, e: &Entity, options: &Options) -> Option<Arc<DisplayList>> {
        let EntityKind::Insert(ins) = &e.kind else { return None };
        if self.limit == 0 || cadcraft_doc::is_annotative(e) || !e.common.xdata.is_empty() || !single(ins) || !ins.insert.xy().is_finite() {
            return None;
        }
        let mut local = ins.clone();
        local.insert.x = 0.0;
        local.insert.y = 0.0;
        if let Some((_, _, list)) = self.entries.iter().find(|(common, key, _)| *common == e.common && *key == local) {
            return Some(list.clone());
        }
        let mut budget = 4096;
        if !eligible(d, &ins.block, 0, &mut budget) {
            return None;
        }
        let template = Entity { handle: e.handle, common: e.common.clone(), kind: EntityKind::Insert(local.clone()) };
        // Eligible leaves have no view-dependent geometry or draw-order masks.
        let options = Options { view: None, ..options.clone() };
        let list = Arc::new(crate::build_entities(d, [&template], &options));
        if options.cancelled() {
            return None;
        }
        let bytes = super::bytes(&list);
        if self.entries.len() < 128 && bytes <= self.limit.saturating_sub(self.bytes) {
            self.bytes = self.bytes.saturating_add(bytes);
            self.entries.push((e.common.clone(), local, list.clone()));
        }
        Some(list)
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
pub(super) fn draw(templates: &std::sync::Mutex<Templates>, b: &mut Builder<'_>, d: &Drawing, e: &Entity) -> bool {
    let EntityKind::Insert(ins) = &e.kind else { return false };
    if d.is_hidden_object(e.handle) || b.opts.cancelled() {
        return true;
    }
    let Some(list) = templates.lock().ok().and_then(|mut cache| cache.get(d, e, b.opts)) else { return false };
    if !b.expand_by(list.expanded) {
        return false;
    }
    let offset = ins.insert.xy();
    let bounds = Bounds2 { min: list.bounds.min + offset, max: list.bounds.max + offset };
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
    let (verts, tris) = (b.list.verts.len() as u32, b.list.tris.len() as u32);
    b.list.prims.extend(list.prims.iter().copied().map(|mut p| {
        p.handle = e.handle;
        p.start += if matches!(p.kind, crate::Kind::Tris | crate::Kind::Mask) { tris } else { verts };
        p
    }));
    b.list.verts.extend(list.verts.iter().map(|p| *p + offset));
    b.list.tris.extend(list.tris.iter().map(|p| *p + offset));
    b.list.bounds = b.list.bounds.union(&bounds);
    true
}
