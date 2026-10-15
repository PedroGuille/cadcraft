//! Per-drawing recovery with durable publication, retained failures and a previous valid index.
#![forbid(unsafe_code)]

use cadcraft_engine::{DocState, Session};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    io::Write,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

const MAX_INDEX: u64 = 1 << 20;
const MAX_DRAWINGS: usize = 256;

pub enum Loaded {
    Index(Vec<Value>),
    Drawing { index: usize, state: Box<DocState>, file: String },
    Notice(String),
    Done,
}

fn read_index(path: &Path) -> Result<Value, String> {
    if std::fs::metadata(path).map_err(|e| e.to_string())?.len() > MAX_INDEX {
        return Err("Recovery index is too large".into());
    }
    let value: Value = serde_json::from_slice(&std::fs::read(path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    if !value.get("drawings").and_then(Value::as_array).is_some_and(|a| a.len() <= MAX_DRAWINGS) {
        return Err("Invalid recovery index".into());
    }
    Ok(value)
}

pub fn load(dir: PathBuf, max_bytes: u64, ctx: egui::Context) -> Result<Receiver<Loaded>, String> {
    let (send, receive) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("tablet-restore".into())
        .spawn(move || {
            let emit = |event| {
                let ok = send.send(event).is_ok();
                ctx.request_repaint();
                ok
            };
            let root = dir.join("tablet-recovery");
            let index = root.join("session.json");
            let data = match read_index(&index) {
                Ok(data) => data,
                Err(error) if index.exists() => {
                    let _ = std::fs::write(root.join("keep-snapshots"), error.as_bytes());
                    if !emit(Loaded::Notice(format!("Recovery: {error}; recovery copies have been kept"))) {
                        return;
                    }
                    match read_index(&root.join("session.previous.json")) {
                        Ok(data) => data,
                        Err(_) => {
                            emit(Loaded::Done);
                            return;
                        }
                    }
                }
                Err(_) => {
                    emit(Loaded::Done);
                    return;
                }
            };
            let records = data.get("drawings").and_then(Value::as_array).cloned().unwrap_or_default();
            if !emit(Loaded::Index(records.clone())) {
                return;
            }
            let active = data.get("active").and_then(Value::as_u64).and_then(|i| usize::try_from(i).ok()).unwrap_or(0);
            let mut order: Vec<_> = (0..records.len()).collect();
            order.sort_by_key(|i| (*i != active, *i));
            // Restore the active drawing first. Other records stay in the recovery index and
            // are opened on demand, so background tabs cannot consume the whole session budget.
            let remaining = max_bytes;
            for index in order {
                let Some(record) = records.get(index) else { continue };
                let file = record.get("file").and_then(Value::as_str).unwrap_or_default();
                let result = load_one(&dir, &root, record, remaining);
                match result {
                    Ok((state, _bytes)) => {
                        if !emit(Loaded::Drawing { index, state: Box::new(state), file: file.into() }) {
                            return;
                        }
                        break;
                    }
                    Err(error) => {
                        if !emit(Loaded::Notice(format!("Recovery: {file}: {error}; the recovery copy has been kept"))) {
                            return;
                        }
                    }
                }
            }
            emit(Loaded::Done);
        })
        .map_err(|e| e.to_string())?;
    Ok(receive)
}

/// Decode one retained recovery record selected in Drawings, without loading the other tabs.
pub fn load_record(dir: PathBuf, index: usize, record: Value, max_bytes: u64, ctx: egui::Context) -> Result<Receiver<Loaded>, String> {
    let (send, receive) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("tablet-restore".into())
        .spawn(move || {
            let root = dir.join("tablet-recovery");
            let file = record.get("file").and_then(Value::as_str).unwrap_or_default().to_owned();
            let event = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| load_one(&dir, &root, &record, max_bytes))) {
                Ok(Ok((state, _))) => Loaded::Drawing { index, state: Box::new(state), file },
                Ok(Err(error)) => Loaded::Notice(format!("Recovery: {error}; the recovery copy has been kept")),
                Err(_) => Loaded::Notice("Recovery stopped unexpectedly; the recovery copy has been kept".into()),
            };
            if send.send(event).is_ok() {
                ctx.request_repaint();
                let _ = send.send(Loaded::Done);
                ctx.request_repaint();
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(receive)
}

fn load_one(dir: &Path, root: &Path, record: &Value, max_bytes: u64) -> Result<(DocState, u64), String> {
    let file = record.get("file").and_then(Value::as_str).ok_or("Missing recovery file")?;
    if file.is_empty() || !Path::new(file).components().all(|c| matches!(c, Component::Normal(_))) {
        return Err("Invalid recovery path".into());
    }
    let path = root.join(file);
    let bytes = std::fs::metadata(&path).map_err(|e| e.to_string())?.len();
    if bytes > max_bytes {
        return Err("The recovery file exceeds the available restore budget".into());
    }
    let mut session = Session::empty();
    session.execute("open", &json!({"path": path.to_string_lossy()})).map_err(|e| e.to_string())?;
    let original = record
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| {
            let path = Path::new(p);
            path.is_absolute() && path.starts_with(dir) && !path.components().any(|c| matches!(c, Component::ParentDir))
        })
        .map(str::to_owned);
    if let Ok(state) = session.state_mut() {
        state.path = original;
    }
    // Resolve relative references from the imported project rather than the snapshot folder.
    let _ = session.execute("-xref", &json!({"option": "reload", "names": "*"}));
    let mut state = session.docs.into_iter().next().ok_or("Recovery did not return a drawing")?;
    state.title = record.get("title").and_then(Value::as_str).unwrap_or("Drawing").chars().take(180).collect();
    state.saved = state.doc.clone();
    if record.get("dirty").and_then(Value::as_bool) == Some(true) {
        state.saved = Arc::new((*state.doc).clone());
    }
    state.binary_dxf = record.get("binaryDxf").and_then(Value::as_bool).unwrap_or(false);
    if let Some(space) = record.get("space").and_then(|s| serde_json::from_value::<cadcraft_engine::doc::Space>(s.clone()).ok())
        && match &space {
            cadcraft_engine::doc::Space::Model => true,
            cadcraft_engine::doc::Space::Paper(name) => state.doc.layout(name).is_some(),
        }
    {
        state.space = space;
    }
    state.mspace = record
        .get("mspace")
        .and_then(Value::as_u64)
        .map(cadcraft_engine::doc::Handle)
        .filter(|h| state.doc.entity(*h).is_some_and(|e| matches!(e.kind, cadcraft_engine::doc::EntityKind::Viewport(_))));
    if let (Some(x), Some(y), Some(height)) = (
        record.pointer("/center/0").and_then(Value::as_f64),
        record.pointer("/center/1").and_then(Value::as_f64),
        record.get("height").and_then(Value::as_f64),
    ) && x.is_finite()
        && y.is_finite()
        && height.is_finite()
        && height > 1e-12
    {
        let view = cadcraft_engine::View { center: cadcraft_engine::geom::Vec2::new(x, y), height };
        state.views.retain(|(space, _)| *space != state.space);
        state.views.push((state.space.clone(), view));
    }
    Ok((state, bytes))
}

pub struct Document {
    pub uid: u64,
    pub drawing: Arc<cadcraft_engine::doc::Drawing>,
    pub record: Value,
    pub reuse: Option<String>,
}
pub struct Snapshot {
    pub docs: Vec<Document>,
    pub retained: Vec<Value>,
    pub active: usize,
    pub prefs: String,
    pub profile: String,
}

struct Pending {
    latest: Option<Snapshot>,
    force: bool,
    stop: bool,
    interval: Duration,
}
#[derive(Clone)]
pub struct Control(Arc<(Mutex<Pending>, Condvar)>);
impl Control {
    pub fn publish(&self, snapshot: Snapshot) {
        if let Ok(mut p) = self.0.0.lock() {
            p.latest = Some(snapshot);
            self.0.1.notify_one();
        }
    }
    pub fn flush(&self) {
        if let Ok(mut p) = self.0.0.lock() {
            p.force = true;
            self.0.1.notify_one();
        }
    }
    pub fn set_interval(&self, seconds: u64) {
        if let Ok(mut p) = self.0.0.lock() {
            let interval = Duration::from_secs(seconds.clamp(2, 60));
            if p.interval != interval {
                p.interval = interval;
                self.0.1.notify_one();
            }
        }
    }
}

pub struct Writer {
    pub control: Control,
    pub notices: Receiver<Result<(), String>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Writer {
    pub fn start(dir: PathBuf, ctx: egui::Context) -> Result<Self, String> {
        let control =
            Control(Arc::new((Mutex::new(Pending { latest: None, force: false, stop: false, interval: Duration::from_secs(5) }), Condvar::new())));
        let state = control.clone();
        let (send, notices) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("tablet-recovery".into())
            .spawn(move || {
                let mut stored = HashMap::new();
                let mut preferences = None;
                let mut written = Instant::now();
                loop {
                    let Ok(mut p) = state.0.0.lock() else { break };
                    while !p.stop && (p.latest.is_none() || (!p.force && written.elapsed() < p.interval)) {
                        if p.latest.is_none() {
                            let Ok(next) = state.0.1.wait(p) else { return };
                            p = next;
                        } else {
                            let delay = p.interval.saturating_sub(written.elapsed());
                            let Ok((next, _)) = state.0.1.wait_timeout(p, delay) else { return };
                            p = next;
                        }
                    }
                    let stop = p.stop;
                    let snapshot = p.latest.take();
                    p.force = false;
                    drop(p);
                    if let Some(snapshot) = snapshot {
                        let result =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| checkpoint(&dir, snapshot, &mut stored, &mut preferences)))
                                .unwrap_or_else(|_| Err("The recovery writer stopped unexpectedly; previous copies were kept".into()));
                        let _ = send.send(result);
                        ctx.request_repaint();
                        written = Instant::now();
                    }
                    if stop {
                        break;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { control, notices, thread: Some(thread) })
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        if let Ok(mut p) = self.control.0.0.lock() {
            p.stop = true;
            self.control.0.1.notify_one();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temp = path.with_extension("tmp");
    let mut out = std::fs::File::create(&temp).map_err(|e| e.to_string())?;
    out.write_all(bytes).and_then(|_| out.sync_all()).map_err(|e| e.to_string())?;
    std::fs::rename(temp, path).map_err(|e| e.to_string())
}

fn sync_dir(path: &Path) -> Result<(), String> {
    std::fs::File::open(path).and_then(|f| f.sync_all()).map_err(|e| e.to_string())
}

type Stored = HashMap<u64, (Arc<cadcraft_engine::doc::Drawing>, String)>;
fn checkpoint(dir: &Path, snapshot: Snapshot, stored: &mut Stored, preferences: &mut Option<(String, String)>) -> Result<(), String> {
    let root = dir.join("tablet-recovery");
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| e.to_string())?.as_nanos();
    let generation = format!("snapshot-{stamp}");
    let folder = root.join(&generation);
    let mut created = false;
    let mut records = Vec::new();
    let uids: HashSet<_> = snapshot.docs.iter().map(|d| d.uid).collect();
    stored.retain(|uid, _| uids.contains(uid));
    for (i, document) in snapshot.docs.into_iter().enumerate() {
        let mut record = document.record;
        let reuse = stored
            .get(&document.uid)
            .filter(|(drawing, file)| Arc::ptr_eq(drawing, &document.drawing) && root.join(file).is_file())
            .map(|(_, f)| f.clone())
            .or(document.reuse.filter(|file| {
                !file.is_empty() && Path::new(file).components().all(|c| matches!(c, Component::Normal(_))) && root.join(file).is_file()
            }));
        let file = if let Some(file) = reuse {
            file
        } else {
            if !created {
                std::fs::create_dir(&folder).map_err(|e| e.to_string())?;
                created = true;
            }
            let name = format!("drawing-{i}.dxf");
            let bytes = cadcraft_io::write(&document.drawing, &name).map_err(|e| e.to_string())?;
            atomic(&folder.join(&name), &bytes)?;
            format!("{generation}/{name}")
        };
        stored.insert(document.uid, (document.drawing, file.clone()));
        record["file"] = json!(file);
        records.push(record);
    }
    records.extend(snapshot.retained);
    if records.len() > MAX_DRAWINGS {
        return Err("Recovery contains too many drawings; the previous index was kept".into());
    }
    if created {
        sync_dir(&folder)?;
    }
    let index = root.join("session.json");
    if index.exists() {
        match read_index(&index) {
            Ok(old) => atomic(&root.join("session.previous.json"), &serde_json::to_vec(&old).map_err(|e| e.to_string())?)?,
            Err(_) => {
                let _ = std::fs::write(root.join("keep-snapshots"), b"Unrecognized recovery index");
                std::fs::copy(&index, root.join(format!("session-protected-{stamp}.json"))).map_err(|e| e.to_string())?;
            }
        }
    }
    let next_preferences = (snapshot.prefs, snapshot.profile);
    if preferences.as_ref() != Some(&next_preferences) {
        atomic(
            &dir.join("tablet-preferences.json"),
            &serde_json::to_vec(&json!({"prefs": next_preferences.0, "profile": next_preferences.1})).map_err(|e| e.to_string())?,
        )?;
        *preferences = Some(next_preferences);
    }
    let index_bytes = serde_json::to_vec(&json!({"active": snapshot.active, "drawings": records})).map_err(|e| e.to_string())?;
    if index_bytes.len() as u64 > MAX_INDEX {
        return Err("Recovery index is too large; the previous index was kept".into());
    }
    sync_dir(&root)?;
    atomic(&index, &index_bytes)?;
    sync_dir(&root)?;
    if !root.join("keep-snapshots").exists() {
        let mut keep = HashSet::new();
        for index in [&index, &root.join("session.previous.json")] {
            if let Ok(data) = read_index(index)
                && let Some(records) = data.get("drawings").and_then(Value::as_array)
            {
                for record in records {
                    if let Some(generation) = record.get("file").and_then(Value::as_str).and_then(|f| f.split('/').next()) {
                        keep.insert(generation.to_owned());
                    }
                }
            }
        }
        for entry in std::fs::read_dir(&root).map_err(|e| e.to_string())?.take(1000).filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("snapshot-") && !keep.contains(&name) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    Ok(())
}
