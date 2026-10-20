//! Host-private, disposable DWG conversion storage. No original or recovery drawing is touched.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{IoError, Result};

static DIRECTORY: OnceLock<PathBuf> = OnceLock::new();
static NEXT: AtomicU64 = AtomicU64::new(1);

pub(super) fn set_directory(directory: PathBuf) {
    // A process killed during conversion leaves only these disposable files. Remove old work
    // at startup; imported source drawings and recovery snapshots live in other directories.
    if let Ok(entries) = std::fs::read_dir(&directory) {
        for entry in entries.filter_map(std::result::Result::ok) {
            if entry.file_type().is_ok_and(|t| t.is_file())
                && entry.file_name().to_str().is_some_and(|n| n.starts_with("cadcraft-work-") && n.ends_with(".dxf"))
                && entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|t| t.as_secs() > 86_400)
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let _ = DIRECTORY.set(directory);
}

struct WorkFile {
    path: PathBuf,
    file: File,
}

impl WorkFile {
    fn new() -> Result<Self> {
        let directory = DIRECTORY.get().cloned().unwrap_or_else(|| std::env::temp_dir().join("cadcraft-import-work"));
        std::fs::create_dir_all(&directory).map_err(storage_error)?;
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let path = directory.join(format!("cadcraft-work-{}-{stamp}-{}.dxf", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path).map_err(storage_error)?;
        Ok(Self { path, file })
    }
}

impl Drop for WorkFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn storage_error(error: impl std::fmt::Display) -> IoError {
    IoError::Format(format!("DWG import needs writable temporary storage: {error}"))
}

pub(super) fn read_dwg(bytes: &[u8]) -> Result<cadcraft_doc::Drawing> {
    let mut work = WorkFile::new()?;
    let (notes, header) = {
        let mut output = BufWriter::with_capacity(64 << 10, &mut work.file);
        let result = cadcraft_dwg::dwg_to_dxf_writer(bytes, cadcraft_dwg::Limits::DEFAULT, &mut output).map_err(IoError::Format)?;
        output.flush().map_err(storage_error)?;
        result
    };
    work.file.seek(SeekFrom::Start(0)).map_err(storage_error)?;
    let mut drawing = super::dxf_read::streaming::read(BufReader::with_capacity(64 << 10, &mut work.file), &header)?;
    drawing.read_notes = notes;
    Ok(drawing)
}
