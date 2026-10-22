//! Retained view/zoom variants preserve full geometry and invalidate edited source chunks.
use cadcraft_doc::{Common, Drawing, EntityKind, Hatch, HatchLoop, Line, Space};
use cadcraft_geom::{Bounds2, PolyVertex, Vec2, Vec3};
use cadcraft_render::{
    Options, build,
    screen::{Cache, Scene},
};
use std::sync::Arc;

fn options(x: f64, tolerance: f64) -> Options {
    Options { view: Some(Bounds2::new(Vec2::new(x, 0.0), Vec2::new(x + 4.0, 4.0))), tolerance, view_height: tolerance * 1000.0, ..Default::default() }
}

fn same(scene: &Scene, d: &Drawing, options: &Options) {
    let actual = scene.flatten();
    let expected = build(d, &Space::Model, options);
    assert_eq!(actual.verts, expected.verts);
    assert_eq!(actual.tris, expected.tris);
    assert_eq!(actual.prims.len(), expected.prims.len());
    for (a, b) in actual.prims.iter().zip(&expected.prims) {
        assert_eq!((a.handle, a.kind, a.color, a.transparency), (b.handle, b.kind, b.color, b.transparency));
    }
}

#[test]
fn ordinary_geometry_survives_many_pans_and_prior_zoom_bands() {
    let mut d = Drawing::new_imperial();
    for x in 0..600 {
        d.add(
            &Space::Model,
            Common::default(),
            EntityKind::Line(Line { a: Vec3::new(f64::from(x % 4), 0.0, 0.0), b: Vec3::new(f64::from(x % 4), 2.0, 0.0) }),
        )
        .unwrap();
    }
    let d = Arc::new(d);
    let mut cache = Cache::default();
    cache.set_budget(64 << 20);
    let first = cache.build_scene(&d, &Space::Model, &options(0.0, 0.1));
    let ids: Vec<_> = first.scene.parts.iter().map(|p| p.id).collect();
    for x in [20.0, 50.0, 100.0, 400.0, 200.0, 0.0] {
        let built = cache.build_scene(&d, &Space::Model, &options(x, 0.1));
        assert_eq!(built.rebuilt, 0);
        assert_eq!(built.scene.parts.iter().map(|p| p.id).collect::<Vec<_>>(), ids);
        same(&built.scene, &d, &options(x, 0.1));
    }
    let fine = cache.build_scene(&d, &Space::Model, &options(0.0, 0.01));
    assert_eq!(fine.rebuilt, 0, "straight CPU geometry does not need finer tessellation");
    assert!(Arc::ptr_eq(&fine.scene.parts[0].list, &first.scene.parts[0].list));
    assert_ne!(fine.scene.parts[0].id, first.scene.parts[0].id, "GPU display precision is rechecked");
    let back = cache.build_scene(&d, &Space::Model, &options(0.0, 0.1));
    assert_eq!(back.rebuilt, 0);
    assert_eq!(back.scene.parts.iter().map(|p| p.id).collect::<Vec<_>>(), ids);
    cache.set_budget(0);
    assert_eq!(cache.bytes(), 0);
    same(&back.scene, &d, &options(0.0, 0.1));
}

#[test]
fn spatial_straight_chunks_keep_complete_geometry_across_zoom_and_pan() {
    let mut d = Drawing::new_imperial();
    for i in 0..1024 {
        let x = f64::from(i) * 10.0;
        d.add(&Space::Model, Common::default(), EntityKind::Line(Line { a: Vec3::new(x, 0.0, 0.0), b: Vec3::new(x, 2.0, 0.0) })).unwrap();
    }
    let d = Arc::new(d);
    let mut cache = Cache::default();
    cache.set_spatial_partition(true);
    cache.set_budget(64 << 20);
    let area = Some(Bounds2::new(Vec2::new(0.0, 0.0), Vec2::new(1200.0, 4.0)));
    let first = cache.build_scene(&d, &Space::Model, &Options { view: area, tolerance: 0.1, ..Default::default() }).scene;
    let fine = cache.build_scene(&d, &Space::Model, &Options { view: area, tolerance: 0.0001, ..Default::default() });
    assert_eq!(fine.rebuilt, 0);
    assert!(Arc::ptr_eq(&first.parts[0].list, &fine.scene.parts[0].list));
    let pan = cache.build_scene(&d, &Space::Model, &options(1000.0, 0.0001));
    assert_eq!(pan.rebuilt, 0);
    assert!(pan.scene.flatten().verts.iter().any(|p| p.x == 1000.0), "the complete cached group includes newly visible edges");
}

#[test]
fn subpixel_polylines_and_patterned_lines_keep_their_detail_dependent_representation() {
    use cadcraft_doc::{Linetype, LwPolyline};
    let mut d = Drawing::new_imperial();
    d.linetypes.push(Linetype::simple("TEST_DASH", "Synthetic dashes", &[0.1, -0.1]));
    d.add(
        &Space::Model,
        Common::default(),
        EntityKind::LwPolyline(LwPolyline {
            vertices: [(0.0, 0.0), (0.01, 0.0), (0.01, 0.01), (0.0, 0.01)].map(|(x, y)| PolyVertex::new(Vec2::new(x, y))).to_vec(),
            closed: true,
            const_width: 0.0,
            elevation: 0.0,
            plinegen: false,
        }),
    )
    .unwrap();
    d.add(
        &Space::Model,
        Common { linetype: "TEST_DASH".into(), ..Default::default() },
        EntityKind::Line(Line { a: Vec3::ZERO, b: Vec3::new(2.0, 0.0, 0.0) }),
    )
    .unwrap();
    let d = Arc::new(d);
    let mut cache = Cache::default();
    for min_dash in [1.0, 0.00001, 1.0] {
        let options = Options { min_dash, tolerance: min_dash * 0.25, ..Default::default() };
        let scene = cache.build_scene(&d, &Space::Model, &options).scene;
        same(&scene, &d, &options);
        assert_eq!(scene.flatten().prims, build(&d, &Space::Model, &options).prims);
    }
}

#[test]
fn clipped_hatches_revisit_old_views_and_edited_chunks_do_not_reappear() {
    let mut d = Drawing::new_imperial();
    let h = d
        .add(
            &Space::Model,
            Common::default(),
            EntityKind::Hatch(Hatch {
                pattern: "ANSI31".into(),
                solid: false,
                loops: vec![HatchLoop {
                    vertices: [(0.0, 0.0), (100.0, 0.0), (100.0, 10.0), (0.0, 10.0)].map(|(x, y)| PolyVertex::new(Vec2::new(x, y))).to_vec(),
                    outer: true,
                    sources: vec![],
                }],
                scale: 1.0,
                angle: 0.0,
                associative: false,
                style: 0,
                elevation: 0.0,
                gradient: None,
                origin: Vec2::ZERO,
                background: None,
                pattern_lines: vec![],
            }),
        )
        .unwrap();
    let d = Arc::new(d);
    let mut cache = Cache::default();
    let first = cache.build_scene(&d, &Space::Model, &options(0.0, 0.001));
    for x in [20.0, 40.0, 60.0, 80.0] {
        let built = cache.build_scene(&d, &Space::Model, &options(x, 0.001));
        same(&built.scene, &d, &options(x, 0.001));
    }
    let back = cache.build_scene(&d, &Space::Model, &options(0.0, 0.001));
    assert_eq!(back.rebuilt, 0);
    assert_eq!(back.scene.parts[0].id, first.scene.parts[0].id);
    let mut edited = (*d).clone();
    edited.model.modify(h, |e| {
        if let EntityKind::Hatch(h) = &mut e.kind {
            h.angle = 0.3;
        }
    });
    let edited = Arc::new(edited);
    let edit = cache.build_scene(&edited, &Space::Model, &options(40.0, 0.001));
    assert!(edit.rebuilt > 0);
    same(&edit.scene, &edited, &options(40.0, 0.001));
    let back = cache.build_scene(&edited, &Space::Model, &options(0.0, 0.001));
    assert_ne!(back.scene.parts[0].id, first.scene.parts[0].id);
    same(&back.scene, &edited, &options(0.0, 0.001));
    let undo = cache.build_scene(&d, &Space::Model, &options(0.0, 0.001));
    same(&undo.scene, &d, &options(0.0, 0.001));
}

#[test]
fn distant_geometry_is_loaded_on_demand_and_pressure_restores_view_culling() {
    use cadcraft_render::screen::Geometry;
    let mut d = Drawing::new_imperial();
    for x in 0..3000 {
        let x = f64::from(x) * 10.0;
        d.add(&Space::Model, Common::default(), EntityKind::Line(Line { a: Vec3::new(x, 0.0, 0.0), b: Vec3::new(x, 2.0, 0.0) })).unwrap();
    }
    let d = Arc::new(d);
    let mut cache = Cache::default();
    let first = cache.build_scene(&d, &Space::Model, &options(0.0, 0.001));
    assert!(first.scene.primitive_count() > 1 && first.scene.primitive_count() < 3000);
    let away = cache.build_scene(&d, &Space::Model, &options(20000.0, 0.001));
    assert!(away.scene.primitives().flat_map(|p| p.points().into_owned()).any(|p| p.x == 20000.0));
    let back = cache.build_scene(&d, &Space::Model, &options(0.0, 0.001));
    assert_eq!(back.rebuilt, 0);
    cache.set_budget(0);
    let clipped = cache.build_scene(&d, &Space::Model, &options(15000.0, 0.001));
    assert_eq!(clipped.scene.primitive_count(), 1);
    assert!(clipped.scene.primitive(0).unwrap().points().iter().all(|p| p.x == 15000.0));
}

#[test]
fn an_edit_and_pan_together_do_not_retain_deleted_drawing_snapshots() {
    let mut d = Drawing::new_imperial();
    let h = d.add(&Space::Model, Common::default(), EntityKind::Line(Line { a: Vec3::ZERO, b: Vec3::new(1.0, 2.0, 0.0) })).unwrap();
    let source = Arc::new(d);
    let deleted = Arc::downgrade(source.model.iter().next().unwrap());
    let mut cache = Cache::default();
    cache.build_scene(&source, &Space::Model, &options(0.0, 0.001));
    cache.build_scene(&source, &Space::Model, &options(20.0, 0.001));
    let mut edited = (*source).clone();
    edited.model.remove(h);
    drop(source);
    cache.build_scene(&Arc::new(edited), &Space::Model, &options(40.0, 0.001));
    assert!(deleted.upgrade().is_none(), "view retention must not become extra undo history");
}
