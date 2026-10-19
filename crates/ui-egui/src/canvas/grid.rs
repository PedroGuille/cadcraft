//! The grid is a reusable anti-aliased mesh, so pen-only frames skip line generation and
//! tessellation. A small LRU also serves inactive viewports without retaining drawing data.
use super::*;

#[derive(Clone, PartialEq)]
struct Key {
    rect: Rect,
    values: [f64; 10],
    enabled: bool,
    iso: bool,
    major: u32,
    theme: Tokens,
    ppp: f32,
    tessellation: egui::epaint::TessellationOptions,
}

struct Entry {
    key: Key,
    mesh: Arc<egui::Mesh>,
}

pub(super) struct Cache {
    entries: VecDeque<Entry>,
    limit: usize,
}

impl Default for Cache {
    fn default() -> Self {
        Self { entries: VecDeque::new(), limit: 2 << 20 }
    }
}

impl Cache {
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn bytes(&self) -> usize {
        self.entries.iter().fold(0usize, |n, e| n.saturating_add(mesh_bytes(&e.mesh)))
    }

    pub fn set_budget(&mut self, bytes: usize) {
        self.limit = bytes;
        self.trim(0);
    }

    fn trim(&mut self, reserve: usize) {
        while !self.entries.is_empty() && (self.entries.len() >= 8 || self.bytes().saturating_add(reserve) > self.limit) {
            self.entries.pop_front();
        }
    }

    fn mesh(&mut self, key: Key, shapes: impl FnOnce() -> Vec<Shape>) -> Arc<egui::Mesh> {
        if let Some(at) = self.entries.iter().position(|e| e.key == key)
            && let Some(entry) = self.entries.remove(at)
        {
            let mesh = entry.mesh.clone();
            self.entries.push_back(entry);
            return mesh;
        }
        let mut mesh = egui::Mesh::default();
        // Grid shapes contain no text or textured discs; only the atlas's constant white UV
        // is used. Lines keep egui's normal pixel rounding and feathering at the current DPI.
        let mut tessellator = egui::epaint::tessellator::Tessellator::new(key.ppp, key.tessellation, [1, 1], Vec::new());
        tessellator.set_clip_rect(key.rect);
        tessellator.tessellate_shape(Shape::Vec(shapes()), &mut mesh);
        let mesh = Arc::new(mesh);
        let bytes = mesh_bytes(&mesh);
        self.trim(bytes);
        if bytes <= self.limit {
            self.entries.push_back(Entry { key, mesh: mesh.clone() });
        }
        mesh
    }
}

fn mesh_bytes(mesh: &egui::Mesh) -> usize {
    mesh.vertices
        .capacity()
        .saturating_mul(std::mem::size_of::<egui::epaint::Vertex>())
        .saturating_add(mesh.indices.capacity().saturating_mul(std::mem::size_of::<u32>()))
}

pub(super) fn draw(app: &CadApp, p: &egui::Painter, xf: &Xf) {
    let ppp = p.ctx().pixels_per_point();
    if !(xf.scale.is_finite() && xf.scale > 0.0 && xf.center.is_finite() && xf.rect.is_finite() && ppp.is_finite() && ppp > 0.0) {
        return;
    }
    let s = app.session.drafting_settings();
    let (origin, angle) = app.session.doc().map(cadcraft_engine::snap::grid_frame).unwrap_or((Vec2::ZERO, 0.0));
    let key = Key {
        rect: xf.rect,
        values: [xf.center.x, xf.center.y, xf.scale, s.gridunit.x, s.gridunit.y, s.snapunit.x, s.snapunit.y, origin.x, origin.y, angle],
        enabled: s.gridmode,
        iso: s.isodraft,
        major: s.gridmajor,
        theme: Tokens::get(),
        ppp,
        tessellation: p.ctx().options(|o| o.tessellation_options),
    };
    if let Ok(mut cache) = app.canvas.grid.try_lock() {
        p.add(Shape::Mesh(cache.mesh(key, || super::grid_shapes(app, xf))));
    } else {
        p.extend(super::grid_shapes(app, xf));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn frame(app: &CadApp, ctx: &egui::Context, xf: &Xf) -> Arc<egui::Mesh> {
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let painter = ui.ctx().layer_painter(egui::LayerId::background());
            draw(app, &painter, xf);
        });
        output.textures_delta.clear(); // This CPU-only check deliberately has no texture uploader.
        app.canvas.grid.lock().unwrap().entries.back().unwrap().mesh.clone()
    }

    #[test]
    fn unchanged_grid_reuses_tessellation_and_settings_dpi_and_views_invalidate_it() {
        let mut app = CadApp::new(cadcraft_engine::Session::new(), crate::Services::default());
        let ctx = egui::Context::default();
        let mut xf = Xf { rect: Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0)), center: Vec2::ZERO, scale: 40.0 };
        let first = frame(&app, &ctx, &xf);
        assert!(Arc::ptr_eq(&first, &frame(&app, &ctx, &xf)));
        // Compare with egui's ordinary tessellation, including its normal anti-aliasing.
        let direct = ctx.tessellate(
            vec![egui::epaint::ClippedShape { clip_rect: xf.rect, shape: Shape::Vec(super::super::grid_shapes(&app, &xf)) }],
            ctx.pixels_per_point(),
        );
        let mut combined = egui::Mesh::default();
        for primitive in direct {
            if let egui::epaint::Primitive::Mesh(mesh) = primitive.primitive {
                combined.append(mesh);
            }
        }
        assert_eq!(first.vertices, combined.vertices);
        assert_eq!(first.indices, combined.indices);
        app.run("setvar", json!({"name":"GRIDUNIT","value":[1.0,1.0]})).unwrap();
        let spacing = frame(&app, &ctx, &xf);
        assert!(!Arc::ptr_eq(&first, &spacing));
        app.run("setvar", json!({"name":"SNAPSTYL","value":1})).unwrap();
        let iso = frame(&app, &ctx, &xf);
        assert!(!Arc::ptr_eq(&spacing, &iso));
        app.run("setvar", json!({"name":"SNAPANG","value":30})).unwrap();
        let rotated = frame(&app, &ctx, &xf);
        assert!(!Arc::ptr_eq(&iso, &rotated));
        xf.center.x = 20.0;
        let pan = frame(&app, &ctx, &xf);
        assert!(!Arc::ptr_eq(&rotated, &pan));
        ctx.set_pixels_per_point(2.0);
        let high_dpi = frame(&app, &ctx, &xf);
        assert!(!Arc::ptr_eq(&pan, &high_dpi));
        app.canvas.set_cache_budget(0);
        assert_eq!(app.canvas.grid.lock().unwrap().bytes(), 0);
    }
}
