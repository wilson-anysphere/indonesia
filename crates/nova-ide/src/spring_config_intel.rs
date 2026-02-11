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

pub(crate) fn workspace_index_for_file(
    db: &dyn Database,
    file: FileId,
) -> Arc<SpringWorkspaceIndex> {
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
        if let Some(entry) = cache
            .get_cloned(&root)
            .filter(|e| e.fingerprint == fingerprint)
        {
            return entry.index;
        }
    }

    let built = Arc::new(build_workspace_index(db, &files, metadata));

    let mut cache = SPRING_CONFIG_INDEX_CACHE
        .lock()
        .expect("spring config workspace cache lock poisoned");
    if let Some(entry) = cache
        .get_cloned(&root)
        .filter(|e| e.fingerprint == fingerprint)
    {
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
            || framework_cache::is_application_properties(path)
            || framework_cache::is_application_yaml(path)
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

    const SAMPLE: usize = 64;
    const FULL_HASH_MAX: usize = 3 * SAMPLE;

    metadata.is_empty().hash(&mut hasher);
    let meta_ptr: *const MetadataIndex = if metadata.is_empty() {
        std::ptr::null()
    } else {
        Arc::as_ptr(metadata)
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
        text.as_ptr().hash(&mut hasher);
        // Pointer/len hashing is fast, but can collide when short-lived buffers reuse the same
        // allocations (common in tests) or when text is mutated in place. Mix in a small,
        // content-dependent sample to make invalidation deterministic without hashing full
        // contents for large files.
        let bytes = text.as_bytes();
        if bytes.len() <= FULL_HASH_MAX {
            bytes.hash(&mut hasher);
        } else {
            bytes[..SAMPLE].hash(&mut hasher);
            let mid = bytes.len() / 2;
            let mid_start = mid.saturating_sub(SAMPLE / 2);
            let mid_end = (mid_start + SAMPLE).min(bytes.len());
            bytes[mid_start..mid_end].hash(&mut hasher);
            bytes[bytes.len() - SAMPLE..].hash(&mut hasher);
        }
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
            // Avoid scanning every Java file in the workspace; only files that might contain
            // Spring config usages can contribute anything to the index.
            if text.contains("@Value") || text.contains("@ConfigurationProperties") {
                index.add_java_file(path.clone(), text);
            }
        } else {
            index.add_config_file(path.clone(), text);
        }
    }

    index
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

    #[test]
    fn invalidates_when_file_text_changes_in_place_with_same_ptr_and_len() {
        /// Minimal `Database` implementation whose file text can be mutated in place (keeping the
        /// backing allocation + length stable). This models scenarios where pointer/len-only cache
        /// fingerprints would fail to invalidate.
        struct MutableDb {
            file_id: FileId,
            path: PathBuf,
            text: String,
        }

        impl Database for MutableDb {
            fn file_content(&self, file_id: FileId) -> &str {
                if file_id == self.file_id {
                    self.text.as_str()
                } else {
                    ""
                }
            }

            fn file_path(&self, file_id: FileId) -> Option<&std::path::Path> {
                (file_id == self.file_id).then_some(self.path.as_path())
            }

            fn all_file_ids(&self) -> Vec<FileId> {
                vec![self.file_id]
            }
        }

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("nova-spring-cache-inplace-test-{unique}"));
        let config_path = root.join("src/main/resources/application.properties");

        let file_id = FileId::from_raw(0);
        let prefix = "server.port=8080\n/*";
        let suffix = "*/\n";
        let mut text = String::new();
        text.push_str(prefix);
        text.push_str(&"a".repeat(1024));
        text.push_str(suffix);

        let mut db = MutableDb {
            file_id,
            path: config_path,
            text,
        };

        let first = workspace_index_for_file(&db, file_id);
        let second = workspace_index_for_file(&db, file_id);
        assert!(Arc::ptr_eq(&first, &second));

        // Mutate a byte in the middle of the buffer, preserving the allocation + length.
        let ptr_before = db.text.as_ptr();
        let len_before = db.text.len();
        let mid_idx = len_before / 2;
        assert!(
            mid_idx > 64 && mid_idx + 64 < len_before,
            "expected mutation index to be outside the sampled prefix/suffix regions"
        );
        unsafe {
            let bytes = db.text.as_mut_vec();
            assert_eq!(
                bytes[mid_idx], b'a',
                "expected mutation index to fall within the repeated marker content"
            );
            bytes[mid_idx] = b'b';
        }
        assert_eq!(
            ptr_before,
            db.text.as_ptr(),
            "expected in-place mutation to keep the same allocation"
        );
        assert_eq!(
            len_before,
            db.text.len(),
            "expected in-place mutation to keep the same length"
        );

        let third = workspace_index_for_file(&db, file_id);
        assert!(
            !Arc::ptr_eq(&second, &third),
            "expected spring config workspace index cache to invalidate when file text changes, even when pointer/len are stable"
        );
    }

    #[test]
    fn does_not_mix_files_across_roots() {
        let mut db = InMemoryFileStore::new();

        let root_a = PathBuf::from("/project-a");
        let file_a_path = root_a.join("src/main/resources/application.properties");
        let file_a = db.file_id_for_path(&file_a_path);
        db.set_file_text(file_a, "a.key=1\n".to_string());

        let root_b = PathBuf::from("/project-b");
        let file_b_path = root_b.join("src/main/resources/application.properties");
        let file_b = db.file_id_for_path(&file_b_path);
        db.set_file_text(file_b, "b.key=2\n".to_string());

        let index_a = workspace_index_for_file(&db, file_a);
        assert!(
            index_a.observed_keys().any(|k| k == "a.key"),
            "expected index for {} to include a.key; got {:?}",
            file_a_path.display(),
            index_a.observed_keys().collect::<Vec<_>>()
        );
        assert!(
            !index_a.observed_keys().any(|k| k == "b.key"),
            "expected index for {} to not include b.key; got {:?}",
            file_a_path.display(),
            index_a.observed_keys().collect::<Vec<_>>()
        );

        let index_b = workspace_index_for_file(&db, file_b);
        assert!(
            index_b.observed_keys().any(|k| k == "b.key"),
            "expected index for {} to include b.key; got {:?}",
            file_b_path.display(),
            index_b.observed_keys().collect::<Vec<_>>()
        );
        assert!(
            !index_b.observed_keys().any(|k| k == "a.key"),
            "expected index for {} to not include a.key; got {:?}",
            file_b_path.display(),
            index_b.observed_keys().collect::<Vec<_>>()
        );
    }
}
