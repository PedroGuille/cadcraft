//! Cleanup for disposable conversion files left by older app versions.
//! Direct DWG imports never create these files or require writable temporary storage.

use std::path::PathBuf;

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
}
