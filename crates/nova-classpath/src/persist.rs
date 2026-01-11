use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::{ClasspathClassStub, ClasspathEntry, ClasspathError, ClasspathFingerprint};

const CACHE_VERSION: u32 = 1;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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
            if file.version == CACHE_VERSION
                && file.fingerprint == fingerprint
                && file.entry == *entry
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
    atomic_write(&cache_path, &bytes)?;
    Ok(stubs)
}

fn cache_file_path(cache_dir: &Path, fingerprint: ClasspathFingerprint) -> PathBuf {
    cache_dir.join(format!("classpath-entry-{}.bin", fingerprint.to_hex()))
}

fn atomic_write(dest: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };

    fs::create_dir_all(parent)?;

    let (tmp_path, mut file) = open_unique_tmp_file(dest, parent)?;
    let write_result = (|| -> io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(err) = write_result {
        drop(file);
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }
    drop(file);

    if let Err(err) = rename_overwrite(&tmp_path, dest) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }

    #[cfg(unix)]
    {
        let _ = fs::File::open(parent).and_then(|dir| dir.sync_all());
    }

    Ok(())
}

fn open_unique_tmp_file(dest: &Path, parent: &Path) -> io::Result<(PathBuf, fs::File)> {
    let file_name = dest
        .file_name()
        .ok_or_else(|| io::Error::other("destination path has no file name"))?;
    let pid = std::process::id();

    loop {
        let counter = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut tmp_name = file_name.to_os_string();
        tmp_name.push(format!(".tmp.{pid}.{counter}"));
        let tmp_path = parent.join(tmp_name);

        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
        {
            Ok(file) => return Ok((tmp_path, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
}

fn rename_overwrite(src: &Path, dest: &Path) -> io::Result<()> {
    const MAX_RENAME_ATTEMPTS: usize = 1024;
    let mut attempts = 0usize;

    loop {
        match fs::rename(src, dest) {
            Ok(()) => return Ok(()),
            Err(err)
                if cfg!(windows)
                    && (err.kind() == io::ErrorKind::AlreadyExists || dest.exists()) =>
            {
                match fs::remove_file(dest) {
                    Ok(()) => {}
                    Err(remove_err) if remove_err.kind() == io::ErrorKind::NotFound => {}
                    Err(remove_err) => return Err(remove_err),
                }

                attempts += 1;
                if attempts >= MAX_RENAME_ATTEMPTS {
                    return Err(err);
                }
                continue;
            }
            Err(err) => return Err(err),
        }
    }
}
