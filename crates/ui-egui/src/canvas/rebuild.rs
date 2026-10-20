//! Display-list rebuilds that only follow the view run on a worker thread, as AutoCAD redraws
//! instead of regenerating while you zoom and pan.
//!
//! Zooming past a 2x tessellation band or panning out of the area the list was built for used to
//! rebuild the list (and convert it to GPU vertex data) on the UI thread: on a hatched 12k-entity
//! drawing each step froze the window for 2-3 s. The current list stays valid at any view (only
//! hatch patterns are limited to its area, and curves and dashes are a band coarser or finer than
//! ideal), so the canvas keeps drawing it, scaled and moved by the view uniform, while the worker
//! builds the new one. A view that needs a different list cancels the build in flight (dropping
//! its [`Job`] sets the build's cancel flag, see `cadcraft_render::Options::cancel`) and starts
//! one for itself, so a long scroll through many zoom bands builds only for where it stops.
//! Expensive edits use the same worker; canvas picks wait for their matching scene. Small edits
//! remain immediate. The web build has no threads and rebuilds synchronously.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};

use cadcraft_color::Rgb;
use cadcraft_doc::{Drawing, Space};
use cadcraft_geom::{Bounds2, Vec2};
use cadcraft_render::screen::{Cache, Geometry, Scene};
use cadcraft_render::{Kind, Options};

/// What a build needs: the drawing (shared, never copied), the space and the options.
pub(super) struct Request {
    pub doc: Arc<Drawing>,
    pub space: Space,
    pub opts: Options,
    /// Also convert the list to GPU vertex data (GPU canvas only).
    pub mesh: Option<MeshRequest>,
    pub cache: Cache,
}

/// How to convert the list for the GPU: the view the vertex origin is chosen for and the
/// background the colours resolve against (paper when the list is a layout's sheet).
pub(super) struct MeshRequest {
    pub view_center: Vec2,
    pub view_height: f64,
    pub model_bg: Rgb,
    pub paper_bg: Rgb,
    pub lwdisplay: bool,
    pub compact: bool,
    /// Create the GPU buffers on the worker too, so the frame that swaps the list in only swaps
    /// buffers (`None`: the paint callback uploads them).
    pub device: Option<egui_wgpu::wgpu::Device>,
    pub queue: Option<egui_wgpu::wgpu::Queue>,
    pub parts: std::collections::HashMap<u64, Arc<crate::gpu::MeshData>>,
    pub blocks: Arc<std::sync::Mutex<crate::gpu::blocks::Cache>>,
}

/// A finished build.
pub(super) struct Built {
    pub list: Scene,
    pub cache: Cache,
    pub source: Arc<Drawing>,
    pub rebuilt: usize,
    pub reused: usize,
    pub build_ms: f64,
    pub mesh: Option<BuiltMesh>,
}

pub(super) struct BuiltMesh {
    pub data: crate::gpu::MeshData,
    pub origin: Vec2,
    pub bg: Rgb,
    pub lwdisplay: bool,
    /// Indices of the infinite lines (drawn on the CPU in GPU mode).
    pub infinite: Vec<usize>,
    pub ms: f64,
}

/// A build in flight: the list key and area it is for, and where its result arrives. Dropping
/// it cancels the build.
pub(super) struct Job {
    pub key: super::ListKey,
    pub window: Option<Bounds2>,
    rx: Receiver<Built>,
    cancel: Arc<AtomicBool>,
}

impl Drop for Job {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// What [`Job::poll`] found.
pub(super) enum Poll {
    Pending,
    Done(Box<Built>),
    /// The worker died without a result (it panicked).
    Lost,
}

impl Job {
    pub fn poll(&self) -> Poll {
        match self.rx.try_recv() {
            Ok(b) => Poll::Done(Box::new(b)),
            Err(TryRecvError::Empty) => Poll::Pending,
            Err(TryRecvError::Disconnected) => Poll::Lost,
        }
    }

    /// Whether this build serves list `key` for the visible area `visible`.
    pub fn serves(&self, key: &super::ListKey, visible: &Bounds2) -> bool {
        self.key == *key && self.window.is_none_or(|w| visible.is_empty() || w.contains_box(visible))
    }
}

/// Build `req` on the calling thread; `None` once it has been cancelled (the partial list is
/// thrown away, and no vertex data is made for it).
fn run(mut req: Request) -> Option<Built> {
    if req.opts.cancelled() {
        return None;
    }
    let t0 = crate::now_ms();
    let built = req.cache.build_scene(&req.doc, &req.space, &req.opts);
    let list = built.scene;
    if req.opts.cancelled() {
        return None;
    }
    let build_ms = crate::now_ms() - t0;
    let mesh = match req.mesh {
        None => None,
        Some(m) => {
            let t1 = crate::now_ms();
            let origin = crate::gpu::choose_origin(req.opts.view.as_ref().unwrap_or(&list.bounds), m.view_center, m.view_height);
            let bg = if list.sheet.is_some() { m.paper_bg } else { m.model_bg };
            let mut data = crate::gpu::MeshData::default();
            for part in &list.parts {
                if req.opts.cancelled() {
                    return None;
                }
                let id = super::part_mesh_key(part.id, origin, bg, m.lwdisplay);
                let mesh = m.parts.get(&id).cloned().unwrap_or_else(|| {
                    let mut mesh = crate::gpu::blocks::build_part_with_precision(
                        part,
                        origin,
                        bg,
                        m.lwdisplay,
                        &m.blocks,
                        if m.compact { req.opts.tolerance.abs() * 0.125 } else { 0.0 },
                    );
                    if let (Some(device), Some(queue)) = (&m.device, &m.queue) {
                        mesh.upload_now(device, queue);
                    }
                    Arc::new(mesh)
                });
                data.parts.push((id, mesh));
            }
            if req.opts.cancelled() {
                return None;
            }
            let infinite = list.primitives().enumerate().filter(|(_, p)| matches!(p.prim.kind, Kind::Infinite { .. })).map(|(i, _)| i).collect();
            Some(BuiltMesh { data, origin, bg, lwdisplay: m.lwdisplay, infinite, ms: crate::now_ms() - t1 })
        }
    };
    Some(Built { list, build_ms, mesh, cache: req.cache, source: Arc::new((*req.doc).clone()), rebuilt: built.rebuilt, reused: built.reused })
}

// A single worker keeps one active build and only the latest pending view. Replacing a
// pending request drops its drawing and GPU references immediately instead of spawning another
// thread while the cancelled build is still unwinding.
#[cfg(not(target_arch = "wasm32"))]
struct Task {
    req: Request,
    tx: std::sync::mpsc::SyncSender<Built>,
    ctx: egui::Context,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
struct Worker {
    pending: std::sync::Mutex<Option<Task>>,
    ready: std::sync::Condvar,
}

#[cfg(not(target_arch = "wasm32"))]
fn worker() -> Option<&'static Arc<Worker>> {
    static WORKER: std::sync::OnceLock<Option<Arc<Worker>>> = std::sync::OnceLock::new();
    WORKER
        .get_or_init(|| {
            let worker = Arc::new(Worker::default());
            let state = worker.clone();
            std::thread::Builder::new()
                .name("cadcraft-regen".into())
                .spawn(move || {
                    loop {
                        let Ok(mut pending) = state.pending.lock() else { break };
                        while pending.is_none() {
                            let Ok(next) = state.ready.wait(pending) else { return };
                            pending = next;
                        }
                        let task = pending.take();
                        drop(pending);
                        let Some(Task { req, tx, ctx }) = task else { continue };
                        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(req))) {
                            Ok(None) => {}
                            Ok(Some(built)) => {
                                let _ = tx.send(built);
                                ctx.request_repaint();
                            }
                            Err(_) => {
                                drop(tx);
                                ctx.request_repaint();
                            }
                        }
                    }
                })
                .map(|_| worker)
                .map_err(|e| log::warn!("canvas: could not start a regen worker ({e})"))
                .ok()
        })
        .as_ref()
}

/// Queue the latest view on a bounded worker; the old list remains drawable until it completes.
/// Without threads or a usable worker the caller retains the synchronous fallback.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn spawn(mut req: Request, key: super::ListKey, window: Option<Bounds2>, ctx: &egui::Context) -> Option<Job> {
    let worker = worker()?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let cancel = Arc::new(AtomicBool::new(false));
    req.opts.cancel = Some(cancel.clone());
    let mut pending = worker.pending.lock().ok()?;
    *pending = Some(Task { req, tx, ctx: ctx.clone() });
    worker.ready.notify_one();
    Some(Job { key, window, rx, cancel })
}

#[cfg(target_arch = "wasm32")]
pub(super) fn spawn(_req: Request, _key: super::ListKey, _window: Option<Bounds2>, _ctx: &egui::Context) -> Option<Job> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_build_matches_a_synchronous_one() {
        use cadcraft_geom::Vec3;
        let mut d = Drawing::new_imperial();
        let line = cadcraft_doc::Line { a: Vec3::new(0.0, 0.0, 0.0), b: Vec3::new(10.0, 5.0, 0.0) };
        let Ok(_) = d.add(&Space::Model, cadcraft_doc::Common::default(), cadcraft_doc::EntityKind::Line(line)) else { panic!("add") };
        let doc = Arc::new(d);
        let opts = Options::default();
        let sync = cadcraft_render::build(&doc, &Space::Model, &opts);
        let req = Request {
            doc: doc.clone(),
            space: Space::Model,
            opts: opts.clone(),
            cache: Cache::default(),
            mesh: Some(MeshRequest {
                view_center: Vec2::ZERO,
                view_height: 20.0,
                model_bg: Rgb(0, 0, 0),
                paper_bg: Rgb(255, 255, 255),
                lwdisplay: false,
                compact: false,
                device: None,
                queue: None,
                parts: Default::default(),
                blocks: Default::default(),
            }),
        };
        let ctx = egui::Context::default();
        let Some(job) = spawn(req, Default::default(), None, &ctx) else { panic!("no worker") };
        let built = loop {
            match job.poll() {
                Poll::Done(b) => break b,
                Poll::Pending => std::thread::sleep(std::time::Duration::from_millis(2)),
                Poll::Lost => panic!("worker lost"),
            }
        };
        assert_eq!(built.list.primitive_count(), sync.prims.len());
        assert_eq!(built.list.flatten().verts, sync.verts);
        let Some(mesh) = built.mesh else { panic!("no mesh") };
        assert_eq!(mesh.bg, Rgb(0, 0, 0));
        assert!(mesh.data.parts.iter().map(|(_, m)| m.line_vertices()).sum::<usize>() >= 2);
    }

    #[test]
    fn cancelled_run_makes_nothing() {
        let opts = Options { cancel: Some(Arc::new(AtomicBool::new(true))), ..Default::default() };
        let req = Request { doc: Arc::new(Drawing::new_imperial()), space: Space::Model, opts, mesh: None, cache: Cache::default() };
        assert!(run(req).is_none());
    }

    #[test]
    fn dropping_a_job_cancels_its_build() {
        let (_tx, rx) = std::sync::mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        drop(Job { key: Default::default(), window: None, rx, cancel: cancel.clone() });
        assert!(cancel.load(Ordering::Relaxed));
    }
}
