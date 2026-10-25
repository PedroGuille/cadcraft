//! A clipped layout viewport's contents and selection highlight are clipped on the GPU (#688):
//! the outline is drawn into the stencil buffer as a triangle fan that flips a bit (even-odd),
//! and they draw only where it is set. Renders through the canvas pipelines into an offscreen
//! colour + stencil target and reads the pixels back.

use std::sync::Arc;

use cadcraft_engine::doc::Handle;
use cadcraft_engine::geom::{Bounds2, Vec2};
use cadcraft_engine::render::{DPrim, DisplayList, Kind};
use cadcraft_ui_egui::gpu::{self, CanvasCallback, GpuFaults, LAYER_HIGHLIGHT, LAYER_MASK, LAYER_VIEWPORT, MeshSlot, Stencil};
use egui_wgpu::wgpu;
use egui_wgpu::{CallbackTrait, ScreenDescriptor};

const SIZE: u32 = 64;
const STENCIL: wgpu::TextureFormat = wgpu::TextureFormat::Stencil8;

/// A display list of one black filled polygon given as triangles.
fn tris(pts: Vec<Vec2>) -> DisplayList {
    let len = pts.len() as u32;
    let bounds = Bounds2::from_points(pts.iter().copied());
    let prim =
        DPrim { handle: Handle(1), color: cadcraft_color::Rgb(0, 0, 0), aci7: false, lw: 0.0, transparency: 0, kind: Kind::Tris, start: 0, len };
    DisplayList { prims: vec![prim], tris: pts, bounds, ..Default::default() }
}

/// A triangle fan over `outline` (what the canvas masks a clipped viewport with).
fn fan(outline: &[Vec2]) -> DisplayList {
    let mut t = Vec::new();
    for w in outline[1..].windows(2) {
        t.extend_from_slice(&[outline[0], w[0], w[1]]);
    }
    tris(t)
}

/// A callback drawing `list` on `layer`, one world unit per pixel, world (32, 32) at the centre.
fn callback(list: &DisplayList, layer: usize, key: u64, stencil: Stencil) -> CanvasCallback {
    let slot: MeshSlot = Arc::default();
    *slot.lock().unwrap() = Some(gpu::build_mesh(list, Vec2::ZERO, cadcraft_color::Rgb(255, 255, 255), false));
    CanvasCallback { key, slot, offset: [-32.0, -32.0], scale: 1.0, center: [32.0, 32.0], layer, xform: gpu::IDENTITY_XFORM, dash: 0.0, stencil }
}

/// Render the callbacks in order into a white target with a cleared stencil buffer; returns
/// whether each pixel came out dark, row by row from the top.
fn render(rs: &egui_wgpu::RenderState, callbacks: &[CanvasCallback]) -> Vec<bool> {
    let (device, queue) = (&rs.device, &rs.queue);
    let extent = wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 };
    let texture = |format, usage| {
        device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage,
            view_formats: &[],
        })
    };
    let color = texture(rs.target_format, wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC);
    let stencil = texture(STENCIL, wgpu::TextureUsages::RENDER_ATTACHMENT);
    let (color_view, stencil_view) = (color.create_view(&Default::default()), stencil.create_view(&Default::default()));
    let screen = ScreenDescriptor { size_in_pixels: [SIZE, SIZE], pixels_per_point: 1.0 };
    let mut encoder = device.create_command_encoder(&Default::default());
    let mut renderer = rs.renderer.write();
    for cb in callbacks {
        let _ = cb.prepare(device, queue, &screen, &mut encoder, &mut renderer.callback_resources);
    }
    {
        let mut pass = encoder
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &color_view,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::WHITE), store: wgpu::StoreOp::Store },
                    depth_slice: None,
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &stencil_view,
                    depth_ops: None,
                    stencil_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(0), store: wgpu::StoreOp::Discard }),
                }),
                ..Default::default()
            })
            .forget_lifetime();
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(SIZE as f32, SIZE as f32));
        for cb in callbacks {
            let info = egui::PaintCallbackInfo { viewport: rect, clip_rect: rect, pixels_per_point: 1.0, screen_size_px: [SIZE, SIZE] };
            cb.paint(info, &mut pass, &renderer.callback_resources);
        }
    }
    let row = SIZE * 4;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(row * SIZE),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        color.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(SIZE) },
        },
        extent,
    );
    queue.submit([encoder.finish()]);
    buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    device.poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(std::time::Duration::from_secs(10)) }).unwrap();
    let pixels = buffer.slice(..).get_mapped_range().unwrap();
    pixels.chunks(4).map(|p| u32::from(p[0]) + u32::from(p[1]) + u32::from(p[2]) < 300).collect()
}

fn dark(px: &[bool], x: u32, y: u32) -> bool {
    px[(y * SIZE + x) as usize]
}

#[test]
fn clipped_viewport_contents_draw_only_inside_their_outline() {
    let rs = egui_kittest::wgpu::create_render_state(egui_kittest::wgpu::default_wgpu_setup(), Default::default());
    let faults = Arc::new(GpuFaults::default());
    let target = gpu::install(&rs, &faults, Some(STENCIL));
    assert_eq!(target.stencil, Some(STENCIL));
    // The contents: a black square over the whole target.
    let square = |key| {
        let (a, b, c, d) = (Vec2::new(0.0, 0.0), Vec2::new(64.0, 0.0), Vec2::new(64.0, 64.0), Vec2::new(0.0, 64.0));
        callback(&tris(vec![a, b, c, a, c, d]), LAYER_VIEWPORT, key, Stencil::Inside)
    };

    // A circle of radius 20 about the centre (screen and world agree at the centre; y is up).
    let circle: Vec<Vec2> =
        (0..=64).map(|i| f64::from(i) * std::f64::consts::TAU / 64.0).map(|a| Vec2::new(32.0 + 20.0 * a.cos(), 32.0 + 20.0 * a.sin())).collect();
    let px = render(&rs, &[callback(&fan(&circle), LAYER_MASK, 1, Stencil::Mask), square(2)]);
    assert!(dark(&px, 32, 32) && dark(&px, 45, 32) && dark(&px, 32, 18));
    assert!(!dark(&px, 4, 4) && !dark(&px, 60, 60) && !dark(&px, 58, 32) && !dark(&px, 32, 58));

    // A concave L: the fan covers the notch twice, which leaves it unmasked.
    let l = [(8.0, 8.0), (56.0, 8.0), (56.0, 24.0), (24.0, 24.0), (24.0, 56.0), (8.0, 56.0), (8.0, 8.0)].map(|(x, y)| Vec2::new(x, y));
    let px = render(&rs, &[callback(&fan(&l), LAYER_MASK, 3, Stencil::Mask), square(4)]);
    // World (16, 40) is screen (16, 24); world (40, 16) is screen (40, 48); the notch, world
    // (40, 40), is screen (40, 24).
    assert!(dark(&px, 16, 24) && dark(&px, 40, 48), "inside both arms of the L");
    assert!(!dark(&px, 40, 24), "the notch is outside");
    assert!(!dark(&px, 2, 2) && !dark(&px, 60, 60));

    // The selection highlight inside a clipped viewport stays inside its outline too: a wide
    // line across the whole target at world y 32, masked by the circle.
    let line = DisplayList {
        prims: vec![DPrim {
            handle: Handle(1),
            color: cadcraft_color::Rgb(0, 0, 0),
            aci7: false,
            lw: 0.0,
            transparency: 0,
            kind: Kind::Polyline,
            start: 0,
            len: 2,
        }],
        verts: vec![Vec2::new(0.0, 32.0), Vec2::new(64.0, 32.0)],
        bounds: Bounds2::new(Vec2::new(0.0, 32.0), Vec2::new(64.0, 32.0)),
        ..Default::default()
    };
    let slot: MeshSlot = Arc::default();
    *slot.lock().unwrap() = Some(gpu::build_highlight_mesh(&line, &[0], Vec2::ZERO, [0, 0, 0, 255], 3.0, false));
    let highlight = CanvasCallback { slot, layer: LAYER_HIGHLIGHT, ..callback(&line, LAYER_HIGHLIGHT, 6, Stencil::Inside) };
    let px = render(&rs, &[callback(&fan(&circle), LAYER_MASK, 1, Stencil::Mask), highlight]);
    assert!(dark(&px, 32, 32) && dark(&px, 20, 32) && dark(&px, 44, 32), "inside the circle");
    assert!(!dark(&px, 4, 32) && !dark(&px, 60, 32), "outside it");

    // Without a mask, Off draws everywhere.
    let mut off = square(5);
    off.stencil = Stencil::Off;
    let px = render(&rs, &[off]);
    assert!(px.iter().all(|d| *d));
    // wgpu reported nothing (validation errors land in the faults instead of panicking).
    assert_eq!(faults.take(), None);
}

#[test]
fn tablet_compact_buffers_and_analytic_curves_preserve_visible_geometry() {
    use cadcraft_doc::{Arc as CadArc, Circle, Common, Drawing, EntityKind, Space};
    use cadcraft_geom::Vec3;
    use cadcraft_render::{Options, screen::Cache};

    let rs = egui_kittest::wgpu::create_render_state(egui_kittest::wgpu::default_wgpu_setup(), Default::default());
    let faults = Arc::new(GpuFaults::default());
    gpu::install(&rs, &faults, Some(STENCIL));
    let mut drawing = Drawing::new_imperial();
    let mut centers = Vec::new();
    for i in 0..8 {
        let center = Vec3::new(9.0 + f64::from(i % 4) * 15.0, if i < 4 { 18.0 } else { 46.0 }, 0.0);
        centers.push(center.xy());
        let kind = if i < 4 {
            EntityKind::Circle(Circle { center, radius: 5.0 })
        } else {
            EntityKind::Arc(CadArc { center, radius: 5.0, start: 0.0, end: std::f64::consts::PI * 1.5 })
        };
        drawing.add(&Space::Model, Common::default(), kind).unwrap();
    }
    let drawing = Arc::new(drawing);
    let options = Options { tolerance: 0.125, ..Default::default() };
    let baseline = cadcraft_render::build(&drawing, &Space::Model, &options);
    let mut cache = Cache::default();
    cache.set_spatial_partition(true);
    let scene = cache.build_scene(&drawing, &Space::Model, &options).scene;
    let mut compact = gpu::MeshData::default();
    let templates = Default::default();
    for part in &scene.parts {
        compact.parts.push((
            part.id,
            Arc::new(gpu::blocks::build_part_with_precision(part, Vec2::ZERO, cadcraft_color::Rgb(255, 255, 255), false, &templates, 0.015625)),
        ));
    }
    assert!(compact.bytes() < gpu::build_mesh(&baseline, Vec2::ZERO, cadcraft_color::Rgb(255, 255, 255), false).bytes() / 2);
    let old = render(&rs, &[callback(&baseline, LAYER_VIEWPORT, 10, Stencil::Off)]);
    let slot: MeshSlot = Arc::default();
    *slot.lock().unwrap() = Some(compact);
    let new_callback = CanvasCallback { slot, ..callback(&baseline, LAYER_VIEWPORT, 11, Stencil::Off) };
    let new = render(&rs, std::slice::from_ref(&new_callback));
    if let Ok(directory) = std::env::var("CADCRAFT_GPU_CHECK_DIR") {
        std::fs::create_dir_all(&directory).unwrap();
        let picture = image::RgbImage::from_fn(SIZE, SIZE, |x, y| image::Rgb(if dark(&new, x, y) { [0, 0, 0] } else { [255, 255, 255] }));
        picture.save(std::path::Path::new(&directory).join("analytic-curves.png")).unwrap();
    }
    assert!(new.iter().filter(|p| **p).count() > 100, "the analytic instances were drawn");
    for y in 0..SIZE {
        for x in 0..SIZE {
            let at = (y * SIZE + x) as usize;
            let world = Vec2::new(f64::from(x) + 0.5, 64.0 - f64::from(y) - 0.5);
            if old[at] != new[at] {
                assert!(centers.iter().any(|c| (world.dist(*c) - 5.0).abs() <= 1.5), "difference away from a curve: {x},{y}");
            }
        }
    }
    for c in &centers {
        assert!(!dark(&new, c.x as u32, (64.0 - c.y) as u32), "circle interiors stay empty");
    }
    for c in centers.iter().skip(4) {
        assert!(!dark(&new, (c.x + 3.0) as u32, (64.0 - c.y + 3.0) as u32), "missing arc quadrant stays empty");
    }
    let left_half = [Vec2::ZERO, Vec2::new(32.0, 0.0), Vec2::new(32.0, 64.0), Vec2::new(0.0, 64.0), Vec2::ZERO];
    let mut clipped_curve = new_callback;
    clipped_curve.stencil = Stencil::Inside;
    let clipped = render(&rs, &[callback(&fan(&left_half), LAYER_MASK, 20, Stencil::Mask), clipped_curve]);
    assert!(clipped.iter().filter(|p| **p).count() > 40);
    assert!((0..SIZE).all(|y| (33..SIZE).all(|x| !dark(&clipped, x, y))));

    // Dense filled geometry exercises the normalized-integer vertex pipeline, including
    // opaque writes and stencil clipping. Vertices sit clear of pixel centres.
    let mut vertices = Vec::new();
    for y in 0..8 {
        for x in 0..8 {
            let a = Vec2::new(f64::from(x) * 8.0 + 0.25, f64::from(y) * 8.0 + 0.25);
            let b = a + Vec2::new(4.0, 0.0);
            let c = a + Vec2::new(4.0, 4.0);
            let d = a + Vec2::new(0.0, 4.0);
            vertices.extend([a, b, c, a, c, d]);
        }
    }
    let list = tris(vertices);
    let mut mesh = gpu::build_mesh(&list, Vec2::ZERO, cadcraft_color::Rgb(255, 255, 255), false);
    mesh.set_display_precision(0.01);
    let packed: MeshSlot = Arc::default();
    *packed.lock().unwrap() = Some(mesh);
    let packed = CanvasCallback { slot: packed, ..callback(&list, LAYER_VIEWPORT, 13, Stencil::Off) };
    let packed_pixels = render(&rs, std::slice::from_ref(&packed));
    let mut clipped_packed = packed;
    clipped_packed.stencil = Stencil::Inside;
    let clipped = render(&rs, &[callback(&fan(&left_half), LAYER_MASK, 20, Stencil::Mask), clipped_packed]);
    assert!(clipped.iter().any(|p| *p));
    assert!((0..SIZE).all(|y| (33..SIZE).all(|x| !dark(&clipped, x, y))));
    assert_eq!(packed_pixels, render(&rs, &[callback(&list, LAYER_VIEWPORT, 14, Stencil::Off)]));
    assert_eq!(faults.take(), None);
}
