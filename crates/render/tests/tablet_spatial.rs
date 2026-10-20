//! Spatial selection must preserve the pixels and ordering of the complete CAD renderer.
use std::sync::Arc;

use cadcraft_color::{Color, Rgb};
use cadcraft_doc::{Block, Circle, Common, Drawing, Entity, EntityKind, Handle, Insert, Line, Space, Wipeout};
use cadcraft_geom::{Bounds2, Vec2, Vec3};
use cadcraft_render::{
    Options, build,
    raster::{self, RasterOptions, View},
    screen::Cache,
};

#[test]
fn spatial_pages_preserve_visible_pixels_masks_and_rotated_arrays_after_edits() {
    let mut drawing = Drawing::new_imperial();
    let mut symbol = Block::new("symbol");
    symbol.entities.push(Entity::new(Handle(10), EntityKind::Circle(Circle { center: Vec3::ZERO, radius: 2.0 })));
    drawing.blocks.insert(symbol.name.clone(), Arc::new(symbol));
    for i in 0..700 {
        let x = if i % 7 == 0 { 4.0 } else { f64::from(i) * 40.0 };
        drawing
            .add(
                &Space::Model,
                Common { color: Color::Index(1 + (i % 6) as u8), ..Default::default() },
                EntityKind::Line(Line { a: Vec3::new(x, -3.0, 0.0), b: Vec3::new(x + 3.0, 3.0, 0.0) }),
            )
            .unwrap();
        if i == 350 {
            drawing
                .add(
                    &Space::Model,
                    Common::default(),
                    EntityKind::Wipeout(Wipeout {
                        boundary: vec![Vec2::new(3.0, -1.0), Vec2::new(8.0, -1.0), Vec2::new(8.0, 1.0), Vec2::new(3.0, 1.0)],
                    }),
                )
                .unwrap();
        }
    }
    let insert = drawing
        .add(
            &Space::Model,
            Common::default(),
            EntityKind::Insert(Insert {
                block: "symbol".into(),
                insert: Vec3::new(3.0, 1.0, 0.0),
                scale: Vec3::new(-1.5, 0.5, 1.0),
                rotation: 0.7,
                cols: 3,
                rows: 2,
                col_spacing: -5.0,
                row_spacing: 6.0,
                attribs: vec![],
            }),
        )
        .unwrap();
    let mut cache = Cache::default();
    cache.set_spatial_partition(true);
    cache.set_budget(32 << 20);
    let overview = Options { view: Some(Bounds2::new(Vec2::new(-100.0, -100.0), Vec2::new(30_000.0, 100.0))), ..Default::default() };
    let all = Arc::new(drawing.clone());
    assert_eq!(
        cache.build_scene(&all, &Space::Model, &overview).scene.parts.len(),
        all.model.chunk_slices().count(),
        "overview keeps the original large batches"
    );
    for (step, x) in [0.0, 200.0, 0.0, 0.0].into_iter().enumerate() {
        if step == 3 {
            drawing.model.modify(insert, |e| {
                if let EntityKind::Insert(i) = &mut e.kind {
                    i.insert.x += 4.0;
                }
            });
        }
        let drawing = Arc::new(drawing.clone());
        let area = Bounds2::new(Vec2::new(x - 10.0, -10.0), Vec2::new(x + 10.0, 10.0));
        let options = Options { tolerance: 0.025, view: Some(area), ..Default::default() };
        let complete = Cache::default().build_scene(&drawing, &Space::Model, &options).scene.flatten();
        let selected = cache.build_scene(&drawing, &Space::Model, &options).scene.flatten();
        let view = View { center: area.center(), scale: 12.0, width: 240, height: 240 };
        let opts = RasterOptions { background: Rgb(255, 255, 255), ..Default::default() };
        let actual = raster::render(&selected, &view, &opts).unwrap();
        let expected = raster::render(&complete, &view, &opts).unwrap();
        let differences = actual.data().iter().zip(expected.data()).filter(|(a, b)| a != b).count();
        assert_eq!(differences, 0, "step {step}");
        assert!(selected.prims.len() < build(&drawing, &Space::Model, &options).prims.len() / 2, "off-screen entities were not tessellated");
    }
}

#[test]
fn analytic_hints_leave_cpu_and_export_geometry_unchanged() {
    let mut drawing = Drawing::new_imperial();
    for i in 0..8 {
        drawing
            .add(&Space::Model, Common::default(), EntityKind::Circle(Circle { center: Vec3::new(f64::from(i) * 20.0, 0.0, 0.0), radius: 5.0 }))
            .unwrap();
    }
    let drawing = Arc::new(drawing);
    let options = Options { tolerance: 0.125, ..Default::default() };
    let full = build(&drawing, &Space::Model, &options);
    assert!(full.analytic.is_empty());
    let scene = Cache::default().build_scene(&drawing, &Space::Model, &options).scene;
    let interactive = scene.flatten();
    assert_eq!(interactive.verts, full.verts);
    assert_eq!(interactive.prims, full.prims);
    assert_eq!(scene.parts.iter().map(|p| p.list.analytic.len()).sum::<usize>(), 8);
    let hint = scene.parts[0].list.analytic[0];
    assert!(hint.transformed(cadcraft_geom::Mat3 { a: 2.0, d: 1.0, ..cadcraft_geom::Mat3::IDENTITY }, 0).is_none());
}
