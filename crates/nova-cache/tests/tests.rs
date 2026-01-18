mod suite;

use nova_cache::{
    CacheDir, Fingerprint, CACHE_METADATA_BIN_FILENAME, CACHE_METADATA_JSON_FILENAME,
    CACHE_PACKAGE_MANIFEST_PATH,
};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|err| err.into_inner())
}

fn collect_cache_files(root: &Path) -> Result<Vec<PathBuf>, nova_cache::CacheError> {
    let mut files = Vec::new();

    let metadata_path = root.join(CACHE_METADATA_JSON_FILENAME);
    if metadata_path.is_file() {
        files.push(PathBuf::from(CACHE_METADATA_JSON_FILENAME));
    } else {
        return Err(nova_cache::CacheError::MissingArchiveEntry {
            path: CACHE_METADATA_JSON_FILENAME,
        });
    }

    let metadata_bin = root.join(CACHE_METADATA_BIN_FILENAME);
    if metadata_bin.is_file() {
        files.push(PathBuf::from(CACHE_METADATA_BIN_FILENAME));
    }

    for component_dir in ["indexes", "queries", "ast"] {
        let path = root.join(component_dir);
        if !path.is_dir() {
            continue;
        }

        for entry in walkdir::WalkDir::new(&path).follow_links(false) {
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }

            let file_name = entry.file_name().to_string_lossy();
            if file_name.ends_with(".tmp") || file_name.contains(".tmp.") {
                continue;
            }

            let rel = entry.path().strip_prefix(root).map_err(|_| {
                nova_cache::CacheError::InvalidArchivePath {
                    path: entry.path().to_path_buf(),
                }
            })?;
            files.push(rel.to_path_buf());
        }
    }

    files.sort();
    Ok(files)
}

/// Test helper that mirrors `nova_cache::pack_cache_package`, but uses a lower zstd compression
/// level to keep peak address-space usage within the agent harness limits (RLIMIT_AS).
fn pack_cache_package_low_mem(
    cache_dir: &CacheDir,
    out_file: &Path,
) -> Result<(), nova_cache::CacheError> {
    let root = cache_dir.root();
    let files = collect_cache_files(root)?;

    let mut manifest: BTreeMap<String, String> = BTreeMap::new();
    for rel in &files {
        let disk_path = root.join(rel);
        let fingerprint = Fingerprint::from_file(&disk_path)?;
        manifest.insert(
            rel.to_string_lossy().replace('\\', "/"),
            fingerprint.as_str().to_string(),
        );
    }

    let parent = out_file.parent().unwrap_or(Path::new("."));
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    std::fs::create_dir_all(parent)?;

    let out = File::create(out_file)?;
    let encoder = zstd::Encoder::new(out, 1)?;
    let mut builder = tar::Builder::new(encoder);

    let manifest_json = serde_json::to_vec_pretty(&manifest)?;
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest_json.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append_data(
        &mut header,
        CACHE_PACKAGE_MANIFEST_PATH,
        Cursor::new(manifest_json),
    )?;

    let metadata_json = PathBuf::from(CACHE_METADATA_JSON_FILENAME);
    let metadata_bin = PathBuf::from(CACHE_METADATA_BIN_FILENAME);
    let include_bin = files.iter().any(|p| p == &metadata_bin);

    for rel in [&metadata_json, &metadata_bin] {
        if rel == &metadata_bin && !include_bin {
            continue;
        }
        let disk_path = root.join(rel);
        let rel_string = rel.to_string_lossy().replace('\\', "/");
        builder.append_path_with_name(&disk_path, &rel_string)?;
    }

    for rel in &files {
        if rel == &metadata_json || rel == &metadata_bin {
            continue;
        }
        let disk_path = root.join(rel);
        let rel_string = rel.to_string_lossy().replace('\\', "/");
        builder.append_path_with_name(&disk_path, &rel_string)?;
    }

    let encoder = builder.into_inner()?;
    encoder.finish()?;
    Ok(())
}
