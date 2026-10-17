//! Tablet host: native file services, bounded workers and recovery over the shared CAD UI.
#![forbid(unsafe_code)]

use crate::{
    jobs::{self, Action},
    recovery::{self, Loaded},
};
use cadcraft_engine::{Session, doc::Space};
use cadcraft_ui_egui::{CadApp, Services, tablet::FileAction};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, mpsc::Receiver},
    time::{Duration, Instant},
};

#[derive(Clone, PartialEq, Eq)]
struct ContentKey {
    revision: u64,
    pointer: usize,
    host: usize,
    block: Option<String>,
}

impl ContentKey {
    fn of(d: &cadcraft_engine::DocState) -> Self {
        Self {
            revision: d.revision,
            pointer: Arc::as_ptr(&d.doc) as usize,
            host: Arc::as_ptr(d.drawing()) as usize,
            block: d.block_editor().map(str::to_owned),
        }
    }
}

#[derive(PartialEq, Eq)]
struct Key {
    uid: u64,
    content: ContentKey,
    dirty: bool,
    title: String,
    path: Option<String>,
    view: [u64; 3],
    space: Space,
    mspace: Option<cadcraft_engine::doc::Handle>,
    binary: bool,
}

pub struct Shell {
    pub cad: CadApp,
    dir: PathBuf,
    writer: recovery::Writer,
    loader: Option<Receiver<Loaded>>,
    known: bool,
    retained: Vec<Option<Value>>,
    restored: HashMap<u64, (Arc<cadcraft_engine::doc::Drawing>, String)>,
    restore_order: HashMap<u64, usize>,
    checkpoint_key: Vec<Key>,
    checkpoint_active: usize,
    checkpoint_prefs: (String, String),
    preferences_at: Instant,
    snapshots: HashMap<u64, (ContentKey, Arc<cadcraft_engine::doc::Drawing>)>,
    saves: jobs::Worker,
    saving: bool,
    name_uid: Option<u64>,
    files: Vec<PathBuf>,
    files_at: Instant,
    files_refresh: bool,
    drawings_open: bool,
    render: Option<eframe::egui_wgpu::RenderState>,
    cache_mib: u32,
    requested_cache_mib: u32,
    memory_at: Instant,
    #[cfg(target_os = "android")]
    native_language: Option<&'static str>,
}

fn install_io() {
    cadcraft_engine::cmd::file::set_io(cadcraft_engine::cmd::file::IoHooks {
        read: |b, name| cadcraft_io::read(b, name).map_err(|e| e.to_string()),
        write: |d, name| cadcraft_io::write(d, name).map_err(|e| e.to_string()),
        plot: Some(|d, space, opts| cadcraft_io::plotter::plot(d, space, opts).map_err(|e| e.to_string())),
    });
    cadcraft_engine::cmd::file::set_write_as(|d, name, o| {
        cadcraft_io::write_with(d, name, &cadcraft_io::SaveOptions { version: o.version, binary: o.binary }).map_err(|e| e.to_string())
    });
}

fn filename(name: &str, extension: &str) -> String {
    let name: String = name.chars().take(180).map(|c| if c == '/' || c == '\\' || c.is_control() { '_' } else { c }).collect();
    let name = if name.is_empty() || name == "." || name == ".." { "Drawing".into() } else { name };
    let stem = Path::new(&name).file_stem().map_or_else(|| "Drawing".into(), |s| s.to_string_lossy().into_owned());
    format!("{stem}.{extension}")
}

fn save_path(dir: &Path, name: &str) -> Option<String> {
    let extension = Path::new(name).extension().and_then(|s| s.to_str()).filter(|e| e.eq_ignore_ascii_case("dwg")).unwrap_or("dxf");
    let name = filename(name, extension);
    let stem = Path::new(&name).file_stem()?.to_string_lossy();
    let folder = dir.join("drawings");
    std::fs::create_dir_all(&folder).ok()?;
    for i in 0..10_000 {
        let path = folder.join(if i == 0 { name.clone() } else { format!("{stem}-{i}.{extension}") });
        if !path.exists() {
            return Some(path.to_string_lossy().into_owned());
        }
    }
    None
}

fn files(dir: &Path) -> Vec<PathBuf> {
    fn visit(dir: &Path, depth: usize, out: &mut Vec<PathBuf>, folders: &mut usize) {
        if depth > 8 || out.len() >= 1000 || *folders >= 1000 {
            return;
        }
        *folders += 1;
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.take(1000).filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    visit(&path, depth + 1, out, folders);
                } else if path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("dxf") || e.eq_ignore_ascii_case("dwg") || e.eq_ignore_ascii_case("pdf"))
                {
                    out.push(path);
                }
                if out.len() >= 1000 {
                    break;
                }
            }
        }
    }
    let mut out = Vec::new();
    let mut folders = 0;
    for name in ["drawings", "imports", "exports", "linked-backups"] {
        visit(&dir.join(name), 0, &mut out, &mut folders);
    }
    out.sort();
    out
}

impl Shell {
    pub fn new(cc: &eframe::CreationContext<'_>, dir: PathBuf) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        std::fs::create_dir_all(&dir)?;
        install_io();
        let save_dir = dir.clone();
        let services = Services {
            pick_open: Some(Box::new(|| {
                #[cfg(target_os = "android")]
                if let Err(error) = crate::platform::pick_open() {
                    log::warn!("Open: {error}");
                }
                None
            })),
            pick_save: Some(Box::new(move |name| save_path(&save_dir, name))),
        };
        let mut cad = CadApp::new(Session::empty(), services);
        #[cfg(target_os = "android")]
        if let Ok(Some(locale)) = crate::platform::take("interfaceLocale") {
            cad.system_languages.push(locale);
        }
        if let Some(storage) = cc.storage {
            if let Some(prefs) = storage.get_string(cadcraft_ui_egui::PREFS_KEY) {
                cad.load_prefs(&prefs);
            }
            if let Some(profile) = storage.get_string(cadcraft_ui_egui::options::PROFILE_KEY) {
                cad.load_profile(&profile);
            }
        }
        let prefs = dir.join("tablet-preferences.json");
        if std::fs::metadata(&prefs).is_ok_and(|m| m.len() <= 1 << 20)
            && let Ok(bytes) = std::fs::read(prefs)
            && let Ok(data) = serde_json::from_slice::<Value>(&bytes)
        {
            if let Some(prefs) = data.get("prefs").and_then(Value::as_str) {
                cad.load_prefs(prefs);
            }
            if let Some(profile) = data.get("profile").and_then(Value::as_str) {
                cad.load_profile(profile);
            }
        }
        cadcraft_ui_egui::tablet::configure(&mut cad);
        cc.egui_ctx.options_mut(|o| o.input_options.max_click_dist = 12.0);
        // Keep the legacy recovery list, but use one session writer for tablet snapshots.
        // SAVETIME remains the user's stored setting rather than being overwritten at launch.
        cad.enable_autosave(&dir.join("autosave"));
        cad.autosave = None;
        cad.opening.set_repaint_context(cc.egui_ctx.clone());
        cad.opening.set_parallel_reads(1);
        if let Some(rs) = &cc.wgpu_render_state {
            cad.set_wgpu(rs, None);
        }
        let writer = recovery::Writer::start(dir.clone(), cc.egui_ctx.clone())?;
        #[cfg(target_os = "android")]
        crate::platform::register(cc.egui_ctx.clone(), writer.control.clone());
        #[cfg(target_os = "android")]
        let max_bytes = crate::platform::memory_budget().unwrap_or(64 << 20);
        #[cfg(not(target_os = "android"))]
        let max_bytes = 256 << 20;
        let cache_mib = cad.tablet_prefs.cache_mib;
        if let Some(rs) = &cc.wgpu_render_state {
            cadcraft_ui_egui::gpu::set_cache_budget(rs, cache_mib);
        }
        let loader = recovery::load(dir.clone(), max_bytes, cc.egui_ctx.clone())?;
        let saves = jobs::Worker::start(cc.egui_ctx.clone())?;
        let mut shell = Self {
            cad,
            dir,
            writer,
            loader: Some(loader),
            known: false,
            retained: Vec::new(),
            restored: HashMap::new(),
            restore_order: HashMap::new(),
            checkpoint_key: Vec::new(),
            checkpoint_active: usize::MAX,
            checkpoint_prefs: (String::new(), String::new()),
            preferences_at: Instant::now(),
            snapshots: HashMap::new(),
            saves,
            saving: false,
            name_uid: None,
            files: Vec::new(),
            files_at: Instant::now(),
            files_refresh: true,
            drawings_open: false,
            render: cc.wgpu_render_state.clone(),
            cache_mib,
            requested_cache_mib: cache_mib,
            memory_at: Instant::now(),
            #[cfg(target_os = "android")]
            native_language: None,
        };
        shell.tune_cache(true);
        Ok(shell)
    }

    fn restore_tick(&mut self) {
        while let Some(event) = self.loader.as_ref().and_then(|r| r.try_recv().ok()) {
            match event {
                Loaded::Index(records) => {
                    self.retained = records.into_iter().map(Some).collect();
                    self.known = true;
                }
                Loaded::Drawing { index, state, file } => {
                    if let Some(record) = self.retained.get_mut(index) {
                        *record = None;
                    }
                    self.restored.insert(state.uid, (state.doc.clone(), file));
                    self.restore_order.insert(state.uid, index);
                    self.cad.session.docs.push(*state);
                    self.cad.session.active = self.cad.session.docs.len().saturating_sub(1);
                    self.cad.ui.start_tab = false;
                }
                Loaded::Notice(message) => self.cad.set_status(message),
                Loaded::Done => {
                    let uid = self.cad.session.state().map(|s| s.uid).ok();
                    self.cad.session.docs.sort_by_key(|s| self.restore_order.get(&s.uid).copied().unwrap_or(usize::MAX));
                    if let Some(index) = self.cad.session.docs.iter().position(|s| Some(s.uid) == uid) {
                        self.cad.session.active = index;
                    }
                    if self.cad.session.docs.is_empty() {
                        self.cad.session.new_drawing(false);
                    }
                    self.known = true;
                    self.loader = None;
                    break;
                }
            }
        }
    }

    fn checkpoint(&mut self, force_preferences: bool) {
        while let Ok(result) = self.writer.notices.try_recv() {
            if let Err(error) = result {
                self.checkpoint_key.clear();
                self.checkpoint_prefs = (String::new(), String::new());
                self.cad.set_status(format!("{}: {error}", cadcraft_ui_egui::i18n::t("Recovery could not be saved")));
            }
        }
        if !self.known {
            return;
        }
        self.writer.control.set_interval(self.cad.tablet_prefs.recovery_seconds);
        let key: Vec<_> = self
            .cad
            .session
            .docs
            .iter()
            .map(|d| {
                let view = d.paper_view();
                Key {
                    uid: d.uid,
                    content: ContentKey::of(d),
                    dirty: d.is_dirty(),
                    title: d.title.clone(),
                    path: d.path.clone(),
                    view: [view.center.x.to_bits(), view.center.y.to_bits(), view.height.to_bits()],
                    space: d.space.clone(),
                    mspace: d.mspace,
                    binary: d.binary_dxf,
                }
            })
            .collect();
        // Pointer input need not serialize the entire user profile twice per frame. A final
        // delayed frame and pause/exit capture preferences after the last interaction.
        let preferences = if force_preferences || self.checkpoint_prefs.0.is_empty() || self.preferences_at.elapsed() >= Duration::from_millis(500) {
            self.preferences_at = Instant::now();
            Some((self.cad.prefs_json(), self.cad.profile_json()))
        } else {
            None
        };
        let prefs = preferences.as_ref().unwrap_or(&self.checkpoint_prefs);
        if key == self.checkpoint_key && self.cad.session.active == self.checkpoint_active && *prefs == self.checkpoint_prefs {
            return;
        }
        self.restored.retain(|uid, (doc, _)| self.cad.session.docs.iter().any(|s| s.uid == *uid && Arc::ptr_eq(doc, &s.doc)));
        self.snapshots.retain(|uid, _| self.cad.session.docs.iter().any(|d| d.uid == *uid));
        let snapshots = &mut self.snapshots;
        let docs = self
            .cad
            .session
            .docs
            .iter()
            .map(|d| {
                let view = d.paper_view();
                let content = ContentKey::of(d);
                let drawing = match snapshots.get(&d.uid).filter(|(old, _)| *old == content) {
                    Some((_, drawing)) => drawing.clone(),
                    None => {
                        // Own the outer snapshot: retaining the live document's Arc would make
                        // view changes trigger copy-on-write and invalidate otherwise unchanged geometry.
                        let drawing = Arc::new((*d.recovery_drawing()).clone());
                        snapshots.insert(d.uid, (content, drawing.clone()));
                        drawing
                    }
                };
                recovery::Document {
                    uid: d.uid,
                    drawing,
                    reuse: self.restored.get(&d.uid).map(|(_, file)| file.clone()),
                    record: json!({"title": d.title, "path": d.path, "dirty": d.is_dirty(), "space": d.space, "mspace": d.mspace.map(|h| h.0),
                    "center": [view.center.x, view.center.y], "height": view.height, "binaryDxf": d.binary_dxf}),
                }
            })
            .collect();
        self.writer.control.publish(recovery::Snapshot {
            docs,
            retained: self.retained.iter().flatten().cloned().collect(),
            active: self.cad.session.active,
            prefs: prefs.0.clone(),
            profile: prefs.1.clone(),
        });
        self.checkpoint_key = key;
        self.checkpoint_active = self.cad.session.active;
        if let Some(prefs) = preferences {
            self.checkpoint_prefs = prefs;
        }
    }

    fn request_name(&mut self) {
        if self.saving || self.name_uid.is_some() {
            self.cad.set_status(cadcraft_ui_egui::i18n::t("A file operation is already in progress"));
            return;
        }
        let Ok(state) = self.cad.session.state() else { return };
        let uid = state.uid;
        let name = filename(&state.title, self.cad.ui.save_format.as_str());
        self.name_uid = Some(uid);
        #[cfg(target_os = "android")]
        if let Err(error) = crate::platform::name(&name) {
            self.name_uid = None;
            self.cad.tablet_save_completed(uid, false);
            self.cad.set_status(error);
        }
        #[cfg(not(target_os = "android"))]
        {
            self.name_uid = None;
            self.start_job_for(Action::SaveAs, Some(name), uid);
        }
    }

    fn start_job(&mut self, action: Action) {
        if let Ok(state) = self.cad.session.state() {
            self.start_job_for(action, None, state.uid);
        }
    }

    fn start_job_for(&mut self, action: Action, name: Option<String>, uid: u64) {
        if self.saving {
            self.cad.tablet_save_completed(uid, false);
            self.cad.set_status(cadcraft_ui_egui::i18n::t("A file operation is already in progress"));
            return;
        }
        let Some(state) = self.cad.session.docs.iter().find(|d| d.uid == uid).cloned() else { return };
        let path = if action == Action::Save {
            state.path.clone().map(PathBuf::from)
        } else if action == Action::SaveAs {
            save_path(&self.dir, name.as_deref().unwrap_or(&state.title)).map(PathBuf::from)
        } else {
            let ext = if matches!(action, Action::Pdf | Action::SharePdf) {
                "pdf"
            } else {
                state
                    .path
                    .as_deref()
                    .and_then(|p| Path::new(p).extension())
                    .and_then(|e| e.to_str())
                    .filter(|e| e.eq_ignore_ascii_case("dwg"))
                    .unwrap_or("dxf")
            };
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
            let folder = self.dir.join("exports").join(stamp.to_string());
            std::fs::create_dir_all(&folder).ok().map(|_| folder.join(filename(&state.title, ext)))
        };
        let Some(path) = path else {
            self.cad.tablet_save_completed(uid, false);
            self.cad.set_status("Could not create the save file");
            return;
        };
        let job = jobs::Job {
            action,
            state,
            path,
            settings: self.cad.session.settings.clone(),
            version: self.cad.ui.save_version.clone(),
            viewport: self.cad.session.viewport_px,
        };
        match self.saves.submit(job) {
            Ok(()) => {
                self.saving = true;
                self.cad.set_status(cadcraft_ui_egui::i18n::t("Saving…"));
            }
            Err(error) => {
                self.cad.tablet_save_completed(uid, false);
                self.cad.set_status(error);
            }
        }
    }

    fn finish_jobs(&mut self) {
        while let Ok(done) = self.saves.receive.try_recv() {
            self.saving = false;
            match done.result {
                Ok((saved, path)) => {
                    self.cad.set_status(format!(
                        "{}: {}",
                        cadcraft_ui_egui::i18n::t("File saved"),
                        path.file_name().map_or_else(String::new, |s| s.to_string_lossy().into_owned())
                    ));
                    if matches!(done.action, Action::Save | Action::SaveAs) {
                        if let Some(current) = self.cad.session.docs.iter_mut().find(|d| d.uid == done.uid) {
                            jobs::acknowledge(current, &done.source, saved);
                        }
                        #[cfg(target_os = "android")]
                        if let Err(error) = crate::platform::sync_linked(&path.to_string_lossy()) {
                            self.cad.set_status(error);
                        }
                        self.cad.tablet_save_completed(done.uid, true);
                    } else {
                        #[cfg(target_os = "android")]
                        {
                            let name = path.file_name().map(|s| s.to_string_lossy()).unwrap_or_else(|| "Drawing.dxf".into());
                            let result = if matches!(done.action, Action::Share | Action::SharePdf) {
                                crate::platform::share(&path.to_string_lossy(), &name)
                            } else {
                                crate::platform::export(&path.to_string_lossy(), &name)
                            };
                            if let Err(error) = result {
                                self.cad.set_status(error);
                            }
                        }
                    }
                    self.files_refresh = true;
                }
                Err(error) => {
                    self.cad.tablet_save_completed(done.uid, false);
                    self.cad.set_status(format!("{}: {error}", cadcraft_ui_egui::i18n::t("Save failed")));
                }
            }
        }
    }

    fn file_action(&mut self) {
        let Some(action) = self.cad.tablet_file_action.take() else { return };
        match action {
            FileAction::Drawings => self.drawings_open = true,
            FileAction::Save if self.cad.session.state().is_ok_and(|d| d.path.is_none()) => self.request_name(),
            FileAction::Save => self.start_job(Action::Save),
            FileAction::SaveAs => self.request_name(),
            FileAction::Export => self.start_job(Action::Export),
            FileAction::Pdf => self.start_job(Action::Pdf),
            FileAction::Share => self.start_job(Action::Share),
            FileAction::SharePdf => self.start_job(Action::SharePdf),
            FileAction::Link | FileAction::Project => {
                #[cfg(target_os = "android")]
                if let Err(error) = crate::platform::pick_special(matches!(action, FileAction::Project)) {
                    self.cad.set_status(error);
                }
                #[cfg(not(target_os = "android"))]
                self.cad.set_status("This picker is available on Android");
            }
        }
    }

    fn drawings(&mut self, ctx: &egui::Context) {
        if !self.drawings_open {
            return;
        }
        if self.files_refresh || self.files_at.elapsed() > Duration::from_secs(30) {
            self.files = files(&self.dir);
            self.files_at = Instant::now();
            self.files_refresh = false;
        }
        let mut open = true;
        let mut selected = false;
        let mut restore = None;
        egui::Window::new(cadcraft_ui_egui::i18n::t("Drawings")).id(egui::Id::new("tablet-drawings")).open(&mut open).default_width(400.0).show(
            ctx,
            |ui| {
                if self.loader.is_some() || self.saving {
                    ui.spinner();
                }
                egui::ScrollArea::vertical().max_height(ctx.content_rect().height() * 0.65).show(ui, |ui| {
                    for (index, record) in self.retained.iter().enumerate().filter_map(|(i, r)| r.as_ref().map(|r| (i, r))) {
                        let title = record.get("title").and_then(Value::as_str).unwrap_or("Drawing");
                        let label = format!("{}: {title}", cadcraft_ui_egui::i18n::t("Restore drawing"));
                        if ui.add_enabled(self.loader.is_none(), egui::Button::new(label)).clicked() {
                            restore = Some((index, record.clone()));
                            selected = true;
                        }
                    }
                    if self.files.is_empty() && self.retained.iter().all(Option::is_none) {
                        ui.label(cadcraft_ui_egui::i18n::t("Open or save a drawing to see it here"));
                    }
                    for path in &self.files {
                        let label = path.file_name().map_or_else(String::new, |s| s.to_string_lossy().into_owned());
                        if ui.add_sized(egui::vec2(ui.available_width(), self.cad.tablet_prefs.height()), egui::Button::new(label)).clicked() {
                            if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf")) {
                                #[cfg(target_os = "android")]
                                {
                                    let name = path.file_name().map(|s| s.to_string_lossy()).unwrap_or_else(|| "Drawing.pdf".into());
                                    if let Err(error) = crate::platform::export(&path.to_string_lossy(), &name) {
                                        self.cad.set_status(error);
                                    }
                                }
                            } else {
                                self.cad.open_path(&path.to_string_lossy());
                            }
                            selected = true;
                        }
                    }
                    if ui
                        .add_sized(
                            egui::vec2(ui.available_width(), self.cad.tablet_prefs.height()),
                            egui::Button::new(cadcraft_ui_egui::i18n::t("Sample drawing")),
                        )
                        .clicked()
                    {
                        let _ = self.cad.run("ui.sample", json!({}));
                        selected = true;
                    }
                });
            },
        );
        if let Some((index, record)) = restore {
            #[cfg(target_os = "android")]
            let max_bytes = crate::platform::memory_budget().unwrap_or(64 << 20);
            #[cfg(not(target_os = "android"))]
            let max_bytes = 256 << 20;
            match recovery::load_record(self.dir.clone(), index, record, max_bytes, ctx.clone()) {
                Ok(loader) => self.loader = Some(loader),
                Err(error) => self.cad.set_status(error),
            }
        }
        self.drawings_open = open && !selected;
    }

    fn tune_cache(&mut self, force: bool) {
        if !force && self.requested_cache_mib == self.cad.tablet_prefs.cache_mib && self.memory_at.elapsed() < Duration::from_secs(5) {
            return;
        }
        // Retained rendering and history both follow system headroom. Exact live geometry is
        // never discarded; history has its own bounded allowance shared across open drawings.
        let requested = self.cad.tablet_prefs.cache_mib;
        self.requested_cache_mib = requested;
        #[cfg(target_os = "android")]
        let mib = crate::platform::memory_state()
            .map(|state| crate::memory::cache_mib(requested, crate::memory::resident_bytes(), state))
            .unwrap_or(requested);
        #[cfg(not(target_os = "android"))]
        let mib = requested;
        self.memory_at = Instant::now();
        self.cad.session.set_undo_memory_budget(((mib.saturating_mul(2).clamp(8, 64)) as usize) << 20);
        if force || self.cache_mib != mib {
            self.cache_mib = mib;
            let gpu = mib * 2 / 3;
            let glyph = mib / 8;
            let spatial = mib / 12;
            let snap = (mib / 16).min(2);
            self.cad.session.set_snap_cache_budget((snap as usize) << 20);
            cadcraft_engine::spatial::set_cache_budget((spatial as usize) << 20);
            self.cad.canvas.set_glyph_cache_budget((glyph as usize) << 20);
            self.cad.canvas.set_cache_budget(((mib - gpu - glyph - spatial - snap) as usize) << 20);
            if let Some(rs) = &self.render {
                cadcraft_ui_egui::gpu::set_cache_budget(rs, gpu);
            }
        }
    }

    pub fn trim_render_cache(&mut self) {
        self.cad.canvas.trim_render_cache();
        if let Some(rs) = &self.render {
            cadcraft_ui_egui::gpu::trim_cache(rs);
        }
    }

    #[cfg(target_os = "android")]
    fn poll_documents(&mut self, ctx: &egui::Context) {
        let language = self.cad.language();
        if self.native_language != Some(language) {
            let labels = cadcraft_ui_egui::i18n::with_language(language, || {
                let keys = [
                    "Command or value",
                    "Drawing name",
                    "Cancel",
                    "Enter",
                    "Save",
                    "Open",
                    "Export",
                    "Keyboard",
                    "Choose drawing",
                    "Drawing exported",
                    "Linked drawing saved",
                    "External save failed; local copy and backup kept",
                    "Read-only provider; opened a copy",
                    "File operation failed",
                    "The drawing has not been saved in the app",
                    "Could not prepare a shared file",
                    "The provider did not return a writable file",
                    "Choose a DXF or DWG drawing",
                    "Could not create the import folder",
                    "The provider did not return a readable file",
                    "Could not remember the linked drawing",
                    "Could not create a backup folder",
                    "Could not read the original for backup",
                    "Could not preserve the original backup",
                    "Could not create the project folder",
                    "Choose a folder containing DXF or DWG drawings",
                    "Project folders are nested too deeply",
                    "Could not read the project folder",
                    "Project contains too many files or folders",
                    "Could not create a project subfolder",
                    "Project contains duplicate file names",
                    "Could not read a project file",
                    "This file exceeds the available memory budget",
                ];
                let labels: serde_json::Map<String, Value> =
                    keys.into_iter().map(|key| (key.into(), json!(cadcraft_ui_egui::i18n::t(key)))).collect();
                Value::Object(labels).to_string()
            });
            if crate::platform::labels(&labels).is_ok() {
                self.native_language = Some(language);
            }
        }
        if std::mem::take(&mut self.cad.tablet_keyboard_requested) {
            let prompt = self.cad.session.current_prompt();
            let numeric = prompt.as_ref().is_some_and(|p| p.accept.number && !p.accept.point && p.keywords.is_empty());
            let title = prompt
                .map(|p| cadcraft_ui_egui::i18n::t(&p.message).to_owned())
                .unwrap_or_else(|| cadcraft_ui_egui::i18n::t("Command or value").to_owned());
            if let Err(error) = crate::platform::input(&self.cad.cmd.buffer, &title, numeric) {
                self.cad.set_status(error);
            }
        }
        if !crate::platform::has_events() {
            return;
        }
        for i in 0..32 {
            let mut any = false;
            if let Ok(Some(path)) = crate::platform::take("takeImportedPath") {
                self.cad.open_path(&path);
                self.files_refresh = true;
                any = true;
            }
            if let Ok(Some(message)) = crate::platform::take("takeNotice") {
                self.cad.set_status(message);
                any = true;
            }
            if let Ok(Some(command)) = crate::platform::take("takeCommand") {
                self.cad.cmd.buffer.clear();
                let _ = self.cad.cmdline(&command);
                any = true;
            }
            if let Ok(Some(event)) = crate::platform::take("takeEvent") {
                any = true;
                if let Ok(event) = serde_json::from_str::<Value>(&event) {
                    match event.get("kind").and_then(Value::as_str) {
                        Some("name") => {
                            if let Some(uid) = self.name_uid.take() {
                                if let Some(name) = event.get("value").and_then(Value::as_str) {
                                    self.start_job_for(Action::SaveAs, Some(name.into()), uid);
                                } else {
                                    self.cad.tablet_save_completed(uid, false);
                                }
                            }
                        }
                        Some("name-canceled") => {
                            if let Some(uid) = self.name_uid.take() {
                                self.cad.tablet_save_completed(uid, false);
                            }
                        }
                        Some("trim") => self.trim_render_cache(),
                        Some("locale") => {
                            self.cad.system_languages.clear();
                            if let Some(value) = event.get("value").and_then(Value::as_str) {
                                self.cad.system_languages.push(value.into());
                            }
                            self.native_language = None;
                        }
                        Some("back") => {
                            if egui::Popup::is_any_open(ctx) {
                                egui::Popup::close_all(ctx);
                            } else if self.drawings_open {
                                self.drawings_open = false;
                            } else if self.cad.tablet_settings_open {
                                self.cad.tablet_settings_open = false;
                            } else if self.cad.ui.dialog.take().is_some() {
                            } else if self.cad.closing.is_some() {
                                self.cad.answer_close(cadcraft_ui_egui::closing::Choice::Cancel);
                            } else if self.cad.canvas.tablet_pan {
                                self.cad.canvas.set_tablet_pan(false);
                            } else if self.cad.session.running.is_some() {
                                self.cad.session.cancel();
                                self.cad.cmd.buffer.clear();
                            } else {
                                self.checkpoint(false);
                                self.writer.control.flush();
                                let _ = crate::platform::background();
                            }
                        }
                        _ => {}
                    }
                }
            }
            if !any {
                break;
            }
            if i == 31 {
                crate::platform::wake(0);
            }
        }
    }
}

impl eframe::App for Shell {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        self.cad.canvas.filter_tablet_input(ctx, raw, &self.cad.tablet_prefs);
        self.cad.raw_input_hook(raw);
        cadcraft_ui_egui::cmdline::capture_tab(ctx, raw);
        if !raw.events.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        cadcraft_ui_egui::i18n::with_language(self.cad.language(), || {
            self.restore_tick();
            self.finish_jobs();
            #[cfg(target_os = "android")]
            self.poll_documents(ctx);
            self.file_action();
            self.cad.logic(ctx);
            // Logic may run without a visible UI pass (pause or a native picker). Capture edits
            // here too; unchanged content reuses its snapshot and does not serialize preferences.
            self.checkpoint(false);
            if !ctx.input(|i| i.focused) {
                self.checkpoint(true);
                self.writer.control.flush();
            }
            self.tune_cache(false);
            if std::mem::take(&mut self.cad.quit_requested) {
                self.checkpoint(true);
                self.writer.control.flush();
                #[cfg(target_os = "android")]
                let _ = crate::platform::background();
                #[cfg(not(target_os = "android"))]
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        cadcraft_ui_egui::i18n::with_language(self.cad.language(), || {
            self.cad.ui(ui);
            self.file_action();
            self.drawings(ui.ctx());
            self.checkpoint(false);
        });
    }
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        storage.set_string(cadcraft_ui_egui::PREFS_KEY, self.cad.prefs_json());
        storage.set_string(cadcraft_ui_egui::options::PROFILE_KEY, self.cad.profile_json());
    }
    fn auto_save_interval(&self) -> Duration {
        Duration::from_secs(5)
    }
    fn persist_egui_memory(&self) -> bool {
        false
    }
    fn on_exit(&mut self) {
        self.checkpoint(true);
        self.writer.control.flush();
    }
}
