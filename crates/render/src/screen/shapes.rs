//! Share exact affine rectangle outlines after the normal builder has resolved detail/styles.
use super::*;
use cadcraft_geom::Vec2;

pub(super) fn share(b: &mut Builder<'_>, e: &Entity, first: usize, templates: &std::sync::Mutex<blocks::Templates>) {
    let EntityKind::LwPolyline(poly) = &e.kind else { return };
    if !poly.closed || poly.vertices.len() != 4 || cadcraft_doc::is_annotative(e) || b.list.prims.len() != first + 1 {
        return;
    }
    let Some(p) = b.list.prims.get(first).copied().filter(|p| p.kind == crate::Kind::Polyline && p.len == 5) else { return };
    let Ok(points): Result<&[Vec2; 5], _> = b.list.points(&p).try_into() else { return };
    let points = *points;
    if points.iter().any(|p| !p.is_finite()) || points[0] != points[4] {
        return;
    }
    let x = points[1] - points[0];
    let y = points[3] - points[0];
    let transform = Mat3 { a: x.x, b: x.y, c: y.x, d: y.y, e: points[0].x, f: points[0].y };
    let unit = [Vec2::ZERO, Vec2::new(1.0, 0.0), Vec2::new(1.0, 1.0), Vec2::new(0.0, 1.0), Vec2::ZERO];
    // Do not introduce even an f64 coordinate change, or substitute the builder's tiny markers.
    if !transform.det().is_finite() || transform.det() == 0.0 || unit.iter().zip(points).any(|(a, b)| transform.apply(*a) != b) {
        return;
    }
    let list = DisplayList {
        prims: vec![crate::DPrim { start: 0, ..p }],
        verts: unit.to_vec(),
        bounds: Bounds2::new(Vec2::ZERO, Vec2::new(1.0, 1.0)),
        ..Default::default()
    };
    let Some(template) = templates.lock().ok().map(|mut cache| cache.intern(list)) else { return };
    b.list.prims.truncate(first);
    b.list.verts.truncate(p.start as usize);
    b.instance(template, transform, e.handle, None);
}
