//! Compare retained/partially redrawn pixels with a complete direct rendering.
#![allow(clippy::unwrap_used)]
use super::*;
use cadcraft_doc::Handle;
use egui_wgpu::{CallbackTrait, ScreenDescriptor};

const SIZE: [u32; 2] = [1536, 1024];

fn list(x: f64, mask: bool) -> DisplayList {
    let points = vec![Vec2::new(x, -80.0), Vec2::new(x + 80.0, -80.0), Vec2::new(x, -180.0)];
    DisplayList {
        bounds: Bounds2::from_points(points.iter().copied()),
        tris: points,
        prims: vec![cadcraft_render::DPrim {
            handle: Handle(1),
            color: Rgb(0, 0, 0),
            aci7: false,
            transparency: 0,
            lw: 0.0,
            kind: if mask { Kind::Mask } else { Kind::Tris },
            start: 0,
            len: 3,
        }],
        ..Default::default()
    }
}

fn callback(key: u64, edited: Option<f64>, offset: [f32; 2]) -> CachedCanvasCallback {
    let mut mesh = MeshData::default();
    let mut parts = Vec::new();
    // A stable, transparent background and an intervening wipeout exercise CAD ordering.
    for (id, mut source) in [(1, list(60.0, false)), (2, list(70.0, true))] {
        if id == 1 {
            source.prims[0].transparency = 50;
        }
        parts.push(cadcraft_render::screen::DamagePart { id, bounds: source.bounds, details: None, origin: Vec2::ZERO });
        mesh.parts.push((id, Arc::new(build_mesh(&source, Vec2::ZERO, Rgb(255, 255, 255), false))));
    }
    if let Some(x) = edited {
        let source = list(x, false);
        let id = x.to_bits();
        parts.push(cadcraft_render::screen::DamagePart { id, bounds: source.bounds, details: None, origin: Vec2::ZERO });
        mesh.parts.push((id, Arc::new(build_mesh(&source, Vec2::ZERO, Rgb(255, 255, 255), false))));
    }
    let slot = Arc::new(Mutex::new(Some(mesh)));
    CachedCanvasCallback {
        canvas: CanvasCallback {
            key,
            slot,
            offset,
            scale: 1.0,
            center: [0.0, 0.0],
            layer: LAYER_DRAWING,
            xform: IDENTITY_XFORM,
            dash: 0.0,
            stencil: Stencil::Off,
        },
        parts: parts.into(),
        style: 1,
        clip: egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(SIZE[0] as f32, SIZE[1] as f32)),
        navigation: false,
        reduced_resolution: false,
        navigation_fps: 30,
        time: key as f64,
        context: egui::Context::default(),
    }
}

fn render(rs: &egui_wgpu::RenderState, callback: &dyn CallbackTrait) -> Vec<u8> {
    let (device, queue) = (&rs.device, &rs.queue);
    let size = wgpu::Extent3d { width: SIZE[0], height: SIZE[1], depth_or_array_layers: 1 };
    let color = device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: rs.target_format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = color.create_view(&Default::default());
    let screen = ScreenDescriptor { size_in_pixels: SIZE, pixels_per_point: 1.0 };
    let mut encoder = device.create_command_encoder(&Default::default());
    let mut renderer = rs.renderer.write();
    let _ = callback.prepare(device, queue, &screen, &mut encoder, &mut renderer.callback_resources);
    {
        let mut pass = encoder
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::WHITE), store: wgpu::StoreOp::Store },
                })],
                ..Default::default()
            })
            .forget_lifetime();
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(SIZE[0] as f32, SIZE[1] as f32));
        callback.paint(
            egui::PaintCallbackInfo { viewport: rect, clip_rect: rect, pixels_per_point: 1.0, screen_size_px: SIZE },
            &mut pass,
            &renderer.callback_resources,
        );
    }
    let row = SIZE[0] * 4;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(row * SIZE[1]),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        color.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(SIZE[1]) },
        },
        size,
    );
    queue.submit([encoder.finish()]);
    buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    device.poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(std::time::Duration::from_secs(20)) }).unwrap();
    buffer.slice(..).get_mapped_range().unwrap().to_vec()
}

fn same_pixels(actual: &[u8], expected: &[u8], context: &str) {
    assert_eq!(actual.len(), expected.len());
    let different = actual.iter().zip(expected).filter(|(a, b)| a != b).count();
    assert_eq!(different, 0, "{context}: differing color channels");
}

fn dirty_count(rs: &egui_wgpu::RenderState) -> usize {
    rs.renderer.read().callback_resources.get::<Resources>().unwrap().image.as_ref().unwrap().dirty_tiles
}

#[test]
fn tiled_edits_deletes_and_scrolled_pixels_match_direct_rendering() {
    let rs = egui_kittest::wgpu::create_render_state(egui_kittest::wgpu::default_wgpu_setup(), Default::default());
    let faults = Arc::new(GpuFaults::default());
    install(&rs, &faults, None);
    set_cache_budget(&rs, 12);
    for (key, x, expected_tiles) in [(10, Some(200.0), 6), (11, Some(220.0), 1), (12, Some(800.0), 2), (13, None, 1), (14, Some(220.0), 1)] {
        let cached = render(&rs, &callback(key, x, [0.0, 0.0]));
        assert_eq!(dirty_count(&rs), expected_tiles, "frame {key}");
        let complete = render(&rs, &callback(key + 100, x, [0.0, 0.0]).canvas);
        same_pixels(&cached, &complete, &format!("edit/deletion frame {key}"));
    }
    for (i, offset) in [[17.0, -11.0], [-13.0, 21.0], [0.0, 0.0]].into_iter().enumerate() {
        let cached = render(&rs, &callback(14, Some(220.0), offset));
        assert!(dirty_count(&rs) < 6, "pan should retain covered tiles");
        same_pixels(&cached, &render(&rs, &callback(200 + i as u64, Some(220.0), offset).canvas), &format!("pan {offset:?}"));
    }
    let cached = render(&rs, &callback(14, Some(220.0), [0.0, 0.0]));
    assert_eq!(dirty_count(&rs), 0, "cursor-only frame submits no drawing geometry");
    set_cache_budget(&rs, 0);
    same_pixels(&cached, &render(&rs, &callback(15, Some(220.0), [0.0, 0.0])), "direct rendering under pressure");
    assert_eq!(faults.take(), None);
}

#[test]
fn shared_outline_glyphs_match_full_geometry_and_survive_cache_trimming() {
    use cadcraft_doc::{Common, Drawing, EntityKind, HAlign, Space, Text, VAlign};
    use cadcraft_geom::Vec3;
    use cadcraft_render::screen::{Cache as SceneCache, Geometry};
    // Fonts are optional build/runtime input. Exercise an installed open font when available;
    // the complete repository suite must also work on systems with only our stroke font.
    if cadcraft_fonts::ttf::find("DejaVuSans").is_none() {
        return;
    }
    let mut d = Drawing::new_imperial();
    d.text_styles[0].font = "DejaVuSans".into();
    d.add(
        &Space::Model,
        Common::default(),
        EntityKind::Text(Text {
            insert: Vec3::new(40.0, -160.0, 0.0),
            align_pt: None,
            height: 40.0,
            value: "AAAAAAAA 111111 000000".into(),
            rotation: 0.0,
            width_factor: 1.0,
            oblique: 0.0,
            style: "Standard".into(),
            halign: HAlign::Left,
            valign: VAlign::Baseline,
        }),
    )
    .unwrap();
    let drawing = Arc::new(d);
    let options = cadcraft_render::Options::default();
    let mut scene_cache = SceneCache::default();
    let scene = scene_cache.build_scene(&drawing, &Space::Model, &options).scene;
    let glyphs: Vec<_> = scene.parts.iter().flat_map(|p| p.instances.iter()).filter(|i| i.glyph).collect();
    assert!(!glyphs.is_empty());
    assert!(Arc::ptr_eq(&glyphs[0].template, &glyphs[1].template));
    assert_eq!(scene.parts.iter().map(|p| p.list.tris.len()).sum::<usize>(), 0);
    cadcraft_render::trim_glyph_cache();
    scene_cache.set_budget(0);
    let again = SceneCache::default().build_scene(&drawing, &Space::Model, &options).scene;
    assert!(Arc::ptr_eq(&scene.parts[0].instances[0].template, &again.parts[0].instances[0].template));
    let full = cadcraft_render::build(&drawing, &Space::Model, &options);
    assert_eq!(scene.primitive_count(), full.prims.len());
    let flat = scene.flatten();
    for (a, b) in flat.tris.iter().zip(&full.tris) {
        assert!(a.dist(*b) < 1e-9);
    }
    let rs = egui_kittest::wgpu::create_render_state(egui_kittest::wgpu::default_wgpu_setup(), Default::default());
    let faults = Arc::new(GpuFaults::default());
    install(&rs, &faults, None);
    let cache = Mutex::new(blocks::Cache::default());
    cache.lock().unwrap().set_budget(0);
    let mut mesh = MeshData::default();
    for part in &scene.parts {
        mesh.parts.push((part.id, Arc::new(blocks::build_part(part, Vec2::ZERO, Rgb(255, 255, 255), false, &cache))));
    }
    let mut shared = callback(500, None, [0.0, 0.0]).canvas;
    shared.slot = Arc::new(Mutex::new(Some(mesh)));
    let pixels = render(&rs, &shared);
    let mut direct = callback(501, None, [0.0, 0.0]).canvas;
    direct.slot = Arc::new(Mutex::new(Some(build_mesh(&full, Vec2::ZERO, Rgb(255, 255, 255), false))));
    same_pixels(&pixels, &render(&rs, &direct), "shared GPU geometry");
    assert_eq!(faults.take(), None);
}

#[test]
fn attributed_array_bodies_and_styled_labels_match_the_flat_gpu_path() {
    use cadcraft_doc::{Attrib, Block, Common, Drawing, Entity, EntityKind, HAlign, Insert, Line, Space, Text, VAlign};
    use cadcraft_geom::Vec3;
    let mut d = Drawing::new_imperial();
    let mut block = Block::new("symbol");
    for y in [0.0, 8.0] {
        block.entities.push(Entity::new(Handle(1), EntityKind::Line(Line { a: Vec3::new(0.0, y, 0.0), b: Vec3::new(30.0, y, 0.0) })));
    }
    d.blocks.insert("symbol".into(), Arc::new(block));
    for i in 0..8 {
        let x = 30.0 + f64::from(i) * 130.0;
        let text = Text {
            insert: Vec3::new(x, -200.0, 0.0),
            align_pt: None,
            height: 14.0,
            value: format!("P{i}"),
            rotation: 0.0,
            width_factor: 1.0,
            oblique: 0.0,
            style: "Standard".into(),
            halign: HAlign::Left,
            valign: VAlign::Baseline,
        };
        let attribute = Attrib {
            tag: "LABEL".into(),
            text,
            invisible: false,
            constant: false,
            prompt: String::new(),
            props: Default::default(),
            multiline: None,
            modes: Default::default(),
        };
        d.add(
            &Space::Model,
            Common::default(),
            EntityKind::Insert(Insert {
                block: "symbol".into(),
                insert: Vec3::new(x, -150.0, 0.0),
                scale: Vec3::new(1.0, 1.0, 1.0),
                rotation: 0.0,
                attribs: vec![attribute],
                cols: 2,
                rows: 2,
                col_spacing: 40.0,
                row_spacing: 30.0,
            }),
        )
        .unwrap();
    }
    let drawing = Arc::new(d);
    let options = cadcraft_render::Options::default();
    let scene = cadcraft_render::screen::Cache::default().build_scene(&drawing, &Space::Model, &options).scene;
    let cache = Mutex::new(blocks::Cache::default());
    cache.lock().unwrap().set_budget(0);
    let mut mesh = MeshData::default();
    for part in &scene.parts {
        mesh.parts.push((part.id, Arc::new(blocks::build_part(part, Vec2::ZERO, Rgb(255, 255, 255), false, &cache))));
    }
    let rs = egui_kittest::wgpu::create_render_state(egui_kittest::wgpu::default_wgpu_setup(), Default::default());
    let faults = Arc::new(GpuFaults::default());
    install(&rs, &faults, None);
    let mut shared = callback(600, None, [0.0, 0.0]).canvas;
    shared.slot = Arc::new(Mutex::new(Some(mesh)));
    let pixels = render(&rs, &shared);
    let full = cadcraft_render::build(&drawing, &Space::Model, &options);
    let mut direct = callback(601, None, [0.0, 0.0]).canvas;
    direct.slot = Arc::new(Mutex::new(Some(build_mesh(&full, Vec2::ZERO, Rgb(255, 255, 255), false))));
    same_pixels(&pixels, &render(&rs, &direct), "shared GPU geometry");
    assert_eq!(cache.lock().unwrap().bytes(), 0, "shared active buffers do not require cold retention");
    assert_eq!(faults.take(), None);
}

fn scene_callback(scene: &cadcraft_render::screen::Scene, cache: &Mutex<blocks::Cache>, key: u64, lw: bool) -> CachedCanvasCallback {
    let mut cb = callback(key, None, [0.0, 0.0]);
    let mut mesh = MeshData::default();
    for part in &scene.parts {
        mesh.parts.push((part.id, Arc::new(blocks::build_part(part, Vec2::ZERO, Rgb(255, 255, 255), lw, cache))));
    }
    cb.canvas.slot = Arc::new(Mutex::new(Some(mesh)));
    cb.parts = scene.parts.iter().map(|p| p.damage_part(Vec2::ZERO)).collect();
    cb
}

#[test]
fn placement_edits_keep_old_snapshots_and_shared_geometry_valid() {
    use cadcraft_doc::{Block, Common, Drawing, Entity, EntityKind, Insert, Line, Space};
    use cadcraft_geom::Vec3;
    let rs = egui_kittest::wgpu::create_render_state(egui_kittest::wgpu::default_wgpu_setup(), Default::default());
    let faults = Arc::new(GpuFaults::default());
    install(&rs, &faults, None);
    let mut d = Drawing::new_imperial();
    let mut block = Block::new("symbol");
    block.entities.push(Entity::new(Handle(1), EntityKind::Line(Line { a: Vec3::ZERO, b: Vec3::new(8.0, 4.0, 0.0) })));
    d.blocks.insert("symbol".into(), Arc::new(block));
    let mut handles = Vec::new();
    for i in 0..256 {
        handles.push(
            d.add(
                &Space::Model,
                Common::default(),
                EntityKind::Insert(Insert {
                    block: "symbol".into(),
                    insert: Vec3::new(20.0 + f64::from(i % 32) * 30.0, -40.0 - f64::from(i / 32) * 30.0, 0.0),
                    scale: Vec3::new(1.0, 1.0, 1.0),
                    rotation: 0.0,
                    attribs: vec![],
                    cols: 1,
                    rows: 1,
                    col_spacing: 0.0,
                    row_spacing: 0.0,
                }),
            )
            .unwrap(),
        );
    }
    let mut scenes = cadcraft_render::screen::Cache::default();
    let templates = Mutex::new(blocks::Cache::default());
    let options = cadcraft_render::Options::default();
    let first = scenes.build_scene(&Arc::new(d.clone()), &Space::Model, &options).scene;
    let upload = |scene: &cadcraft_render::screen::Scene| {
        let mut mesh = MeshData::default();
        for part in &scene.parts {
            mesh.parts.push((part.id, Arc::new(blocks::build_part(part, Vec2::ZERO, Rgb(255, 255, 255), false, &templates))));
        }
        Uploaded::new(&rs.device, &rs.queue, &mesh)
    };
    let original = upload(&first);
    let repeat = upload(&first);
    assert_eq!(original.lines[0].buffer, repeat.lines[0].buffer);
    assert_eq!(
        original.lines[0].placements.as_ref().unwrap().buffer,
        repeat.lines[0].placements.as_ref().unwrap().buffer,
        "unchanged placements reuse their buffer"
    );
    let mut cb = callback(1600, None, [0.0, 0.0]).canvas;
    cb.slot = Arc::new(Mutex::new(Some(MeshData { uploaded: Some(original.clone()), ..Default::default() })));
    let before = render(&rs, &cb);
    d.model.modify(handles[100], |e| {
        if let EntityKind::Insert(i) = &mut e.kind {
            i.insert.x += 12.0;
            i.rotation = 0.5;
        }
    });
    let edited = scenes.build_scene(&Arc::new(d.clone()), &Space::Model, &options).scene;
    let changed = upload(&edited);
    assert_eq!(original.lines[0].buffer, changed.lines[0].buffer, "moving a block retains its geometry buffer");
    assert_ne!(
        original.lines[0].placements.as_ref().unwrap().buffer,
        changed.lines[0].placements.as_ref().unwrap().buffer,
        "the old snapshot remains immutable"
    );
    cb.key += 1;
    cb.slot = Arc::new(Mutex::new(Some(MeshData { uploaded: Some(changed), ..Default::default() })));
    let actual = render(&rs, &cb);
    cb.key += 1;
    cb.slot = Arc::new(Mutex::new(Some(build_mesh(&cadcraft_render::build(&d, &Space::Model, &options), Vec2::ZERO, Rgb(255, 255, 255), false))));
    same_pixels(&actual, &render(&rs, &cb), "sparse placement update");
    cb.key += 1;
    cb.slot = Arc::new(Mutex::new(Some(MeshData { uploaded: Some(original), ..Default::default() })));
    same_pixels(&before, &render(&rs, &cb), "old placement snapshot after edit");
    assert_eq!(faults.take(), None);
}

#[test]
fn reordered_disjoint_blocks_preserve_transparency_and_overlap_pixels() {
    use cadcraft_doc::{Block, Common, Drawing, Entity, EntityKind, Insert, Line, Space, Transparency};
    use cadcraft_geom::Vec3;
    let rs = egui_kittest::wgpu::create_render_state(egui_kittest::wgpu::default_wgpu_setup(), Default::default());
    let faults = Arc::new(GpuFaults::default());
    install(&rs, &faults, None);
    for (case, (overlap, transparent, different_inks)) in
        [(false, false, false), (true, false, false), (false, true, true), (true, true, true), (false, false, true)].into_iter().enumerate()
    {
        let mut d = Drawing::new_imperial();
        for kind in 0..2 {
            let mut block = Block::new(&format!("symbol{kind}"));
            let mut line = Entity::new(Handle(1), EntityKind::Line(Line { a: Vec3::ZERO, b: Vec3::new(14.0 + f64::from(kind), 10.0, 0.0) }));
            line.common.color = cadcraft_color::Color::ByBlock;
            line.common.transparency = Transparency::ByBlock;
            block.entities.push(line);
            d.blocks.insert(block.name.clone(), Arc::new(block));
        }
        for i in 0..16 {
            d.add(
                &Space::Model,
                Common {
                    color: cadcraft_color::Color::Index(if different_inks { 1 + i % 2 } else { 1 }),
                    transparency: Transparency::Percent(if transparent { 40 } else { 0 }),
                    ..Default::default()
                },
                EntityKind::Insert(Insert {
                    block: format!("symbol{}", i % 2),
                    insert: Vec3::new(60.0 + if overlap { 0.0 } else { f64::from(i) * 30.0 }, -100.0, 0.0),
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
        let options = cadcraft_render::Options { transparency: true, ..Default::default() };
        let scene = cadcraft_render::screen::Cache::default().build_scene(&Arc::new(d.clone()), &Space::Model, &options).scene;
        let mut mesh = MeshData::default();
        let cache = Mutex::new(blocks::Cache::default());
        for part in &scene.parts {
            mesh.parts.push((part.id, Arc::new(blocks::build_part(part, Vec2::ZERO, Rgb(255, 255, 255), false, &cache))));
        }
        let data = Uploaded::new(&rs.device, &rs.queue, &mesh);
        assert_eq!(data.lines.len(), if overlap || transparent || different_inks { 16 } else { 2 });
        for (zoom, scale) in [1.0, 0.01].into_iter().enumerate() {
            let mut cb = callback(1700 + case as u64 * 10 + zoom as u64 * 2, None, [0.0, 0.0]).canvas;
            cb.scale = scale;
            cb.slot = Arc::new(Mutex::new(Some(MeshData { uploaded: Some(data.clone()), ..Default::default() })));
            let actual = render(&rs, &cb);
            cb.key += 1;
            cb.slot =
                Arc::new(Mutex::new(Some(build_mesh(&cadcraft_render::build(&d, &Space::Model, &options), Vec2::ZERO, Rgb(255, 255, 255), false))));
            same_pixels(&actual, &render(&rs, &cb), "compatible block batching at native and coarse cached zooms");
        }
    }
    assert_eq!(faults.take(), None);
}

#[test]
fn editing_one_entity_in_a_scattered_chunk_only_repaints_its_tiles() {
    use cadcraft_doc::{Common, Drawing, EntityKind, Line, Space};
    use cadcraft_geom::Vec3;
    use cadcraft_render::screen::Cache;
    let rs = egui_kittest::wgpu::create_render_state(egui_kittest::wgpu::default_wgpu_setup(), Default::default());
    let faults = Arc::new(GpuFaults::default());
    install(&rs, &faults, None);
    set_cache_budget(&rs, 12);
    let mut d = Drawing::new_imperial();
    let mut handle = Handle(0);
    for i in 0..64 {
        let x = if i == 0 { 80.0 } else { 1100.0 + f64::from(i % 8) * 5.0 };
        let y = -100.0 - f64::from(i % 8) * 16.0;
        let h = d
            .add(&Space::Model, Common::default(), EntityKind::Line(Line { a: Vec3::new(x, y, 0.0), b: Vec3::new(x + 20.0, y - 10.0, 0.0) }))
            .unwrap();
        if i == 0 {
            handle = h;
        }
    }
    let original = d.clone();
    let mut scenes = Cache::default();
    let meshes = Mutex::new(blocks::Cache::default());
    let opts = cadcraft_render::Options::default();
    let first = scenes.build_scene(&Arc::new(d.clone()), &Space::Model, &opts).scene;
    assert_eq!(first.parts.len(), 1);
    render(&rs, &scene_callback(&first, &meshes, 1000, false));
    let mut previous = first;
    for (key, x, tiles) in [(1001, Some(90.0), 1), (1002, Some(700.0), 2), (1003, None, 1), (1004, Some(80.0), 1)] {
        if let Some(x) = x {
            if key == 1004 {
                d = original.clone();
            } else {
                let mut e = (**d.model.get(handle).unwrap()).clone();
                e.kind = EntityKind::Line(Line { a: Vec3::new(x, -100.0, 0.0), b: Vec3::new(x + 20.0, -110.0, 0.0) });
                assert!(d.model.replace(e));
            }
        } else {
            d.model.remove(handle);
        }
        let scene = scenes.build_scene(&Arc::new(d.clone()), &Space::Model, &opts).scene;
        let cb = scene_callback(&scene, &meshes, key, false);
        let actual = render(&rs, &cb);
        assert_eq!(dirty_count(&rs), tiles, "frame {key}");
        let mut direct = callback(key + 100, None, [0.0, 0.0]).canvas;
        direct.slot =
            Arc::new(Mutex::new(Some(build_mesh(&cadcraft_render::build(&d, &Space::Model, &opts), Vec2::ZERO, Rgb(255, 255, 255), false))));
        same_pixels(&actual, &render(&rs, &direct), "entity-level tile damage");
        previous = scene;
    }
    assert_eq!(previous.parts.len(), 1);
    assert_eq!(faults.take(), None);
}

#[test]
fn spatial_culling_pixels_match_an_unculled_draw_with_transparency_and_wipeouts() {
    let rs = egui_kittest::wgpu::create_render_state(egui_kittest::wgpu::default_wgpu_setup(), Default::default());
    let faults = Arc::new(GpuFaults::default());
    install(&rs, &faults, None);
    let mut source = DisplayList::default();
    for i in 0..256 {
        let x = if i % 2 == 0 { 20.0 + f64::from(i) * 3.0 } else { 10_000.0 };
        let mut triangle = list(x, i == 64);
        triangle.prims[0].start = source.tris.len() as u32;
        if i % 3 == 0 {
            triangle.prims[0].transparency = 50;
        }
        source.tris.extend(triangle.tris);
        source.prims.extend(triangle.prims);
    }
    let mesh = build_mesh(&source, Vec2::ZERO, Rgb(255, 255, 255), false);
    let mut unculled = Uploaded::new(&rs.device, &rs.queue, &mesh);
    for batch in unculled.lines.iter_mut().chain(&mut unculled.tris).chain(&mut unculled.quads) {
        batch.spatial = None;
        batch.bounds = Bounds2::EMPTY;
    }
    let mut cb = callback(1100, None, [0.0, 0.0]).canvas;
    cb.slot = Arc::new(Mutex::new(Some(mesh)));
    let actual = render(&rs, &cb);
    cb.key += 1;
    cb.slot = Arc::new(Mutex::new(Some(MeshData { uploaded: Some(unculled), ..Default::default() })));
    same_pixels(&actual, &render(&rs, &cb), "spatially culled draw order");
    assert_eq!(faults.take(), None);
}

#[test]
fn mixed_character_vector_batches_match_full_text_with_strokes_and_outline_fonts() {
    use cadcraft_doc::{Common, Drawing, EntityKind, HAlign, Lineweight, Space, Text, Transparency, VAlign};
    use cadcraft_geom::Vec3;
    fn batched(mesh: &MeshData) -> bool {
        mesh.glyphs.as_ref().is_some_and(|g| {
            let (copies, letters) = g.counts();
            copies > 8 && letters > 4
        }) || mesh.parts.iter().any(|(_, p)| batched(p))
    }
    let rs = egui_kittest::wgpu::create_render_state(egui_kittest::wgpu::default_wgpu_setup(), Default::default());
    let faults = Arc::new(GpuFaults::default());
    install(&rs, &faults, None);
    let mut key = 1200;
    for font in [cadcraft_fonts::BUILTIN_FONT, "DejaVuSans"] {
        if font != cadcraft_fonts::BUILTIN_FONT && cadcraft_fonts::ttf::find(font).is_none() {
            continue;
        }
        for wide in [false, true] {
            let mut d = Drawing::new_imperial();
            d.text_styles[0].font = font.into();
            for row in 0..4 {
                d.add(
                    &Space::Model,
                    Common {
                        color: cadcraft_color::Color::Index(if row % 2 == 0 { 1 } else { 5 }),
                        lineweight: Lineweight::Mm100(100),
                        transparency: Transparency::Percent(50),
                        ..Default::default()
                    },
                    EntityKind::Text(Text {
                        insert: Vec3::new(40.0, -100.0 - f64::from(row) * 90.0, 0.0),
                        align_pt: None,
                        height: 30.0,
                        value: "ROOM 123 + PUMP".into(),
                        rotation: 0.0,
                        width_factor: 1.0,
                        oblique: 0.0,
                        style: "Standard".into(),
                        halign: HAlign::Left,
                        valign: VAlign::Baseline,
                    }),
                )
                .unwrap();
            }
            let d = Arc::new(d);
            let options = cadcraft_render::Options { transparency: true, ..Default::default() };
            let mut scenes = cadcraft_render::screen::Cache::default();
            let scene = scenes.build_scene(&d, &Space::Model, &options).scene;
            let cache = Mutex::new(blocks::Cache::default());
            cache.lock().unwrap().set_budget(0);
            let cb = scene_callback(&scene, &cache, key, wide);
            assert!(batched(cb.canvas.slot.lock().unwrap().as_ref().unwrap()), "{font}: mixed letters share a GPU batch");
            let actual = render(&rs, &cb.canvas);
            cadcraft_render::trim_glyph_cache();
            scenes.set_budget(0);
            let mut direct = callback(key + 1, None, [0.0, 0.0]).canvas;
            direct.slot =
                Arc::new(Mutex::new(Some(build_mesh(&cadcraft_render::build(&d, &Space::Model, &options), Vec2::ZERO, Rgb(255, 255, 255), wide))));
            same_pixels(&actual, &render(&rs, &direct), &format!("{font}, wide={wide}"));
            key += 2;
        }
    }
    assert_eq!(faults.take(), None);
}

#[test]
fn a_single_outline_character_keeps_the_smaller_packed_upload() {
    use cadcraft_doc::{Common, Drawing, EntityKind, HAlign, Space, Text, VAlign};
    use cadcraft_geom::Vec3;
    if cadcraft_fonts::ttf::find("DejaVuSans").is_none() {
        return;
    }
    let mut d = Drawing::new_imperial();
    d.text_styles[0].font = "DejaVuSans".into();
    d.add(
        &Space::Model,
        Common::default(),
        EntityKind::Text(Text {
            insert: Vec3::ZERO,
            align_pt: None,
            height: 30.0,
            value: "W".into(),
            rotation: 0.0,
            width_factor: 1.0,
            oblique: 0.0,
            style: "Standard".into(),
            halign: HAlign::Left,
            valign: VAlign::Baseline,
        }),
    )
    .unwrap();
    let scene = cadcraft_render::screen::Cache::default().build_scene(&Arc::new(d), &Space::Model, &Default::default()).scene;
    let cache = Mutex::new(blocks::Cache::default());
    let mesh = blocks::build_part(&scene.parts[0], Vec2::ZERO, Rgb(255, 255, 255), false, &cache);
    fn packed_vertices(m: &MeshData) -> usize {
        m.tri_vertices() + m.parts.iter().map(|(_, p)| packed_vertices(p)).sum::<usize>()
    }
    fn vector_page(m: &MeshData) -> bool {
        m.glyphs.is_some() || m.parts.iter().any(|(_, p)| vector_page(p))
    }
    assert!(packed_vertices(&mesh) > 0);
    assert!(!vector_page(&mesh), "an isolated outline should not allocate a vector texture page");
    assert_eq!(cache.lock().unwrap().bytes(), 0);
}

#[test]
fn revisiting_pan_zoom_and_resolution_views_reuses_exact_tiles() {
    let rs = egui_kittest::wgpu::create_render_state(egui_kittest::wgpu::default_wgpu_setup(), Default::default());
    install(&rs, &Arc::new(GpuFaults::default()), None);
    set_cache_budget(&rs, 128);
    let first = callback(501, Some(200.0), [0.0, 0.0]);
    let pixels = render(&rs, &first);
    let away = callback(502, Some(200.0), [1200.0, 0.0]);
    same_pixels(&render(&rs, &away), &render(&rs, &away.canvas), "distant pan");
    let back = callback(503, Some(200.0), [0.0, 0.0]);
    same_pixels(&render(&rs, &back), &pixels, "return to retained pan tiles");
    assert_eq!(dirty_count(&rs), 0);
    let mut zoom = callback(504, Some(200.0), [0.0, 0.0]);
    zoom.canvas.scale = 2.0;
    same_pixels(&render(&rs, &zoom), &render(&rs, &zoom.canvas), "zoomed geometry");
    let mut reduced = callback(505, Some(200.0), [0.0, 0.0]);
    reduced.navigation = true;
    reduced.reduced_resolution = true;
    render(&rs, &reduced);
    let back = callback(506, Some(200.0), [0.0, 0.0]);
    same_pixels(&render(&rs, &back), &pixels, "native-resolution tiles restored");
    assert_eq!(dirty_count(&rs), 0);
    let renderer = rs.renderer.read();
    let image = renderer.callback_resources.get::<Resources>().unwrap().image.as_ref().unwrap();
    assert!(image.history.len() >= 2);
    assert!(image.bytes() <= 64 << 20);
    drop(renderer);
    set_cache_budget(&rs, 6);
    assert!(rs.renderer.read().callback_resources.get::<Resources>().unwrap().image.as_ref().unwrap().history.is_empty());
    same_pixels(&render(&rs, &back), &pixels, "pressure trims history without changing the drawing");
}

#[test]
fn view_reuse_requires_complete_content_identities_and_display_style() {
    let rs = egui_kittest::wgpu::create_render_state(egui_kittest::wgpu::default_wgpu_setup(), Default::default());
    install(&rs, &Arc::new(GpuFaults::default()), None);
    set_cache_budget(&rs, 64);
    let mut original = callback(601, Some(200.0), [0.0, 0.0]);
    original.parts = Arc::default();
    render(&rs, &original);
    let mut edit = callback(602, Some(800.0), [0.0, 0.0]);
    edit.parts = Arc::default();
    same_pixels(&render(&rs, &edit), &render(&rs, &edit.canvas), "missing damage metadata must not hide edits");
    assert_eq!(dirty_count(&rs), 6);
    edit.style += 1;
    render(&rs, &edit);
    assert_eq!(dirty_count(&rs), 6, "style changes invalidate the image even with the same mesh key");
}

#[test]
fn gradual_panning_keeps_past_tiles_within_the_shared_image_allowance() {
    let rs = egui_kittest::wgpu::create_render_state(egui_kittest::wgpu::default_wgpu_setup(), Default::default());
    install(&rs, &Arc::new(GpuFaults::default()), None);
    set_cache_budget(&rs, 128);
    render(&rs, &callback(701, Some(200.0), [0.0, 0.0]));
    for step in 1..=16 {
        let view = callback(701 + step, Some(200.0), [step as f32 * 80.0, 0.0]);
        let cached = render(&rs, &view);
        if step == 16 {
            same_pixels(&cached, &render(&rs, &view.canvas), "incremental pan after retaining an old view");
        }
    }
    let renderer = rs.renderer.read();
    let image = renderer.callback_resources.get::<Resources>().unwrap().image.as_ref().unwrap();
    assert!(!image.history.is_empty(), "small pans must also retain previously visited tiles");
    assert!(image.bytes() <= 64 << 20);
    drop(renderer);
    let back = callback(730, Some(200.0), [0.0, 0.0]);
    same_pixels(&render(&rs, &back), &render(&rs, &back.canvas), "return through retained scrolling history");
}
