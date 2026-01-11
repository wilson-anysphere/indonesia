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
    /// Package was installed with `metadata.json`/`metadata.bin`, `indexes/`, `queries/`, and `ast/` (if present).
    Full,
    /// Only `metadata.json`/`metadata.bin` and `indexes/` were installed because the local project fingerprints
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

    let metadata_path = root.join(crate::metadata::CACHE_METADATA_JSON_FILENAME);
    if metadata_path.is_file() {
        files.push(PathBuf::from(crate::metadata::CACHE_METADATA_JSON_FILENAME));
    } else {
        return Err(CacheError::MissingArchiveEntry {
            path: crate::metadata::CACHE_METADATA_JSON_FILENAME,
        });
    }

    let metadata_bin = root.join(crate::metadata::CACHE_METADATA_BIN_FILENAME);
    if metadata_bin.is_file() {
        files.push(PathBuf::from(crate::metadata::CACHE_METADATA_BIN_FILENAME));
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
            // Skip crashed atomic-write temp files: `atomic_write` uses unique
            // names like `<dest>.tmp.<pid>.<counter>`.
            let file_name = entry.file_name().to_string_lossy();
            if file_name.ends_with(".tmp") || file_name.contains(".tmp.") {
                continue;
            }

            let rel =
                entry
                    .path()
                    .strip_prefix(root)
                    .map_err(|_| CacheError::InvalidArchivePath {
                        path: entry.path().to_path_buf(),
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
    let mut metadata: Option<CacheMetadata> = None;
    let mut manifest: Option<BTreeMap<String, String>> = None;

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
                metadata = Some(serde_json::from_reader(&mut entry)?);
            }
            CACHE_PACKAGE_MANIFEST_PATH => {
                manifest = Some(serde_json::from_reader(&mut entry)?);
            }
            _ => {}
        }

        if metadata.is_some() && manifest.is_some() {
            break;
        }
    }

    let metadata = metadata.ok_or(CacheError::MissingArchiveEntry {
        path: "metadata.json",
    })?;
    let manifest = manifest.ok_or(CacheError::MissingArchiveEntry {
        path: CACHE_PACKAGE_MANIFEST_PATH,
    })?;

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
                let expected =
                    manifest
                        .get(&entry_path_str)
                        .ok_or_else(|| CacheError::MissingChecksum {
                            path: entry_path_str.clone(),
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
    if path == crate::metadata::CACHE_METADATA_JSON_FILENAME
        || path == crate::metadata::CACHE_METADATA_BIN_FILENAME
    {
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
    // Prefer fast, metadata-only fingerprints when available. This avoids hashing
    // full file contents in the common mismatch case.
    if !metadata.file_metadata_fingerprints.is_empty() {
        let files: Vec<PathBuf> = metadata
            .file_fingerprints
            .keys()
            .map(PathBuf::from)
            .collect();

        let local = ProjectSnapshot::new_fast(cache_dir.project_root(), files);
        let Ok(local) = local else {
            return (false, metadata.file_fingerprints.len());
        };

        let mismatched = metadata
            .file_fingerprints
            .keys()
            .filter(|path| {
                metadata.file_metadata_fingerprints.get(*path)
                    != local.file_fingerprints().get(*path)
            })
            .count();

        if mismatched != 0 {
            return (false, mismatched);
        }

        // Optional safety: verify a small sample of full content hashes to reduce
        // false positives from coarse mtimes.
        let sample_size = cache_package_verify_sample_size();
        if sample_size > 0 {
            let sampled_files = sample_files(
                metadata.file_fingerprints.keys(),
                &metadata.project_hash,
                sample_size,
            );
            let sampled_paths: Vec<PathBuf> = sampled_files
                .iter()
                .map(|p| PathBuf::from(p.as_str()))
                .collect();

            let local_full = ProjectSnapshot::new(cache_dir.project_root(), sampled_paths);
            let Ok(local_full) = local_full else {
                return (false, sampled_files.len());
            };

            let mismatched_full = sampled_files
                .iter()
                .filter(|path| {
                    let expected = metadata.file_fingerprints.get(path.as_str());
                    expected != local_full.file_fingerprints().get(path.as_str())
                })
                .count();
            if mismatched_full != 0 {
                return (false, mismatched_full);
            }
        }

        return (true, 0);
    }

    // Backwards-compatibility fallback for packages built before
    // `file_metadata_fingerprints` was introduced.
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

fn cache_package_verify_sample_size() -> usize {
    std::env::var("NOVA_CACHE_PACKAGE_VERIFY_SAMPLE")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0)
}

fn sample_files<'a>(
    files: impl Iterator<Item = &'a String>,
    seed: &Fingerprint,
    sample_size: usize,
) -> Vec<&'a String> {
    use sha2::{Digest, Sha256};

    let mut scored: Vec<(u64, &'a String)> = files
        .map(|path| {
            let mut hasher = Sha256::new();
            hasher.update(seed.as_str().as_bytes());
            hasher.update(b":");
            hasher.update(path.as_bytes());
            let digest = hasher.finalize();
            let score = u64::from_le_bytes(digest[0..8].try_into().expect("sha256 digest len"));
            (score, path)
        })
        .collect();

    scored
        .sort_by(|(a_score, a_path), (b_score, b_path)| (a_score, a_path).cmp(&(b_score, b_path)));
    scored
        .into_iter()
        .take(sample_size)
        .map(|(_, path)| path)
        .collect()
}

fn replace_dir_atomically(src_dir: &Path, dest_dir: &Path) -> Result<()> {
    let parent = dest_dir
        .parent()
        .ok_or_else(|| CacheError::InvalidArchivePath {
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

    let src_metadata_bin = src_dir.join(crate::metadata::CACHE_METADATA_BIN_FILENAME);
    if src_metadata_bin.is_file() {
        replace_file_atomically(
            &src_metadata_bin,
            &dest_dir.join(crate::metadata::CACHE_METADATA_BIN_FILENAME),
        )?;
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
    let file_name = path.file_name().and_then(OsStr::to_str).unwrap_or("cache");
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
            _ => {
                return Err(CacheError::InvalidArchivePath {
                    path: path.to_path_buf(),
                })
            }
        }
    }
    Ok(parts.join("/"))
}

fn validate_archive_relative_path(path: &Path) -> Result<()> {
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => {
                return Err(CacheError::InvalidArchivePath {
                    path: path.to_path_buf(),
                })
            }
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
        let cache_dir = CacheDir::new(
            &project_root,
            CacheConfig {
                cache_root_override: Some(cache_root.clone()),
            },
        )?;

        let snapshot = ProjectSnapshot::new(&project_root, vec![PathBuf::from("src/Main.java")])?;
        let metadata = CacheMetadata::new(&snapshot);
        metadata.save(cache_dir.metadata_path())?;

        write_fake_cache(&cache_dir)?;

        let package_path = tmp.path().join("cache.tar.zst");
        pack_cache_package(&cache_dir, &package_path)?;

        std::fs::remove_dir_all(cache_dir.root())?;
        let cache_dir2 = CacheDir::new(
            &project_root,
            CacheConfig {
                cache_root_override: Some(cache_root),
            },
        )?;

        let outcome = install_cache_package(&cache_dir2, &package_path)?;
        assert_eq!(outcome, CachePackageInstallOutcome::Full);

        assert!(cache_dir2.metadata_path().is_file());
        assert!(cache_dir2.metadata_bin_path().is_file());
        assert!(cache_dir2.indexes_dir().join("symbols.idx").is_file());
        assert!(cache_dir2.queries_dir().join("types.cache").is_file());
        assert!(cache_dir2.ast_dir().join("metadata.bin").is_file());
        Ok(())
    }

    #[test]
    fn pack_skips_atomic_temp_files() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(project_root.join("src"))?;
        std::fs::write(project_root.join("src/Main.java"), b"class Main {}")?;

        let cache_root = tmp.path().join("cache-root");
        let cache_dir = CacheDir::new(
            &project_root,
            CacheConfig {
                cache_root_override: Some(cache_root),
            },
        )?;

        let snapshot = ProjectSnapshot::new(&project_root, vec![PathBuf::from("src/Main.java")])?;
        let metadata = CacheMetadata::new(&snapshot);
        metadata.save(cache_dir.metadata_path())?;
        write_fake_cache(&cache_dir)?;

        // Simulate a crash leaving behind atomic-write tempfiles.
        std::fs::write(
            cache_dir.indexes_dir().join("symbols.idx.tmp.123.0"),
            b"tmp",
        )?;
        std::fs::write(
            cache_dir.queries_dir().join("types.cache.tmp.123.0"),
            b"tmp",
        )?;
        std::fs::write(
            cache_dir.ast_dir().join("metadata.bin.tmp.123.0"),
            b"tmp",
        )?;

        let files = collect_cache_files(cache_dir.root())?;
        assert!(
            !files.iter().any(|path| path.to_string_lossy().contains(".tmp.")),
            "collect_cache_files included atomic-write tempfiles: {files:?}"
        );
        Ok(())
    }

    #[test]
    fn schema_mismatch_rejected() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(project_root.join("src"))?;
        std::fs::write(project_root.join("src/Main.java"), b"class Main {}")?;

        let cache_root = tmp.path().join("cache-root");
        let cache_dir = CacheDir::new(
            &project_root,
            CacheConfig {
                cache_root_override: Some(cache_root.clone()),
            },
        )?;

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
        let cache_dir = CacheDir::new(
            &project_root,
            CacheConfig {
                cache_root_override: Some(cache_root.clone()),
            },
        )?;

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
