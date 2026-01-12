use crate::ast_cache::AST_ARTIFACT_SCHEMA_VERSION;
use crate::error::Result;
use crate::fingerprint::Fingerprint;
use crate::util::{
    atomic_write, bincode_deserialize, bincode_options_limited, bincode_serialize, now_millis,
};
use crate::CacheDir;
use bincode::Options;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const DAY_MILLIS: u64 = 24 * 60 * 60 * 1000;

/// Controls how a project cache directory is pruned.
#[derive(Clone, Debug, Default)]
pub struct PrunePolicy {
    /// Delete cache entries older than this many days.
    pub max_age_days: Option<u64>,
    /// Best-effort limit for the total size of the project cache directory.
    ///
    /// When exceeded, the pruner deletes the oldest cache entries first until
    /// the size is under the limit or there is nothing left that's safe to
    /// delete.
    pub max_total_bytes: Option<u64>,
    /// Optional size limit applied to each query directory under `queries/`.
    ///
    /// When exceeded, entries are deleted oldest-first within that query
    /// directory.
    pub max_query_bytes: Option<u64>,
    /// If true, don't delete anything; only report what would be removed.
    pub dry_run: bool,
}

/// Summary of a prune operation.
#[derive(Clone, Debug, Default)]
pub struct PruneReport {
    pub dry_run: bool,
    pub total_bytes_before: u64,
    pub total_bytes_after: u64,
    pub deleted_files: usize,
    pub deleted_bytes: u64,
    pub would_delete_files: usize,
    pub would_delete_bytes: u64,
    pub errors: Vec<PruneError>,
}

#[derive(Clone, Debug)]
pub struct PruneError {
    pub path: PathBuf,
    pub action: &'static str,
    pub error: String,
}

impl PruneReport {
    fn push_error(&mut self, path: impl Into<PathBuf>, action: &'static str, err: impl ToString) {
        self.errors.push(PruneError {
            path: path.into(),
            action,
            error: err.to_string(),
        });
    }

    fn record_delete(&mut self, bytes: u64, dry_run: bool) {
        if dry_run {
            self.would_delete_files += 1;
            self.would_delete_bytes = self.would_delete_bytes.saturating_add(bytes);
        } else {
            self.deleted_files += 1;
            self.deleted_bytes = self.deleted_bytes.saturating_add(bytes);
        }
    }
}

/// Prune a project's persistent cache directory according to `policy`.
///
/// This function is intentionally best-effort: IO failures are collected into
/// the returned report and do not abort the overall pruning pass.
pub fn prune_cache(cache_dir: &CacheDir, policy: PrunePolicy) -> Result<PruneReport> {
    let mut report = PruneReport {
        dry_run: policy.dry_run,
        ..PruneReport::default()
    };

    report.total_bytes_before = dir_size_bytes(cache_dir.root(), &mut report);

    let cutoff_millis = policy
        .max_age_days
        .map(|days| now_millis().saturating_sub(days.saturating_mul(DAY_MILLIS)));

    prune_ast(cache_dir, cutoff_millis, &policy, &mut report);
    prune_queries(cache_dir, cutoff_millis, &policy, &mut report);
    prune_indexes(cache_dir, cutoff_millis, &policy, &mut report);
    prune_classpath(cache_dir, cutoff_millis, &policy, &mut report);

    if let Some(limit) = policy.max_total_bytes {
        enforce_total_size(cache_dir, limit, &policy, &mut report);
    }

    report.total_bytes_after = if policy.dry_run {
        report.total_bytes_before
    } else {
        dir_size_bytes(cache_dir.root(), &mut report)
    };

    Ok(report)
}

fn prune_ast(
    cache_dir: &CacheDir,
    cutoff_millis: Option<u64>,
    policy: &PrunePolicy,
    report: &mut PruneReport,
) {
    let ast_dir = cache_dir.ast_dir();
    if !ast_dir.is_dir() {
        return;
    }

    let metadata_path = ast_dir.join("metadata.bin");
    let metadata = load_ast_metadata(&metadata_path, report);

    let mut referenced = HashMap::<String, u64>::new();
    let mut compatible_metadata = metadata.filter(is_compatible_ast_metadata);

    if let Some(meta) = &compatible_metadata {
        for entry in meta.files.values() {
            referenced.insert(entry.artifact_file.clone(), entry.saved_at_millis);
        }
    }

    let mut deleted_artifacts = HashSet::new();

    let dir_entries = match std::fs::read_dir(&ast_dir) {
        Ok(entries) => entries,
        Err(err) => {
            report.push_error(&ast_dir, "read_dir", err);
            return;
        }
    };

    for entry in dir_entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                report.push_error(&ast_dir, "read_dir_entry", err);
                continue;
            }
        };

        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(err) => {
                report.push_error(entry.path(), "file_type", err);
                continue;
            }
        };
        if !file_type.is_file() {
            continue;
        }

        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy().to_string();
        // Lockfiles are out-of-band coordination primitives; never prune them.
        if file_name == "metadata.bin" || file_name.ends_with(".lock") {
            continue;
        }

        let mut should_delete = !referenced.contains_key(&file_name);
        if !should_delete {
            if let (Some(cutoff), Some(saved_at)) =
                (cutoff_millis, referenced.get(&file_name).copied())
            {
                if saved_at < cutoff {
                    should_delete = true;
                }
            }
        }

        if should_delete {
            let path = entry.path();
            let size = file_size_bytes(&path, report).unwrap_or(0);
            if delete_file(&path, policy, report) {
                deleted_artifacts.insert(file_name);
                report.record_delete(size, policy.dry_run);
            }
        }
    }

    let Some(meta) = compatible_metadata.as_mut() else {
        return;
    };

    let mut changed = false;
    meta.files.retain(|_, entry| {
        if deleted_artifacts.contains(&entry.artifact_file) {
            changed = true;
            return false;
        }

        let artifact_path = ast_dir.join(&entry.artifact_file);
        if !artifact_path.is_file() {
            changed = true;
            return false;
        }
        true
    });

    if changed {
        store_ast_metadata(&metadata_path, meta, policy, report);
    }
}

fn prune_queries(
    cache_dir: &CacheDir,
    cutoff_millis: Option<u64>,
    policy: &PrunePolicy,
    report: &mut PruneReport,
) {
    let queries_dir = cache_dir.queries_dir();
    if !queries_dir.is_dir() {
        return;
    }

    let dir_entries = match std::fs::read_dir(&queries_dir) {
        Ok(entries) => entries,
        Err(err) => {
            report.push_error(&queries_dir, "read_dir", err);
            return;
        }
    };

    for entry in dir_entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                report.push_error(&queries_dir, "read_dir_entry", err);
                continue;
            }
        };

        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(err) => {
                report.push_error(entry.path(), "file_type", err);
                continue;
            }
        };
        if !file_type.is_dir() {
            continue;
        }

        let query_dir = entry.path();
        prune_single_query_dir(&query_dir, cutoff_millis, policy, report);
    }
}

fn prune_single_query_dir(
    query_dir: &Path,
    cutoff_millis: Option<u64>,
    policy: &PrunePolicy,
    report: &mut PruneReport,
) {
    let mut entries = Vec::new();

    let dir_entries = match std::fs::read_dir(query_dir) {
        Ok(entries) => entries,
        Err(err) => {
            report.push_error(query_dir.to_path_buf(), "read_dir", err);
            return;
        }
    };

    for entry in dir_entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                report.push_error(query_dir.to_path_buf(), "read_dir_entry", err);
                continue;
            }
        };

        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(err) => {
                report.push_error(entry.path(), "file_type", err);
                continue;
            }
        };
        if !file_type.is_file() {
            continue;
        }

        let path = entry.path();
        let size = file_size_bytes(&path, report).unwrap_or(0);
        let last_used = derived_entry_saved_at_millis(&path, report)
            .unwrap_or_else(|| file_modified_millis(&path, report).unwrap_or(0));

        if let Some(cutoff) = cutoff_millis {
            if last_used < cutoff {
                if delete_file(&path, policy, report) {
                    report.record_delete(size, policy.dry_run);
                }
                continue;
            }
        }

        entries.push(QueryEntryInfo {
            path,
            size_bytes: size,
            last_used_millis: last_used,
        });
    }

    let Some(limit) = policy.max_query_bytes else {
        return;
    };

    let mut total: u64 = entries.iter().map(|e| e.size_bytes).sum();
    if total <= limit {
        return;
    }

    entries.sort_by(|a, b| {
        a.last_used_millis
            .cmp(&b.last_used_millis)
            .then_with(|| a.path.cmp(&b.path))
    });

    for entry in entries {
        if total <= limit {
            break;
        }
        if delete_file(&entry.path, policy, report) {
            total = total.saturating_sub(entry.size_bytes);
            report.record_delete(entry.size_bytes, policy.dry_run);
        }
    }
}

#[derive(Clone, Debug)]
struct QueryEntryInfo {
    path: PathBuf,
    size_bytes: u64,
    last_used_millis: u64,
}

fn prune_indexes(
    cache_dir: &CacheDir,
    cutoff_millis: Option<u64>,
    policy: &PrunePolicy,
    report: &mut PruneReport,
) {
    let indexes_dir = cache_dir.indexes_dir();
    if !indexes_dir.is_dir() {
        return;
    }

    let dir_entries = match std::fs::read_dir(&indexes_dir) {
        Ok(entries) => entries,
        Err(err) => {
            report.push_error(&indexes_dir, "read_dir", err);
            return;
        }
    };

    let Some(cutoff) = cutoff_millis else {
        return;
    };

    for entry in dir_entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                report.push_error(&indexes_dir, "read_dir_entry", err);
                continue;
            }
        };

        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(err) => {
                report.push_error(entry.path(), "file_type", err);
                continue;
            }
        };
        if !file_type.is_file() {
            continue;
        }

        let path = entry.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".lock"))
        {
            continue;
        }
        if is_current_index_file(&path) {
            continue;
        }

        let last_used = file_modified_millis(&path, report).unwrap_or(0);
        if last_used >= cutoff {
            continue;
        }

        let size = file_size_bytes(&path, report).unwrap_or(0);
        if delete_file(&path, policy, report) {
            report.record_delete(size, policy.dry_run);
        }
    }
}

fn prune_classpath(
    cache_dir: &CacheDir,
    cutoff_millis: Option<u64>,
    policy: &PrunePolicy,
    report: &mut PruneReport,
) {
    let classpath_dir = cache_dir.classpath_dir();
    if !classpath_dir.is_dir() {
        return;
    }

    let dir_entries = match std::fs::read_dir(&classpath_dir) {
        Ok(entries) => entries,
        Err(err) => {
            report.push_error(&classpath_dir, "read_dir", err);
            return;
        }
    };

    for entry in dir_entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                report.push_error(&classpath_dir, "read_dir_entry", err);
                continue;
            }
        };

        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(err) => {
                report.push_error(entry.path(), "file_type", err);
                continue;
            }
        };
        if !file_type.is_file() {
            continue;
        }

        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        // Lockfiles are out-of-band coordination primitives; never prune them.
        if file_name.ends_with(".lock") {
            continue;
        }

        let size = file_size_bytes(&path, report).unwrap_or(0);

        // Always delete crashed atomic-write temp files, regardless of age policy.
        if file_name.ends_with(".tmp") || file_name.contains(".tmp.") {
            if delete_file(&path, policy, report) {
                report.record_delete(size, policy.dry_run);
            }
            continue;
        }

        let Some(cutoff) = cutoff_millis else {
            continue;
        };

        let last_used = file_modified_millis(&path, report).unwrap_or(0);
        if last_used < cutoff && delete_file(&path, policy, report) {
            report.record_delete(size, policy.dry_run);
        }
    }
}

fn enforce_total_size(
    cache_dir: &CacheDir,
    limit: u64,
    policy: &PrunePolicy,
    report: &mut PruneReport,
) {
    let mut total = dir_size_bytes(cache_dir.root(), report);
    if total <= limit {
        return;
    }

    let mut removed_ast_artifacts = HashSet::<String>::new();
    let mut candidates = Vec::<EvictionCandidate>::new();

    gather_ast_candidates(cache_dir, report, &mut candidates);
    gather_query_candidates(cache_dir, report, &mut candidates);
    gather_classpath_candidates(cache_dir, report, &mut candidates);
    gather_legacy_index_candidates(cache_dir, report, &mut candidates);

    candidates.sort_by(|a, b| {
        a.last_used_millis
            .cmp(&b.last_used_millis)
            .then_with(|| a.path.cmp(&b.path))
    });

    for candidate in candidates {
        if total <= limit {
            break;
        }

        if delete_file(&candidate.path, policy, report) {
            total = total.saturating_sub(candidate.size_bytes);
            if let CandidateKind::AstArtifact { artifact_file } = candidate.kind {
                removed_ast_artifacts.insert(artifact_file);
            }
            report.record_delete(candidate.size_bytes, policy.dry_run);
        }
    }

    if removed_ast_artifacts.is_empty() {
        return;
    }

    if policy.dry_run {
        return;
    }

    // Best-effort cleanup to ensure AST metadata doesn't point at deleted artifacts.
    let ast_dir = cache_dir.ast_dir();
    let metadata_path = ast_dir.join("metadata.bin");
    let Some(mut meta) = load_ast_metadata(&metadata_path, report) else {
        return;
    };

    if !is_compatible_ast_metadata(&meta) {
        return;
    }

    let mut changed = false;
    meta.files.retain(|_, entry| {
        if removed_ast_artifacts.contains(&entry.artifact_file) {
            changed = true;
            return false;
        }
        let artifact_path = ast_dir.join(&entry.artifact_file);
        if !artifact_path.is_file() {
            changed = true;
            return false;
        }
        true
    });

    if changed {
        store_ast_metadata(&metadata_path, &meta, policy, report);
    }
}

#[derive(Clone, Debug)]
struct EvictionCandidate {
    path: PathBuf,
    size_bytes: u64,
    last_used_millis: u64,
    kind: CandidateKind,
}

#[derive(Clone, Debug)]
enum CandidateKind {
    AstArtifact { artifact_file: String },
    QueryEntry,
    ClasspathEntry,
    LegacyIndex,
}

fn gather_ast_candidates(
    cache_dir: &CacheDir,
    report: &mut PruneReport,
    out: &mut Vec<EvictionCandidate>,
) {
    let ast_dir = cache_dir.ast_dir();
    if !ast_dir.is_dir() {
        return;
    }

    let metadata_path = ast_dir.join("metadata.bin");
    let mut artifact_saved_at = HashMap::<String, u64>::new();
    if let Some(meta) = load_ast_metadata(&metadata_path, report) {
        if is_compatible_ast_metadata(&meta) {
            for entry in meta.files.values() {
                artifact_saved_at.insert(entry.artifact_file.clone(), entry.saved_at_millis);
            }
        }
    }

    let dir_entries = match std::fs::read_dir(&ast_dir) {
        Ok(entries) => entries,
        Err(err) => {
            report.push_error(&ast_dir, "read_dir", err);
            return;
        }
    };

    for entry in dir_entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                report.push_error(&ast_dir, "read_dir_entry", err);
                continue;
            }
        };

        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(err) => {
                report.push_error(entry.path(), "file_type", err);
                continue;
            }
        };
        if !file_type.is_file() {
            continue;
        }

        let file_name = entry.file_name().to_string_lossy().to_string();
        // Lockfiles are out-of-band coordination primitives; never prune them.
        if file_name == "metadata.bin" || file_name.ends_with(".lock") {
            continue;
        }

        let path = entry.path();
        let size = file_size_bytes(&path, report).unwrap_or(0);
        let last_used = artifact_saved_at
            .get(&file_name)
            .copied()
            .or_else(|| file_modified_millis(&path, report))
            .unwrap_or(0);

        out.push(EvictionCandidate {
            path,
            size_bytes: size,
            last_used_millis: last_used,
            kind: CandidateKind::AstArtifact {
                artifact_file: file_name,
            },
        });
    }
}

fn gather_query_candidates(
    cache_dir: &CacheDir,
    report: &mut PruneReport,
    out: &mut Vec<EvictionCandidate>,
) {
    let queries_dir = cache_dir.queries_dir();
    if !queries_dir.is_dir() {
        return;
    }

    for entry in walkdir::WalkDir::new(&queries_dir).follow_links(false) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                let path = err
                    .path()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| queries_dir.clone());
                report.push_error(path, "walkdir", err);
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.into_path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".lock"))
        {
            continue;
        }
        let size = file_size_bytes(&path, report).unwrap_or(0);
        let last_used = derived_entry_saved_at_millis(&path, report)
            .or_else(|| file_modified_millis(&path, report))
            .unwrap_or(0);

        out.push(EvictionCandidate {
            path,
            size_bytes: size,
            last_used_millis: last_used,
            kind: CandidateKind::QueryEntry,
        });
    }
}

fn gather_classpath_candidates(
    cache_dir: &CacheDir,
    report: &mut PruneReport,
    out: &mut Vec<EvictionCandidate>,
) {
    let classpath_dir = cache_dir.classpath_dir();
    if !classpath_dir.is_dir() {
        return;
    }

    let dir_entries = match std::fs::read_dir(&classpath_dir) {
        Ok(entries) => entries,
        Err(err) => {
            report.push_error(&classpath_dir, "read_dir", err);
            return;
        }
    };

    for entry in dir_entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                report.push_error(&classpath_dir, "read_dir_entry", err);
                continue;
            }
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(err) => {
                report.push_error(entry.path(), "file_type", err);
                continue;
            }
        };
        if !file_type.is_file() {
            continue;
        }

        let path = entry.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".lock"))
        {
            continue;
        }

        let size = file_size_bytes(&path, report).unwrap_or(0);
        let last_used = file_modified_millis(&path, report).unwrap_or(0);
        out.push(EvictionCandidate {
            path,
            size_bytes: size,
            last_used_millis: last_used,
            kind: CandidateKind::ClasspathEntry,
        });
    }
}

fn gather_legacy_index_candidates(
    cache_dir: &CacheDir,
    report: &mut PruneReport,
    out: &mut Vec<EvictionCandidate>,
) {
    let indexes_dir = cache_dir.indexes_dir();
    if !indexes_dir.is_dir() {
        return;
    }

    let dir_entries = match std::fs::read_dir(&indexes_dir) {
        Ok(entries) => entries,
        Err(err) => {
            report.push_error(&indexes_dir, "read_dir", err);
            return;
        }
    };

    for entry in dir_entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                report.push_error(&indexes_dir, "read_dir_entry", err);
                continue;
            }
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(err) => {
                report.push_error(entry.path(), "file_type", err);
                continue;
            }
        };
        if !file_type.is_file() {
            continue;
        }

        let path = entry.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".lock"))
        {
            continue;
        }
        if is_current_index_file(&path) {
            continue;
        }

        let size = file_size_bytes(&path, report).unwrap_or(0);
        let last_used = file_modified_millis(&path, report).unwrap_or(0);
        out.push(EvictionCandidate {
            path,
            size_bytes: size,
            last_used_millis: last_used,
            kind: CandidateKind::LegacyIndex,
        });
    }
}

fn is_current_index_file(path: &Path) -> bool {
    match path.extension().and_then(|s| s.to_str()) {
        Some("idx") => return true,
        Some("bin") => {
            if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                if name.starts_with("shard_") {
                    return true;
                }
            }
        }
        _ => {}
    }
    false
}

fn delete_file(path: &Path, policy: &PrunePolicy, report: &mut PruneReport) -> bool {
    if path.file_name().and_then(|s| s.to_str()) == Some("metadata.json") {
        return false;
    }

    if policy.dry_run {
        return true;
    }

    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(err) => {
            report.push_error(path, "remove_file", err);
            false
        }
    }
}

fn file_size_bytes(path: &Path, report: &mut PruneReport) -> Option<u64> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) => {
            report.push_error(path, "metadata", err);
            return None;
        }
    };
    Some(meta.len())
}

fn file_modified_millis(path: &Path, report: &mut PruneReport) -> Option<u64> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) => {
            report.push_error(path, "metadata", err);
            return None;
        }
    };

    let modified = match meta.modified() {
        Ok(t) => t,
        Err(err) => {
            report.push_error(path, "modified_time", err);
            return None;
        }
    };

    Some(system_time_to_millis(modified))
}

fn system_time_to_millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn dir_size_bytes(path: &Path, report: &mut PruneReport) -> u64 {
    let mut total = 0u64;
    if !path.exists() {
        return 0;
    }

    for entry in walkdir::WalkDir::new(path).follow_links(false) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                let p = err
                    .path()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| path.to_path_buf());
                report.push_error(p, "walkdir", err);
                continue;
            }
        };

        if !entry.file_type().is_file() {
            continue;
        }

        match entry.metadata() {
            Ok(meta) => total = total.saturating_add(meta.len()),
            Err(err) => report.push_error(entry.path(), "metadata", err),
        }
    }

    total
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AstCacheMetadata {
    schema_version: u32,
    syntax_schema_version: u32,
    hir_schema_version: u32,
    nova_version: String,
    files: BTreeMap<String, AstCacheFileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AstCacheFileEntry {
    #[allow(dead_code)]
    fingerprint: Fingerprint,
    artifact_file: String,
    saved_at_millis: u64,
}

fn is_compatible_ast_metadata(meta: &AstCacheMetadata) -> bool {
    meta.schema_version == AST_ARTIFACT_SCHEMA_VERSION
        && meta.syntax_schema_version == nova_syntax::SYNTAX_SCHEMA_VERSION
        && meta.hir_schema_version == nova_hir::HIR_SCHEMA_VERSION
        && meta.nova_version == nova_core::NOVA_VERSION
}

fn load_ast_metadata(path: &Path, report: &mut PruneReport) -> Option<AstCacheMetadata> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            report.push_error(path, "read_ast_metadata", err);
            return None;
        }
    };
    if meta.len() > crate::BINCODE_PAYLOAD_LIMIT_BYTES as u64 {
        report.push_error(path, "read_ast_metadata", "metadata payload exceeds limit");
        return None;
    }

    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) => {
            report.push_error(path, "read_ast_metadata", err);
            return None;
        }
    };

    match bincode_deserialize::<AstCacheMetadata>(&bytes) {
        Ok(metadata) => Some(metadata),
        Err(err) => {
            report.push_error(path, "decode_ast_metadata", err);
            None
        }
    }
}

fn store_ast_metadata(
    path: &Path,
    metadata: &AstCacheMetadata,
    policy: &PrunePolicy,
    report: &mut PruneReport,
) {
    if policy.dry_run {
        return;
    }

    let bytes = match bincode_serialize(metadata) {
        Ok(bytes) => bytes,
        Err(err) => {
            report.push_error(path, "encode_ast_metadata", err);
            return;
        }
    };

    if let Err(err) = atomic_write(path, &bytes) {
        report.push_error(path, "write_ast_metadata", err);
    }
}

fn derived_entry_saved_at_millis(path: &Path, report: &mut PruneReport) -> Option<u64> {
    if path.extension().and_then(|ext| ext.to_str()) != Some("bin") {
        return None;
    }

    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) => {
            report.push_error(path, "stat_query_entry", err);
            return None;
        }
    };
    if meta.len() > crate::BINCODE_PAYLOAD_LIMIT_BYTES as u64 {
        return None;
    }

    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) => {
            report.push_error(path, "read_query_entry", err);
            return None;
        }
    };

    // Derived cache entries are bincode-serialized with Nova's shared options
    // (fixed-int, little-endian). Use the same config here; mixing bincode
    // encodings can lead to bogus lengths and OOMs on corrupted data.
    let mut reader = std::io::BufReader::new(file);
    let (schema_version, _query_schema_version, nova_version, saved_at_millis): (
        u32,
        u32,
        String,
        u64,
    ) = match bincode_options_limited().deserialize_from(&mut reader) {
        Ok(value) => value,
        Err(err) => {
            report.push_error(path, "decode_query_entry_header", err);
            return None;
        }
    };

    if schema_version != crate::derived_cache::DERIVED_CACHE_SCHEMA_VERSION {
        return None;
    }
    if nova_version != nova_core::NOVA_VERSION {
        return None;
    }

    Some(saved_at_millis)
}
