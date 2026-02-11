//! Cached workspace-level Java source index for import completions.
//!
//! `WorkspaceJavaIndex::build` does a full scan of all Java files in the `Database`, which can be
//! expensive when it runs on every completion request. This module provides a small, thread-safe
//! cache keyed by the discovered project root.

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use once_cell::sync::Lazy;

use nova_db::{Database, FileId};

use crate::framework_cache;
use crate::java_completion::workspace_index::WorkspaceJavaIndex;

const MAX_CACHED_ROOTS: usize = 32;

static WORKSPACE_INDEX_CACHE: Lazy<WorkspaceJavaIndexCache> =
    Lazy::new(WorkspaceJavaIndexCache::new);

/// A best-effort identifier for the current database instance.
///
/// The workspace index cache is global (shared across threads) and keyed by project root. In tests,
/// many fixtures reuse the same virtual roots (e.g. `/workspace`) while constructing independent
/// in-memory databases, so we include the database address in the key to avoid cross-test
/// interference under parallel execution.
fn db_cache_id(db: &dyn Database) -> usize {
    // Cast the fat pointer to a thin pointer, dropping the vtable metadata.
    db as *const dyn Database as *const () as usize
}

type WorkspaceIndexCacheKey = (usize, PathBuf);

#[derive(Debug)]
struct WorkspaceJavaIndexCache {
    entries: Mutex<LruCache<WorkspaceIndexCacheKey, CachedWorkspaceJavaIndex>>,
}

#[derive(Clone, Debug)]
struct CachedWorkspaceJavaIndex {
    fingerprint: u64,
    index: Arc<WorkspaceJavaIndex>,
}

trait CanEvict {
    fn can_evict(&self) -> bool;
}

impl CanEvict for CachedWorkspaceJavaIndex {
    fn can_evict(&self) -> bool {
        // Avoid evicting entries that are still in use outside of the cache (e.g. in-flight
        // completion requests that hold an `Arc<WorkspaceJavaIndex>`). This keeps back-to-back
        // requests stable under concurrent cache pressure and prevents test flakiness.
        Arc::strong_count(&self.index) <= 1
    }
}

impl WorkspaceJavaIndexCache {
    fn new() -> Self {
        Self {
            entries: Mutex::new(LruCache::new(MAX_CACHED_ROOTS)),
        }
    }

    fn index_for_root(&self, db: &dyn Database, raw_root: &Path) -> Arc<WorkspaceJavaIndex> {
        let canonical_root = normalize_root_for_cache(raw_root);
        let has_alt_root = canonical_root != raw_root;
        let key = (db_cache_id(db), canonical_root.clone());

        // Collect java files under the root (fallback to all Java files if the root contains none).
        let mut under_root = Vec::<(PathBuf, FileId)>::new();
        let mut all = Vec::<(PathBuf, FileId)>::new();

        for file_id in db.all_file_ids() {
            let Some(path) = db.file_path(file_id) else {
                continue;
            };
            if path.extension().and_then(|e| e.to_str()) != Some("java") {
                continue;
            }

            let tuple = (path.to_path_buf(), file_id);
            if path.starts_with(raw_root) || (has_alt_root && path.starts_with(&canonical_root)) {
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
        files.sort_by(|(a, _), (b, _)| a.cmp(b));

        // Fingerprint sources (fast pointer/len hashing, plus a small content sample).
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for (path, file_id) in &files {
            path.hash(&mut hasher);
            let text = db.file_content(*file_id);
            text.len().hash(&mut hasher);
            text.as_ptr().hash(&mut hasher);

            // Pointer/len hashing is fast, but can collide when short-lived buffers reuse the same
            // allocations (e.g. in tests) or when text is mutated in place. Mix in a small,
            // content-dependent sample to make invalidation deterministic without hashing full
            // contents for large files.
            let bytes = text.as_bytes();
            const SAMPLE: usize = 64;
            const FULL_HASH_MAX: usize = 3 * SAMPLE;
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
        let fingerprint = hasher.finish();

        // Cache hit.
        {
            let mut cache = lock_unpoison(&self.entries);
            if let Some(entry) = cache.get_cloned(&key) {
                if entry.fingerprint == fingerprint {
                    return entry.index;
                }
            }
        }

        // Cache miss; rebuild.
        let index = Arc::new(WorkspaceJavaIndex::build_for_files(db, &files));
        let entry = CachedWorkspaceJavaIndex {
            fingerprint,
            index: Arc::clone(&index),
        };
        lock_unpoison(&self.entries).insert(key, entry);
        index
    }
}

/// Return a cached workspace Java index for `file`.
///
/// The cache is keyed by (normalized) project root, using the same root discovery as
/// [`crate::framework_cache`]. The entry is invalidated when the set of Java files under the root
/// changes (path set or file text pointer/length).
#[must_use]
pub(crate) fn workspace_index_for_file(db: &dyn Database, file: FileId) -> Arc<WorkspaceJavaIndex> {
    let Some(root) = framework_cache::project_root_for_file(db, file) else {
        return Arc::new(WorkspaceJavaIndex::build(db));
    };

    WORKSPACE_INDEX_CACHE.index_for_root(db, &root)
}

// -----------------------------------------------------------------------------
// Minimal LRU cache (copied from `completion_cache` / `framework_cache`).
// -----------------------------------------------------------------------------

#[derive(Debug)]
struct LruCache<K, V> {
    capacity: usize,
    map: HashMap<K, V>,
    order: VecDeque<K>,
}

impl<K, V> LruCache<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone + CanEvict,
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
        let mut scanned = 0usize;
        while self.map.len() > self.capacity && !self.order.is_empty() {
            let Some(key) = self.order.pop_front() else {
                break;
            };

            let Some(value) = self.map.get(&key) else {
                continue;
            };

            if value.can_evict() {
                self.map.remove(&key);
                scanned = 0;
            } else {
                self.order.push_back(key);
                scanned += 1;

                // If we made a full pass without evicting anything, all entries are currently in
                // use. Allow the cache to temporarily exceed capacity instead of spinning.
                if scanned >= self.order.len() {
                    break;
                }
            }
        }
    }
}

fn normalize_root_for_cache(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

fn lock_unpoison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use nova_db::InMemoryFileStore;
    use tempfile::TempDir;

    #[test]
    fn workspace_index_cache_hits_and_invalidates_on_java_edit() {
        let mut db = InMemoryFileStore::new();

        let a = db.file_id_for_path("/workspace/src/main/java/com/foo/A.java");
        let b = db.file_id_for_path("/workspace/src/main/java/com/foo/B.java");

        db.set_file_text(a, "package com.foo;\npublic class A {}\n".to_string());
        db.set_file_text(b, "package com.foo;\npublic class B {}\n".to_string());

        let first = workspace_index_for_file(&db, a);
        let second = workspace_index_for_file(&db, a);
        assert!(Arc::ptr_eq(&first, &second));
        assert!(first.contains_fqn("com.foo.A"));
        assert!(first.contains_fqn("com.foo.B"));

        // Mutate a different Java file under the same root to force invalidation.
        db.set_file_text(b, "package com.foo;\npublic class B2 {}\n".to_string());

        let third = workspace_index_for_file(&db, a);
        assert!(!Arc::ptr_eq(&first, &third));
        assert!(third.contains_fqn("com.foo.B2"));
        assert!(!third.contains_fqn("com.foo.B"));
    }

    #[test]
    fn workspace_index_cache_invalidates_when_file_text_changes_in_place_with_same_ptr_and_len() {
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

            fn file_path(&self, file_id: FileId) -> Option<&Path> {
                (file_id == self.file_id).then_some(self.path.as_path())
            }

            fn all_file_ids(&self) -> Vec<FileId> {
                vec![self.file_id]
            }
        }

        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();
        let src_dir = root.join("src/main/java/p");
        std::fs::create_dir_all(&src_dir).expect("create dirs");
        let file_path = src_dir.join("Main.java");
        // Create a real file so `nova_project::workspace_root` prefers this tempdir instead of
        // walking up to unrelated ancestors like `/tmp`.
        std::fs::write(&file_path, "").expect("write file");

        let file_id = FileId::from_raw(0);
        let prefix = "package p;\n";
        let decl = "public class A {}\n";
        let filler_a = format!("/*{}*/\n", "a".repeat(1024));
        let filler_b = format!("/*{}*/\n", "b".repeat(1024));
        let text = format!("{prefix}{filler_a}{decl}{filler_b}");

        let a_idx = text
            .find("class A")
            .map(|idx| idx + "class ".len())
            .expect("expected class name in fixture");
        const SAMPLE: usize = 64;
        let mid = text.len() / 2;
        let mid_start = mid.saturating_sub(SAMPLE / 2);
        let mid_end = (mid_start + SAMPLE).min(text.len());
        assert!(
            a_idx >= mid_start && a_idx < mid_end,
            "expected class name byte to fall within the middle hash sample region"
        );

        let mut db = MutableDb {
            file_id,
            path: file_path,
            text,
        };

        let first = workspace_index_for_file(&db, file_id);
        let second = workspace_index_for_file(&db, file_id);
        assert!(Arc::ptr_eq(&first, &second));
        assert!(first.contains_fqn("p.A"));

        // Mutate the file content in place, preserving allocation + length.
        let ptr_before = db.text.as_ptr();
        let len_before = db.text.len();
        unsafe {
            let bytes = db.text.as_mut_vec();
            assert_eq!(bytes[a_idx], b'A');
            bytes[a_idx] = b'B';
        }
        assert_eq!(ptr_before, db.text.as_ptr());
        assert_eq!(len_before, db.text.len());

        let third = workspace_index_for_file(&db, file_id);
        assert!(!Arc::ptr_eq(&first, &third));
        assert!(third.contains_fqn("p.B"));
        assert!(!third.contains_fqn("p.A"));
    }
}
