//! Shared screen templates retain the full builder's positions, handles and styles.
use std::sync::Arc;

use cadcraft_color::Color;
use cadcraft_doc::{Attrib, Block, Common, Drawing, Entity, EntityKind, HAlign, Handle, Insert, Line, Space, Text, VAlign};
use cadcraft_geom::{Vec2, Vec3};
use cadcraft_render::{
    Options, build,
    screen::{Cache, Geometry},
};

fn drawing(attributed: bool, array: bool) -> Arc<Drawing> {
    let mut d = Drawing::new_imperial();
    let mut block = Block::new("symbol");
    block.base = Vec3::new(2.0, -3.0, 0.0);
    for (i, (a, b)) in [(Vec3::ZERO, Vec3::new(3.0, 2.0, 0.0)), (Vec3::new(0.0, 2.0, 0.0), Vec3::new(3.0, 0.0, 0.0))].into_iter().enumerate() {
        let mut e = Entity::new(Handle(i as u64 + 1), EntityKind::Line(Line { a, b }));
        e.common.color = Color::ByBlock;
        block.entities.push(e);
    }
    d.blocks.insert("symbol".into(), Arc::new(block));
    for i in 0..40 {
        let attributes = if attributed {
            vec![Attrib {
                tag: "LABEL".into(),
                text: Text {
                    insert: Vec3::new(f64::from(i) * 10.0, 4.0, 0.0),
                    value: format!("A{i}"),
                    height: 1.0,
                    align_pt: None,
                    rotation: 0.2,
                    width_factor: 1.0,
                    oblique: 0.0,
                    style: "Standard".into(),
                    halign: HAlign::Left,
                    valign: VAlign::Baseline,
                },
                invisible: i % 3 == 0,
                constant: false,
                prompt: String::new(),
                props: Default::default(),
                multiline: None,
                modes: Default::default(),
            }]
        } else {
            Vec::new()
        };
        d.add(
            &Space::Model,
            Common { color: Color::Index(2), ..Default::default() },
            EntityKind::Insert(Insert {
                block: "symbol".into(),
                insert: Vec3::new(f64::from(i) * 10.0, 0.0, 0.0),
                scale: Vec3::new(if i % 2 == 0 { 2.0 } else { -2.0 }, 0.5, 1.0),
                rotation: f64::from(i % 5) * 0.3,
                attribs: attributes,
                cols: if array { 3 } else { 1 },
                rows: if array { 2 } else { 1 },
                col_spacing: 8.0,
                row_spacing: 12.0,
            }),
        )
        .unwrap();
        d.add(
            &Space::Model,
            Common::default(),
            EntityKind::Line(Line { a: Vec3::new(f64::from(i), -20.0, 0.0), b: Vec3::new(f64::from(i), -21.0, 0.0) }),
        )
        .unwrap();
    }
    Arc::new(d)
}

#[test]
fn shared_blocks_arrays_and_attributes_match_the_complete_renderer() {
    for attributed in [false, true] {
        for array in [false, true] {
            let d = drawing(attributed, array);
            let options = Options::default();
            let expected = build(&d, &Space::Model, &options);
            let scene = Cache::default().build_scene(&d, &Space::Model, &options).scene;
            assert_eq!(scene.primitive_count(), expected.prims.len());
            assert_eq!(scene.primitives().count(), expected.prims.len());
            let expanded = scene.flatten();
            for (index, (a, b)) in expanded.prims.iter().zip(&expected.prims).enumerate() {
                assert_eq!((a.handle, a.color, a.aci7, a.kind, a.lw, a.transparency), (b.handle, b.color, b.aci7, b.kind, b.lw, b.transparency));
                let actual = scene.primitive(index).unwrap();
                assert_eq!(actual.prim.handle, b.handle);
                assert_eq!(actual.points().len(), expected.points(b).len());
                for ((p, q), r) in expanded.points(a).iter().zip(expected.points(b)).zip(actual.points().iter()) {
                    assert!(p.dist(*q) < 1e-9 && p.dist(*r) < 1e-9, "{index}: {p:?} != {q:?}");
                }
            }
            assert!(scene.parts.iter().any(|p| !p.instances.is_empty()));
            assert!(scene.parts.iter().map(|p| p.list.verts.len()).sum::<usize>() < expected.verts.len());
            let copies: Vec<_> = scene.parts.iter().flat_map(|p| p.instances.iter()).filter(|i| !i.glyph).collect();
            assert_eq!(copies.len(), if array { 240 } else { 40 });
            assert!(copies.windows(2).all(|pair| Arc::ptr_eq(&pair[0].template, &pair[1].template)));
        }
    }
}

#[test]
fn selected_shared_geometry_stays_valid_after_optional_caches_are_trimmed() {
    let d = drawing(false, false);
    let mut cache = Cache::default();
    let scene = cache.build_scene(&d, &Space::Model, &Options::default()).scene;
    let before = scene.primitive(0).unwrap().points().into_owned();
    cache.set_budget(0);
    let selected = scene.primitive(0).unwrap();
    assert_eq!(selected.points().as_ref(), before);
    assert!(selected.points().iter().all(|p| p.dist(Vec2::ZERO).is_finite()));
    let rebuilt = cache.build_scene(&d, &Space::Model, &Options::default()).scene;
    assert_eq!(scene.primitive_count(), rebuilt.primitive_count());
}

#[test]
fn straight_block_templates_survive_many_detail_levels() {
    let d = drawing(false, false);
    let mut cache = Cache::default();
    cache.set_budget(64 << 20);
    let first = cache.build_scene(&d, &Space::Model, &Options::default()).scene;
    let template = first.parts.iter().flat_map(|p| p.instances.iter()).next().unwrap().template.clone();
    for tolerance in [1.0, 0.00001, 0.1, 0.0000001] {
        let options = Options { tolerance, min_dash: tolerance * 4.0, view_height: tolerance * 1000.0, ..Default::default() };
        let scene = cache.build_scene(&d, &Space::Model, &options).scene;
        assert!(scene.parts.iter().flat_map(|p| p.instances.iter()).all(|i| Arc::ptr_eq(&i.template, &template)));
        let expected = build(&d, &Space::Model, &options);
        let actual = scene.flatten();
        for (a, b) in actual.prims.iter().zip(&expected.prims) {
            assert_eq!(a.handle, b.handle);
            for (a, b) in actual.points(a).iter().zip(expected.points(b)) {
                assert!(a.dist(*b) < 1e-9);
            }
        }
    }
}

#[test]
fn equivalent_block_names_share_resolved_geometry_but_keep_handles_and_styles() {
    let mut d = (*drawing(false, false)).clone();
    let mut alias = (**d.blocks.get("symbol").unwrap()).clone();
    alias.name = "equivalent".into();
    let original = std::mem::take(&mut alias.entities);
    for e in original.iter() {
        alias.entities.push(Entity { handle: Handle(e.handle.0 + 1000), ..(**e).clone() });
    }
    d.blocks.insert(alias.name.clone(), Arc::new(alias));
    let mut e = (**d.model.iter().find(|e| matches!(e.kind, EntityKind::Insert(_))).unwrap()).clone();
    if let EntityKind::Insert(i) = &mut e.kind {
        i.block = "equivalent".into();
    }
    let alias_handle = d.add(&Space::Model, e.common.clone(), e.kind.clone()).unwrap();
    e.common.color = Color::Index(1);
    let red_handle = d.add(&Space::Model, e.common, e.kind).unwrap();
    let scene = Cache::default().build_scene(&Arc::new(d.clone()), &Space::Model, &Options::default()).scene;
    let copies: Vec<_> = scene.parts.iter().flat_map(|p| p.instances.iter()).collect();
    let original = copies.first().unwrap();
    let alias = copies.iter().find(|i| i.handle == alias_handle).unwrap();
    let red = copies.iter().find(|i| i.handle == red_handle).unwrap();
    assert!(Arc::ptr_eq(&original.template, &alias.template));
    assert!(!Arc::ptr_eq(&original.template, &red.template));
    let full = build(&d, &Space::Model, &Options::default());
    let actual = scene.flatten();
    assert_eq!(actual.prims, full.prims);
    for (a, b) in actual.verts.iter().zip(&full.verts) {
        assert!(a.dist(*b) < 1e-9);
    }
}

#[test]
fn shared_drafting_glyphs_keep_decorations_transforms_and_distant_detail() {
    let mut d = Drawing::new_imperial();
    for i in 0..24 {
        d.add(
            &Space::Model,
            Common::default(),
            EntityKind::Text(Text {
                insert: Vec3::new(f64::from(i) * 20.0, -10.0, 0.0),
                align_pt: None,
                height: 2.0,
                value: "ROOM 101 %%uAB%%u %%d".into(),
                rotation: f64::from(i % 4) * 0.3,
                width_factor: if i % 2 == 0 { 0.8 } else { -1.2 },
                oblique: 0.2,
                style: "Standard".into(),
                halign: HAlign::Left,
                valign: VAlign::Baseline,
            }),
        )
        .unwrap();
    }
    let drawing = Arc::new(d);
    for pixel in [0.0, 0.1, 1.0, 4.0, 20.0] {
        let options = Options { min_dash: pixel, ..Default::default() };
        let full = build(&drawing, &Space::Model, &options);
        let scene = Cache::default().build_scene(&drawing, &Space::Model, &options).scene;
        let flat = scene.flatten();
        assert_eq!(flat.prims.len(), full.prims.len());
        for (a, b) in flat.prims.iter().zip(&full.prims) {
            assert_eq!((a.handle, a.kind, a.color, a.lw), (b.handle, b.kind, b.color, b.lw));
            assert_eq!(flat.points(a).len(), full.points(b).len());
            for (p, q) in flat.points(a).iter().zip(full.points(b)) {
                assert!(p.dist(*q) < 1e-9, "{pixel}: {p:?} != {q:?}");
            }
        }
        assert!(scene.parts.iter().flat_map(|p| p.instances.iter()).any(|i| i.glyph));
        assert!(scene.parts.iter().map(|p| p.list.verts.len()).sum::<usize>() < full.verts.len() / 2);
    }
}
