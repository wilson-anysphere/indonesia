use crate::error::{CacheError, Result};
use crate::fingerprint::Fingerprint;
use crate::{CacheDir, CacheMetadata, ProjectSnapshot};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::{Component, Path, PathBuf};
use tar::EntryType;

use crate::store::store_for_url;

pub const CACHE_PACKAGE_MANIFEST_PATH: &str = "checksums.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CachePackageInstallOutcome {
    /// Package was installed with `metadata.json`, `indexes/`, `queries/`, and `ast/` (if present).
    Full,
    /// Only `metadata.json` and `indexes/` were installed because the local project fingerprints
    /// didn't match what the package was built against.
    IndexesOnly { mismatched_files: usize },
}

pub fn pack_cache_package(cache_dir: &CacheDir, out_file: &Path) -> Result<()> {
    let root = cache_dir.root();
    let files = collect_cache_files(root)?;

    let mut manifest: BTreeMap<String, String> = BTreeMap::new();
    for rel in &files {
        let disk_path = root.join(rel);
        let fingerprint = Fingerprint::from_file(&disk_path)?;
        manifest.insert(rel.to_string_lossy().replace('\\', "/"), fingerprint.as_str().to_string());
    }

    if let Some(parent) = out_file.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let out = File::create(out_file)?;
    let encoder = zstd::Encoder::new(out, 19)?;
    let mut builder = tar::Builder::new(encoder);

    for rel in &files {
        let disk_path = root.join(rel);
        let rel_string = rel.to_string_lossy().replace('\\', "/");
        builder.append_path_with_name(&disk_path, &rel_string)?;
    }

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

    let encoder = builder.into_inner()?;
    encoder.finish()?;
    Ok(())
}

pub fn install_cache_package(
    cache_dir: &CacheDir,
    package_file: &Path,
) -> Result<CachePackageInstallOutcome> {
    let (metadata, manifest) = read_metadata_and_manifest(package_file)?;
    metadata.ensure_compatible()?;

    if metadata.project_hash != *cache_dir.project_hash() {
        return Err(CacheError::IncompatibleProjectHash {
            expected: cache_dir.project_hash().as_str().to_string(),
            found: metadata.project_hash.as_str().to_string(),
        });
    }

    let (full_install, mismatched_files) = fingerprints_match(cache_dir, &metadata);

    // Extract into a temp dir first to ensure either the old cache or the new cache is usable.
    let parent = cache_dir
        .root()
        .parent()
        .ok_or_else(|| CacheError::InvalidArchivePath {
            path: cache_dir.root().to_path_buf(),
        })?;
    let temp_dir = tempfile::Builder::new()
        .prefix("nova-cache-install-")
        .tempdir_in(parent)?;

    extract_selected(package_file, &manifest, temp_dir.path(), full_install)?;

    if full_install {
        replace_dir_atomically(temp_dir.path(), cache_dir.root())?;
        Ok(CachePackageInstallOutcome::Full)
    } else {
        install_indexes_only(temp_dir.path(), cache_dir.root())?;
        Ok(CachePackageInstallOutcome::IndexesOnly { mismatched_files })
    }
}

pub fn fetch_cache_package(cache_dir: &CacheDir, url: &str) -> Result<CachePackageInstallOutcome> {
    let store = store_for_url(url)?;

    let parent = cache_dir
        .root()
        .parent()
        .ok_or_else(|| CacheError::InvalidArchivePath {
            path: cache_dir.root().to_path_buf(),
        })?;
    let temp = tempfile::Builder::new()
        .prefix("nova-cache-")
        .suffix(".tar.zst")
        .tempfile_in(parent)?;

    store.fetch(url, temp.path())?;
    install_cache_package(cache_dir, temp.path())
}

fn collect_cache_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();

    let metadata_path = root.join("metadata.json");
    if metadata_path.is_file() {
        files.push(PathBuf::from("metadata.json"));
    } else {
        return Err(CacheError::MissingArchiveEntry {
            path: "metadata.json",
        });
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

            let rel = entry.path().strip_prefix(root).map_err(|_| {
                CacheError::InvalidArchivePath {
                    path: entry.path().to_path_buf(),
                }
            })?;
            files.push(rel.to_path_buf());
        }
    }

    files.sort();
    Ok(files)
}

fn read_metadata_and_manifest(
    package_file: &Path,
) -> Result<(CacheMetadata, BTreeMap<String, String>)> {
    let mut metadata_bytes = None;
    let mut manifest_bytes = None;

    let file = File::open(package_file)?;
    let decoder = zstd::Decoder::new(file)?;
    let mut archive = tar::Archive::new(decoder);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        validate_archive_relative_path(&path)?;
        let path_string = archive_path_string(&path)?;

        match path_string.as_str() {
            "metadata.json" => {
                let mut buf = Vec::new();
                entry.read_to_end(&mut buf)?;
                metadata_bytes = Some(buf);
            }
            CACHE_PACKAGE_MANIFEST_PATH => {
                let mut buf = Vec::new();
                entry.read_to_end(&mut buf)?;
                manifest_bytes = Some(buf);
            }
            _ => {}
        }

        if metadata_bytes.is_some() && manifest_bytes.is_some() {
            break;
        }
    }

    let metadata_bytes = metadata_bytes.ok_or(CacheError::MissingArchiveEntry {
        path: "metadata.json",
    })?;
    let manifest_bytes = manifest_bytes.ok_or(CacheError::MissingArchiveEntry {
        path: CACHE_PACKAGE_MANIFEST_PATH,
    })?;

    let metadata = serde_json::from_slice(&metadata_bytes)?;
    let manifest = serde_json::from_slice(&manifest_bytes)?;
    Ok((metadata, manifest))
}

fn extract_selected(
    package_file: &Path,
    manifest: &BTreeMap<String, String>,
    dest: &Path,
    full_install: bool,
) -> Result<()> {
    let file = File::open(package_file)?;
    let decoder = zstd::Decoder::new(file)?;
    let mut archive = tar::Archive::new(decoder);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let entry_path = entry.path()?.into_owned();
        validate_archive_relative_path(&entry_path)?;
        let entry_path_str = archive_path_string(&entry_path)?;

        if entry_path_str == CACHE_PACKAGE_MANIFEST_PATH {
            continue;
        }

        if !should_extract(&entry_path_str, full_install) {
            continue;
        }

        let entry_type = entry.header().entry_type();
        match entry_type {
            EntryType::Directory => {
                std::fs::create_dir_all(dest.join(&entry_path))?;
                continue;
            }
            EntryType::Regular => {
                let expected = manifest.get(&entry_path_str).ok_or_else(|| {
                    CacheError::MissingChecksum {
                        path: entry_path_str.clone(),
                    }
                })?;
                let out_path = dest.join(&entry_path);
                if let Some(parent) = out_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut out = File::create(&out_path)?;
                let found = sha256_copy(&mut entry, &mut out)?;
                if &found != expected {
                    return Err(CacheError::ChecksumMismatch {
                        path: entry_path_str,
                        expected: expected.clone(),
                        found,
                    });
                }
            }
            _ => {
                return Err(CacheError::UnsupportedArchiveEntryType { path: entry_path });
            }
        }
    }

    Ok(())
}

fn should_extract(path: &str, full_install: bool) -> bool {
    if path == "metadata.json" {
        return true;
    }
    if path.starts_with("indexes/") {
        return true;
    }
    if !full_install {
        return false;
    }
    path.starts_with("ast/") || path.starts_with("queries/")
}

fn fingerprints_match(cache_dir: &CacheDir, metadata: &CacheMetadata) -> (bool, usize) {
    let files: Vec<PathBuf> = metadata
        .file_fingerprints
        .keys()
        .map(PathBuf::from)
        .collect();
    let local = ProjectSnapshot::new(cache_dir.project_root(), files);
    let Ok(local) = local else {
        return (false, metadata.file_fingerprints.len());
    };

    let mismatched = metadata
        .file_fingerprints
        .iter()
        .filter(|(path, fp)| local.file_fingerprints().get(*path) != Some(*fp))
        .count();
    (mismatched == 0, mismatched)
}

fn replace_dir_atomically(src_dir: &Path, dest_dir: &Path) -> Result<()> {
    let parent = dest_dir.parent().ok_or_else(|| CacheError::InvalidArchivePath {
        path: dest_dir.to_path_buf(),
    })?;
    std::fs::create_dir_all(parent)?;

    let backup_dir = sibling_with_suffix(dest_dir, "old");
    if backup_dir.exists() {
        std::fs::remove_dir_all(&backup_dir)?;
    }

    if dest_dir.exists() {
        std::fs::rename(dest_dir, &backup_dir)?;
    }

    std::fs::rename(src_dir, dest_dir)?;

    if backup_dir.exists() {
        std::fs::remove_dir_all(&backup_dir)?;
    }

    Ok(())
}

fn install_indexes_only(src_dir: &Path, dest_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dest_dir)?;

    let src_indexes = src_dir.join("indexes");
    if src_indexes.is_dir() {
        let dest_indexes = dest_dir.join("indexes");
        replace_dir_atomically(&src_indexes, &dest_indexes)?;
    }

    let src_metadata = src_dir.join("metadata.json");
    if src_metadata.is_file() {
        replace_file_atomically(&src_metadata, &dest_dir.join("metadata.json"))?;
    }

    Ok(())
}

fn replace_file_atomically(src_file: &Path, dest_file: &Path) -> Result<()> {
    if let Some(parent) = dest_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let backup = sibling_with_suffix(dest_file, "old");
    if backup.exists() {
        let _ = std::fs::remove_file(&backup);
    }
    if dest_file.exists() {
        std::fs::rename(dest_file, &backup)?;
    }
    std::fs::rename(src_file, dest_file)?;
    if backup.exists() {
        let _ = std::fs::remove_file(&backup);
    }
    Ok(())
}

fn sibling_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("cache");
    path.with_file_name(format!("{file_name}.{suffix}"))
}

fn sha256_copy(reader: &mut impl Read, writer: &mut impl std::io::Write) -> Result<String> {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    let mut buf = [0_u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        writer.write_all(&buf[..read])?;
    }
    Ok(hex::encode(hasher.finalize()))
}

fn archive_path_string(path: &Path) -> Result<String> {
    validate_archive_relative_path(path)?;
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(os) => parts.push(os.to_string_lossy()),
            Component::CurDir => {}
            _ => return Err(CacheError::InvalidArchivePath { path: path.to_path_buf() }),
        }
    }
    Ok(parts.join("/"))
}

fn validate_archive_relative_path(path: &Path) -> Result<()> {
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => return Err(CacheError::InvalidArchivePath { path: path.to_path_buf() }),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CacheConfig;

    fn write_fake_cache(cache_dir: &CacheDir) -> Result<()> {
        std::fs::write(cache_dir.indexes_dir().join("symbols.idx"), b"symbols")?;
        std::fs::write(cache_dir.queries_dir().join("types.cache"), b"types")?;
        std::fs::write(cache_dir.ast_dir().join("metadata.bin"), b"ast-metadata")?;
        Ok(())
    }

    #[test]
    fn round_trip_pack_install() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(project_root.join("src"))?;
        std::fs::write(project_root.join("src/Main.java"), b"class Main {}")?;

        let cache_root = tmp.path().join("cache-root");
        let cache_dir = CacheDir::new(&project_root, CacheConfig {
            cache_root_override: Some(cache_root.clone()),
        })?;

        let snapshot = ProjectSnapshot::new(&project_root, vec![PathBuf::from("src/Main.java")])?;
        let metadata = CacheMetadata::new(&snapshot);
        metadata.save(cache_dir.metadata_path())?;

        write_fake_cache(&cache_dir)?;

        let package_path = tmp.path().join("cache.tar.zst");
        pack_cache_package(&cache_dir, &package_path)?;

        std::fs::remove_dir_all(cache_dir.root())?;
        let cache_dir2 = CacheDir::new(&project_root, CacheConfig {
            cache_root_override: Some(cache_root),
        })?;

        let outcome = install_cache_package(&cache_dir2, &package_path)?;
        assert_eq!(outcome, CachePackageInstallOutcome::Full);

        assert!(cache_dir2.metadata_path().is_file());
        assert!(cache_dir2.indexes_dir().join("symbols.idx").is_file());
        assert!(cache_dir2.queries_dir().join("types.cache").is_file());
        assert!(cache_dir2.ast_dir().join("metadata.bin").is_file());
        Ok(())
    }

    #[test]
    fn schema_mismatch_rejected() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(project_root.join("src"))?;
        std::fs::write(project_root.join("src/Main.java"), b"class Main {}")?;

        let cache_root = tmp.path().join("cache-root");
        let cache_dir = CacheDir::new(&project_root, CacheConfig {
            cache_root_override: Some(cache_root.clone()),
        })?;

        let snapshot = ProjectSnapshot::new(&project_root, vec![PathBuf::from("src/Main.java")])?;
        let mut metadata = CacheMetadata::new(&snapshot);
        metadata.schema_version += 1;
        metadata.save(cache_dir.metadata_path())?;
        write_fake_cache(&cache_dir)?;

        let package_path = tmp.path().join("bad-schema.tar.zst");
        pack_cache_package(&cache_dir, &package_path)?;

        let err = install_cache_package(&cache_dir, &package_path).unwrap_err();
        match err {
            CacheError::IncompatibleSchemaVersion { .. } => {}
            other => panic!("unexpected error: {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn nova_version_mismatch_rejected() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(project_root.join("src"))?;
        std::fs::write(project_root.join("src/Main.java"), b"class Main {}")?;

        let cache_root = tmp.path().join("cache-root");
        let cache_dir = CacheDir::new(&project_root, CacheConfig {
            cache_root_override: Some(cache_root.clone()),
        })?;

        let snapshot = ProjectSnapshot::new(&project_root, vec![PathBuf::from("src/Main.java")])?;
        let mut metadata = CacheMetadata::new(&snapshot);
        metadata.nova_version = "999.0.0".to_string();
        metadata.save(cache_dir.metadata_path())?;
        write_fake_cache(&cache_dir)?;

        let package_path = tmp.path().join("bad-version.tar.zst");
        pack_cache_package(&cache_dir, &package_path)?;

        let err = install_cache_package(&cache_dir, &package_path).unwrap_err();
        match err {
            CacheError::IncompatibleNovaVersion { .. } => {}
            other => panic!("unexpected error: {other:?}"),
        }
        Ok(())
    }
}
