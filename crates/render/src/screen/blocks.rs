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
    weak: Vec<(cadcraft_doc::Common, Insert, u64, std::sync::Weak<DisplayList>)>,
}

impl Templates {
    pub fn set_budget(&mut self, bytes: usize) {
        self.limit = bytes.min(128 << 20);
        while self.bytes > self.limit {
            let Some((_, _, _, list)) = self.entries.pop() else { break };
            self.bytes = self.bytes.saturating_sub(super::bytes(&list));
        }
    }

    fn get(&mut self, d: &Drawing, e: &Entity, options: &Options) -> Option<(Arc<DisplayList>, Mat3)> {
        let EntityKind::Insert(ins) = &e.kind else { return None };
        if cadcraft_doc::is_annotative(e) || !e.common.xdata.is_empty() || !finite(ins) || !ins.insert.xy().is_finite() {
            return None;
        }
        let mut local = ins.clone();
        local.insert.x = 0.0;
        local.insert.y = 0.0;
        local.attribs.clear();
        local.cols = 1;
        local.rows = 1;
        local.col_spacing = 0.0;
        local.row_spacing = 0.0;
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
        self.weak.retain(|(_, _, _, list)| list.strong_count() > 0);
        if let Some(list) = self
            .weak
            .iter()
            .find_map(|(common, key, tol, list)| (*common == e.common && *key == local && *tol == band).then(|| list.upgrade()).flatten())
        {
            return Some((list, transform));
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
            self.entries.push((e.common.clone(), local.clone(), band, list.clone()));
        }
        if self.weak.len() < 128 {
            self.weak.push((e.common.clone(), local, band, Arc::downgrade(&list)));
        }
        Some((list, transform))
    }
}

fn single(i: &Insert) -> bool {
    i.cols <= 1 && i.rows <= 1 && i.attribs.is_empty() && finite(i)
}

fn finite(i: &Insert) -> bool {
    i.scale.x.is_finite() && i.scale.y.is_finite() && i.rotation.is_finite() && i.row_spacing.is_finite() && i.col_spacing.is_finite()
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
    let ctx = crate::top_ctx(d, Mat3::IDENTITY, e.handle, &[], &[]);
    let (rgb, _, _, _, shown, layer_t) = crate::resolve(&ctx, e, false);
    if !shown {
        return true;
    }
    let Some((list, transform)) = templates.lock().ok().and_then(|mut cache| cache.get(d, e, b.opts)) else { return false };
    'copies: for row in 0..ins.rows.clamp(1, 10_000) {
        for col in 0..ins.cols.clamp(1, 10_000) {
            if b.opts.cancelled() || !b.expand_by(list.expanded) {
                break 'copies;
            }
            // MINSERT spacing rotates with the reference, but is not multiplied by its scale.
            let offset = cadcraft_geom::Vec2::new(ins.col_spacing * f64::from(col), ins.row_spacing * f64::from(row)).rotate(ins.rotation);
            let xf = Mat3::translate(offset).then_before(transform);
            let bounds = if list.bounds.is_empty() { Bounds2::EMPTY } else { Bounds2::from_points(list.bounds.corners().map(|p| xf.apply(p))) };
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
                continue;
            }
            b.instance(list.clone(), xf, e.handle, None);
        }
    }
    // Attributes are stored in world coordinates and follow the complete array body, exactly
    // as on the full renderer. Their individual visibility, styles and transparency still resolve.
    let saved = b.transparency;
    b.transparency = if b.opts.transparency { crate::transparency(e.common.transparency, layer_t, 0) } else { 0 };
    crate::attributes(b, &ctx, e, ins, rgb);
    b.transparency = saved;
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
