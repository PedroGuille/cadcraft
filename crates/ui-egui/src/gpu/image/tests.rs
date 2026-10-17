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
        parts.push((id, source.bounds));
        mesh.parts.push((id, Arc::new(build_mesh(&source, Vec2::ZERO, Rgb(255, 255, 255), false))));
    }
    if let Some(x) = edited {
        let source = list(x, false);
        let id = x.to_bits();
        parts.push((id, source.bounds));
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
