use std::cmp::Ordering;
use std::collections::{hash_map::DefaultHasher, BTreeSet, HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use lsp_types::Uri;
use nova_core::{path_to_file_uri, AbsPathBuf};
use nova_db::{Database, FileId};
use nova_index::{InheritanceEdge, InheritanceIndex};
use nova_types::Span;
use once_cell::sync::Lazy;

use crate::parse::{parse_file, MethodDef, ParsedFile, TypeDef};

const MAX_CACHED_WORKSPACES: usize = 8;

/// Best-effort, stable-ish identifier for a workspace, derived from the set of Java files.
///
/// This is intentionally opaque; it exists solely as an LRU cache key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct WorkspaceId(u64);

#[derive(Clone, Debug)]
struct CachedEntry {
    fingerprint: u64,
    index: Arc<WorkspaceHierarchyIndex>,
}

/// Tiny LRU cache used by workspace-scoped indexes.
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

static WORKSPACE_HIERARCHY_INDEX_CACHE: Lazy<Mutex<LruCache<WorkspaceId, CachedEntry>>> =
    Lazy::new(|| Mutex::new(LruCache::new(MAX_CACHED_WORKSPACES)));

#[cfg(test)]
static WORKSPACE_HIERARCHY_REBUILDS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[derive(Clone, Debug)]
pub(crate) struct TypeInfo {
    pub(crate) file_id: FileId,
    pub(crate) uri: Uri,
    pub(crate) def: TypeDef,
}

#[derive(Clone, Debug)]
pub(crate) struct MethodInfo {
    pub(crate) file_id: FileId,
    pub(crate) uri: Uri,
    pub(crate) type_name: String,
    pub(crate) name: String,
    pub(crate) name_span: Span,
    pub(crate) body_span: Option<Span>,
}

#[derive(Debug, Default)]
pub(crate) struct WorkspaceHierarchyIndex {
    file_ids: Vec<FileId>,
    files: HashMap<FileId, ParsedFile>,
    types: HashMap<String, TypeInfo>,
    inheritance: InheritanceIndex,
    methods: HashMap<(String, String), MethodInfo>,
}

impl WorkspaceHierarchyIndex {
    pub(crate) fn new(db: &dyn Database) -> Self {
        #[cfg(test)]
        WORKSPACE_HIERARCHY_REBUILDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let mut file_ids: Vec<FileId> = db
            .all_file_ids()
            .into_iter()
            .filter(|id| is_java_file(db, *id))
            .collect();
        // Keep iteration deterministic for tests.
        file_ids.sort_by_key(|id| id.to_raw());

        let mut files = HashMap::new();
        for file_id in &file_ids {
            let uri = uri_for_file(db, *file_id);
            let text = db.file_content(*file_id).to_string();
            files.insert(*file_id, parse_file(uri, text));
        }

        let mut types: HashMap<String, TypeInfo> = HashMap::new();
        let mut methods: HashMap<(String, String), MethodInfo> = HashMap::new();

        for file_id in &file_ids {
            let Some(parsed) = files.get(file_id) else {
                continue;
            };

            for ty in &parsed.types {
                types.entry(ty.name.clone()).or_insert_with(|| TypeInfo {
                    file_id: *file_id,
                    uri: parsed.uri.clone(),
                    def: ty.clone(),
                });

                for m in &ty.methods {
                    methods
                        .entry((ty.name.clone(), m.name.clone()))
                        .or_insert_with(|| {
                            method_info_from_def(*file_id, &parsed.uri, &ty.name, m)
                        });
                }
            }
        }

        let mut inheritance = InheritanceIndex::default();
        let mut edges: Vec<InheritanceEdge> = Vec::new();
        for file_id in &file_ids {
            let Some(parsed) = files.get(file_id) else {
                continue;
            };
            for ty in &parsed.types {
                if let Some(super_class) = &ty.super_class {
                    edges.push(InheritanceEdge {
                        file: parsed.uri.to_string(),
                        subtype: ty.name.clone(),
                        supertype: super_class.clone(),
                    });
                }
                for iface in &ty.interfaces {
                    edges.push(InheritanceEdge {
                        file: parsed.uri.to_string(),
                        subtype: ty.name.clone(),
                        supertype: iface.clone(),
                    });
                }
            }
        }
        inheritance.extend(edges);

        Self {
            file_ids,
            files,
            types,
            inheritance,
            methods,
        }
    }

    /// Returns a cached workspace index when the current workspace fingerprint matches.
    ///
    /// This is a best-effort cache intended to avoid re-parsing all Java source files for repeated
    /// call/type hierarchy requests. Invalidation is done via a cheap `(path, len, ptr)` fingerprint
    /// per Java file (see [`workspace_identity`]).
    pub(crate) fn get_cached(db: &dyn Database) -> Arc<Self> {
        let (workspace_id, fingerprint) = workspace_identity(db);

        {
            let mut cache = WORKSPACE_HIERARCHY_INDEX_CACHE
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            if let Some(entry) = cache
                .get_cloned(&workspace_id)
                .filter(|entry| entry.fingerprint == fingerprint)
            {
                return entry.index;
            }
        }

        let built = Arc::new(Self::new(db));

        let mut cache = WORKSPACE_HIERARCHY_INDEX_CACHE
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if let Some(entry) = cache
            .get_cloned(&workspace_id)
            .filter(|entry| entry.fingerprint == fingerprint)
        {
            return entry.index;
        }

        cache.insert(
            workspace_id,
            CachedEntry {
                fingerprint,
                index: Arc::clone(&built),
            },
        );

        built
    }

    pub(crate) fn file_ids(&self) -> &[FileId] {
        &self.file_ids
    }

    pub(crate) fn file(&self, file_id: FileId) -> Option<&ParsedFile> {
        self.files.get(&file_id)
    }

    pub(crate) fn type_info(&self, name: &str) -> Option<&TypeInfo> {
        self.types.get(name)
    }

    pub(crate) fn inheritance(&self) -> &InheritanceIndex {
        &self.inheritance
    }

    #[allow(dead_code)]
    pub(crate) fn method_info(&self, type_name: &str, method_name: &str) -> Option<&MethodInfo> {
        self.methods
            .get(&(type_name.to_string(), method_name.to_string()))
    }

    pub(crate) fn resolve_method_definition(
        &self,
        type_name: &str,
        method_name: &str,
    ) -> Option<MethodInfo> {
        let mut visited = BTreeSet::new();
        self.resolve_method_definition_inner(type_name, method_name, &mut visited)
    }

    fn resolve_method_definition_inner(
        &self,
        type_name: &str,
        method_name: &str,
        visited: &mut BTreeSet<String>,
    ) -> Option<MethodInfo> {
        if !visited.insert(type_name.to_string()) {
            return None;
        }

        let type_info = self.type_info(type_name)?;
        if let Some(method) = type_info.def.methods.iter().find(|m| m.name == method_name) {
            return Some(method_info_from_def(
                type_info.file_id,
                &type_info.uri,
                &type_info.def.name,
                method,
            ));
        }

        // Search interfaces / extended interfaces first.
        for iface in &type_info.def.interfaces {
            if let Some(def) = self.resolve_method_definition_inner(iface, method_name, visited) {
                return Some(def);
            }
        }

        // Then walk the superclass chain.
        if let Some(super_name) = type_info.def.super_class.as_deref() {
            return self.resolve_method_definition_inner(super_name, method_name, visited);
        }

        None
    }

    pub(crate) fn resolve_super_types(&self, type_name: &str) -> Vec<String> {
        self.inheritance
            .supertypes
            .get(type_name)
            .cloned()
            .unwrap_or_default()
    }

    pub(crate) fn resolve_sub_types(&self, type_name: &str) -> Vec<String> {
        self.inheritance
            .subtypes
            .get(type_name)
            .cloned()
            .unwrap_or_default()
    }
}

fn workspace_identity(db: &dyn Database) -> (WorkspaceId, u64) {
    #[derive(Clone, Copy)]
    struct JavaFile<'a> {
        file_id: FileId,
        path: Option<&'a Path>,
    }

    let mut files: Vec<JavaFile<'_>> = db
        .all_file_ids()
        .into_iter()
        .filter(|id| is_java_file(db, *id))
        .map(|file_id| JavaFile {
            file_id,
            path: db.file_path(file_id),
        })
        .collect();

    // Deterministic ordering: stable paths first, then virtual buffers (by FileId).
    files.sort_by(|a, b| match (&a.path, &b.path) {
        (Some(a_path), Some(b_path)) => a_path
            .cmp(b_path)
            .then(a.file_id.to_raw().cmp(&b.file_id.to_raw())),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.file_id.to_raw().cmp(&b.file_id.to_raw()),
    });

    let mut workspace_hasher = DefaultHasher::new();
    let mut fingerprint_hasher = DefaultHasher::new();

    for file in files {
        match file.path {
            Some(path) => {
                path.hash(&mut workspace_hasher);
                path.hash(&mut fingerprint_hasher);
            }
            None => {
                file.file_id.to_raw().hash(&mut workspace_hasher);
                file.file_id.to_raw().hash(&mut fingerprint_hasher);
            }
        }

        // Avoid hashing full file contents on every request: we only need a cheap signal that the
        // workspace has changed. For `InMemoryFileStore`, edits replace the underlying `String`, so
        // `(len, ptr)` is a reasonable proxy for content identity.
        let text = db.file_content(file.file_id);
        text.len().hash(&mut fingerprint_hasher);
        (text.as_ptr() as usize).hash(&mut fingerprint_hasher);
    }

    (
        WorkspaceId(workspace_hasher.finish()),
        fingerprint_hasher.finish(),
    )
}

fn method_info_from_def(
    file_id: FileId,
    uri: &Uri,
    type_name: &str,
    method: &MethodDef,
) -> MethodInfo {
    MethodInfo {
        file_id,
        uri: uri.clone(),
        type_name: type_name.to_string(),
        name: method.name.clone(),
        name_span: method.name_span,
        body_span: method.body_span,
    }
}

fn is_java_file(db: &dyn Database, file_id: FileId) -> bool {
    db.file_path(file_id)
        .is_some_and(|path| path.extension().and_then(|e| e.to_str()) == Some("java"))
}

fn uri_for_file(db: &dyn Database, file_id: FileId) -> Uri {
    if let Some(path) = db.file_path(file_id) {
        if let Some(uri) = uri_for_path(path) {
            return uri;
        }
    }

    Uri::from_str(&format!("file:///unknown/{}.java", file_id.to_raw()))
        .expect("fallback URI is valid")
}

fn uri_for_path(path: &Path) -> Option<Uri> {
    let abs = AbsPathBuf::new(path.to_path_buf()).ok()?;
    let uri = path_to_file_uri(&abs).ok()?;
    Uri::from_str(&uri).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    use nova_db::InMemoryFileStore;

    fn reset_rebuild_counter() {
        WORKSPACE_HIERARCHY_REBUILDS.store(0, std::sync::atomic::Ordering::Relaxed);
        // Ensure the cache doesn't carry state across unrelated unit tests.
        let mut cache = WORKSPACE_HIERARCHY_INDEX_CACHE
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        cache.map.clear();
        cache.order.clear();
    }

    #[test]
    fn workspace_hierarchy_index_is_cached_across_type_hierarchy_requests() {
        reset_rebuild_counter();

        let mut db = InMemoryFileStore::new();

        let file_a = db.file_id_for_path("/A.java");
        db.set_file_text(file_a, "class A {}".to_string());

        let file_b = db.file_id_for_path("/B.java");
        db.set_file_text(file_b, "class B extends A {}".to_string());

        let pos_b = lsp_types::Position::new(0, 6); // `B` in `class B`.

        let items = crate::prepare_type_hierarchy(&db, file_b, pos_b)
            .expect("expected type hierarchy items");
        assert_eq!(items[0].name, "B");

        let items_again = crate::prepare_type_hierarchy(&db, file_b, pos_b)
            .expect("expected type hierarchy items");
        assert_eq!(items_again[0].name, "B");

        assert_eq!(
            WORKSPACE_HIERARCHY_REBUILDS.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "expected hierarchy index to rebuild once for repeated requests without edits"
        );

        // Edit a file and ensure the cache invalidates.
        db.set_file_text(file_a, "class A { int x; }".to_string());

        let items_after_edit = crate::prepare_type_hierarchy(&db, file_b, pos_b)
            .expect("expected type hierarchy items");
        assert_eq!(items_after_edit[0].name, "B");

        assert_eq!(
            WORKSPACE_HIERARCHY_REBUILDS.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "expected hierarchy index to rebuild after a file edit"
        );
    }
}
