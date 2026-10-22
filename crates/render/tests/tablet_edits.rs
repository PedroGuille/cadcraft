//! Entity cache reuse preserves analytic metadata across a neighbouring edit and undo.
use cadcraft_doc::{Circle, Common, Drawing, EntityKind, Space};
use cadcraft_geom::Vec3;
use cadcraft_render::{Options, build, screen::Cache};
use std::sync::Arc;

#[test]
fn ordinary_neighbor_edits_and_undo_keep_exact_vertices_and_curve_hints() {
    let mut d = Drawing::new_imperial();
    let mut handles = Vec::new();
    for i in 0..300 {
        handles.push(
            d.add(&Space::Model, Common::default(), EntityKind::Circle(Circle { center: Vec3::new(f64::from(i) * 5.0, 0.0, 0.0), radius: 2.0 }))
                .unwrap(),
        );
    }
    let before = Arc::new(d);
    let options = Options { tolerance: 0.125, min_dash: 0.5, ..Default::default() };
    let mut cache = Cache::default();
    let first = cache.build_scene(&before, &Space::Model, &options).scene;
    let mut edited = (*before).clone();
    edited
        .modify_entity(handles[150], |e| {
            if let EntityKind::Circle(c) = &mut e.kind {
                c.center.y += 1.0;
            }
        })
        .unwrap();
    for source in [Arc::new(edited), before.clone()] {
        let scene = cache.build_scene(&source, &Space::Model, &options).scene;
        let actual = scene.flatten();
        let expected = build(&source, &Space::Model, &options);
        assert_eq!(actual.prims, expected.prims);
        assert_eq!(actual.verts, expected.verts);
        // The parallel compatibility builder may omit optional GPU hints. Validate each
        // retained hint against its exact current CAD entity instead.
        assert_eq!(scene.parts.iter().map(|p| p.list.analytic.len()).sum::<usize>(), 300);
        for part in &scene.parts {
            for hint in &part.list.analytic {
                let prim = &part.list.prims[hint.prim];
                let EntityKind::Circle(circle) = &source.entity(prim.handle).unwrap().kind else { panic!("circle expected") };
                assert_eq!((hint.center, hint.radius, hint.tolerance), (circle.center.xy(), circle.radius, options.tolerance));
            }
        }
    }
    assert_eq!(first.flatten().verts, build(&before, &Space::Model, &options).verts);
}
