use std::collections::{hash_map::DefaultHasher, HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;

use nova_config_metadata::MetadataIndex;
use nova_db::{Database, FileId};
use nova_framework_spring::SpringWorkspaceIndex;

use crate::framework_cache;

const MAX_CACHED_ROOTS: usize = 32;

#[derive(Debug, Clone)]
pub(crate) struct CachedSpringWorkspaceIndex {
    #[allow(dead_code)]
    pub(crate) root: PathBuf,
    pub(crate) fingerprint: u64,
    pub(crate) index: Arc<SpringWorkspaceIndex>,
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

static SPRING_CONFIG_INDEX_CACHE: Lazy<Mutex<LruCache<PathBuf, CachedSpringWorkspaceIndex>>> =
    Lazy::new(|| Mutex::new(LruCache::new(MAX_CACHED_ROOTS)));

pub(crate) fn workspace_index_for_file(db: &dyn Database, file: FileId) -> Arc<SpringWorkspaceIndex> {
    let Some(path) = db.file_path(file) else {
        return Arc::new(SpringWorkspaceIndex::new(Arc::new(MetadataIndex::new())));
    };

    let root = framework_cache::project_root_for_path(path);
    workspace_index_for_root(db, root)
}

fn workspace_index_for_root(db: &dyn Database, root: PathBuf) -> Arc<SpringWorkspaceIndex> {
    let metadata = framework_cache::spring_metadata_index(&root);
    let files = collect_relevant_files(db, &root);
    let fingerprint = workspace_fingerprint(db, &files, &metadata);

    {
        let mut cache = SPRING_CONFIG_INDEX_CACHE
            .lock()
            .expect("spring config workspace cache lock poisoned");
        if let Some(entry) = cache.get_cloned(&root).filter(|e| e.fingerprint == fingerprint) {
            return entry.index;
        }
    }

    let built = Arc::new(build_workspace_index(db, &files, metadata));

    let mut cache = SPRING_CONFIG_INDEX_CACHE
        .lock()
        .expect("spring config workspace cache lock poisoned");
    if let Some(entry) = cache.get_cloned(&root).filter(|e| e.fingerprint == fingerprint) {
        return entry.index;
    }

    cache.insert(
        root.clone(),
        CachedSpringWorkspaceIndex {
            root,
            fingerprint,
            index: Arc::clone(&built),
        },
    );

    built
}

fn collect_relevant_files(db: &dyn Database, root: &Path) -> Vec<(PathBuf, FileId)> {
    let mut out = Vec::new();
    for id in db.all_file_ids() {
        let Some(path) = db.file_path(id) else {
            continue;
        };
        if !path.starts_with(root) {
            continue;
        }

        if path.extension().and_then(|e| e.to_str()) == Some("java")
            || is_spring_properties_file(path)
            || is_spring_yaml_file(path)
        {
            out.push((path.to_path_buf(), id));
        }
    }

    out.sort_by(|(a, _), (b, _)| a.cmp(b));
    out
}

fn workspace_fingerprint(
    db: &dyn Database,
    files: &[(PathBuf, FileId)],
    metadata: &Arc<MetadataIndex>,
) -> u64 {
    let mut hasher = DefaultHasher::new();

    metadata.is_empty().hash(&mut hasher);
    let meta_ptr = if metadata.is_empty() {
        0usize
    } else {
        Arc::as_ptr(metadata) as usize
    };
    meta_ptr.hash(&mut hasher);

    for (path, id) in files {
        path.hash(&mut hasher);
        let text = db.file_content(*id);
        // NOTE: We intentionally avoid hashing the full file contents here: this code runs on
        // every keystroke, and hashing an entire workspace's Java sources and config files would
        // be prohibitively expensive.
        //
        // The `nova_db::Database` implementations used by Nova replace the underlying `String`
        // on edits (rather than mutating in-place), so using the content pointer/len is a cheap
        // best-effort invalidation signal.
        text.len().hash(&mut hasher);
        (text.as_ptr() as usize).hash(&mut hasher);
    }

    hasher.finish()
}

fn build_workspace_index(
    db: &dyn Database,
    files: &[(PathBuf, FileId)],
    metadata: Arc<MetadataIndex>,
) -> SpringWorkspaceIndex {
    let mut index = SpringWorkspaceIndex::new(metadata);

    for (path, id) in files {
        let text = db.file_content(*id);
        if path.extension().and_then(|e| e.to_str()) == Some("java") {
            index.add_java_file(path.clone(), text);
        } else {
            index.add_config_file(path.clone(), text);
        }
    }

    index
}

fn is_spring_properties_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    name.starts_with("application") && path.extension().and_then(|e| e.to_str()) == Some("properties")
}

fn is_spring_yaml_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if !name.starts_with("application") {
        return false;
    }
    matches!(path.extension().and_then(|e| e.to_str()), Some("yml" | "yaml"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nova_db::InMemoryFileStore;

    #[test]
    fn caches_index_when_metadata_is_empty() {
        let mut db = InMemoryFileStore::new();

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("nova-spring-cache-test-{unique}"));
        let config_path = root.join("src/main/resources/application.properties");

        let file = db.file_id_for_path(&config_path);
        db.set_file_text(file, "server.port=8080\n".to_string());

        // The temp root does not exist on disk, so `framework_cache::spring_metadata_index` returns a
        // fresh empty `Arc<MetadataIndex>` on each call. Ensure the spring config workspace cache
        // still hits by treating empty metadata indexes as equivalent (see `meta_ptr` logic).
        let first = workspace_index_for_file(&db, file);
        let second = workspace_index_for_file(&db, file);
        assert!(Arc::ptr_eq(&first, &second));

        // Edits should invalidate the cache (content pointer/len changes).
        db.set_file_text(file, "server.port=9090\n".to_string());
        let third = workspace_index_for_file(&db, file);
        assert!(!Arc::ptr_eq(&first, &third));
    }
}
