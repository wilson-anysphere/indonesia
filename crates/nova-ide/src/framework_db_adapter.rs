//! `nova-db` → `nova-framework` database adapter.
//!
//! Framework analyzers (`nova-framework`) expect a [`nova_framework::Database`]
//! implementation. Nova's IDE/LSP layers primarily operate on [`nova_db::Database`]
//! (a text database keyed by `FileId`).
//!
//! [`FrameworkIdeDatabase`] bridges the gap by delegating file text/path queries to
//! the underlying `nova-db` database and providing best-effort project/classpath
//! queries expected by framework analyzers.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use once_cell::sync::Lazy;

use nova_classpath::ClasspathIndex;
use nova_core::ProjectId;
use nova_db::{Database as TextDatabase, FileId};
use nova_framework::Database as FrameworkDatabase;
use nova_hir::framework::ClassData;
use nova_types::ClassId;

/// A `nova-framework` database implementation backed by an IDE `nova-db` text database.
///
/// The adapter is intentionally best-effort. When workspace/project configuration
/// is unavailable (e.g. in-memory tests with virtual paths), framework analyzers
/// should gracefully degrade.
pub struct FrameworkIdeDatabase {
    inner: Arc<dyn TextDatabase + Send + Sync>,
    project: ProjectId,
    root: OnceLock<Option<PathBuf>>,
    class_index: OnceLock<ClassIndex>,
}

impl FrameworkIdeDatabase {
    pub fn new(inner: Arc<dyn TextDatabase + Send + Sync>, project: ProjectId) -> Self {
        Self {
            inner,
            project,
            root: OnceLock::new(),
            class_index: OnceLock::new(),
        }
    }

    #[must_use]
    pub fn into_arc(self) -> Arc<Self> {
        Arc::new(self)
    }

    fn project_root(&self) -> Option<&PathBuf> {
        if let Some(root) = self.root.get() {
            return root.as_ref();
        }

        let computed = self.discover_project_root();
        let _ = self.root.set(computed);
        self.root.get().and_then(|r| r.as_ref())
    }

    fn discover_project_root(&self) -> Option<PathBuf> {
        for file_id in self.inner.all_file_ids() {
            let Some(path) = self.inner.file_path(file_id) else {
                continue;
            };
            return Some(crate::framework_cache::project_root_for_path(path));
        }
        None
    }

    fn class_index(&self) -> &ClassIndex {
        if let Some(index) = self.class_index.get() {
            return index;
        }

        // Compute outside the `OnceLock` slow path so we don't hold a lock while parsing.
        let built = ClassIndex::build(self.inner.as_ref());
        let _ = self.class_index.set(built);
        self.class_index
            .get()
            .expect("ClassIndex must be initialized after set()")
    }

    fn has_dependency_in_loaded_build_files(&self, group: &str, artifact: &str) -> bool {
        let pom_group = format!("<groupId>{group}</groupId>");
        let pom_artifact = format!("<artifactId>{artifact}</artifactId>");
        let gradle_coord = format!("{group}:{artifact}");

        for file_id in self.inner.all_file_ids() {
            let Some(path) = self.inner.file_path(file_id) else {
                continue;
            };
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let text = self.inner.file_content(file_id);

            match name {
                "pom.xml" => {
                    if text.contains(&pom_group) && text.contains(&pom_artifact) {
                        return true;
                    }
                }
                "build.gradle" | "build.gradle.kts" => {
                    if text.contains(&gradle_coord) {
                        return true;
                    }
                }
                _ => {}
            }
        }

        false
    }

    fn classpath_index(&self) -> Option<Arc<ClasspathIndex>> {
        let root = self.project_root()?;
        let config = crate::framework_cache::project_config(root)?;

        let canonical_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let fingerprint = crate::framework_cache::build_marker_fingerprint(&canonical_root);

        {
            let cache = CLASSPATH_CACHE.lock().ok()?;
            if let Some(entry) = cache.get(&canonical_root) {
                if entry.fingerprint == fingerprint {
                    return Some(Arc::clone(&entry.index));
                }
            }
        }

        // Build outside the lock (classpath indexing can be expensive).
        let built = build_classpath_index(&config)?;

        let mut cache = CLASSPATH_CACHE.lock().ok()?;
        cache.insert(
            canonical_root,
            CachedClasspathIndex {
                fingerprint,
                index: Arc::clone(&built),
            },
        );
        Some(built)
    }
}

impl TextDatabase for FrameworkIdeDatabase {
    fn file_content(&self, file_id: FileId) -> &str {
        self.inner.file_content(file_id)
    }

    fn file_path(&self, file_id: FileId) -> Option<&Path> {
        self.inner.file_path(file_id)
    }

    fn all_file_ids(&self) -> Vec<FileId> {
        self.inner.all_file_ids()
    }

    fn file_id(&self, path: &Path) -> Option<FileId> {
        self.inner.file_id(path)
    }
}

impl FrameworkDatabase for FrameworkIdeDatabase {
    fn class(&self, class: ClassId) -> &ClassData {
        static UNKNOWN: OnceLock<ClassData> = OnceLock::new();

        let index = self.class_index();
        index
            .classes
            .get(class.to_raw() as usize)
            .unwrap_or_else(|| {
                UNKNOWN.get_or_init(|| {
                    let mut data = ClassData::default();
                    data.name = "<unknown>".to_string();
                    data
                })
            })
    }

    fn project_of_class(&self, _class: ClassId) -> ProjectId {
        self.project
    }

    fn project_of_file(&self, _file: FileId) -> ProjectId {
        self.project
    }

    fn file_text(&self, file: FileId) -> Option<&str> {
        Some(self.inner.file_content(file))
    }

    fn file_path(&self, file: FileId) -> Option<&Path> {
        self.inner.file_path(file)
    }

    fn file_id(&self, path: &Path) -> Option<FileId> {
        self.inner.file_id(path)
    }

    fn all_files(&self, project: ProjectId) -> Vec<FileId> {
        if project != self.project {
            return Vec::new();
        }

        let mut files = self.inner.all_file_ids();
        if let Some(root) = self.project_root() {
            files.retain(|file_id| {
                self.inner
                    .file_path(*file_id)
                    .is_some_and(|path| path.starts_with(root))
            });
        }
        files.sort();
        files
    }

    fn all_classes(&self, project: ProjectId) -> Vec<ClassId> {
        if project != self.project {
            return Vec::new();
        }

        self.class_index()
            .classes
            .iter()
            .enumerate()
            .map(|(idx, _)| ClassId::new(idx as u32))
            .collect()
    }

    fn has_dependency(&self, project: ProjectId, group: &str, artifact: &str) -> bool {
        if project != self.project {
            return false;
        }

        if let Some(root) = self.project_root() {
            if let Some(config) = crate::framework_cache::project_config(root) {
                return config
                    .dependencies
                    .iter()
                    .any(|dep| dep.group_id == group && dep.artifact_id == artifact);
            }
        }

        self.has_dependency_in_loaded_build_files(group, artifact)
    }

    fn has_class_on_classpath(&self, project: ProjectId, binary_name: &str) -> bool {
        if project != self.project {
            return false;
        }

        let Some(index) = self.classpath_index() else {
            return false;
        };

        if index.lookup_binary(binary_name).is_some() {
            return true;
        }
        if index.lookup_internal(binary_name).is_some() {
            return true;
        }

        // Be tolerant of callers mixing Java binary names (`java.lang.String`) and
        // JVM internal names (`java/lang/String`).
        if binary_name.contains('/') {
            let alt = binary_name.replace('/', ".");
            return index.lookup_binary(&alt).is_some();
        }
        if binary_name.contains('.') {
            let alt = binary_name.replace('.', "/");
            return index.lookup_internal(&alt).is_some();
        }

        false
    }

    fn has_class_on_classpath_prefix(&self, project: ProjectId, prefix: &str) -> bool {
        if project != self.project {
            return false;
        }

        let Some(index) = self.classpath_index() else {
            return false;
        };

        !index.class_names_with_prefix(prefix).is_empty()
    }
}

#[derive(Debug)]
struct ClassIndex {
    #[allow(dead_code)]
    fingerprint: u64,
    classes: Vec<ClassData>,
}

impl ClassIndex {
    fn build(db: &dyn TextDatabase) -> Self {
        use std::collections::hash_map::DefaultHasher;

        let mut java_files = Vec::<(PathBuf, FileId)>::new();
        for file_id in db.all_file_ids() {
            let Some(path) = db.file_path(file_id) else {
                continue;
            };
            if path.extension().and_then(|e| e.to_str()) != Some("java") {
                continue;
            }
            java_files.push((path.to_path_buf(), file_id));
        }

        java_files.sort_by(|(a, _), (b, _)| a.cmp(b));

        let mut hasher = DefaultHasher::new();
        for (path, file_id) in &java_files {
            path.hash(&mut hasher);
            let text = db.file_content(*file_id);
            text.len().hash(&mut hasher);
            (text.as_ptr() as usize).hash(&mut hasher);
        }
        let fingerprint = hasher.finish();

        let mut classes = Vec::<ClassData>::new();
        for (_path, file_id) in java_files {
            let text = db.file_content(file_id);
            classes.extend(crate::framework_class_data::extract_classes_from_source(text));
        }

        Self {
            fingerprint,
            classes,
        }
    }
}

#[derive(Clone)]
struct CachedClasspathIndex {
    fingerprint: u64,
    index: Arc<ClasspathIndex>,
}

static CLASSPATH_CACHE: Lazy<Mutex<HashMap<PathBuf, CachedClasspathIndex>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

fn build_classpath_index(config: &nova_project::ProjectConfig) -> Option<Arc<ClasspathIndex>> {
    let mut entries = Vec::<nova_classpath::ClasspathEntry>::new();

    for entry in config.classpath.iter().chain(config.module_path.iter()) {
        let Some(entry) = crate::framework_cache::to_classpath_entry(entry) else {
            continue;
        };
        entries.push(entry);
    }

    for out_dir in &config.output_dirs {
        entries.push(nova_classpath::ClasspathEntry::ClassDir(
            out_dir.path.clone(),
        ));
    }

    // Indexing is best-effort; failures should not crash the IDE.
    let index = ClasspathIndex::build(&entries, None).ok()?;
    Some(Arc::new(index))
}
