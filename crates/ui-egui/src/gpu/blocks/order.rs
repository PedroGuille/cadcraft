//! Bounded local batching. Only disjoint instances may cross; ordinary primitives and glyphs
//! remain barriers, as do touching/unknown bounds. Crossed bodies must also have the same
//! opaque ink: screen-width strokes can overlap after a cached scene is zoomed far out.
use super::*;

pub(super) fn schedule(instances: &[BlockInstance]) -> Vec<usize> {
    let mut order: Vec<_> = (0..instances.len()).collect();
    // Each lookahead window computes a body's conservative bounds once, and a complex
    // template's maximum stroke once, rather than rescanning its vertices for every crossing.
    let mut bounds = std::collections::HashMap::new();
    let mut weights = std::collections::HashMap::new();
    let mut inks = std::collections::HashMap::new();
    let mut at = 0;
    while let Some(base) = order.get(at).and_then(|&i| instances.get(i)) {
        if cancel::cancelled() {
            break;
        }
        if base.glyph {
            at += 1;
            continue;
        }
        let mut end = at + 1;
        let mut candidate = end;
        let stop = instances.len().min(at.saturating_add(64));
        bounds.clear();
        if weights.len() > 256 {
            weights.clear();
        }
        if inks.len() > 256 {
            inks.clear();
        }
        while candidate < stop {
            let Some(next) = order.get(candidate).and_then(|&i| instances.get(i)) else { break };
            if next.glyph || next.at != base.at {
                break;
            }
            if Arc::ptr_eq(&next.template, &base.template) {
                let safe = candidate == end
                    || opaque_ink(next, &mut inks).is_some_and(|ink| {
                        order.get(candidate).and_then(|&i| cached_bounds(instances, i, &mut bounds, &mut weights)).is_some_and(|candidate_bounds| {
                            order.get(end..candidate).is_some_and(|crossed| {
                                crossed.iter().all(|&i| {
                                    instances.get(i).is_some_and(|other| opaque_ink(other, &mut inks) == Some(ink))
                                        && cached_bounds(instances, i, &mut bounds, &mut weights)
                                            .is_some_and(|other| !candidate_bounds.intersects(&other))
                                })
                            })
                        })
                    });
                if safe {
                    if let Some(run) = order.get_mut(end..=candidate) {
                        run.rotate_right(1);
                    }
                    end += 1;
                }
            }
            candidate += 1;
        }
        at = end;
    }
    order
}

fn opaque_ink(instance: &BlockInstance, inks: &mut std::collections::HashMap<usize, Option<(Rgb, bool)>>) -> Option<(Rgb, bool)> {
    *inks.entry(Arc::as_ptr(&instance.template) as usize).or_insert_with(|| {
        let first = instance.template.prims.first()?;
        instance
            .template
            .prims
            .iter()
            .all(|p| p.transparency == 0 && p.color == first.color && p.aci7 == first.aci7 && !matches!(p.kind, Kind::Mask | Kind::Infinite { .. }))
            .then_some((first.color, first.aci7))
    })
}

fn cached_bounds(
    instances: &[BlockInstance],
    index: usize,
    cache: &mut std::collections::HashMap<usize, Option<Bounds2>>,
    weights: &mut std::collections::HashMap<usize, f64>,
) -> Option<Bounds2> {
    *cache.entry(index).or_insert_with(|| padded(instances.get(index)?, weights))
}

fn padded(instance: &BlockInstance, weights: &mut std::collections::HashMap<usize, f64>) -> Option<Bounds2> {
    let bounds = instance.bounds;
    let magnitude = bounds.min.x.abs().max(bounds.min.y.abs()).max(bounds.max.x.abs()).max(bounds.max.y.abs());
    let weight = *weights
        .entry(Arc::as_ptr(&instance.template) as usize)
        .or_insert_with(|| instance.template.prims.iter().map(|p| f64::from(p.lw.max(0.0))).fold(0.0, f64::max));
    let margin = instance.tolerance.abs() * (weight * 8.0 + 32.0) + magnitude * f64::EPSILON * 128.0;
    (!bounds.is_empty() && bounds.min.is_finite() && bounds.max.is_finite() && margin.is_finite() && instance.tolerance > 0.0)
        .then(|| bounds.expand(margin))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cadcraft_doc::{Block, Common, Drawing, Entity, EntityKind, Handle, Insert, Line, Space};
    use cadcraft_geom::Vec3;

    fn instances(overlap: bool) -> Vec<BlockInstance> {
        let mut d = Drawing::new_imperial();
        for index in 0..2 {
            let mut block = Block::new(&format!("symbol{index}"));
            let mut line = Entity::new(Handle(1), EntityKind::Line(Line { a: Vec3::ZERO, b: Vec3::new(4.0 + f64::from(index), 2.0, 0.0) }));
            line.common.color = cadcraft_color::Color::ByBlock;
            block.entities.push(line);
            d.blocks.insert(block.name.clone(), Arc::new(block));
        }
        for index in 0..8 {
            d.add(
                &Space::Model,
                Common { color: cadcraft_color::Color::Index(1), ..Default::default() },
                EntityKind::Insert(Insert {
                    block: format!("symbol{}", index % 2),
                    insert: Vec3::new(if overlap { 0.0 } else { f64::from(index) * 50.0 }, 0.0, 0.0),
                    scale: Vec3::new(1.0, 1.0, 1.0),
                    rotation: 0.0,
                    attribs: vec![],
                    cols: 1,
                    rows: 1,
                    col_spacing: 0.0,
                    row_spacing: 0.0,
                }),
            )
            .unwrap();
        }
        let scene = cadcraft_render::screen::Cache::default().build_scene(&Arc::new(d), &Space::Model, &Default::default()).scene;
        scene.parts.iter().flat_map(|p| p.instances.iter().cloned()).collect()
    }

    #[test]
    fn disjoint_symbols_join_compatible_batches() {
        assert_eq!(schedule(&instances(false)), vec![0, 2, 4, 6, 1, 3, 5, 7]);
    }

    #[test]
    fn overlaps_unknown_bounds_and_intervening_primitives_remain_barriers() {
        assert_eq!(schedule(&instances(true)), (0..8).collect::<Vec<_>>());
        let mut source = instances(false);
        source[2].bounds = Bounds2 { min: Vec2::new(f64::NAN, 0.0), max: Vec2::new(f64::NAN, 0.0) };
        assert_eq!(&schedule(&source)[..2], &[0, 1]);
        let mut source = instances(false);
        for instance in source.iter_mut().skip(2) {
            instance.at += 1;
        }
        assert_eq!(&schedule(&source)[..2], &[0, 1]);
        let mut source = instances(false);
        source[1].glyph = true;
        assert_eq!(&schedule(&source)[..2], &[0, 1]);
    }

    #[test]
    fn different_inks_and_transparency_keep_order_at_every_view_scale() {
        for transparency in [0, 40] {
            let mut source = instances(false);
            let mut changed = (*source[1].template).clone();
            for p in &mut changed.prims {
                p.color = Rgb(0, 255, 0);
                p.transparency = transparency;
            }
            let changed = Arc::new(changed);
            for instance in source.iter_mut().skip(1).step_by(2) {
                instance.template = changed.clone();
            }
            assert_eq!(schedule(&source), (0..8).collect::<Vec<_>>());
        }
    }
}
