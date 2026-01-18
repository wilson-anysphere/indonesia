//! Adapter between `nova-db` and `nova-framework`.
//!
//! `nova-framework` analyzers expect a small [`nova_framework::Database`] surface: file text/path
//! lookup, project scoping, dependency/classpath queries, and (best-effort) class metadata.
//!
//! Nova's IDE layer (`nova-ide`/`nova-lsp`) is currently built around the legacy
//! [`nova_db::Database`] trait. This module bridges the two so `nova-framework` analyzers can run
//! inside `nova-ide` without bespoke per-framework glue.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use once_cell::sync::Lazy;

use nova_core::ProjectId;
use nova_db::{Database as HostDatabase, FileId};
use nova_framework::Database as FrameworkDatabase;
use nova_hir::framework::ClassData;
use nova_scheduler::CancellationToken;
use nova_types::ClassId;
use zip::ZipArchive;

const MAX_CACHED_ROOTS: usize = 16;

static FRAMEWORK_DB_CACHE: Lazy<Mutex<LruCache<FrameworkDbCacheKey, CachedFrameworkDb>>> =
    Lazy::new(|| Mutex::new(LruCache::new(MAX_CACHED_ROOTS)));

static PROJECT_IDS: Lazy<Mutex<ProjectIdAllocator>> =
    Lazy::new(|| Mutex::new(ProjectIdAllocator::new()));

/// Returns a cached, best-effort [`nova_framework::Database`] for `file`.
///
/// The returned database:
/// - preserves host `FileId` identity (delegates to the host DB for file text/path/id).
/// - scopes `ProjectId` and `all_files(project)` to the workspace root containing `file`.
/// - uses `framework_cache::project_config` (when available) for dependency/classpath queries.
/// - parses Java sources in the workspace root into [`nova_hir::framework::ClassData`] so
///   framework analyzers can inspect annotations/members.
///
/// This is cancellation-aware: if `cancel` is triggered while building/updating the cached DB, we
/// fall back to a stale entry (when available).
pub fn framework_db_for_file(
    db: Arc<dyn HostDatabase + Send + Sync>,
    file: FileId,
    cancel: &CancellationToken,
) -> Option<Arc<dyn FrameworkDatabase + Send + Sync>> {
    let shared = shared_db_for_file(db, file, cancel)?;
    let wrapper = FrameworkDb {
        shared,
        cancel: cancel.clone(),
    };
    Some(Arc::new(wrapper))
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct FrameworkDbCacheKey {
    host_db_ptr: usize,
    root: PathBuf,
}

#[derive(Clone)]
struct CachedFrameworkDb {
    fingerprint: u64,
    db: Arc<FrameworkDbShared>,
}

struct FrameworkDbShared {
    host_db: Arc<dyn HostDatabase + Send + Sync>,
    project: ProjectId,

    all_files: Vec<FileId>,

    classes: Vec<ClassData>,

    config: Option<Arc<nova_project::ProjectConfig>>,

    synthetic_files_by_id: HashMap<FileId, (PathBuf, String)>,
    synthetic_ids_by_path: HashMap<PathBuf, FileId>,

    classpath_exact_cache: Mutex<HashMap<String, bool>>,
    classpath_prefix_cache: Mutex<HashMap<String, bool>>,
}

#[derive(Clone)]
struct FrameworkDb {
    shared: Arc<FrameworkDbShared>,
    cancel: CancellationToken,
}

impl FrameworkDbShared {
    fn class(&self, class: ClassId) -> &ClassData {
        static UNKNOWN: OnceLock<ClassData> = OnceLock::new();

        let idx = class.to_raw() as usize;
        self.classes.get(idx).unwrap_or_else(|| {
            UNKNOWN.get_or_init(|| {
                let mut data = ClassData::default();
                data.name = "<unknown>".to_string();
                data
            })
        })
    }

    fn all_classes(&self) -> Vec<ClassId> {
        (0..self.classes.len())
            .map(|idx| ClassId::new(idx as u32))
            .collect()
    }

    fn has_dependency(&self, group: &str, artifact: &str) -> bool {
        let Some(config) = self.config.as_ref() else {
            return false;
        };
        config
            .dependencies
            .iter()
            .any(|dep| dep.group_id == group && dep.artifact_id == artifact)
    }

    fn classpath_entries(&self) -> impl Iterator<Item = &nova_project::ClasspathEntry> {
        self.config
            .as_ref()
            .into_iter()
            .flat_map(|cfg| cfg.classpath.iter().chain(cfg.module_path.iter()))
    }

    fn classpath_contains_exact(
        &self,
        normalized_class_file: &str,
        cancel: &CancellationToken,
    ) -> bool {
        if cancel.is_cancelled() {
            return false;
        }

        let target_release = self.config.as_ref().map(|cfg| cfg.java.target.0);
        for entry in self.classpath_entries() {
            if cancel.is_cancelled() {
                return false;
            }

            match entry.kind {
                nova_project::ClasspathEntryKind::Directory => {
                    let candidate = entry.path.join(normalized_class_file);
                    if std::fs::metadata(&candidate)
                        .map(|m| m.is_file())
                        .unwrap_or(false)
                    {
                        return true;
                    }
                }
                nova_project::ClasspathEntryKind::Jar => {
                    if zip_contains_exact(
                        entry.path.as_path(),
                        normalized_class_file,
                        target_release,
                        cancel,
                    ) {
                        return true;
                    }
                }
            }
        }

        false
    }

    fn classpath_contains_prefix(
        &self,
        normalized_prefix: &str,
        cancel: &CancellationToken,
    ) -> bool {
        if cancel.is_cancelled() {
            return false;
        }

        let target_release = self.config.as_ref().map(|cfg| cfg.java.target.0);
        for entry in self.classpath_entries() {
            if cancel.is_cancelled() {
                return false;
            }

            match entry.kind {
                nova_project::ClasspathEntryKind::Directory => {
                    let candidate = entry.path.join(normalized_prefix);
                    if std::fs::metadata(&candidate).is_ok() {
                        return true;
                    }
                }
                nova_project::ClasspathEntryKind::Jar => {
                    if zip_contains_prefix(
                        entry.path.as_path(),
                        normalized_prefix,
                        target_release,
                        cancel,
                    ) {
                        return true;
                    }
                }
            }
        }

        false
    }

    fn synthetic_file_text(&self, file: FileId) -> Option<&str> {
        self.synthetic_files_by_id
            .get(&file)
            .map(|(_, text)| text.as_str())
    }

    fn synthetic_file_path(&self, file: FileId) -> Option<&Path> {
        self.synthetic_files_by_id
            .get(&file)
            .map(|(path, _)| path.as_path())
    }

    fn synthetic_file_id(&self, path: &Path) -> Option<FileId> {
        self.synthetic_ids_by_path.get(path).copied()
    }
}

impl FrameworkDatabase for FrameworkDb {
    fn class(&self, class: ClassId) -> &ClassData {
        self.shared.class(class)
    }

    fn project_of_class(&self, _class: ClassId) -> ProjectId {
        self.shared.project
    }

    fn project_of_file(&self, _file: FileId) -> ProjectId {
        // Root-scoped DB: callers are expected to pass file IDs belonging to this root.
        self.shared.project
    }

    fn file_text(&self, file: FileId) -> Option<&str> {
        if let Some(text) = self.shared.synthetic_file_text(file) {
            return Some(text);
        }
        Some(self.shared.host_db.file_content(file))
    }

    fn file_path(&self, file: FileId) -> Option<&Path> {
        if let Some(path) = self.shared.synthetic_file_path(file) {
            return Some(path);
        }
        self.shared.host_db.file_path(file)
    }

    fn file_id(&self, path: &Path) -> Option<FileId> {
        if let Some(id) = self.shared.synthetic_file_id(path) {
            return Some(id);
        }
        self.shared.host_db.file_id(path)
    }

    fn all_files(&self, project: ProjectId) -> Vec<FileId> {
        if project != self.shared.project || self.cancel.is_cancelled() {
            return Vec::new();
        }
        self.shared.all_files.clone()
    }

    fn all_classes(&self, project: ProjectId) -> Vec<ClassId> {
        if project != self.shared.project || self.cancel.is_cancelled() {
            return Vec::new();
        }
        self.shared.all_classes()
    }

    fn has_dependency(&self, project: ProjectId, group: &str, artifact: &str) -> bool {
        if project != self.shared.project || self.cancel.is_cancelled() {
            return false;
        }
        self.shared.has_dependency(group, artifact)
    }

    fn has_class_on_classpath(&self, project: ProjectId, binary_name: &str) -> bool {
        if project != self.shared.project || self.cancel.is_cancelled() {
            return false;
        }

        let normalized = normalize_binary_name(binary_name);
        if normalized.is_empty() {
            return false;
        }
        let normalized = format!("{normalized}.class");

        // Cache results per (root fingerprint, query) by keeping the cache on the root-scoped shared
        // DB. The shared DB is recreated when the root fingerprint changes.
        if let Some(hit) = lock_unpoison(&self.shared.classpath_exact_cache).get(&normalized) {
            return *hit;
        }

        let result = self
            .shared
            .classpath_contains_exact(&normalized, &self.cancel);

        lock_unpoison(&self.shared.classpath_exact_cache).insert(normalized, result);
        result
    }

    fn has_class_on_classpath_prefix(&self, project: ProjectId, prefix: &str) -> bool {
        if project != self.shared.project || self.cancel.is_cancelled() {
            return false;
        }

        let normalized = normalize_prefix(prefix);
        if normalized.is_empty() {
            return false;
        }

        if let Some(hit) = lock_unpoison(&self.shared.classpath_prefix_cache).get(&normalized) {
            return *hit;
        }

        let result = self
            .shared
            .classpath_contains_prefix(&normalized, &self.cancel);
        lock_unpoison(&self.shared.classpath_prefix_cache).insert(normalized, result);
        result
    }
}

fn shared_db_for_file(
    db: Arc<dyn HostDatabase + Send + Sync>,
    file: FileId,
    cancel: &CancellationToken,
) -> Option<Arc<FrameworkDbShared>> {
    if cancel.is_cancelled() {
        return None;
    }

    let file_path = db.file_path(file)?.to_path_buf();
    let root = crate::framework_cache::project_root_for_path(&file_path);
    let root_key = normalize_root_for_cache(&root);

    let key = FrameworkDbCacheKey {
        host_db_ptr: Arc::as_ptr(&db) as *const () as usize,
        root: root_key.clone(),
    };

    let (all_files, java_files) = match collect_files(db.as_ref(), &root, &root_key, cancel) {
        Some(files) => files,
        None => {
            return lock_unpoison(&FRAMEWORK_DB_CACHE)
                .get_cloned(&key)
                .map(|entry| entry.db);
        }
    };

    let fingerprint = match root_fingerprint(db.as_ref(), &root_key, &java_files, cancel) {
        Some(fp) => fp,
        None => {
            return lock_unpoison(&FRAMEWORK_DB_CACHE)
                .get_cloned(&key)
                .map(|entry| entry.db);
        }
    };

    {
        let mut cache = lock_unpoison(&FRAMEWORK_DB_CACHE);
        if let Some(hit) = cache.get_cloned(&key) {
            if hit.fingerprint == fingerprint {
                return Some(hit.db);
            }
        }
    }

    if cancel.is_cancelled() {
        return lock_unpoison(&FRAMEWORK_DB_CACHE)
            .get_cloned(&key)
            .map(|entry| entry.db);
    }

    let project = project_id_for_root(&root_key);
    let config = crate::framework_cache::project_config(&root_key);

    let mut classes = Vec::new();
    for (_, file_id) in &java_files {
        if cancel.is_cancelled() {
            return lock_unpoison(&FRAMEWORK_DB_CACHE)
                .get_cloned(&key)
                .map(|entry| entry.db);
        }
        let text = db.file_content(*file_id);
        classes.extend(crate::framework_class_data::extract_classes_from_source(
            text,
        ));
    }

    let mut used_file_ids: HashSet<u32> = db
        .all_file_ids()
        .into_iter()
        .map(|id| id.to_raw())
        .collect();

    let (synthetic_files_by_id, synthetic_ids_by_path, synthetic_entries) =
        collect_spring_metadata_synthetic_files(config.as_deref(), &mut used_file_ids, cancel);

    let mut all_file_entries: Vec<(PathBuf, FileId)> = all_files;
    all_file_entries.extend(synthetic_entries);
    all_file_entries.sort_by(|(a, _), (b, _)| a.cmp(b));

    let all_file_ids = all_file_entries.iter().map(|(_, id)| *id).collect();

    let shared = Arc::new(FrameworkDbShared {
        host_db: Arc::clone(&db),
        project,
        all_files: all_file_ids,
        classes,
        config,
        synthetic_files_by_id,
        synthetic_ids_by_path,
        classpath_exact_cache: Mutex::new(HashMap::new()),
        classpath_prefix_cache: Mutex::new(HashMap::new()),
    });

    lock_unpoison(&FRAMEWORK_DB_CACHE).insert(
        key,
        CachedFrameworkDb {
            fingerprint,
            db: Arc::clone(&shared),
        },
    );

    Some(shared)
}

fn root_fingerprint(
    db: &dyn HostDatabase,
    root: &Path,
    java_files: &[(PathBuf, FileId)],
    cancel: &CancellationToken,
) -> Option<u64> {
    if cancel.is_cancelled() {
        return None;
    }

    let build_fp = build_marker_fingerprint(root);

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    build_fp.hash(&mut hasher);

    // Pointer/len hashing for cheap invalidation (see `jpa_intel.rs` for rationale).
    for (path, file_id) in java_files {
        if cancel.is_cancelled() {
            return None;
        }
        path.hash(&mut hasher);
        let text = db.file_content(*file_id);
        text.len().hash(&mut hasher);
        text.as_ptr().hash(&mut hasher);
    }

    Some(hasher.finish())
}

fn collect_files(
    db: &dyn HostDatabase,
    raw_root: &Path,
    canonical_root: &Path,
    cancel: &CancellationToken,
) -> Option<(Vec<(PathBuf, FileId)>, Vec<(PathBuf, FileId)>)> {
    let mut under_root = Vec::new();
    let mut all = Vec::new();

    for file_id in db.all_file_ids() {
        if cancel.is_cancelled() {
            return None;
        }
        let Some(path) = db.file_path(file_id) else {
            continue;
        };
        let tuple = (path.to_path_buf(), file_id);
        all.push(tuple.clone());
        if path.starts_with(raw_root) || path.starts_with(canonical_root) {
            under_root.push(tuple);
        }
    }

    let mut files = if under_root.is_empty() {
        all
    } else {
        under_root
    };
    files.sort_by(|(a, _), (b, _)| a.cmp(b));

    let mut java_files: Vec<_> = files
        .iter()
        .filter_map(|(path, id)| {
            (path.extension().and_then(|e| e.to_str()) == Some("java"))
                .then_some((path.clone(), *id))
        })
        .collect();
    java_files.sort_by(|(a, _), (b, _)| a.cmp(b));

    Some((files, java_files))
}

#[derive(Debug)]
struct ProjectIdAllocator {
    next: u32,
    roots: HashMap<PathBuf, ProjectId>,
}

impl ProjectIdAllocator {
    fn new() -> Self {
        Self {
            next: 0,
            roots: HashMap::new(),
        }
    }

    fn project_id(&mut self, root: &Path) -> ProjectId {
        if let Some(&id) = self.roots.get(root) {
            return id;
        }
        let id = ProjectId::new(self.next);
        self.next = self.next.saturating_add(1);
        self.roots.insert(root.to_path_buf(), id);
        id
    }
}

fn project_id_for_root(root: &Path) -> ProjectId {
    lock_unpoison(&PROJECT_IDS).project_id(root)
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
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.map.remove(&oldest);
        }
    }
}

fn lock_unpoison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    crate::poison::lock(mutex, "framework_db")
}

fn normalize_root_for_cache(root: &Path) -> PathBuf {
    match std::fs::canonicalize(root) {
        Ok(path) => path,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => root.to_path_buf(),
        Err(err) => {
            tracing::debug!(
                target = "nova.ide",
                root = %root.display(),
                error = %err,
                "failed to canonicalize root for framework db cache"
            );
            root.to_path_buf()
        }
    }
}

fn build_marker_fingerprint(root: &Path) -> u64 {
    use std::collections::hash_map::DefaultHasher;

    fn modified_best_effort(
        meta: &std::fs::Metadata,
        path: &Path,
        context: &'static str,
    ) -> Option<SystemTime> {
        match meta.modified() {
            Ok(time) => Some(time),
            Err(err) => {
                tracing::debug!(
                    target = "nova.ide",
                    context,
                    path = %path.display(),
                    error = %err,
                    "failed to read mtime"
                );
                None
            }
        }
    }

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
                hash_mtime(
                    &mut hasher,
                    modified_best_effort(&meta, &path, "marker_mtime"),
                );
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                false.hash(&mut hasher);
            }
            Err(err) => {
                tracing::debug!(
                    target = "nova.ide",
                    root = %root.display(),
                    marker,
                    path = %path.display(),
                    error = %err,
                    "failed to read build marker metadata"
                );
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
    match std::fs::read_dir(root) {
        Ok(entries) => {
            let mut logged_entry_error = false;
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(err) => {
                        if !logged_entry_error {
                            tracing::debug!(
                                target = "nova.ide",
                                root = %root.display(),
                                error = %err,
                                "failed to read directory entry while scanning bazelrc fragments"
                            );
                            logged_entry_error = true;
                        }
                        continue;
                    }
                };
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
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            tracing::debug!(
                target = "nova.ide",
                root = %root.display(),
                error = %err,
                "failed to scan workspace root directory for bazelrc fragments"
            );
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
                hash_mtime(
                    &mut hasher,
                    modified_best_effort(&meta, &path, "bazelrc_fragment_mtime"),
                );
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                false.hash(&mut hasher);
            }
            Err(err) => {
                tracing::debug!(
                    target = "nova.ide",
                    root = %root.display(),
                    path = %path.display(),
                    error = %err,
                    "failed to read bazelrc fragment metadata"
                );
                false.hash(&mut hasher);
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

    static MTIME_DURATION_ERROR_LOGGED: std::sync::OnceLock<()> = std::sync::OnceLock::new();

    let duration = time.duration_since(UNIX_EPOCH).unwrap_or_else(|err| {
        if MTIME_DURATION_ERROR_LOGGED.set(()).is_ok() {
            tracing::debug!(
                target = "nova.ide",
                error = %err,
                "failed to compute mtime duration since UNIX_EPOCH; hashing as 0"
            );
        }
        std::time::Duration::from_secs(0)
    });
    duration.as_secs().hash(hasher);
    duration.subsec_nanos().hash(hasher);
}

fn normalize_binary_name(value: &str) -> String {
    let raw = value.trim().trim_end_matches(".class");
    raw.replace('.', "/")
}

fn normalize_prefix(value: &str) -> String {
    let raw = value.trim();
    if raw.is_empty() {
        return String::new();
    }

    let mut out = raw.replace('.', "/");
    if raw.ends_with('.') || raw.ends_with('/') {
        if !out.ends_with('/') {
            out.push('/');
        }
    }
    out
}

fn zip_contains_exact(
    path: &Path,
    entry: &str,
    target_release: Option<u16>,
    cancel: &CancellationToken,
) -> bool {
    if cancel.is_cancelled() {
        return false;
    }

    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return false,
    };

    let mut archive = match ZipArchive::new(file) {
        Ok(archive) => archive,
        Err(_) => return false,
    };

    if cancel.is_cancelled() {
        return false;
    }

    if archive.by_name(entry).is_ok() {
        return true;
    }

    // JMODs store classes under `classes/`.
    let alt = format!("classes/{entry}");
    if archive.by_name(&alt).is_ok() {
        return true;
    }

    let Some(target_release) = target_release.filter(|release| *release >= 9) else {
        return false;
    };

    // Multi-release JARs can store version-specific class files under
    // `META-INF/versions/<n>/...`. Honor `--release` so MR-only classes are
    // only considered when the project's target supports them.
    if !jar_is_multi_release(&mut archive) {
        return false;
    }

    for version in (9..=target_release).rev() {
        let candidate = format!("META-INF/versions/{version}/{entry}");
        if archive.by_name(&candidate).is_ok() {
            return true;
        }
    }

    false
}

fn zip_contains_prefix(
    path: &Path,
    prefix: &str,
    target_release: Option<u16>,
    cancel: &CancellationToken,
) -> bool {
    if cancel.is_cancelled() {
        return false;
    }

    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return false,
    };
    let archive = match ZipArchive::new(file) {
        Ok(archive) => archive,
        Err(_) => return false,
    };

    let alt_prefix = format!("classes/{prefix}");
    let mr_target_release = target_release.filter(|release| *release >= 9);
    let mut mr_archive = archive;
    let is_multi_release = mr_target_release.is_some_and(|_| jar_is_multi_release(&mut mr_archive));

    for name in mr_archive.file_names() {
        if cancel.is_cancelled() {
            return false;
        }
        if name.starts_with(prefix) || name.starts_with(&alt_prefix) {
            return true;
        }

        if is_multi_release {
            if let Some(rest) = name.strip_prefix("META-INF/versions/") {
                let Some((version, inner)) = rest.split_once('/') else {
                    continue;
                };
                let Ok(version) = version.parse::<u16>() else {
                    continue;
                };
                let Some(target) = mr_target_release else {
                    continue;
                };
                if version > target {
                    continue;
                }
                if inner.starts_with(prefix) {
                    return true;
                }
            }
        }
    }
    false
}

fn jar_is_multi_release<R: std::io::Read + std::io::Seek>(archive: &mut ZipArchive<R>) -> bool {
    let mut file = match archive.by_name("META-INF/MANIFEST.MF") {
        Ok(file) => file,
        Err(zip::result::ZipError::FileNotFound) => return false,
        Err(_) => return false,
    };

    let mut manifest = String::new();
    if file.read_to_string(&mut manifest).is_err() {
        return false;
    }

    manifest
        .to_ascii_lowercase()
        .contains("multi-release: true")
}

#[cfg(test)]
mod tests {
    use super::*;

    use nova_db::InMemoryFileStore;

    #[test]
    fn zip_contains_exact_respects_multi_release_and_target_release() {
        let jar = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../nova-classpath/testdata/multirelease.jar");
        assert!(jar.is_file(), "fixture missing: {}", jar.display());

        let cancel = CancellationToken::new();
        let entry = "com/example/mr/MultiReleaseOnly.class";

        assert!(
            !zip_contains_exact(&jar, entry, Some(8), &cancel),
            "expected MR-only class to be absent for Java 8"
        );
        assert!(
            zip_contains_exact(&jar, entry, Some(17), &cancel),
            "expected MR-only class to be present for Java 17"
        );
    }

    #[test]
    fn zip_contains_prefix_respects_multi_release_and_target_release() {
        let jar = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../nova-classpath/testdata/multirelease.jar");
        assert!(jar.is_file(), "fixture missing: {}", jar.display());

        let cancel = CancellationToken::new();
        let prefix = "com/example/mr/";

        assert!(
            !zip_contains_prefix(&jar, prefix, Some(8), &cancel),
            "expected MR-only package prefix to be absent for Java 8"
        );
        assert!(
            zip_contains_prefix(&jar, prefix, Some(17), &cancel),
            "expected MR-only package prefix to be present for Java 17"
        );
    }

    #[test]
    fn unknown_class_id_is_best_effort() {
        let mut store = InMemoryFileStore::new();
        let file_path = PathBuf::from("/__nova_test__/framework_db_unknown_class_id/src/Main.java");
        let file = store.file_id_for_path(&file_path);
        store.set_file_text(file, "package test; class Main {}".to_string());

        let db: Arc<dyn HostDatabase + Send + Sync> = Arc::new(store);
        let cancel = CancellationToken::new();
        let framework_db =
            framework_db_for_file(Arc::clone(&db), file, &cancel).expect("db should build");

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            framework_db.class(ClassId::new(9999)).name.clone()
        }));

        assert!(
            result.is_ok(),
            "expected framework db adapter to never panic on unknown ClassId"
        );
        assert_eq!(result.unwrap(), "<unknown>");
    }
}

fn collect_spring_metadata_synthetic_files(
    config: Option<&nova_project::ProjectConfig>,
    used_file_ids: &mut HashSet<u32>,
    cancel: &CancellationToken,
) -> (
    HashMap<FileId, (PathBuf, String)>,
    HashMap<PathBuf, FileId>,
    Vec<(PathBuf, FileId)>,
) {
    let mut synthetic_files_by_id: HashMap<FileId, (PathBuf, String)> = HashMap::new();
    let mut synthetic_ids_by_path: HashMap<PathBuf, FileId> = HashMap::new();
    let mut synthetic_entries: Vec<(PathBuf, FileId)> = Vec::new();

    let Some(config) = config else {
        return (
            synthetic_files_by_id,
            synthetic_ids_by_path,
            synthetic_entries,
        );
    };

    // SpringAnalyzer expects to discover these metadata files from `Database::all_files` by
    // filename, so we synthesize pseudo-paths that end with the expected basename.
    const SPRING_META_FILES: &[&str] = &[
        "META-INF/spring-configuration-metadata.json",
        "META-INF/additional-spring-configuration-metadata.json",
    ];

    for entry in config.classpath.iter().chain(config.module_path.iter()) {
        if cancel.is_cancelled() {
            break;
        }

        if entry.kind != nova_project::ClasspathEntryKind::Jar {
            continue;
        }

        let jar_path = entry.path.as_path();
        let file = match std::fs::File::open(jar_path) {
            Ok(file) => file,
            Err(err) => {
                // Classpath entries can race with changes or be missing in partial workspaces.
                // Missing files are expected; only log unexpected filesystem errors.
                if err.kind() != std::io::ErrorKind::NotFound {
                    tracing::debug!(
                        target = "nova.ide",
                        jar_path = %jar_path.display(),
                        error = %err,
                        "failed to open classpath jar while scanning Spring metadata"
                    );
                }
                continue;
            }
        };

        let mut archive = match ZipArchive::new(file) {
            Ok(archive) => archive,
            Err(err) => {
                tracing::debug!(
                    target = "nova.ide",
                    jar_path = %jar_path.display(),
                    error = %err,
                    "failed to read classpath jar as zip while scanning Spring metadata"
                );
                continue;
            }
        };

        for rel in SPRING_META_FILES {
            if cancel.is_cancelled() {
                break;
            }

            let mut zip_file = match archive.by_name(rel) {
                Ok(file) => file,
                Err(zip::result::ZipError::FileNotFound) => continue,
                Err(err) => {
                    tracing::debug!(
                        target = "nova.ide",
                        jar_path = %jar_path.display(),
                        rel,
                        error = %err,
                        "failed to open Spring metadata entry in classpath jar"
                    );
                    continue;
                }
            };

            let mut bytes = Vec::new();
            if let Err(err) = zip_file.read_to_end(&mut bytes) {
                tracing::debug!(
                    target = "nova.ide",
                    jar_path = %jar_path.display(),
                    rel,
                    error = %err,
                    "failed to read Spring metadata entry from classpath jar"
                );
                continue;
            }

            let text = match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(err) => {
                    tracing::debug!(
                        target = "nova.ide",
                        jar_path = %jar_path.display(),
                        rel,
                        error = %err,
                        "Spring metadata entry is not valid UTF-8"
                    );
                    continue;
                }
            };

            let synthetic_path = PathBuf::from(format!("{}!/{rel}", jar_path.display()));
            if synthetic_ids_by_path.contains_key(&synthetic_path) {
                continue;
            }

            let file_id = allocate_synthetic_file_id(&synthetic_path, used_file_ids);
            synthetic_ids_by_path.insert(synthetic_path.clone(), file_id);
            synthetic_entries.push((synthetic_path.clone(), file_id));
            synthetic_files_by_id.insert(file_id, (synthetic_path, text));
        }
    }

    (
        synthetic_files_by_id,
        synthetic_ids_by_path,
        synthetic_entries,
    )
}

fn allocate_synthetic_file_id(path: &Path, used_file_ids: &mut HashSet<u32>) -> FileId {
    use std::collections::hash_map::DefaultHasher;

    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    let hash = hasher.finish() as u32;

    // Allocate IDs with the high bit set so they are extremely unlikely to collide with
    // host-allocated IDs (which are typically dense from 0).
    let mut candidate = 0x8000_0000u32 | (hash & 0x7fff_ffff);

    // Resolve collisions (including with existing host IDs) via linear probing.
    while !used_file_ids.insert(candidate) {
        candidate = 0x8000_0000u32 | ((candidate.wrapping_add(1)) & 0x7fff_ffff);
    }

    FileId::from_raw(candidate)
}
