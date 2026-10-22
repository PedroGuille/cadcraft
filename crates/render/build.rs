//! Invalidate disposable geometry when renderer sources or locked dependencies change.
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::Path,
};

fn sources(path: &Path, hash: &mut DefaultHasher, depth: usize) -> std::io::Result<()> {
    if depth > 32 {
        return Ok(());
    }
    let mut entries: Vec<_> = std::fs::read_dir(path)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries.into_iter().take(4096) {
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            sources(&path, hash, depth + 1)?;
        } else if kind.is_file() {
            path.hash(hash);
            std::fs::read(path)?.hash(hash);
        }
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    println!("cargo:rerun-if-changed=../../Cargo.lock");
    let mut hash = DefaultHasher::new();
    for path in ["src", "../geom/src", "../doc/src", "../color/src"] {
        println!("cargo:rerun-if-changed={path}");
        sources(Path::new(path), &mut hash, 0)?;
    }
    std::env::var("TARGET").unwrap_or_default().hash(&mut hash);
    std::env::var("OPT_LEVEL").unwrap_or_default().hash(&mut hash);
    std::fs::read("../../Cargo.lock")?.hash(&mut hash);
    println!("cargo:rustc-env=CADCRAFT_GEOMETRY_CACHE_BUILD={:016x}", hash.finish());
    Ok(())
}
