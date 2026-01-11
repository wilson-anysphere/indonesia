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

use once_cell::sync::Lazy;

use nova_config_metadata::MetadataIndex;
use nova_db::{Database, FileId};

const MAX_CACHED_ROOTS: usize = 32;

static WORKSPACE_CACHE: Lazy<FrameworkWorkspaceCache> = Lazy::new(FrameworkWorkspaceCache::new);

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

    nova_project::workspace_root(start).unwrap_or_else(|| start.to_path_buf())
}

/// Convenience helper for `FileId`-based queries.
///
/// Returns `None` when the database does not know the file path (e.g. virtual
/// buffers).
#[must_use]
pub fn project_root_for_file(db: &dyn Database, file: FileId) -> Option<PathBuf> {
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
            Some(nova_classpath::ClasspathEntry::Jar(entry.path.clone()))
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

#[derive(Debug)]
pub struct FrameworkWorkspaceCache {
    project_configs: Mutex<LruCache<PathBuf, CachedProjectConfig>>,
    spring_metadata: Mutex<LruCache<PathBuf, CachedMetadataIndex>>,
}

#[derive(Clone, Debug)]
struct CachedProjectConfig {
    fingerprint: u64,
    value: Option<Arc<nova_project::ProjectConfig>>,
}

#[derive(Clone, Debug)]
struct CachedMetadataIndex {
    fingerprint: u64,
    value: Arc<MetadataIndex>,
}

impl FrameworkWorkspaceCache {
    #[must_use]
    pub fn new() -> Self {
        Self {
            project_configs: Mutex::new(LruCache::new(MAX_CACHED_ROOTS)),
            spring_metadata: Mutex::new(LruCache::new(MAX_CACHED_ROOTS)),
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

    fn spring_metadata_index(&self, root: &Path) -> Arc<MetadataIndex> {
        let Some(root) = canonicalize_root(root) else {
            return Arc::new(MetadataIndex::new());
        };
        let fingerprint = build_marker_fingerprint(&root);

        {
            let mut cache = lock_unpoison(&self.spring_metadata);
            if let Some(entry) = cache.get_cloned(&root) {
                if entry.fingerprint == fingerprint {
                    return entry.value;
                }
            }
        }

        let value = self
            .project_config(&root)
            .map(|config| {
                let classpath: Vec<_> = config
                    .classpath
                    .iter()
                    .chain(config.module_path.iter())
                    .filter(|entry| match entry.kind {
                        nova_project::ClasspathEntryKind::Directory => entry.path.is_dir(),
                        nova_project::ClasspathEntryKind::Jar => entry.path.is_file(),
                        #[allow(unreachable_patterns)]
                        _ => false,
                    })
                    .filter_map(to_classpath_entry)
                    .collect();
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

fn lock_unpoison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}

fn build_marker_fingerprint(root: &Path) -> u64 {
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
