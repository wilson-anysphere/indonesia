//! Adapter between `nova-db` and `nova-framework`.
//!
//! `nova-framework` analyzers expect a small [`nova_framework::Database`] surface: file text/path
//! lookup, project scoping, dependency/classpath queries, and (best-effort) class metadata.
//!
//! Nova's IDE layer (`nova-ide`/`nova-lsp`) is currently built around the legacy
//! [`nova_db::Database`] trait. This module bridges the two so `nova-framework` analyzers can run
//! inside `nova-ide` without bespoke per-framework glue.

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use once_cell::sync::Lazy;

use nova_core::ProjectId;
use nova_db::{Database as HostDatabase, FileId};
use nova_framework::Database as FrameworkDatabase;
use nova_hir::framework::{Annotation, ClassData, ConstructorData, FieldData, MethodData};
use nova_scheduler::CancellationToken;
use nova_syntax::ast::{self as syntax_ast, AstNode};
use nova_syntax::SyntaxKind;
use nova_types::{ClassId, Parameter, PrimitiveType, Span, Type};
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
    root: PathBuf,
    project: ProjectId,

    all_files: Vec<FileId>,

    classes: Vec<ClassData>,

    config: Option<Arc<nova_project::ProjectConfig>>,

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
        let idx = class.to_raw() as usize;
        self.classes
            .get(idx)
            .unwrap_or_else(|| panic!("unknown ClassId passed to framework db: {class:?}"))
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
                    if zip_contains_exact(entry.path.as_path(), normalized_class_file, cancel) {
                        return true;
                    }
                }
            }
        }

        false
    }

    fn classpath_contains_prefix(&self, normalized_prefix: &str, cancel: &CancellationToken) -> bool {
        if cancel.is_cancelled() {
            return false;
        }

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
                    if zip_contains_prefix(entry.path.as_path(), normalized_prefix, cancel) {
                        return true;
                    }
                }
            }
        }

        false
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
        Some(self.shared.host_db.file_content(file))
    }

    fn file_path(&self, file: FileId) -> Option<&Path> {
        self.shared.host_db.file_path(file)
    }

    fn file_id(&self, path: &Path) -> Option<FileId> {
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
        classes.extend(extract_classes_from_source(text));
    }

    let all_file_ids = all_files.iter().map(|(_, id)| *id).collect();

    let shared = Arc::new(FrameworkDbShared {
        host_db: Arc::clone(&db),
        root: root_key,
        project,
        all_files: all_file_ids,
        classes,
        config,
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
        (text.as_ptr() as usize).hash(&mut hasher);
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

    let mut files = if under_root.is_empty() { all } else { under_root };
    files.sort_by(|(a, _), (b, _)| a.cmp(b));

    let mut java_files: Vec<_> = files
        .iter()
        .filter_map(|(path, id)| {
            (path.extension().and_then(|e| e.to_str()) == Some("java")).then_some((path.clone(), *id))
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
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}

fn normalize_root_for_cache(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
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

fn zip_contains_exact(path: &Path, entry: &str, cancel: &CancellationToken) -> bool {
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
    let ok = archive.by_name(&alt).is_ok();
    ok
}

fn zip_contains_prefix(path: &Path, prefix: &str, cancel: &CancellationToken) -> bool {
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
    for name in archive.file_names() {
        if cancel.is_cancelled() {
            return false;
        }
        if name.starts_with(prefix) || name.starts_with(&alt_prefix) {
            return true;
        }
    }
    false
}

// -----------------------------------------------------------------------------
// Java source parsing (best-effort)
// -----------------------------------------------------------------------------

fn extract_classes_from_source(source: &str) -> Vec<ClassData> {
    let mut classes = Vec::new();

    let parse = nova_syntax::parse_java(source);
    for node in parse.syntax().descendants() {
        let Some(class) = syntax_ast::ClassDeclaration::cast(node) else {
            continue;
        };
        if let Some(class) = parse_class_declaration(class, source) {
            classes.push(class);
        }
    }

    classes
}

fn parse_class_declaration(node: syntax_ast::ClassDeclaration, source: &str) -> Option<ClassData> {
    let modifiers = node.modifiers();
    let annotations = modifiers
        .as_ref()
        .map(|m| collect_annotations(m))
        .unwrap_or_default();

    let class_name = node.name_token()?.text().to_string();

    let body = node.body()?;
    let mut fields = Vec::new();
    let mut methods = Vec::new();
    let mut constructors = Vec::new();

    for member in body.members() {
        match member {
            syntax_ast::ClassMember::FieldDeclaration(field) => {
                let mut parsed = parse_field_declaration(field, source);
                fields.append(&mut parsed);
            }
            syntax_ast::ClassMember::MethodDeclaration(method) => {
                if let Some(method) = parse_method_declaration(method, source) {
                    methods.push(method);
                }
            }
            syntax_ast::ClassMember::ConstructorDeclaration(ctor) => {
                if let Some(ctor) = parse_constructor_declaration(ctor, source) {
                    constructors.push(ctor);
                }
            }
            _ => {}
        }
    }

    Some(ClassData {
        name: class_name,
        annotations,
        fields,
        methods,
        constructors,
    })
}

fn parse_field_declaration(node: syntax_ast::FieldDeclaration, source: &str) -> Vec<FieldData> {
    let modifiers = node.modifiers();
    let annotations = modifiers
        .as_ref()
        .map(collect_annotations)
        .unwrap_or_default();

    let (is_static, is_final) = modifiers
        .as_ref()
        .map(modifier_flags)
        .unwrap_or((false, false));

    let ty = node
        .ty()
        .map(|n| parse_type(node_text(source, n.syntax())))
        .unwrap_or(Type::Unknown);

    let mut out = Vec::new();
    for declarator in node.declarators() {
        let Some(name_node) = declarator.name_token() else {
            continue;
        };
        out.push(FieldData {
            name: name_node.text().to_string(),
            ty: ty.clone(),
            is_static,
            is_final,
            annotations: annotations.clone(),
        });
    }
    out
}

fn parse_method_declaration(node: syntax_ast::MethodDeclaration, source: &str) -> Option<MethodData> {
    let modifiers = node.modifiers();
    let is_static = modifiers
        .as_ref()
        .is_some_and(|m| modifier_contains_keyword(m, SyntaxKind::StaticKw));

    let name = node.name_token()?.text().to_string();

    let return_type = if node
        .syntax()
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .any(|tok| tok.kind() == SyntaxKind::VoidKw)
    {
        Type::Void
    } else {
        node.return_type()
            .map(|n| parse_type(node_text(source, n.syntax())))
            .unwrap_or(Type::Unknown)
    };

    let params = parse_formal_parameters(node.parameter_list(), source);

    Some(MethodData {
        name,
        return_type,
        params,
        is_static,
    })
}

fn parse_constructor_declaration(
    node: syntax_ast::ConstructorDeclaration,
    source: &str,
) -> Option<ConstructorData> {
    let params = parse_formal_parameters(node.parameter_list(), source);
    Some(ConstructorData { params })
}

fn parse_formal_parameters(node: Option<syntax_ast::ParameterList>, source: &str) -> Vec<Parameter> {
    let mut out = Vec::new();
    let Some(node) = node else {
        return out;
    };

    for child in node.parameters() {
        let Some(name_node) = child.name_token() else {
            continue;
        };
        let name = name_node.text().to_string();
        let ty = child
            .ty()
            .map(|n| parse_type(node_text(source, n.syntax())))
            .unwrap_or(Type::Unknown);
        out.push(Parameter::new(name, ty));
    }

    out
}

fn node_text<'a>(source: &'a str, node: &nova_syntax::SyntaxNode) -> &'a str {
    let range = node.text_range();
    let start: usize = u32::from(range.start()) as usize;
    let end: usize = u32::from(range.end()) as usize;
    source.get(start..end).unwrap_or("")
}

fn collect_annotations(modifiers: &syntax_ast::Modifiers) -> Vec<Annotation> {
    let mut out = Vec::new();
    for annotation in modifiers.annotations() {
        let Some(name) = annotation.name().map(|name| name.text()) else {
            continue;
        };
        let simple = name.rsplit('.').next().unwrap_or(name.as_str()).trim();
        if simple.is_empty() {
            continue;
        }

        let range = annotation.syntax().text_range();
        let start: usize = u32::from(range.start()) as usize;
        let end: usize = u32::from(range.end()) as usize;
        out.push(Annotation::new_with_span(
            simple.to_string(),
            Span::new(start, end),
        ));
    }
    out
}

fn modifier_flags(modifiers: &syntax_ast::Modifiers) -> (bool, bool) {
    (
        modifier_contains_keyword(modifiers, SyntaxKind::StaticKw),
        modifier_contains_keyword(modifiers, SyntaxKind::FinalKw),
    )
}

fn modifier_contains_keyword(modifiers: &syntax_ast::Modifiers, kind: SyntaxKind) -> bool {
    modifiers.keywords().any(|tok| tok.kind() == kind)
}

fn parse_type(raw: &str) -> Type {
    let mut raw = raw.trim().to_string();
    if raw.is_empty() {
        return Type::Unknown;
    }

    // Drop whitespace (type nodes may include spaces in generics).
    raw.retain(|ch| !ch.is_ascii_whitespace());

    // Count array dimensions.
    let mut dims = 0usize;
    while raw.ends_with("[]") {
        dims += 1;
        raw.truncate(raw.len().saturating_sub(2));
    }

    let base = strip_generic_args(&raw);
    let mut ty = match base.as_str() {
        "void" => Type::Void,
        "boolean" => Type::Primitive(PrimitiveType::Boolean),
        "byte" => Type::Primitive(PrimitiveType::Byte),
        "short" => Type::Primitive(PrimitiveType::Short),
        "char" => Type::Primitive(PrimitiveType::Char),
        "int" => Type::Primitive(PrimitiveType::Int),
        "long" => Type::Primitive(PrimitiveType::Long),
        "float" => Type::Primitive(PrimitiveType::Float),
        "double" => Type::Primitive(PrimitiveType::Double),
        other => Type::Named(other.to_string()),
    };

    for _ in 0..dims {
        ty = Type::Array(Box::new(ty));
    }
    ty
}

fn strip_generic_args(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut depth = 0u32;
    for ch in raw.chars() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out
}
