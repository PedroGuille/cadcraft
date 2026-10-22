//! Exact, handle-independent identity for resolved screen templates. Never changes the CAD DB.
use std::hash::{Hash, Hasher};

use crate::{DisplayList, Kind};

pub(super) fn fingerprint(list: &DisplayList) -> u64 {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    list.expanded.hash(&mut hash);
    for p in &list.prims {
        std::mem::discriminant(&p.kind).hash(&mut hash);
        if let Kind::Infinite { ray } = p.kind {
            ray.hash(&mut hash);
        }
        (p.color.0, p.color.1, p.color.2, p.aci7, p.lw.to_bits(), p.transparency, p.start, p.len).hash(&mut hash);
    }
    for p in list.verts.iter().chain(&list.tris) {
        // Equality treats signed zero as equal, so its hash must too.
        (bits(p.x), bits(p.y)).hash(&mut hash);
    }
    for h in &list.analytic {
        h.prim.hash(&mut hash);
        for v in [h.center.x, h.center.y, h.radius, h.start, h.sweep, h.tolerance] {
            bits(v).hash(&mut hash);
        }
    }
    hash.finish()
}

fn bits(v: f64) -> u64 {
    if v == 0.0 { 0 } else { v.to_bits() }
}

pub(super) fn equivalent(a: &DisplayList, b: &DisplayList) -> bool {
    a.expanded == b.expanded
        && a.bounds == b.bounds
        && a.verts == b.verts
        && a.tris == b.tris
        && a.prims.len() == b.prims.len()
        && a.prims.iter().zip(&b.prims).all(|(a, b)| crate::DPrim { handle: b.handle, ..*a } == *b)
        && a.analytic.len() == b.analytic.len()
        && a.analytic.iter().zip(&b.analytic).all(|(a, b)| {
            a.prim == b.prim && a.center == b.center && a.radius == b.radius && a.start == b.start && a.sweep == b.sweep && a.tolerance == b.tolerance
        })
        // Screen templates never contain plot data. Keep future additions on the safe path.
        && a.prim_layers.is_empty() && b.prim_layers.is_empty()
        && a.texts.is_empty() && b.texts.is_empty()
        && a.pens.is_empty() && b.pens.is_empty()
        && a.sheet.is_none() && b.sheet.is_none()
}
