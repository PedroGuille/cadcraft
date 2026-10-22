//! Rectangle instancing retains exact coordinates, handles, styles and subpixel behavior.
use cadcraft_doc::{Common, Drawing, EntityKind, LwPolyline, Space};
use cadcraft_geom::{PolyVertex, Vec2};
use cadcraft_render::{Options, build, screen::Cache};
use std::sync::Arc;

#[test]
fn independent_rectangles_share_affine_geometry_without_changing_logical_objects() {
    let mut d = Drawing::new_imperial();
    for i in 0..128 {
        let x = f64::from(i) * 32.0;
        let width = f64::from(i % 5 + 1) * 2.0;
        d.add(
            &Space::Model,
            Common { linetype: "Continuous".into(), ..Default::default() },
            EntityKind::LwPolyline(LwPolyline {
                vertices: [(x, 0.0), (x + width, 0.0), (x + width, width * 0.5), (x, width * 0.5)]
                    .map(|(x, y)| PolyVertex::new(Vec2::new(x, y)))
                    .to_vec(),
                closed: true,
                const_width: 0.0,
                elevation: 0.0,
                plinegen: false,
            }),
        )
        .unwrap();
    }
    let d = Arc::new(d);
    let mut cache = Cache::default();
    for min_dash in [0.0, 0.01, 1000.0, 0.0] {
        let options = Options { min_dash, ..Options::default() };
        let scene = cache.build_scene(&d, &Space::Model, &options).scene;
        let full = build(&d, &Space::Model, &options);
        let actual = scene.flatten();
        assert_eq!(actual.prims, full.prims);
        assert_eq!(actual.verts, full.verts);
        assert_eq!(actual.tris, full.tris);
        if min_dash == 0.0 {
            let instances: Vec<_> = scene.parts.iter().flat_map(|p| p.instances.iter()).collect();
            assert_eq!(instances.len(), 128);
            assert!(instances.iter().all(|i| Arc::ptr_eq(&i.template, &instances[0].template)));
            assert!(scene.parts.iter().all(|p| p.list.verts.is_empty()));
        }
    }
}
