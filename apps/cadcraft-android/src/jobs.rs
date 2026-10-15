//! Bounded background saves. The UI acknowledges the exact version written by a job.
#![forbid(unsafe_code)]

use cadcraft_engine::{DocState, Session, Settings};
use serde_json::json;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{self, Receiver, SyncSender},
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Save,
    SaveAs,
    Export,
    Pdf,
    Share,
    SharePdf,
}

pub struct Job {
    pub action: Action,
    pub state: DocState,
    pub path: PathBuf,
    pub settings: Settings,
    pub version: String,
    pub viewport: (f64, f64),
}

pub struct Finished {
    pub action: Action,
    pub uid: u64,
    pub source: Arc<cadcraft_engine::doc::Drawing>,
    pub result: Result<(DocState, PathBuf), String>,
}

/// Keep newer geometry dirty while retaining the format chosen by the completed save.
pub fn acknowledge(current: &mut DocState, source: &Arc<cadcraft_engine::doc::Drawing>, saved: DocState) {
    if Arc::ptr_eq(current.drawing(), source) {
        *current.drawing_arc_mut() = saved.drawing().clone();
    } else if current.drawing().header.get("ACADVER") == source.header.get("ACADVER")
        && let Some(version) = saved.drawing().header.get("ACADVER").and_then(cadcraft_engine::doc::HVal::as_str)
        && current.drawing().header.get("ACADVER").and_then(cadcraft_engine::doc::HVal::as_str) != Some(version)
    {
        Arc::make_mut(current.drawing_arc_mut()).header.set_str("ACADVER", version);
        current.revision = current.revision.saturating_add(1);
    }
    current.saved = saved.saved;
    current.path = saved.path;
    current.title = saved.title;
    current.binary_dxf = saved.binary_dxf;
}

pub struct Worker {
    send: SyncSender<Job>,
    pub receive: Receiver<Finished>,
}

impl Worker {
    pub fn start(ctx: egui::Context) -> Result<Self, String> {
        let (send, jobs) = mpsc::sync_channel::<Job>(1);
        let (done, receive) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("tablet-save".into())
            .spawn(move || {
                while let Ok(job) = jobs.recv() {
                    let uid = job.state.uid;
                    let source = job.state.drawing().clone();
                    let action = job.action;
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| execute(job)))
                        .unwrap_or_else(|_| Err("The file writer stopped unexpectedly; your drawing remains open".into()));
                    if done.send(Finished { action, uid, source, result }).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { send, receive })
    }

    pub fn submit(&self, job: Job) -> Result<(), String> {
        self.send.try_send(job).map_err(|_| "The file writer is busy".into())
    }
}

fn execute(job: Job) -> Result<(DocState, PathBuf), String> {
    let mut session = Session::empty();
    session.docs.push(job.state);
    session.settings = job.settings;
    session.viewport_px = job.viewport;
    let path = job.path.to_string_lossy();
    match job.action {
        Action::Pdf | Action::SharePdf => session.execute("exportpdf", &json!({"path": path})),
        Action::SaveAs => session.execute("saveas", &json!({"path": path, "version": job.version})),
        _ => session.execute("qsave", &json!({"path": path})),
    }
    .map_err(|e| e.to_string())?;
    let state = session.docs.into_iter().next().ok_or("The file writer did not return a drawing")?;
    Ok((state, job.path))
}
