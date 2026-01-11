use nova_cache::{atomic_write, now_millis, CacheDir, Fingerprint, ProjectSnapshot};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::persistence::IndexPersistenceError;

pub const SEGMENT_MANIFEST_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentManifest {
    pub schema_version: u32,
    pub nova_version: String,
    pub created_at_millis: u64,
    pub last_updated_millis: u64,
    pub segments: Vec<SegmentEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentEntry {
    pub id: u64,
    pub created_at_millis: u64,
    pub file_name: String,
    /// Files covered by this segment.
    ///
    /// A file can appear with `fingerprint: None` when the segment represents a
    /// tombstone for a deleted file (the segment supersedes the base indexes but
    /// contributes no symbols/references/etc).
    pub files: Vec<SegmentFile>,
    /// Optional size hint for compaction decisions.
    pub bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentFile {
    pub path: String,
    pub fingerprint: Option<Fingerprint>,
}

impl SegmentManifest {
    pub fn new() -> Self {
        let now = now_millis();
        Self {
            schema_version: SEGMENT_MANIFEST_SCHEMA_VERSION,
            nova_version: nova_core::NOVA_VERSION.to_string(),
            created_at_millis: now,
            last_updated_millis: now,
            segments: Vec::new(),
        }
    }

    pub fn is_compatible(&self) -> bool {
        self.schema_version == SEGMENT_MANIFEST_SCHEMA_VERSION
            && self.nova_version == nova_core::NOVA_VERSION
    }

    pub fn next_segment_id(&self) -> u64 {
        self.segments
            .last()
            .map(|s| s.id.saturating_add(1))
            .unwrap_or(1)
    }
}

impl Default for SegmentManifest {
    fn default() -> Self {
        Self::new()
    }
}

pub fn segments_dir(indexes_dir: &Path) -> PathBuf {
    indexes_dir.join("segments")
}

pub fn manifest_path(indexes_dir: &Path) -> PathBuf {
    segments_dir(indexes_dir).join("manifest.json")
}

pub fn load_manifest(indexes_dir: &Path) -> Result<Option<SegmentManifest>, IndexPersistenceError> {
    let path = manifest_path(indexes_dir);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(path)?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

pub fn save_manifest(
    indexes_dir: &Path,
    manifest: &SegmentManifest,
) -> Result<(), IndexPersistenceError> {
    let path = manifest_path(indexes_dir);
    let json = serde_json::to_vec_pretty(manifest)?;
    atomic_write(&path, &json)?;
    Ok(())
}

pub fn segment_file_name(id: u64) -> String {
    format!("seg_{id}.idx")
}

pub fn segment_path(indexes_dir: &Path, file_name: &str) -> PathBuf {
    segments_dir(indexes_dir).join(file_name)
}

pub fn build_segment_files(
    snapshot: &ProjectSnapshot,
    covered_files: &[String],
) -> Vec<SegmentFile> {
    covered_files
        .iter()
        .map(|path| SegmentFile {
            path: path.clone(),
            fingerprint: snapshot.file_fingerprints().get(path).cloned(),
        })
        .collect()
}

pub fn build_file_to_newest_segment_map(manifest: &SegmentManifest) -> BTreeMap<String, usize> {
    let mut map = BTreeMap::new();
    for (idx, segment) in manifest.segments.iter().enumerate() {
        for file in &segment.files {
            map.insert(file.path.clone(), idx);
        }
    }
    map
}

pub fn clear_segments(cache_dir: &CacheDir) -> Result<(), IndexPersistenceError> {
    let indexes_dir = cache_dir.indexes_dir();
    let segments_dir = segments_dir(&indexes_dir);
    if segments_dir.exists() {
        std::fs::remove_dir_all(segments_dir)?;
    }
    Ok(())
}

pub fn estimate_segment_bytes(indexes_dir: &Path, file_name: &str) -> Option<u64> {
    let path = segment_path(indexes_dir, file_name);
    std::fs::metadata(path).ok().map(|m| m.len())
}

pub fn manifest_or_new(indexes_dir: &Path) -> Result<SegmentManifest, IndexPersistenceError> {
    match load_manifest(indexes_dir)? {
        Some(manifest) if manifest.is_compatible() => Ok(manifest),
        _ => Ok(SegmentManifest::new()),
    }
}

pub fn ensure_segments_dir(indexes_dir: &Path) -> Result<PathBuf, IndexPersistenceError> {
    let dir = segments_dir(indexes_dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn update_manifest_for_new_segment(
    indexes_dir: &Path,
    snapshot: &ProjectSnapshot,
    covered_files: &[String],
) -> Result<(SegmentManifest, SegmentEntry), IndexPersistenceError> {
    let mut manifest = manifest_or_new(indexes_dir)?;
    let id = manifest.next_segment_id();
    let file_name = segment_file_name(id);
    let created_at_millis = now_millis();

    let files = build_segment_files(snapshot, covered_files);
    let bytes = estimate_segment_bytes(indexes_dir, &file_name);
    let entry = SegmentEntry {
        id,
        created_at_millis,
        file_name,
        files,
        bytes,
    };

    manifest.last_updated_millis = created_at_millis;
    manifest.segments.push(entry.clone());
    Ok((manifest, entry))
}
