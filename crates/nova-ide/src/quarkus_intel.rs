use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;

use nova_db::{Database, FileId};
use nova_framework_quarkus::AnalysisResultWithSpans;
use nova_types::Diagnostic;

use crate::framework_cache;

const MAX_CACHED_ROOTS: usize = 32;

static QUARKUS_ANALYSIS_CACHE: Lazy<Mutex<LruCache<PathBuf, Arc<CachedQuarkusProject>>>> =
    Lazy::new(|| Mutex::new(LruCache::new(MAX_CACHED_ROOTS)));

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

#[derive(Debug)]
pub(crate) struct CachedQuarkusProject {
    /// Java sources included in the analysis, sorted by path (stable).
    pub(crate) java_sources: Vec<PathBuf>,
    file_to_source: HashMap<PathBuf, usize>,
    file_ids: Vec<FileId>,
    file_id_to_source: HashMap<FileId, usize>,
    pub(crate) analysis: Option<Arc<AnalysisResultWithSpans>>,
    fingerprint: u64,
}

impl CachedQuarkusProject {
    pub(crate) fn source_index_for_file(&self, file: FileId) -> Option<usize> {
        self.file_id_to_source.get(&file).copied()
    }

    #[allow(dead_code)]
    pub(crate) fn source_index_for_path(&self, path: &Path) -> Option<usize> {
        self.file_to_source.get(path).copied()
    }

    #[allow(dead_code)]
    pub(crate) fn path_for_source_index(&self, index: usize) -> Option<&Path> {
        self.java_sources.get(index).map(|p| p.as_path())
    }

    #[allow(dead_code)]
    pub(crate) fn file_id_for_source_index(&self, index: usize) -> Option<FileId> {
        self.file_ids.get(index).copied()
    }
}

pub(crate) fn diagnostics_for_file(db: &dyn Database, file: FileId) -> Vec<Diagnostic> {
    let Some(project) = project_for_file(db, file) else {
        return Vec::new();
    };
    let Some(analysis) = project.analysis.as_ref() else {
        return Vec::new();
    };
    let Some(source_idx) = project.source_index_for_file(file) else {
        return Vec::new();
    };

    analysis
        .diagnostics
        .iter()
        .filter(|d| d.source == source_idx)
        .map(|d| d.diagnostic.clone())
        .collect()
}

#[allow(dead_code)]
pub(crate) fn analysis_for_file(
    db: &dyn Database,
    file: FileId,
) -> Option<Arc<AnalysisResultWithSpans>> {
    let project = project_for_file(db, file)?;
    let _ = project.source_index_for_file(file)?;
    project.analysis.clone()
}

fn project_for_file(db: &dyn Database, file: FileId) -> Option<Arc<CachedQuarkusProject>> {
    let file_path = db.file_path(file)?;
    if file_path.extension().and_then(|e| e.to_str()) != Some("java") {
        return None;
    }

    let root_raw = discover_project_root(file_path);
    let root_key = canonicalize_root(&root_raw);

    let java_files = collect_java_files(db, &root_raw, &root_key);
    if java_files.is_empty() {
        return None;
    }

    let fingerprint = fingerprint_sources(db, &java_files);

    if let Some(hit) = QUARKUS_ANALYSIS_CACHE
        .lock()
        .expect("quarkus analysis cache mutex poisoned")
        .get_cloned(&root_key)
        .filter(|entry| entry.fingerprint == fingerprint)
    {
        return Some(hit);
    }

    let sources: Vec<&str> = java_files
        .iter()
        .map(|(_, id)| db.file_content(*id))
        .collect();
    let applicable = is_quarkus_applicable(&root_raw, &sources);
    let analysis = applicable.then(|| {
        Arc::new(nova_framework_quarkus::analyze_java_sources_with_spans(
            &sources,
        ))
    });

    let (java_sources, file_ids): (Vec<PathBuf>, Vec<FileId>) = java_files.into_iter().unzip();
    let file_to_source = java_sources
        .iter()
        .enumerate()
        .map(|(idx, path)| (path.clone(), idx))
        .collect();
    let file_id_to_source = file_ids
        .iter()
        .enumerate()
        .map(|(idx, id)| (*id, idx))
        .collect();

    let entry = Arc::new(CachedQuarkusProject {
        java_sources,
        file_to_source,
        file_ids,
        file_id_to_source,
        analysis,
        fingerprint,
    });

    QUARKUS_ANALYSIS_CACHE
        .lock()
        .expect("quarkus analysis cache mutex poisoned")
        .insert(root_key, entry.clone());

    Some(entry)
}

fn discover_project_root(path: &Path) -> PathBuf {
    if path.exists() {
        return framework_cache::project_root_for_path(path);
    }

    let dir = if path.is_file() {
        path.parent().unwrap_or(path)
    } else {
        path
    };

    // Best-effort fallback for in-memory DB fixtures: if the path has a
    // `src/` segment, treat its parent as the project root.
    for ancestor in dir.ancestors() {
        if ancestor.file_name().and_then(|n| n.to_str()) == Some("src") {
            if let Some(parent) = ancestor.parent() {
                return parent.to_path_buf();
            }
        }
    }

    dir.to_path_buf()
}

fn canonicalize_root(root: &Path) -> PathBuf {
    match std::fs::canonicalize(root) {
        Ok(path) => path,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => root.to_path_buf(),
        Err(err) => {
            tracing::debug!(
                target = "nova.ide",
                root = %root.display(),
                error = %err,
                "failed to canonicalize root for quarkus analysis"
            );
            root.to_path_buf()
        }
    }
}

fn collect_java_files(
    db: &dyn Database,
    root: &Path,
    canonical_root: &Path,
) -> Vec<(PathBuf, FileId)> {
    let has_alt_root = canonical_root != root;
    let mut out = Vec::new();

    for file_id in db.all_file_ids() {
        let Some(path) = db.file_path(file_id) else {
            continue;
        };
        if path.extension().and_then(|e| e.to_str()) != Some("java") {
            continue;
        }
        if !path.starts_with(root) && !(has_alt_root && path.starts_with(canonical_root)) {
            continue;
        }
        out.push((path.to_path_buf(), file_id));
    }

    out.sort_by(|(a, _), (b, _)| a.cmp(b));
    out
}

fn fingerprint_sources(db: &dyn Database, files: &[(PathBuf, FileId)]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (path, file_id) in files {
        path.hash(&mut hasher);
        let text = db.file_content(*file_id);

        // NOTE: Avoid hashing full source contents here; diagnostics can run on
        // every keystroke and hashing an entire workspace worth of Java would be
        // prohibitively expensive. The database swaps the backing `String` on
        // edits, so (ptr,len) is a cheap best-effort invalidation signal.
        text.len().hash(&mut hasher);
        text.as_ptr().hash(&mut hasher);
    }
    hasher.finish()
}

fn is_quarkus_applicable(root: &Path, sources: &[&str]) -> bool {
    if let Some(config) = framework_cache::project_config(root) {
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
            sources,
        );
    }

    nova_framework_quarkus::is_quarkus_applicable(&[], sources)
}
