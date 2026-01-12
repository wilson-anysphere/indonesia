#[cfg(any(test, debug_assertions))]
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use lsp_types::{Location, Position, Uri};
use nova_cache::Fingerprint;
use nova_core::{path_to_file_uri, AbsPathBuf, LineIndex};
use nova_db::{Database, FileId};
use nova_framework_mapstruct::NavigationTarget as MapStructNavigationTarget;
use nova_index::{InheritanceEdge, InheritanceIndex};
use once_cell::sync::Lazy;

use crate::framework_cache;
use crate::lombok_intel;
use crate::nav_core;
use crate::parse::{parse_file, ParsedFile, TypeDef};
use crate::text::{position_to_offset_with_index, span_to_lsp_range_with_index};

// The file-navigation index cache is a global LRU keyed by workspace root. In debug/test builds we
// intentionally give it a larger budget so parallel test execution doesn't evict entries between
// back-to-back requests within a single test (which would make caching tests flaky).
#[cfg(any(test, debug_assertions))]
const MAX_CACHED_ROOTS: usize = 256;
#[cfg(not(any(test, debug_assertions)))]
const MAX_CACHED_ROOTS: usize = 8;

/// Sentinel root used when the database cannot map a `FileId` to a path (e.g. virtual buffers
/// and in-memory fixtures).
const IN_MEMORY_ROOT: &str = "<in-memory>";

/// Tiny LRU cache used by workspace-scoped indexes.
///
/// This is a copy of the minimal implementation used by `spring_di.rs` so file navigation
/// can keep its own (smaller) cache budget.
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

#[derive(Clone, Debug)]
struct CachedIndex {
    fingerprints: Arc<BTreeMap<FileId, Fingerprint>>,
    index: Arc<FileNavigationIndex>,
}

static FILE_NAVIGATION_INDEX_CACHE: Lazy<Mutex<LruCache<PathBuf, CachedIndex>>> =
    Lazy::new(|| Mutex::new(LruCache::new(MAX_CACHED_ROOTS)));

#[derive(Clone, Debug)]
struct TypeInfo {
    file_id: FileId,
    uri: Uri,
    def: TypeDef,
}

impl nav_core::NavTypeInfo for TypeInfo {
    fn uri(&self) -> &Uri {
        &self.uri
    }

    fn def(&self) -> &TypeDef {
        &self.def
    }
}

#[derive(Debug, Default)]
struct FileNavigationIndex {
    files: HashMap<FileId, ParsedFile>,
    uri_to_file_id: HashMap<String, FileId>,
    types: HashMap<String, TypeInfo>,
    inheritance: InheritanceIndex,
}

#[cfg(any(test, debug_assertions))]
static FILE_NAVIGATION_INDEX_BUILD_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

// `file_navigation_index_build_count_for_tests` is used by integration tests that run in parallel.
// Maintain a thread-local counter so tests can observe build counts without interference from other
// concurrently executing tests.
#[cfg(any(test, debug_assertions))]
thread_local! {
    static FILE_NAVIGATION_INDEX_BUILD_COUNT_LOCAL: Cell<usize> = Cell::new(0);
}

#[cfg(any(test, debug_assertions))]
static FILE_NAVIGATION_INDEX_BUILD_COUNTS_BY_ROOT: Lazy<Mutex<HashMap<PathBuf, usize>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
impl FileNavigationIndex {
    #[allow(dead_code)]
    fn new(db: &dyn Database) -> Self {
        Self::new_for_file_ids(db, db.all_file_ids())
    }

    fn new_for_file_ids(db: &dyn Database, mut file_ids: Vec<FileId>) -> Self {
        #[cfg(any(test, debug_assertions))]
        {
            FILE_NAVIGATION_INDEX_BUILD_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            FILE_NAVIGATION_INDEX_BUILD_COUNT_LOCAL.with(|count| count.set(count.get() + 1));
        }

        file_ids.sort_by_key(|id| id.to_raw());

        let mut files = HashMap::new();

        let mut uri_to_file_id = HashMap::new();
        for file_id in &file_ids {
            let uri = uri_for_file(db, *file_id);
            let text = db.file_content(*file_id).to_string();
            let parsed = parse_file(uri, text);
            uri_to_file_id.insert(parsed.uri.to_string(), *file_id);
            files.insert(*file_id, parsed);
        }

        let mut types: HashMap<String, TypeInfo> = HashMap::new();
        for file_id in &file_ids {
            let Some(parsed_file) = files.get(file_id) else {
                continue;
            };
            for ty in &parsed_file.types {
                types.entry(ty.name.clone()).or_insert_with(|| TypeInfo {
                    file_id: *file_id,
                    uri: parsed_file.uri.clone(),
                    def: ty.clone(),
                });
            }
        }

        let mut inheritance = InheritanceIndex::default();
        let mut edges: Vec<InheritanceEdge> = Vec::new();
        for file_id in &file_ids {
            let Some(parsed_file) = files.get(file_id) else {
                continue;
            };
            for ty in &parsed_file.types {
                if let Some(super_class) = &ty.super_class {
                    edges.push(InheritanceEdge {
                        file: parsed_file.uri.to_string(),
                        subtype: ty.name.clone(),
                        supertype: super_class.clone(),
                    });
                }
                for iface in &ty.interfaces {
                    edges.push(InheritanceEdge {
                        file: parsed_file.uri.to_string(),
                        subtype: ty.name.clone(),
                        supertype: iface.clone(),
                    });
                }
            }
        }
        inheritance.extend(edges);

        Self {
            files,
            uri_to_file_id,
            types,
            inheritance,
        }
    }

    fn file(&self, file_id: FileId) -> Option<&ParsedFile> {
        self.files.get(&file_id)
    }

    fn file_by_uri(&self, uri: &Uri) -> Option<&ParsedFile> {
        let file_id = self.uri_to_file_id.get(uri.as_str())?;
        self.file(*file_id)
    }

    fn type_info(&self, name: &str) -> Option<&TypeInfo> {
        self.types.get(name)
    }
}

#[cfg(any(test, debug_assertions))]
pub fn file_navigation_index_build_count_for_tests() -> usize {
    FILE_NAVIGATION_INDEX_BUILD_COUNT_LOCAL.with(|count| count.get())
}

#[cfg(any(test, debug_assertions))]
pub fn file_navigation_index_build_count_for_file_for_tests(
    db: &dyn Database,
    file: FileId,
) -> usize {
    let (raw_root, mut root_key) = file_navigation_roots(db, file);
    let workspace_files = workspace_java_files(db, &raw_root, &root_key);
    if root_key.as_path() == Path::new(IN_MEMORY_ROOT) {
        root_key = in_memory_workspace_key(&workspace_files);
    }

    let counts = FILE_NAVIGATION_INDEX_BUILD_COUNTS_BY_ROOT
        .lock()
        .expect("file navigation build-count lock poisoned");
    counts.get(&root_key).copied().unwrap_or_default()
}

#[derive(Debug, Clone)]
struct WorkspaceJavaFile {
    path: Option<PathBuf>,
    file_id: FileId,
}

fn cached_file_navigation_index(db: &dyn Database, file: FileId) -> Arc<FileNavigationIndex> {
    let (raw_root, mut root_key) = file_navigation_roots(db, file);
    let workspace_files = workspace_java_files(db, &raw_root, &root_key);

    // When the database cannot provide file paths, fall back to a stable in-memory key derived from
    // the file list. This avoids mixing distinct virtual-buffer workspaces in the cache.
    if root_key.as_path() == Path::new(IN_MEMORY_ROOT) {
        root_key = in_memory_workspace_key(&workspace_files);
    }

    let file_ids: Vec<FileId> = workspace_files.iter().map(|f| f.file_id).collect();
    let fingerprints = Arc::new(workspace_fingerprints(db, &file_ids));

    {
        let mut cache = FILE_NAVIGATION_INDEX_CACHE
            .lock()
            .expect("file navigation cache lock poisoned");
        if let Some(entry) = cache
            .get_cloned(&root_key)
            .filter(|entry| entry.fingerprints == fingerprints)
        {
            return entry.index;
        }
    }

    let built = Arc::new(FileNavigationIndex::new_for_file_ids(db, file_ids));

    #[cfg(any(test, debug_assertions))]
    {
        let mut counts = FILE_NAVIGATION_INDEX_BUILD_COUNTS_BY_ROOT
            .lock()
            .expect("file navigation build-count lock poisoned");
        *counts.entry(root_key.clone()).or_default() += 1;
    }

    let mut cache = FILE_NAVIGATION_INDEX_CACHE
        .lock()
        .expect("file navigation cache lock poisoned");
    if let Some(entry) = cache
        .get_cloned(&root_key)
        .filter(|entry| entry.fingerprints == fingerprints)
    {
        return entry.index;
    }

    cache.insert(
        root_key,
        CachedIndex {
            fingerprints,
            index: Arc::clone(&built),
        },
    );

    built
}

fn file_navigation_roots(db: &dyn Database, file: FileId) -> (PathBuf, PathBuf) {
    match db.file_path(file) {
        Some(path) => {
            let raw_root = framework_cache::project_root_for_path(path);
            let root_key = normalize_root_for_cache(&raw_root);
            (raw_root, root_key)
        }
        None => {
            let root = PathBuf::from(IN_MEMORY_ROOT);
            (root.clone(), root)
        }
    }
}

fn normalize_root_for_cache(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

fn in_memory_workspace_key(files: &[WorkspaceJavaFile]) -> PathBuf {
    let mut bytes = Vec::new();
    for file in files {
        bytes.extend_from_slice(&file.file_id.to_raw().to_le_bytes());
        if let Some(path) = &file.path {
            bytes.extend_from_slice(path.to_string_lossy().as_bytes());
        }
        bytes.push(0);
    }

    let fp = Fingerprint::from_bytes(bytes);
    PathBuf::from(format!("{IN_MEMORY_ROOT}:{fp}"))
}

fn workspace_java_files(
    db: &dyn Database,
    raw_root: &Path,
    root_key: &Path,
) -> Vec<WorkspaceJavaFile> {
    use std::cmp::Ordering;

    let mut under_root = Vec::new();
    let mut all_java = Vec::new();
    let in_memory = raw_root == Path::new(IN_MEMORY_ROOT);
    let has_alt_root = raw_root != root_key;

    for file_id in db.all_file_ids() {
        match db.file_path(file_id) {
            Some(path) => {
                if path.extension().and_then(|e| e.to_str()) != Some("java") {
                    continue;
                }

                let entry = WorkspaceJavaFile {
                    path: Some(path.to_path_buf()),
                    file_id,
                };

                if in_memory
                    || path.starts_with(raw_root)
                    || (has_alt_root && path.starts_with(root_key))
                {
                    under_root.push(entry);
                } else {
                    all_java.push(entry);
                }
            }
            None => {
                if in_memory {
                    under_root.push(WorkspaceJavaFile {
                        path: None,
                        file_id,
                    });
                }
            }
        }
    }

    let mut files = if in_memory || !under_root.is_empty() {
        under_root
    } else {
        all_java
    };

    // Deterministic ordering: stable paths first, then virtual buffers (by FileId).
    files.sort_by(|a, b| match (&a.path, &b.path) {
        (Some(a_path), Some(b_path)) => a_path
            .cmp(b_path)
            .then(a.file_id.to_raw().cmp(&b.file_id.to_raw())),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.file_id.to_raw().cmp(&b.file_id.to_raw()),
    });

    files
}

fn workspace_fingerprints(db: &dyn Database, file_ids: &[FileId]) -> BTreeMap<FileId, Fingerprint> {
    let mut out = BTreeMap::new();
    for file_id in file_ids {
        let text = db.file_content(*file_id);
        out.insert(*file_id, Fingerprint::from_bytes(text.as_bytes()));
    }
    out
}

/// Best-effort `textDocument/implementation` for FileId-based databases.
#[must_use]
pub fn implementation(db: &dyn Database, file: FileId, position: Position) -> Vec<Location> {
    let index = cached_file_navigation_index(db, file);
    let Some(parsed) = index.file(file) else {
        return Vec::new();
    };
    let Some(offset) = position_to_offset_with_index(&parsed.line_index, &parsed.text, position)
    else {
        return Vec::new();
    };

    let method_decl = nav_core::method_decl_at(parsed, offset);

    // MapStruct: prioritize "go to implementation" from mapper interface methods into generated
    // `*MapperImpl` methods when the generated file exists on disk.
    if method_decl.is_some() {
        if let Some(path) = db.file_path(file) {
            if path.extension().and_then(|e| e.to_str()) == Some("java")
                && nova_framework_mapstruct::looks_like_mapstruct_source(&parsed.text)
            {
                let root = framework_cache::project_root_for_path(path);
                if let Ok(targets) = nova_framework_mapstruct::goto_definition_in_source(
                    &root,
                    path,
                    &parsed.text,
                    offset,
                ) {
                    if let Some(target) = targets.into_iter().next() {
                        if target
                            .file
                            .file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.ends_with("Impl.java"))
                        {
                            if let Some(location) = mapstruct_target_location(db, &index, target) {
                                return vec![location];
                            }
                        }
                    }
                }
            }
        }
    }

    let lookup_type_info = |name: &str| index.type_info(name);
    let lookup_file = |uri: &Uri| index.file_by_uri(uri);
    let lombok_fallback = |receiver_ty: &str, method_name: &str| {
        lombok_intel::goto_virtual_member_definition(db, file, receiver_ty, method_name)
            .map(|(target_file, target_span)| (uri_for_file(db, target_file), target_span))
    };

    let locations = if let Some(call) = parsed
        .calls
        .iter()
        .find(|call| nav_core::span_contains(call.method_span, offset))
    {
        nav_core::implementation_for_call(
            &index.inheritance,
            &lookup_type_info,
            &lookup_file,
            parsed,
            offset,
            call,
            &lombok_fallback,
        )
    } else if let Some((ty_name, method_name)) = method_decl {
        nav_core::implementation_for_abstract_method(
            &index.inheritance,
            &lookup_type_info,
            &lookup_file,
            &ty_name,
            &method_name,
        )
    } else if let Some(type_name) = parsed
        .types
        .iter()
        .find(|ty| nav_core::span_contains(ty.name_span, offset))
        .map(|ty| ty.name.clone())
    {
        nav_core::implementation_for_type(
            &index.inheritance,
            &lookup_type_info,
            &lookup_file,
            &type_name,
        )
    } else {
        Vec::new()
    };

    locations
}

/// Best-effort `textDocument/declaration` for FileId-based databases.
#[must_use]
pub fn declaration(db: &dyn Database, file: FileId, position: Position) -> Option<Location> {
    let index = cached_file_navigation_index(db, file);
    let parsed = index.file(file)?;
    let offset = position_to_offset_with_index(&parsed.line_index, &parsed.text, position)?;

    let lookup_type_info = |name: &str| index.type_info(name);
    let lookup_file = |uri: &Uri| index.file_by_uri(uri);

    let mut location = if let Some((ty_name, method_name)) =
        nav_core::method_decl_at(parsed, offset)
    {
        nav_core::declaration_for_override(&lookup_type_info, &lookup_file, &ty_name, &method_name)
    } else if let Some((ident, _span)) = nav_core::identifier_at(&parsed.text, offset) {
        if let Some((decl_uri, decl_span)) = nav_core::variable_declaration(parsed, offset, &ident)
        {
            if let Some(decl_parsed) = index.file_by_uri(&decl_uri) {
                Some(Location {
                    uri: decl_uri,
                    range: span_to_lsp_range_with_index(
                        &decl_parsed.line_index,
                        &decl_parsed.text,
                        decl_span,
                    ),
                })
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    if location.is_none() {
        location = mapstruct_fallback_locations(db, &index, file, &parsed.text, offset)
            .into_iter()
            .next();
    }

    location
}

/// Best-effort `textDocument/typeDefinition` for FileId-based databases.
#[must_use]
pub fn type_definition(db: &dyn Database, file: FileId, position: Position) -> Option<Location> {
    let index = cached_file_navigation_index(db, file);
    let parsed = index.file(file)?;
    let offset = position_to_offset_with_index(&parsed.line_index, &parsed.text, position)?;

    let lookup_type_info = |name: &str| index.type_info(name);
    let lookup_file = |uri: &Uri| index.file_by_uri(uri);
    nav_core::type_definition_best_effort(&lookup_type_info, &lookup_file, parsed, offset)
}

fn mapstruct_fallback_locations(
    db: &dyn Database,
    index: &FileNavigationIndex,
    file: FileId,
    text: &str,
    offset: usize,
) -> Vec<Location> {
    let Some(path) = db.file_path(file) else {
        return Vec::new();
    };
    if path.extension().and_then(|e| e.to_str()) != Some("java") {
        return Vec::new();
    }
    if !nova_framework_mapstruct::looks_like_mapstruct_source(text) {
        return Vec::new();
    }

    let root = framework_cache::project_root_for_path(path);
    let targets =
        match nova_framework_mapstruct::goto_definition_in_source(&root, path, text, offset) {
            Ok(targets) => targets,
            Err(_) => return Vec::new(),
        };

    targets
        .into_iter()
        .filter_map(|target| mapstruct_target_location(db, index, target))
        .collect()
}

fn mapstruct_target_location(
    db: &dyn Database,
    index: &FileNavigationIndex,
    target: MapStructNavigationTarget,
) -> Option<Location> {
    if let Some(file_id) = db.file_id(&target.file) {
        if let Some(parsed) = index.file(file_id) {
            return Some(Location {
                uri: uri_for_file(db, file_id),
                range: span_to_lsp_range_with_index(&parsed.line_index, &parsed.text, target.span),
            });
        }

        let text = db.file_content(file_id);
        let line_index = LineIndex::new(text);
        return Some(Location {
            uri: uri_for_file(db, file_id),
            range: span_to_lsp_range_with_index(&line_index, text, target.span),
        });
    }

    let text = std::fs::read_to_string(&target.file).ok()?;
    let line_index = LineIndex::new(&text);
    Some(Location {
        uri: uri_for_path(&target.file).unwrap_or_else(fallback_unknown_uri),
        range: span_to_lsp_range_with_index(&line_index, &text, target.span),
    })
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

fn fallback_unknown_uri() -> Uri {
    Uri::from_str("file:///unknown").expect("fallback URI is valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    use nova_db::InMemoryFileStore;
    use tempfile::TempDir;

    #[cfg(unix)]
    #[test]
    fn file_navigation_root_key_is_canonicalized() {
        use std::os::unix::fs::symlink;

        let temp_dir = TempDir::new().expect("tempdir");
        let real_root = temp_dir.path().join("real");
        let link_root = temp_dir.path().join("link");
        std::fs::create_dir_all(real_root.join("src")).expect("create real src");
        symlink(&real_root, &link_root).expect("symlink");

        let mut db = InMemoryFileStore::new();
        let file_path = link_root.join("src/Main.java");
        let file = db.file_id_for_path(&file_path);
        db.set_file_text(file, "class Main {}".to_string());

        let (_raw_root, got) = file_navigation_roots(&db, file);
        let expected = std::fs::canonicalize(&real_root).expect("canonical real root");
        assert_eq!(got, expected);
    }

    #[test]
    fn file_navigation_index_cache_reuses_index_until_workspace_changes() {
        FILE_NAVIGATION_INDEX_BUILD_COUNT_LOCAL.with(|count| count.set(0));

        let temp_dir = TempDir::new().expect("tempdir");
        let root = temp_dir.path();

        let mut db = InMemoryFileStore::new();

        let i_path = root.join("I.java");
        let c_path = root.join("C.java");

        let i_file = db.file_id_for_path(&i_path);
        let c_file = db.file_id_for_path(&c_path);

        db.set_file_text(i_file, "interface I {\n    void foo();\n}\n".to_string());
        db.set_file_text(
            c_file,
            "class C implements I {\n    public void foo() {}\n}\n".to_string(),
        );

        // "foo" in `I.java` is on line 1 after `    void ` (9 UTF-16 units).
        let pos = Position {
            line: 1,
            character: 9,
        };

        let got_first = implementation(&db, i_file, pos);
        assert_eq!(
            file_navigation_index_build_count_for_tests(),
            1,
            "expected initial request to build the workspace index"
        );

        let got_second = implementation(&db, i_file, pos);
        assert_eq!(
            file_navigation_index_build_count_for_tests(),
            1,
            "expected repeated request to reuse the cached workspace index"
        );
        assert_eq!(got_first, got_second);

        assert_eq!(got_first.len(), 1);
        assert_eq!(got_first[0].uri, uri_for_path(&c_path).expect("c uri"));
        assert_eq!(
            got_first[0].range.start,
            Position {
                line: 1,
                character: 16,
            }
        );

        // Mutate `C.java` so the workspace fingerprint changes.
        db.set_file_text(
            c_file,
            "class C implements I {\n    // shifted\n    public void foo() {}\n}\n".to_string(),
        );

        let got_third = implementation(&db, i_file, pos);
        assert_eq!(
            file_navigation_index_build_count_for_tests(),
            2,
            "expected cache invalidation after editing a Java file"
        );
        assert_eq!(got_third.len(), 1);
        assert_eq!(
            got_third[0].range.start,
            Position {
                line: 2,
                character: 16,
            }
        );
    }

    #[test]
    fn go_to_implementation_on_super_call_resolves_super_method_definition() {
        let temp_dir = TempDir::new().expect("tempdir");
        let root = temp_dir.path();

        let mut db = InMemoryFileStore::new();

        let base_path = root.join("Base.java");
        let sub_path = root.join("Sub.java");

        let base_file = db.file_id_for_path(&base_path);
        let sub_file = db.file_id_for_path(&sub_path);

        let base_text = "class Base {\n    void foo() {}\n}\n".to_string();
        let sub_text = "class Sub extends Base {\n    @Override\n    void foo() {}\n    void test() { super.foo(); }\n}\n".to_string();

        db.set_file_text(base_file, base_text);
        db.set_file_text(sub_file, sub_text);

        let index = cached_file_navigation_index(&db, sub_file);
        let parsed_sub = index.file(sub_file).expect("sub parsed");
        assert_eq!(
            parsed_sub.types.len(),
            1,
            "expected one type in Sub.java, got types={:?}",
            parsed_sub.types
        );
        let sub_ty = &parsed_sub.types[0];
        assert!(
            sub_ty.methods.iter().any(|m| m.name == "test"),
            "expected Sub.test() to be indexed, got methods={:?}",
            sub_ty.methods
        );
        assert!(
            parsed_sub
                .calls
                .iter()
                .any(|call| call.receiver == "super" && call.method == "foo"),
            "expected `super.foo()` to be indexed as a call site, got calls={:?}",
            parsed_sub.calls
        );
        let offset = parsed_sub
            .text
            .find("super.foo")
            .expect("super.foo")
            + "super.".len();
        let pos = crate::text::offset_to_position_with_index(&parsed_sub.line_index, &parsed_sub.text, offset);

        let got = implementation(&db, sub_file, pos);
        assert_eq!(got.len(), 1);

        let parsed_base = index.file(base_file).expect("base parsed");
        let base_foo_offset = parsed_base.text.find("foo").expect("foo");
        let expected_pos = crate::text::offset_to_position_with_index(
            &parsed_base.line_index,
            &parsed_base.text,
            base_foo_offset,
        );

        assert_eq!(got[0].uri, uri_for_path(&base_path).expect("base uri"));
        assert_eq!(got[0].range.start, expected_pos);
    }
}
