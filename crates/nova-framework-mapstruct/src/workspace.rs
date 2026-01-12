use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use nova_core::ProjectId;
use nova_framework::Database;
use nova_types::{Diagnostic, Span};
use nova_vfs::FileId;
use tree_sitter::Node;

use crate::{AnalysisResult, FileDiagnostic, JavaType, MapperModel};

#[derive(Debug, Default)]
pub(crate) struct WorkspaceCache {
    inner: Mutex<HashMap<ProjectId, CachedWorkspace>>,
}

#[derive(Clone, Debug)]
struct CachedWorkspace {
    fingerprint: u64,
    workspace: Arc<MapStructWorkspace>,
}

impl WorkspaceCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn workspace(
        &self,
        db: &dyn Database,
        project: ProjectId,
    ) -> Arc<MapStructWorkspace> {
        let fingerprint = project_fingerprint(db, project);

        {
            let cache = lock_unpoison(&self.inner);
            if let Some(entry) = cache.get(&project) {
                if entry.fingerprint == fingerprint {
                    return entry.workspace.clone();
                }
            }
        }

        let workspace = Arc::new(MapStructWorkspace::build(db, project));
        let entry = CachedWorkspace {
            fingerprint,
            workspace: workspace.clone(),
        };
        lock_unpoison(&self.inner).insert(project, entry);
        workspace
    }
}

fn lock_unpoison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}

fn project_fingerprint(db: &dyn Database, project: ProjectId) -> u64 {
    use std::collections::hash_map::DefaultHasher;

    let mut files: Vec<(PathBuf, FileId, usize, *const u8)> = Vec::new();
    for file in db.all_files(project) {
        let Some(path) = db.file_path(file) else {
            continue;
        };
        if path.extension().and_then(|e| e.to_str()) != Some("java") {
            continue;
        }
        let Some(text) = db.file_text(file) else {
            continue;
        };
        files.push((path.to_path_buf(), file, text.len(), text.as_ptr()));
    }
    files.sort_by(|(a, ..), (b, ..)| a.cmp(b));

    let mut hasher = DefaultHasher::new();
    // The workspace analysis toggles some diagnostics based on whether MapStruct is on the
    // dependency graph / classpath. Include this bit in the fingerprint so diagnostics update
    // when build metadata changes.
    crate::has_mapstruct_build_dependency(db, project).hash(&mut hasher);
    for (path, _file, len, ptr) in &files {
        path.hash(&mut hasher);
        len.hash(&mut hasher);
        ptr.hash(&mut hasher);
    }
    hasher.finish()
}

#[derive(Debug)]
pub(crate) struct MapStructWorkspace {
    pub(crate) analysis: AnalysisResult,
    type_to_file: HashMap<String, FileId>,
    property_types: Mutex<HashMap<String, Option<Arc<HashMap<String, JavaType>>>>>,
}

impl MapStructWorkspace {
    fn build(db: &dyn Database, project: ProjectId) -> Self {
        let has_mapstruct_dependency = crate::has_mapstruct_build_dependency(db, project);

        let sources = java_sources(db, project);
        let mut builder = WorkspaceBuilder::new(has_mapstruct_dependency);

        for source in sources {
            let Some(text) = db.file_text(source.file) else {
                continue;
            };

            let Ok(tree) = nova_framework_parse::parse_java(text) else {
                continue;
            };
            let root = tree.root_node();
            let package = crate::package_of_source(root, text);
            let imports = crate::imports_of_source(root, text);

            // Index local type declarations so we can resolve DTO property sets without
            // filesystem scanning.
            for ty in top_level_type_names(root, text) {
                builder.index_type(&ty, package.as_deref(), source.file);
            }

            // Discover MapStruct mappers.
            builder.mappers.extend(crate::discover_mappers_in_tree(
                &source.path,
                text,
                root,
                package.as_deref(),
                &imports,
            ));
        }

        builder.finish(db)
    }

    pub(crate) fn property_types_for_type(
        &self,
        db: &dyn Database,
        ty: &JavaType,
    ) -> Option<Arc<HashMap<String, JavaType>>> {
        let key = ty.qualified_name();
        if key.is_empty() {
            return None;
        }

        {
            let cache = lock_unpoison(&self.property_types);
            if let Some(cached) = cache.get(&key) {
                return cached.clone();
            }
        }

        let (file_id, cache_key) = match self.type_to_file.get(&key).copied() {
            Some(file) => (Some(file), key),
            None => match self.type_to_file.get(&ty.name).copied() {
                Some(file) => (Some(file), key),
                None => (None, key),
            },
        };

        let Some(file_id) = file_id else {
            lock_unpoison(&self.property_types).insert(cache_key, None);
            return None;
        };

        let Some(text) = db.file_text(file_id) else {
            lock_unpoison(&self.property_types).insert(cache_key, None);
            return None;
        };

        let Ok(tree) = nova_framework_parse::parse_java(text) else {
            lock_unpoison(&self.property_types).insert(cache_key, None);
            return None;
        };

        let root = tree.root_node();
        let package = crate::package_of_source(root, text);
        let imports = crate::imports_of_source(root, text);
        let map = crate::collect_property_types_in_class(
            root,
            text,
            &ty.name,
            package.as_deref(),
            &imports,
        );
        let map = Arc::new(map);
        lock_unpoison(&self.property_types).insert(cache_key, Some(map.clone()));
        Some(map)
    }
}

#[derive(Debug, Clone)]
struct WorkspaceSource {
    path: PathBuf,
    file: FileId,
}

fn java_sources(db: &dyn Database, project: ProjectId) -> Vec<WorkspaceSource> {
    let mut out = Vec::new();
    for file in db.all_files(project) {
        let Some(path) = db.file_path(file) else {
            continue;
        };
        if path.extension().and_then(|e| e.to_str()) != Some("java") {
            continue;
        }
        if db.file_text(file).is_none() {
            continue;
        }
        out.push(WorkspaceSource {
            path: path.to_path_buf(),
            file,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn top_level_type_names(root: Node<'_>, source: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if !matches!(
            child.kind(),
            "class_declaration"
                | "interface_declaration"
                | "record_declaration"
                | "enum_declaration"
                | "annotation_type_declaration"
        ) {
            continue;
        }
        let name_node = child
            .child_by_field_name("name")
            .or_else(|| nova_framework_parse::find_named_child(child, "identifier"));
        let Some(name_node) = name_node else {
            continue;
        };
        out.push(nova_framework_parse::node_text(source, name_node).to_string());
    }
    out
}

#[derive(Debug)]
struct WorkspaceBuilder {
    has_mapstruct_dependency: bool,
    mappers: Vec<MapperModel>,
    type_to_file: HashMap<String, FileId>,
    properties: HashMap<String, Option<Arc<HashSet<String>>>>,
}

impl WorkspaceBuilder {
    fn new(has_mapstruct_dependency: bool) -> Self {
        Self {
            has_mapstruct_dependency,
            mappers: Vec::new(),
            type_to_file: HashMap::new(),
            properties: HashMap::new(),
        }
    }

    fn index_type(&mut self, name: &str, package: Option<&str>, file: FileId) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }

        // Simple name lookup.
        self.type_to_file.entry(name.to_string()).or_insert(file);

        // Qualified name lookup.
        if let Some(pkg) = package {
            if !pkg.trim().is_empty() {
                self.type_to_file
                    .entry(format!("{pkg}.{name}"))
                    .or_insert(file);
            }
        }
    }

    fn properties_for_type(
        &mut self,
        db: &dyn Database,
        ty: &JavaType,
    ) -> Option<Arc<HashSet<String>>> {
        let key = ty.qualified_name();
        if key.is_empty() {
            return None;
        }

        if let Some(cached) = self.properties.get(&key) {
            return cached.clone();
        }

        let file_id = self
            .type_to_file
            .get(&key)
            .or_else(|| self.type_to_file.get(&ty.name))
            .copied();

        let Some(file_id) = file_id else {
            self.properties.insert(key, None);
            return None;
        };

        let Some(text) = db.file_text(file_id) else {
            self.properties.insert(key, None);
            return None;
        };
        let Ok(tree) = nova_framework_parse::parse_java(text) else {
            self.properties.insert(key, None);
            return None;
        };

        let props = crate::collect_properties_in_class(tree.root_node(), text, &ty.name);
        let props = Arc::new(props);
        self.properties.insert(key, Some(props.clone()));
        Some(props)
    }

    fn finish(mut self, db: &dyn Database) -> MapStructWorkspace {
        let mut diagnostics: Vec<FileDiagnostic> = Vec::new();
        let mappers = std::mem::take(&mut self.mappers);

        // Missing dependency diagnostic for any mapper usage.
        if !self.has_mapstruct_dependency && !mappers.is_empty() {
            for mapper in &mappers {
                diagnostics.push(FileDiagnostic {
                    file: mapper.file.clone(),
                    diagnostic: Diagnostic::error(
                        "MAPSTRUCT_MISSING_DEPENDENCY",
                        "MapStruct annotations are present but no org.mapstruct dependency was detected",
                        Some(mapper.name_span),
                    ),
                });
            }
        }

        // Ambiguous mapping methods (same source->target).
        for mapper in &mappers {
            let mut seen: HashMap<(String, String), Span> = HashMap::new();
            for method in &mapper.methods {
                let key = (
                    method.source_type.qualified_name(),
                    method.target_type.qualified_name(),
                );
                if let Some(prev) = seen.get(&key) {
                    diagnostics.push(FileDiagnostic {
                        file: mapper.file.clone(),
                        diagnostic: Diagnostic::error(
                            "MAPSTRUCT_AMBIGUOUS_MAPPING_METHOD",
                            format!(
                                "Ambiguous mapping method for {} -> {} (another candidate at {}..{})",
                                key.0, key.1, prev.start, prev.end
                            ),
                            Some(method.name_span),
                        ),
                    });
                } else {
                    seen.insert(key, method.name_span);
                }
            }
        }

        // Unmapped target properties (best-effort, workspace-scoped).
        for mapper in &mappers {
            for method in &mapper.methods {
                let Some(source_props) = self.properties_for_type(db, &method.source_type) else {
                    continue;
                };
                let Some(target_props) = self.properties_for_type(db, &method.target_type) else {
                    continue;
                };

                if target_props.is_empty() {
                    continue;
                }

                let mut mapped: HashSet<String> =
                    source_props.intersection(&target_props).cloned().collect();
                for mapping in &method.mappings {
                    let target = mapping
                        .target
                        .split('.')
                        .next()
                        .unwrap_or(&mapping.target)
                        .trim();
                    if !target.is_empty() {
                        mapped.insert(target.to_string());
                    }
                }

                let mut unmapped: Vec<String> = target_props.difference(&mapped).cloned().collect();
                unmapped.sort();
                if unmapped.is_empty() {
                    continue;
                }

                diagnostics.push(FileDiagnostic {
                    file: mapper.file.clone(),
                    diagnostic: Diagnostic::warning(
                        "MAPSTRUCT_UNMAPPED_TARGET_PROPERTIES",
                        format!(
                            "Potentially unmapped target properties for {} -> {}: {}",
                            method.source_type.qualified_name(),
                            method.target_type.qualified_name(),
                            unmapped.join(", ")
                        ),
                        Some(method.name_span),
                    ),
                });
            }
        }

        MapStructWorkspace {
            analysis: AnalysisResult {
                mappers,
                diagnostics,
            },
            type_to_file: self.type_to_file,
            property_types: Mutex::new(HashMap::new()),
        }
    }
}
