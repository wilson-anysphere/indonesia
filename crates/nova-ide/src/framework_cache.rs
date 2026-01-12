//! Workspace discovery + cached framework indexes.
//!
//! Framework analyzers (Spring/JPA/Micronaut/Quarkus/MapStruct/etc) need fast
//! access to the workspace root and build-derived configuration (classpath,
//! source roots). Doing filesystem walks + build file parsing on every request
//! is prohibitively expensive.
//!
//! This module provides:
//! - project root discovery for an arbitrary path / file id
//! - a small, bounded, thread-safe cache for `nova_project::ProjectConfig`
//! - cached Spring Boot `spring-configuration-metadata.json` indexes
//!
//! # Cache invalidation
//!
//! We use a cheap build-marker fingerprint (mtime + size) for invalidation. The
//! cache is keyed by the canonicalized workspace root, and entries are
//! reloaded when any marker changes. This intentionally does *not* attempt to
//! track changes to every classpath entry (which may include hundreds of jars).
//! The tradeoff is acceptable for IDE latency: build marker edits are the
//! primary reason dependency context changes.

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use nova_config_metadata::MetadataIndex;
use nova_db::{Database, FileId};
use nova_scheduler::CancellationToken;
use nova_types::{CompletionItem, Diagnostic};
use once_cell::sync::Lazy;
use nova_classpath::IndexOptions;

const MAX_CACHED_ROOTS: usize = 32;

static WORKSPACE_CACHE: Lazy<FrameworkWorkspaceCache> = Lazy::new(FrameworkWorkspaceCache::new);

/// Best-effort identifier for the current database instance.
///
/// `FrameworkWorkspaceCache` is a global cache shared across threads. Many tests construct
/// independent in-memory databases but reuse the same virtual roots (e.g. `/workspace`). Include
/// the database address in keys for db-backed workspace caches to avoid cross-test interference
/// under parallel execution.
fn db_cache_id<DB: ?Sized + Database>(db: &DB) -> usize {
    db as *const DB as *const () as usize
}

type DbRootKey = (usize, PathBuf);

/// Walk upwards from `path` and attempt to locate the workspace/project root.
///
/// This uses [`nova_project::workspace_root`] for the shared Maven/Gradle/Bazel/Simple
/// workspace discovery logic. If no marker is found, returns the starting directory
/// (the parent directory when `path` points at a file).
#[must_use]
pub fn project_root_for_path(path: &Path) -> PathBuf {
    let start = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    };

    // Best-effort fallback for in-memory DB fixtures: when the path lives in a virtual/unbacked
    // directory, avoid `nova_project::workspace_root` (which consults the host filesystem and can
    // pick surprising roots like `/` if the machine happens to have a `/src` folder).
    //
    // If the virtual path has a `src/` segment, treat its parent as the project root. This matches
    // early framework analyzer heuristics and keeps test fixtures deterministic.
    if !start.exists() {
        for ancestor in start.ancestors() {
            if ancestor.file_name().and_then(|n| n.to_str()) == Some("src") {
                if let Some(parent) = ancestor.parent() {
                    return parent.to_path_buf();
                }
            }
        }

        return start.to_path_buf();
    }

    if let Some(root) = nova_project::workspace_root(start) {
        return root;
    }

    start.to_path_buf()
}

/// Convenience helper for `FileId`-based queries.
///
/// Returns `None` when the database does not know the file path (e.g. virtual
/// buffers).
#[must_use]
pub fn project_root_for_file<DB: ?Sized + Database>(db: &DB, file: FileId) -> Option<PathBuf> {
    Some(project_root_for_path(db.file_path(file)?))
}

/// Load and cache the [`nova_project::ProjectConfig`] for `root`.
///
/// The cache is keyed by the canonicalized root path and bounded to
/// [`MAX_CACHED_ROOTS`]. Entries are invalidated when build marker fingerprints
/// change (see module-level docs).
#[must_use]
pub fn project_config(root: &Path) -> Option<Arc<nova_project::ProjectConfig>> {
    WORKSPACE_CACHE.project_config(root)
}

/// Load and cache a [`nova_classpath::ClasspathIndex`] for `root`.
///
/// The index is derived from the build-derived [`nova_project::ProjectConfig`] classpath +
/// module-path entries, and cached per (canonicalized) workspace root. The cache is invalidated
/// when build marker fingerprints change (see module-level docs).
///
/// This is best-effort and never panics; failures result in `None`.
#[must_use]
pub fn classpath_index(root: &Path) -> Option<Arc<nova_classpath::ClasspathIndex>> {
    WORKSPACE_CACHE.classpath_index(root)
}

/// Convert a `nova_project` classpath entry into a `nova_classpath` entry.
#[must_use]
pub fn to_classpath_entry(
    entry: &nova_project::ClasspathEntry,
) -> Option<nova_classpath::ClasspathEntry> {
    match entry.kind {
        nova_project::ClasspathEntryKind::Directory => {
            Some(nova_classpath::ClasspathEntry::ClassDir(entry.path.clone()))
        }
        nova_project::ClasspathEntryKind::Jar => {
            // `nova_project::ClasspathEntryKind` doesn't distinguish `.jar` vs `.jmod`, and some
            // build tooling can surface archives as exploded directories (often still ending with
            // `.jar`). Preserve on-disk semantics and infer `.jmod` entries from the file
            // extension.
            if entry.path.is_dir() {
                return Some(nova_classpath::ClasspathEntry::ClassDir(entry.path.clone()));
            }

            let ext = entry
                .path
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("");
            if ext.eq_ignore_ascii_case("jmod") {
                Some(nova_classpath::ClasspathEntry::Jmod(entry.path.clone()))
            } else {
                Some(nova_classpath::ClasspathEntry::Jar(entry.path.clone()))
            }
        }
        #[allow(unreachable_patterns)]
        _ => None,
    }
}

/// Cached Spring Boot `spring-configuration-metadata.json` index for `root`.
///
/// This never panics; errors result in an empty index.
#[must_use]
pub fn spring_metadata_index(root: &Path) -> Arc<MetadataIndex> {
    WORKSPACE_CACHE.spring_metadata_index(root)
}

/// Returns framework diagnostics (Spring/JPA/Micronaut/Quarkus/Dagger) for `file`.
///
/// This is the unified entrypoint used by the `nova-ext` framework providers.
/// Callers should pass the request cancellation token so heavy workspace scanning cooperates with
/// Nova's timeouts.
#[must_use]
pub fn framework_diagnostics<DB: ?Sized + Database>(
    db: &DB,
    file: FileId,
    cancel: &CancellationToken,
) -> Vec<Diagnostic> {
    if cancel.is_cancelled() {
        return Vec::new();
    }

    let text = db.file_content(file);

    // Spring configuration diagnostics (`application*.properties|yml|yaml`).
    if let Some(path) = db.file_path(file) {
        if is_application_properties(path) || is_application_yaml(path) {
            let root = project_root_for_path(path);
            let workspace = WORKSPACE_CACHE.spring_workspace(db, &root, cancel);
            if !workspace.is_spring {
                return Vec::new();
            }
            return nova_framework_spring::diagnostics_for_config_file(
                path,
                text,
                workspace.index.metadata(),
            );
        }
    }

    if !is_java_file(db, file) {
        return Vec::new();
    }

    let mut diagnostics = Vec::new();

    // JPA / JPQL diagnostics (best-effort, workspace-scoped).
    let maybe_jpa_file = text.contains("jakarta.persistence.")
        || text.contains("javax.persistence.")
        || text.contains("@Entity")
        || text.contains("@Query")
        || text.contains("@NamedQuery");
    if maybe_jpa_file {
        if let Some(project) = crate::jpa_intel::project_for_file_with_cancel(db, file, cancel) {
            if let Some(analysis) = project.analysis.as_ref() {
                if let Some(source) = project.source_index_for_file(file) {
                    diagnostics.extend(
                        analysis
                            .diagnostics
                            .iter()
                            .filter(|d| d.source == source)
                            .map(|d| d.diagnostic.clone()),
                    );
                }
            }
        }
    }

    // Spring DI diagnostics (missing / ambiguous beans, circular deps).
    diagnostics.extend(crate::spring_di::diagnostics_for_file_with_cancel(
        db, file, cancel,
    ));

    // Dagger/Hilt diagnostics (workspace-scoped).
    diagnostics.extend(crate::dagger_intel::diagnostics_for_file_with_cancel(
        db, file, cancel,
    ));

    // Quarkus CDI diagnostics (workspace-scoped).
    diagnostics.extend(quarkus_diagnostics_for_file(db, file, text, cancel));

    // Micronaut diagnostics (DI + validation).
    diagnostics.extend(micronaut_diagnostics_for_file(db, file, text, cancel));

    // MapStruct diagnostics (best-effort, file-scoped + light filesystem reads).
    //
    // Gate behind a cheap text heuristic so we don't hit the filesystem / parser for the vast
    // majority of Java files.
    let maybe_mapstruct_file = nova_framework_mapstruct::looks_like_mapstruct_source(text);
    if maybe_mapstruct_file {
        if cancel.is_cancelled() {
            return diagnostics;
        }

        if let Some(file_path) = db.file_path(file) {
            if cancel.is_cancelled() {
                return diagnostics;
            }
            let root = project_root_for_path(file_path);

            if cancel.is_cancelled() {
                return diagnostics;
            }

            let has_mapstruct_dependency = match project_config(&root) {
                Some(config) => match config.build_system {
                    nova_project::BuildSystem::Maven
                    | nova_project::BuildSystem::Gradle
                    | nova_project::BuildSystem::Bazel => config.dependencies.iter().any(|dep| {
                        dep.group_id == "org.mapstruct"
                            && matches!(
                                dep.artifact_id.as_str(),
                                "mapstruct" | "mapstruct-processor"
                            )
                    }),
                    // For "Simple" (build-tool-less) workspaces we can't reliably infer
                    // dependencies; assume they're present to avoid noisy false positives.
                    nova_project::BuildSystem::Simple => true,
                },
                // If we can't load a project config at all, treat dependency presence as unknown and
                // assume it's present to avoid false positives.
                None => true,
            };

            if cancel.is_cancelled() {
                return diagnostics;
            }

            if let Ok(mut diags) = nova_framework_mapstruct::diagnostics_for_file(
                &root,
                file_path,
                text,
                has_mapstruct_dependency,
            ) {
                diagnostics.append(&mut diags);
            }
        }
    }

    diagnostics
}

/// Returns framework completion items for `(file, offset)` (Spring/JPA/Micronaut/Quarkus).
#[must_use]
pub fn framework_completions<DB: ?Sized + Database>(
    db: &DB,
    file: FileId,
    offset: usize,
    cancel: &CancellationToken,
) -> Vec<CompletionItem> {
    if cancel.is_cancelled() {
        return Vec::new();
    }

    let text = db.file_content(file);

    // Spring configuration file completions.
    if let Some(path) = db.file_path(file) {
        if is_application_properties(path) {
            let root = project_root_for_path(path);
            let workspace = WORKSPACE_CACHE.spring_workspace(db, &root, cancel);
            if !workspace.is_spring {
                return Vec::new();
            }
            let mut items = nova_framework_spring::completions_for_properties_file(
                path,
                text,
                offset,
                workspace.index.as_ref(),
            );
            if let Some(span) =
                nova_framework_spring::completion_span_for_properties_file(path, text, offset)
            {
                for item in &mut items {
                    item.replace_span = Some(span);
                }
            }
            return items;
        }

        if is_application_yaml(path) {
            let root = project_root_for_path(path);
            let workspace = WORKSPACE_CACHE.spring_workspace(db, &root, cancel);
            if !workspace.is_spring {
                return Vec::new();
            }
            let mut items = nova_framework_spring::completions_for_yaml_file(
                path,
                text,
                offset,
                workspace.index.as_ref(),
            );
            if let Some(span) = nova_framework_spring::completion_span_for_yaml_file(text, offset) {
                for item in &mut items {
                    item.replace_span = Some(span);
                }
            }
            return items;
        }
    }

    if !is_java_file(db, file) {
        return Vec::new();
    }

    // Spring DI completions (`@Qualifier`, `@Profile`) inside Java source.
    if let Some(ctx) = crate::spring_di::annotation_string_context(text, offset) {
        match ctx {
            crate::spring_di::AnnotationStringContext::Qualifier => {
                let items =
                    crate::spring_di::qualifier_completion_items_with_cancel(db, file, cancel);
                if !items.is_empty() {
                    return items;
                }
            }
            crate::spring_di::AnnotationStringContext::Profile => {
                let items =
                    crate::spring_di::profile_completion_items_with_cancel(db, file, cancel);
                if !items.is_empty() {
                    return items;
                }
            }
        }
    }

    // Spring / Micronaut `@Value("${...}")` completions.
    if cursor_inside_value_placeholder(text, offset) {
        if spring_value_completion_applicable(db, file, text, cancel) {
            if let Some(path) = db.file_path(file) {
                let root = project_root_for_path(path);
                let workspace = WORKSPACE_CACHE.spring_workspace(db, &root, cancel);
                if workspace.is_spring {
                    let mut items = nova_framework_spring::completions_for_value_placeholder(
                        text,
                        offset,
                        workspace.index.as_ref(),
                    );
                    if !items.is_empty() {
                        if let Some(span) =
                            nova_framework_spring::completion_span_for_value_placeholder(
                                text, offset,
                            )
                        {
                            for item in &mut items {
                                item.replace_span = Some(span);
                            }
                        }
                        return items;
                    }
                }
            }
        }

        // Micronaut `@Value("${...}")` completions as a fallback.
        if let Some(analysis) =
            crate::micronaut_intel::analysis_for_file_with_cancel(db, file, cancel)
        {
            let mut items = nova_framework_micronaut::completions_for_value_placeholder(
                text,
                offset,
                &analysis.config_keys,
            );
            if !items.is_empty() {
                if let Some(span) =
                    nova_framework_micronaut::completion_span_for_value_placeholder(text, offset)
                {
                    for item in &mut items {
                        item.replace_span = Some(span);
                    }
                }
                return items;
            }
        }
    }

    // Quarkus `@ConfigProperty(name="...")` completions.
    if let Some(prefix) = quarkus_config_property_prefix(text, offset) {
        if let Some(path) = db.file_path(file) {
            let root = project_root_for_path(path);
            if WORKSPACE_CACHE
                .quarkus_analysis(db, &root, cancel, Some(&[text]))
                .is_some()
            {
                let items = quarkus_config_completions(db, &root, &prefix, cancel);
                if !items.is_empty() {
                    return items;
                }
            }
        }
    }

    // JPQL completions inside JPA `@Query(...)` / `@NamedQuery(query=...)` strings.
    if let Some((query, query_cursor)) = crate::jpa_intel::jpql_query_at_cursor(text, offset) {
        if let Some(project) = crate::jpa_intel::project_for_file_with_cancel(db, file, cancel) {
            if let Some(analysis) = project.analysis.as_ref() {
                let items =
                    nova_framework_jpa::jpql_completions(&query, query_cursor, &analysis.model);
                if !items.is_empty() {
                    return items;
                }
            }
        }
    }

    // MapStruct `@Mapping(target="...")` / `@Mapping(source="...")` property completions.
    //
    // Guard with a cheap text heuristic to avoid filesystem scans on unrelated files.
    let maybe_mapstruct_file = nova_framework_mapstruct::looks_like_mapstruct_source(text);
    if maybe_mapstruct_file {
        if cancel.is_cancelled() {
            return Vec::new();
        }
        if let Some(path) = db.file_path(file) {
            let root = project_root_for_path(path);
            if let Ok(items) =
                nova_framework_mapstruct::completions_for_file(&root, path, text, offset)
            {
                if !items.is_empty() && !cancel.is_cancelled() {
                    return items;
                }
            }
        }
    }

    Vec::new()
}

#[derive(Debug)]
pub struct FrameworkWorkspaceCache {
    project_configs: Mutex<LruCache<PathBuf, CachedProjectConfig>>,
    classpath_indexes: Mutex<LruCache<PathBuf, CachedClasspathIndex>>,
    spring_metadata: Mutex<LruCache<PathBuf, CachedMetadataIndex>>,
    spring_workspace: Mutex<LruCache<DbRootKey, CachedSpringWorkspace>>,
    quarkus: Mutex<LruCache<DbRootKey, CachedQuarkusWorkspace>>,
}

#[derive(Clone, Debug)]
struct CachedProjectConfig {
    fingerprint: u64,
    value: Option<Arc<nova_project::ProjectConfig>>,
}

#[derive(Clone, Debug)]
struct CachedClasspathIndex {
    fingerprint: u64,
    value: Option<Arc<nova_classpath::ClasspathIndex>>,
}

#[derive(Clone, Debug)]
struct CachedMetadataIndex {
    fingerprint: u64,
    value: Arc<MetadataIndex>,
}

#[derive(Clone, Debug)]
struct CachedSpringWorkspace {
    fingerprint: u64,
    is_spring: bool,
    index: Arc<nova_framework_spring::SpringWorkspaceIndex>,
}

#[derive(Clone, Debug)]
struct CachedQuarkusWorkspace {
    fingerprint: u64,
    file_ids: Vec<FileId>,
    file_id_to_source: HashMap<FileId, usize>,
    analysis: Option<Arc<nova_framework_quarkus::AnalysisResultWithSpans>>,
}

#[derive(Clone, Debug)]
struct SpringWorkspace {
    is_spring: bool,
    index: Arc<nova_framework_spring::SpringWorkspaceIndex>,
}

impl FrameworkWorkspaceCache {
    #[must_use]
    pub fn new() -> Self {
        Self {
            project_configs: Mutex::new(LruCache::new(MAX_CACHED_ROOTS)),
            classpath_indexes: Mutex::new(LruCache::new(MAX_CACHED_ROOTS)),
            spring_metadata: Mutex::new(LruCache::new(MAX_CACHED_ROOTS)),
            spring_workspace: Mutex::new(LruCache::new(MAX_CACHED_ROOTS)),
            quarkus: Mutex::new(LruCache::new(MAX_CACHED_ROOTS)),
        }
    }

    fn project_config(&self, root: &Path) -> Option<Arc<nova_project::ProjectConfig>> {
        let root = canonicalize_root(root)?;
        let fingerprint = build_marker_fingerprint(&root);

        {
            let mut cache = lock_unpoison(&self.project_configs);
            if let Some(entry) = cache.get_cloned(&root) {
                if entry.fingerprint == fingerprint {
                    return entry.value;
                }
            }
        }

        let value = match nova_project::load_project(&root) {
            Ok(config) => Some(Arc::new(config)),
            Err(_) => None,
        };

        let entry = CachedProjectConfig {
            fingerprint,
            value: value.clone(),
        };
        let mut cache = lock_unpoison(&self.project_configs);
        cache.insert(root, entry);

        value
    }

    fn classpath_index(&self, root: &Path) -> Option<Arc<nova_classpath::ClasspathIndex>> {
        let root = canonicalize_root(root)?;
        let fingerprint = build_marker_fingerprint(&root);

        {
            let mut cache = lock_unpoison(&self.classpath_indexes);
            if let Some(entry) = cache.get_cloned(&root) {
                if entry.fingerprint == fingerprint {
                    return entry.value;
                }
            }
        }

        let value = self.project_config(&root).and_then(|config| {
            let entries: Vec<_> = config
                .classpath
                .iter()
                .chain(config.module_path.iter())
                .filter(|entry| match entry.kind {
                    nova_project::ClasspathEntryKind::Directory => entry.path.is_dir(),
                    nova_project::ClasspathEntryKind::Jar => entry.path.is_file() || entry.path.is_dir(),
                    #[allow(unreachable_patterns)]
                    _ => false,
                })
                .filter_map(to_classpath_entry)
                .collect();

            // Respect the workspace's configured language level for multi-release jars
            // (`META-INF/versions/<n>/...`).
            let target_release = Some(config.java.target.0)
                .filter(|release| *release >= 1)
                .or_else(|| Some(config.java.source.0).filter(|release| *release >= 1));

            nova_classpath::ClasspathIndex::build_with_options(
                &entries,
                None,
                IndexOptions { target_release },
            )
            .ok()
            .map(Arc::new)
        });

        let entry = CachedClasspathIndex {
            fingerprint,
            value: value.clone(),
        };
        lock_unpoison(&self.classpath_indexes).insert(root, entry);

        value
    }

    fn spring_metadata_index(&self, root: &Path) -> Arc<MetadataIndex> {
        let Some(root) = canonicalize_root(root) else {
            return Arc::new(MetadataIndex::new());
        };

        // Include output-dir metadata files in the fingerprint so local
        // `spring-configuration-metadata.json` generated by annotation processors
        // is picked up after builds (without requiring a build-file change).
        let config = self.project_config(&root);
        let fingerprint = spring_metadata_fingerprint(&root, config.as_deref());

        {
            let mut cache = lock_unpoison(&self.spring_metadata);
            if let Some(entry) = cache.get_cloned(&root) {
                if entry.fingerprint == fingerprint {
                    return entry.value;
                }
            }
        }

        let value = config
            .map(|config| {
                let mut classpath: Vec<_> = config
                    .classpath
                    .iter()
                    .chain(config.module_path.iter())
                    .filter(|entry| match entry.kind {
                        nova_project::ClasspathEntryKind::Directory => entry.path.is_dir(),
                        nova_project::ClasspathEntryKind::Jar => {
                            entry.path.is_file() || entry.path.is_dir()
                        }
                        #[allow(unreachable_patterns)]
                        _ => false,
                    })
                    .filter_map(to_classpath_entry)
                    .collect();

                // Include workspace output directories in the metadata search.
                for output_dir in &config.output_dirs {
                    if output_dir.path.is_dir() {
                        classpath.push(nova_classpath::ClasspathEntry::ClassDir(
                            output_dir.path.clone(),
                        ));
                    }
                }

                let mut index = MetadataIndex::new();
                match index.ingest_classpath(&classpath) {
                    Ok(()) => Arc::new(index),
                    Err(_) => Arc::new(MetadataIndex::new()),
                }
            })
            .unwrap_or_else(|| Arc::new(MetadataIndex::new()));

        let entry = CachedMetadataIndex {
            fingerprint,
            value: value.clone(),
        };
        let mut cache = lock_unpoison(&self.spring_metadata);
        cache.insert(root, entry);

        value
    }

    fn spring_workspace<DB: ?Sized + Database>(
        &self,
        db: &DB,
        root: &Path,
        cancel: &CancellationToken,
    ) -> SpringWorkspace {
        let raw_root = root.to_path_buf();
        let canonical_root = normalize_root_for_cache(root);
        let has_alt_root = canonical_root != raw_root;
        let key = (db_cache_id(db), canonical_root.clone());

        let build_fingerprint = build_marker_fingerprint(&canonical_root);

        let mut files: Vec<(PathBuf, FileId, SpringWorkspaceFileKind)> = Vec::new();
        let mut marker_says_spring = false;

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        build_fingerprint.hash(&mut hasher);

        for file_id in db.all_file_ids() {
            if cancel.is_cancelled() {
                if let Some(existing) = lock_unpoison(&self.spring_workspace).get_cloned(&key) {
                    return SpringWorkspace {
                        is_spring: existing.is_spring,
                        index: existing.index,
                    };
                }
                return SpringWorkspace {
                    is_spring: false,
                    index: Arc::new(nova_framework_spring::SpringWorkspaceIndex::new(Arc::new(
                        MetadataIndex::new(),
                    ))),
                };
            }

            let Some(path) = db.file_path(file_id) else {
                continue;
            };
            if !(path.starts_with(&raw_root) || (has_alt_root && path.starts_with(&canonical_root)))
            {
                continue;
            }

            let kind = if path.extension().and_then(|e| e.to_str()) == Some("java") {
                SpringWorkspaceFileKind::Java
            } else if is_application_properties(path) || is_application_yaml(path) {
                SpringWorkspaceFileKind::Config
            } else {
                continue;
            };

            let text = db.file_content(file_id);
            if matches!(kind, SpringWorkspaceFileKind::Java) && looks_like_spring_source(text) {
                marker_says_spring = true;
            }

            path.hash(&mut hasher);
            text.len().hash(&mut hasher);
            text.as_ptr().hash(&mut hasher);

            files.push((path.to_path_buf(), file_id, kind));
        }

        files.sort_by(|(a, _, _), (b, _, _)| a.cmp(b));
        let fingerprint = hasher.finish();

        let config_says_spring = self
            .project_config(&canonical_root)
            .is_some_and(|cfg| nova_framework_spring::is_spring_applicable(cfg.as_ref()));
        let is_spring = config_says_spring || marker_says_spring;

        // Fast path: cache hit.
        {
            let mut cache = lock_unpoison(&self.spring_workspace);
            if let Some(entry) = cache.get_cloned(&key) {
                if entry.fingerprint == fingerprint {
                    return SpringWorkspace {
                        is_spring: entry.is_spring,
                        index: entry.index,
                    };
                }
            }
        }

        if cancel.is_cancelled() {
            if let Some(existing) = lock_unpoison(&self.spring_workspace).get_cloned(&key) {
                return SpringWorkspace {
                    is_spring: existing.is_spring,
                    index: existing.index,
                };
            }
            return SpringWorkspace {
                is_spring: false,
                index: Arc::new(nova_framework_spring::SpringWorkspaceIndex::new(Arc::new(
                    MetadataIndex::new(),
                ))),
            };
        }

        if !is_spring {
            let index = Arc::new(nova_framework_spring::SpringWorkspaceIndex::new(Arc::new(
                MetadataIndex::new(),
            )));
            let entry = CachedSpringWorkspace {
                fingerprint,
                is_spring,
                index: Arc::clone(&index),
            };
            lock_unpoison(&self.spring_workspace).insert(key.clone(), entry);
            return SpringWorkspace { is_spring, index };
        }

        let metadata = self.spring_metadata_index(&canonical_root);
        let mut index = nova_framework_spring::SpringWorkspaceIndex::new(metadata.clone());

        for (path, file_id, kind) in files {
            if cancel.is_cancelled() {
                if let Some(existing) = lock_unpoison(&self.spring_workspace).get_cloned(&key) {
                    return SpringWorkspace {
                        is_spring: existing.is_spring,
                        index: existing.index,
                    };
                }
                return SpringWorkspace {
                    is_spring: false,
                    index: Arc::new(nova_framework_spring::SpringWorkspaceIndex::new(Arc::new(
                        MetadataIndex::new(),
                    ))),
                };
            }

            let text = db.file_content(file_id);
            match kind {
                SpringWorkspaceFileKind::Java => {
                    // Avoid scanning every Java file in the workspace; only files that might
                    // contain Spring config usages can contribute anything to the index.
                    if text.contains("@Value") || text.contains("@ConfigurationProperties") {
                        index.add_java_file(path, text);
                    }
                }
                SpringWorkspaceFileKind::Config => index.add_config_file(path, text),
            }
        }

        let index = Arc::new(index);
        let entry = CachedSpringWorkspace {
            fingerprint,
            is_spring,
            index: Arc::clone(&index),
        };
        lock_unpoison(&self.spring_workspace).insert(key, entry);
        SpringWorkspace { is_spring, index }
    }

    fn quarkus_analysis<DB: ?Sized + Database>(
        &self,
        db: &DB,
        root: &Path,
        cancel: &CancellationToken,
        applicability_sources: Option<&[&str]>,
    ) -> Option<CachedQuarkusWorkspace> {
        let raw_root = root.to_path_buf();
        let canonical_root = normalize_root_for_cache(root);
        let has_alt_root = canonical_root != raw_root;
        let key = (db_cache_id(db), canonical_root.clone());

        let build_fingerprint = build_marker_fingerprint(&canonical_root);

        // Collect java files under the root (fallback to all Java files if the root contains none).
        let mut under_root = Vec::<(PathBuf, FileId)>::new();
        let mut all = Vec::<(PathBuf, FileId)>::new();

        for file_id in db.all_file_ids() {
            if cancel.is_cancelled() {
                return lock_unpoison(&self.quarkus).get_cloned(&key);
            }

            let Some(path) = db.file_path(file_id) else {
                continue;
            };
            if path.extension().and_then(|e| e.to_str()) != Some("java") {
                continue;
            }
            let tuple = (path.to_path_buf(), file_id);
            if path.starts_with(&raw_root) || (has_alt_root && path.starts_with(&canonical_root)) {
                under_root.push(tuple);
            } else {
                all.push(tuple);
            }
        }

        let mut files = if under_root.is_empty() {
            all
        } else {
            under_root
        };
        if files.is_empty() {
            return None;
        }
        files.sort_by(|(a, _), (b, _)| a.cmp(b));

        // Fingerprint sources (cheap pointer/len hashing).
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        build_fingerprint.hash(&mut hasher);
        for (path, file_id) in &files {
            if cancel.is_cancelled() {
                return lock_unpoison(&self.quarkus).get_cloned(&key);
            }
            path.hash(&mut hasher);
            let text = db.file_content(*file_id);
            text.len().hash(&mut hasher);
            text.as_ptr().hash(&mut hasher);
        }
        let fingerprint = hasher.finish();

        // Cache hit.
        {
            let mut cache = lock_unpoison(&self.quarkus);
            if let Some(entry) = cache.get_cloned(&key) {
                if entry.fingerprint == fingerprint {
                    return Some(entry);
                }
            }
        }

        // If cancelled, fall back to a stale entry.
        if cancel.is_cancelled() {
            return lock_unpoison(&self.quarkus).get_cloned(&key);
        }

        // Determine applicability. If the caller passed a small subset of sources, use that for the
        // check to avoid extra work.
        let mut source_refs = Vec::<&str>::new();
        if let Some(sources) = applicability_sources {
            source_refs.extend_from_slice(sources);
        } else {
            source_refs.extend(files.iter().map(|(_, id)| db.file_content(*id)));
        }

        let applicable = is_quarkus_project_with_root(db, &canonical_root, &source_refs);
        let analysis = applicable.then(|| {
            let sources: Vec<&str> = files.iter().map(|(_, id)| db.file_content(*id)).collect();
            Arc::new(nova_framework_quarkus::analyze_java_sources_with_spans(
                &sources,
            ))
        });

        let file_ids: Vec<FileId> = files.iter().map(|(_, id)| *id).collect();
        let file_id_to_source: HashMap<FileId, usize> = file_ids
            .iter()
            .enumerate()
            .map(|(idx, id)| (*id, idx))
            .collect();

        let entry = CachedQuarkusWorkspace {
            fingerprint,
            file_ids,
            file_id_to_source,
            analysis,
        };
        lock_unpoison(&self.quarkus).insert(key, entry.clone());
        Some(entry)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SpringWorkspaceFileKind {
    Java,
    Config,
}

#[derive(Debug)]
struct LruCache<K, V> {
    capacity: usize,
    map: HashMap<K, V>,
    order: VecDeque<K>,
}

impl<K, V> LruCache<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get_cloned(&mut self, key: &K) -> Option<V> {
        let value = self.map.get(key)?.clone();
        self.touch(key);
        Some(value)
    }

    fn insert(&mut self, key: K, value: V) {
        self.map.insert(key.clone(), value);
        self.touch(&key);
        self.evict_if_needed();
    }

    fn touch(&mut self, key: &K) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
        self.order.push_back(key.clone());
    }

    fn evict_if_needed(&mut self) {
        while self.map.len() > self.capacity {
            let Some(key) = self.order.pop_front() else {
                break;
            };
            self.map.remove(&key);
        }
    }
}

fn canonicalize_root(root: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(root).ok()
}

fn normalize_root_for_cache(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

fn lock_unpoison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}

pub(crate) fn build_marker_fingerprint(root: &Path) -> u64 {
    use std::collections::hash_map::DefaultHasher;

    const MARKERS: &[&str] = &[
        // Maven.
        "pom.xml",
        // Gradle.
        "build.gradle",
        "build.gradle.kts",
        "settings.gradle",
        "settings.gradle.kts",
        // Bazel.
        "WORKSPACE",
        "WORKSPACE.bazel",
        "MODULE.bazel",
        "MODULE.bazel.lock",
        ".bazelrc",
        ".bazelversion",
        "bazelisk.rc",
        ".bazelignore",
        // Simple projects.
        "src",
    ];

    let mut hasher = DefaultHasher::new();
    for marker in MARKERS {
        let path = root.join(marker);
        marker.hash(&mut hasher);
        match std::fs::metadata(&path) {
            Ok(meta) => {
                true.hash(&mut hasher);
                meta.len().hash(&mut hasher);
                hash_mtime(&mut hasher, meta.modified().ok());
            }
            Err(_) => {
                false.hash(&mut hasher);
            }
        }
    }

    // Include any `.bazelrc.*` fragments at the workspace root. These are commonly imported by
    // `.bazelrc` and can affect Bazel query/aquery behavior, which in turn affects Nova's project
    // model and framework/classpath analysis.
    //
    // Keep this best-effort and bounded: scan only the immediate workspace root directory.
    let mut bazelrc_fragments = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if file_name.starts_with(".bazelrc.") {
                bazelrc_fragments.push(path);
                // Avoid pathological roots with huge numbers of dotfiles.
                if bazelrc_fragments.len() >= 128 {
                    break;
                }
            }
        }
    }
    bazelrc_fragments.sort();
    for path in bazelrc_fragments {
        if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
            name.hash(&mut hasher);
        }
        match std::fs::metadata(&path) {
            Ok(meta) => {
                true.hash(&mut hasher);
                meta.len().hash(&mut hasher);
                hash_mtime(&mut hasher, meta.modified().ok());
            }
            Err(_) => {
                false.hash(&mut hasher);
            }
        }
    }

    hasher.finish()
}

fn spring_metadata_fingerprint(root: &Path, config: Option<&nova_project::ProjectConfig>) -> u64 {
    use std::collections::hash_map::DefaultHasher;

    let mut hasher = DefaultHasher::new();
    build_marker_fingerprint(root).hash(&mut hasher);

    const META_FILES: &[&str] = &[
        "META-INF/spring-configuration-metadata.json",
        "META-INF/additional-spring-configuration-metadata.json",
    ];

    if let Some(config) = config {
        for output_dir in &config.output_dirs {
            output_dir.path.hash(&mut hasher);
            for rel in META_FILES {
                rel.hash(&mut hasher);
                let path = output_dir.path.join(rel);
                match std::fs::metadata(&path) {
                    Ok(meta) => {
                        true.hash(&mut hasher);
                        meta.len().hash(&mut hasher);
                        hash_mtime(&mut hasher, meta.modified().ok());
                    }
                    Err(_) => {
                        false.hash(&mut hasher);
                    }
                }
            }
        }
    }

    hasher.finish()
}

fn hash_mtime(hasher: &mut impl Hasher, time: Option<SystemTime>) {
    let Some(time) = time else {
        0u64.hash(hasher);
        return;
    };

    let duration = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| std::time::Duration::from_secs(0));
    duration.as_secs().hash(hasher);
    duration.subsec_nanos().hash(hasher);
}

fn is_java_file<DB: ?Sized + Database>(db: &DB, file: FileId) -> bool {
    db.file_path(file)
        .is_some_and(|path| path.extension().and_then(|e| e.to_str()) == Some("java"))
}

pub(crate) fn is_application_properties(path: &Path) -> bool {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    name.starts_with("application")
        && path.extension().and_then(|e| e.to_str()) == Some("properties")
}

pub(crate) fn is_application_yaml(path: &Path) -> bool {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if !name.starts_with("application") {
        return false;
    }
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("yml" | "yaml")
    )
}

fn looks_like_spring_source(text: &str) -> bool {
    text.contains("import org.springframework") || text.contains("@org.springframework")
}

fn cursor_inside_value_placeholder(java_source: &str, cursor: usize) -> bool {
    // Best-effort detection for `@Value("${...}")` contexts (Spring or Micronaut).
    // This is used purely as a guard to avoid running framework analysis for
    // completions when the cursor isn't inside a placeholder.
    let prefix = java_source.get(..cursor).unwrap_or(java_source);
    let Some(value_start) = prefix.rfind("@Value") else {
        return false;
    };

    let after_value = &java_source[value_start..];
    let Some(open_quote_rel) = after_value.find('"') else {
        return false;
    };
    let content_start = value_start + open_quote_rel + 1;
    let Some(after_open_quote) = java_source.get(content_start..) else {
        return false;
    };
    let Some(close_quote_rel) = after_open_quote.find('"') else {
        return false;
    };
    let content_end = content_start + close_quote_rel;

    if cursor < content_start || cursor > content_end {
        return false;
    }

    let content = &java_source[content_start..content_end];
    let rel_cursor = cursor - content_start;
    let Some(open_rel) = content[..rel_cursor].rfind("${") else {
        return false;
    };
    let key_start_rel = open_rel + 2;
    if rel_cursor < key_start_rel {
        return false;
    }

    let after_key = &content[key_start_rel..];
    let key_end_rel = after_key
        .find(|c| c == '}' || c == ':')
        .unwrap_or(after_key.len())
        + key_start_rel;

    rel_cursor <= key_end_rel
}

fn spring_value_completion_applicable<DB: ?Sized + Database>(
    db: &DB,
    file: FileId,
    java_source: &str,
    cancel: &CancellationToken,
) -> bool {
    if cancel.is_cancelled() {
        return false;
    }

    let Some(path) = db.file_path(file) else {
        return looks_like_spring_source(java_source);
    };

    let root = project_root_for_path(path);
    WORKSPACE_CACHE
        .spring_workspace(db, &root, cancel)
        .is_spring
        || looks_like_spring_source(java_source)
}

fn quarkus_diagnostics_for_file<DB: ?Sized + Database>(
    db: &DB,
    file: FileId,
    java_source: &str,
    cancel: &CancellationToken,
) -> Vec<Diagnostic> {
    if cancel.is_cancelled() {
        return Vec::new();
    }

    let Some(path) = db.file_path(file) else {
        return Vec::new();
    };

    let root = project_root_for_path(path);
    let Some(workspace) = WORKSPACE_CACHE.quarkus_analysis(db, &root, cancel, Some(&[java_source]))
    else {
        return Vec::new();
    };

    let Some(analysis) = workspace.analysis.as_ref() else {
        return Vec::new();
    };
    let Some(source_idx) = workspace.file_id_to_source.get(&file).copied() else {
        return Vec::new();
    };

    analysis
        .diagnostics
        .iter()
        .filter(|d| d.source == source_idx)
        .map(|d| d.diagnostic.clone())
        .collect()
}

fn micronaut_diagnostics_for_file<DB: ?Sized + Database>(
    db: &DB,
    file: FileId,
    java_source: &str,
    cancel: &CancellationToken,
) -> Vec<Diagnostic> {
    if cancel.is_cancelled() {
        return Vec::new();
    }

    let Some(path) = db.file_path(file) else {
        return Vec::new();
    };
    if path.extension().and_then(|e| e.to_str()) != Some("java") {
        return Vec::new();
    }
    if !crate::micronaut_intel::may_have_micronaut_diagnostics(java_source) {
        return Vec::new();
    }

    let Some(analysis) = crate::micronaut_intel::analysis_for_file_with_cancel(db, file, cancel)
    else {
        return Vec::new();
    };

    let path = path.to_string_lossy();
    analysis
        .file_diagnostics
        .iter()
        .filter(|d| d.file == path.as_ref())
        .map(|d| d.diagnostic.clone())
        .collect()
}

fn quarkus_config_completions<DB: ?Sized + Database>(
    db: &DB,
    root: &Path,
    prefix: &str,
    cancel: &CancellationToken,
) -> Vec<CompletionItem> {
    if cancel.is_cancelled() {
        return Vec::new();
    }

    let Some(workspace) = WORKSPACE_CACHE.quarkus_analysis(db, root, cancel, None) else {
        return Vec::new();
    };
    let Some(analysis) = workspace.analysis.as_ref() else {
        return Vec::new();
    };

    let mut props = std::collections::BTreeSet::<String>::new();
    props.extend(analysis.config_properties.iter().cloned());

    for file_id in db.all_file_ids() {
        if cancel.is_cancelled() {
            break;
        }
        let Some(path) = db.file_path(file_id) else {
            continue;
        };
        if !path.starts_with(root) {
            continue;
        }
        if !is_application_properties(path) {
            continue;
        }

        let text = db.file_content(file_id);
        for raw_line in text.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let key = line.split('=').next().unwrap_or(line).trim();
            if !key.is_empty() {
                props.insert(key.to_string());
            }
        }
    }

    props
        .into_iter()
        .filter(|name| name.starts_with(prefix))
        .map(CompletionItem::new)
        .collect()
}

fn is_quarkus_project_with_root<DB: ?Sized + Database>(
    _db: &DB,
    root: &Path,
    java_sources: &[&str],
) -> bool {
    if let Some(config) = project_config(root) {
        let dep_strings: Vec<String> = config
            .dependencies
            .iter()
            .map(|d| format!("{}:{}", d.group_id, d.artifact_id))
            .collect();
        let dep_refs: Vec<&str> = dep_strings.iter().map(String::as_str).collect();

        let classpath: Vec<&Path> = config
            .classpath
            .iter()
            .map(|e| e.path.as_path())
            .chain(config.module_path.iter().map(|e| e.path.as_path()))
            .collect();

        return nova_framework_quarkus::is_quarkus_applicable_with_classpath(
            &dep_refs,
            classpath.as_slice(),
            java_sources,
        );
    }

    nova_framework_quarkus::is_quarkus_applicable(&[], java_sources)
}

fn quarkus_config_property_prefix(text: &str, offset: usize) -> Option<String> {
    let bytes = text.as_bytes();
    if offset > bytes.len() {
        return None;
    }

    // Find the opening quote for the string literal containing the cursor.
    let mut start_quote = None;
    let mut i = offset;
    while i > 0 {
        i -= 1;
        if bytes[i] == b'"' && !is_escaped_quote(bytes, i) {
            start_quote = Some(i);
            break;
        }
    }
    let start_quote = start_quote?;

    // Find the closing quote.
    let mut end_quote = None;
    let mut j = start_quote + 1;
    while j < bytes.len() {
        if bytes[j] == b'"' && !is_escaped_quote(bytes, j) {
            end_quote = Some(j);
            break;
        }
        j += 1;
    }
    let end_quote = end_quote?;

    if !(start_quote < offset && offset <= end_quote) {
        return None;
    }

    // Ensure we're completing the `name = "..."` argument.
    let mut k = start_quote;
    while k > 0 && (bytes[k - 1] as char).is_ascii_whitespace() {
        k -= 1;
    }
    if k == 0 || bytes[k - 1] != b'=' {
        return None;
    }
    k -= 1;
    while k > 0 && (bytes[k - 1] as char).is_ascii_whitespace() {
        k -= 1;
    }
    let mut ident_start = k;
    while ident_start > 0 && is_ident_continue(bytes[ident_start - 1] as char) {
        ident_start -= 1;
    }
    let ident = text.get(ident_start..k)?;
    if ident != "name" {
        return None;
    }

    // Ensure the nearest preceding annotation is `@ConfigProperty`.
    let before_ident = &text[..ident_start];
    let at_idx = before_ident.rfind('@')?;
    let after_at = &before_ident[at_idx + 1..];

    let mut ann_end = 0usize;
    for (idx, ch) in after_at.char_indices() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' || ch == '.' {
            ann_end = idx + ch.len_utf8();
        } else {
            break;
        }
    }
    if ann_end == 0 {
        return None;
    }
    let ann = &after_at[..ann_end];
    let simple = ann.rsplit('.').next().unwrap_or(ann);
    if simple != "ConfigProperty" {
        return None;
    }

    Some(text[start_quote + 1..offset].to_string())
}

fn is_escaped_quote(bytes: &[u8], idx: usize) -> bool {
    let mut backslashes = 0usize;
    let mut i = idx;
    while i > 0 {
        i -= 1;
        if bytes[i] == b'\\' {
            backslashes += 1;
        } else {
            break;
        }
    }
    backslashes % 2 == 1
}

fn is_ident_continue(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '$'
}
