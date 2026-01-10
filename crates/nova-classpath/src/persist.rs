use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{ClasspathClassStub, ClasspathEntry, ClasspathError, ClasspathFingerprint};

const CACHE_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct EntryCacheFile {
    version: u32,
    fingerprint: ClasspathFingerprint,
    entry: ClasspathEntry,
    stubs: Vec<ClasspathClassStub>,
}

pub fn load_or_build_entry<F>(
    cache_dir: &Path,
    entry: &ClasspathEntry,
    fingerprint: ClasspathFingerprint,
    build: F,
) -> Result<Vec<ClasspathClassStub>, ClasspathError>
where
    F: FnOnce() -> Result<Vec<ClasspathClassStub>, ClasspathError>,
{
    std::fs::create_dir_all(cache_dir)?;

    let cache_path = cache_file_path(cache_dir, fingerprint);
    if let Ok(bytes) = std::fs::read(&cache_path) {
        if let Ok(file) = bincode::deserialize::<EntryCacheFile>(&bytes) {
            if file.version == CACHE_VERSION && file.fingerprint == fingerprint && file.entry == *entry
            {
                return Ok(file.stubs);
            }
        }
    }

    let stubs = build()?;

    let file = EntryCacheFile {
        version: CACHE_VERSION,
        fingerprint,
        entry: entry.clone(),
        stubs: stubs.clone(),
    };
    let bytes = bincode::serialize(&file)?;
    std::fs::write(&cache_path, bytes)?;
    Ok(stubs)
}

fn cache_file_path(cache_dir: &Path, fingerprint: ClasspathFingerprint) -> PathBuf {
    cache_dir.join(format!("classpath-entry-{}.bin", fingerprint.to_hex()))
}

