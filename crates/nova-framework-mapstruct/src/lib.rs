//! MapStruct framework intelligence for Nova.
//!
//! MapStruct is a widely-used annotation processor that generates mapper
//! implementations (e.g. `CarMapperImpl`) from `@Mapper` interfaces and
//! `@Mapping` annotations.
//!
//! This crate provides best-effort IDE support:
//! - Detect `@Mapper` types and mapping methods
//! - Read `@Mapping(source=..., target=...)` configuration
//! - If generated sources are present (discovered via `nova-apt`), navigate from
//!   mapper methods into the generated implementation method
//! - Navigate from `@Mapping(target="...")` property references to the target
//!   field/getter/setter definition
//! - Emit common diagnostics (best-effort)

use nova_apt::discover_generated_source_roots;
use nova_core::ProjectId;
use nova_framework::{CompletionContext, Database, FrameworkAnalyzer, VirtualMember};
use nova_framework_parse::{
    annotation_string_value_span, clean_type, collect_annotations, find_named_child, node_text,
    parse_annotation_text, parse_java, visit_nodes, ParsedAnnotation,
};
use nova_types::{ClassId, CompletionItem, Diagnostic, Span};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};
use tree_sitter::Node;

mod workspace;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComponentModel {
    Default,
    Spring,
    Cdi,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaType {
    pub package: Option<String>,
    pub name: String,
}

impl JavaType {
    pub fn qualified_name(&self) -> String {
        match &self.package {
            Some(pkg) => format!("{pkg}.{}", self.name),
            None => self.name.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavigationTarget {
    pub file: PathBuf,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyMappingModel {
    pub source: Option<String>,
    /// Byte span of the source string literal *value* (without quotes) inside the
    /// mapper source file (if `source = "..."` is present).
    pub source_span: Option<Span>,
    pub target: String,
    /// Byte span of the target string literal *value* (without quotes) inside the
    /// mapper source file.
    pub target_span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappingMethodKind {
    Create,
    Update,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappingMethodModel {
    pub file: PathBuf,
    pub name: String,
    pub name_span: Span,
    pub kind: MappingMethodKind,
    /// Parameter types in declaration order.
    pub param_types: Vec<JavaType>,
    /// Index into `param_types` for an `@MappingTarget` parameter (if present).
    pub mapping_target_param: Option<usize>,
    pub source_type: JavaType,
    pub target_type: JavaType,
    pub mappings: Vec<PropertyMappingModel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapperModel {
    pub file: PathBuf,
    pub package: Option<String>,
    pub name: String,
    pub name_span: Span,
    pub component_model: ComponentModel,
    /// Resolved implementation class name (after applying MapStruct placeholders).
    ///
    /// MapStruct defaults this to `<CLASS_NAME>Impl`, but it can be overridden via
    /// `@Mapper(implementationName = "...")`.
    pub implementation_name: String,
    /// Resolved implementation package (after applying MapStruct placeholders).
    ///
    /// MapStruct defaults this to the mapper's own package, but it can be
    /// overridden via `@Mapper(implementationPackage = "...")`.
    pub implementation_package: Option<String>,
    pub methods: Vec<MappingMethodModel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiagnostic {
    pub file: PathBuf,
    pub diagnostic: Diagnostic,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AnalysisResult {
    pub mappers: Vec<MapperModel>,
    pub diagnostics: Vec<FileDiagnostic>,
}

/// Framework analyzer implementation (for applicability detection).
///
/// MapStruct does not currently synthesize virtual members; it primarily
/// provides diagnostics + navigation into generated sources. Those features are
/// exposed via the free functions in this crate.
pub struct MapStructAnalyzer {
    workspace: workspace::WorkspaceCache,
    fs_cache: FsWorkspaceCache,
}

impl MapStructAnalyzer {
    pub fn new() -> Self {
        Self {
            workspace: workspace::WorkspaceCache::new(),
            fs_cache: FsWorkspaceCache::new(),
        }
    }
}

impl Default for MapStructAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameworkAnalyzer for MapStructAnalyzer {
    fn applies_to(&self, db: &dyn Database, project: ProjectId) -> bool {
        // Dependency/classpath-based detection.
        if has_mapstruct_dependency(db, project) {
            return true;
        }

        // Text fallback: if we can enumerate files and read contents, look for MapStruct usage in
        // sources. This is the most precise signal available without build metadata.
        let files = db.all_files(project);
        if files
            .into_iter()
            .any(|file| db.file_text(file).is_some_and(looks_like_mapstruct_source))
        {
            return true;
        }

        // Structural fallback: if the host database exposes HIR classes, look for `@Mapper`.
        //
        // Note: IDE adapters may only expose concrete `class` declarations via `all_classes`
        // (excluding `interface` declarations). File-text scanning above is therefore important for
        // detecting `@Mapper` interfaces when annotations are not surfaced through `all_classes`.
        let classes = db.all_classes(project);
        classes.into_iter().any(|id| {
            let class = db.class(id);
            class.has_annotation("Mapper") || class.has_annotation("org.mapstruct.Mapper")
        })
    }

    fn virtual_members(&self, _db: &dyn Database, _class: ClassId) -> Vec<VirtualMember> {
        Vec::new()
    }

    fn diagnostics(&self, db: &dyn Database, file: nova_vfs::FileId) -> Vec<Diagnostic> {
        let Some(text) = db.file_text(file) else {
            return Vec::new();
        };
        let Some(path) = db.file_path(file) else {
            return Vec::new();
        };
        if path.extension().and_then(|e| e.to_str()) != Some("java") {
            return Vec::new();
        }
        if !looks_like_mapstruct_source(text) {
            return Vec::new();
        }

        let Some(root) = nova_project::workspace_root(path) else {
            return Vec::new();
        };

        let project = db.project_of_file(file);
        let has_mapstruct_dependency = has_mapstruct_build_dependency(db, project);
        match crate::diagnostics_for_file(&root, path, text, has_mapstruct_dependency) {
            Ok(diags) => diags,
            Err(_) => Vec::new(),
        }
    }

    fn navigation(
        &self,
        db: &dyn Database,
        symbol: &nova_framework::Symbol,
    ) -> Vec<nova_framework::NavigationTarget> {
        let nova_framework::Symbol::File(file) = *symbol else {
            return Vec::new();
        };

        let Some(text) = db.file_text(file) else {
            return Vec::new();
        };
        let Some(path) = db.file_path(file) else {
            return Vec::new();
        };
        if path.extension().and_then(|e| e.to_str()) != Some("java") {
            return Vec::new();
        }
        if !looks_like_mapstruct_source(text) {
            return Vec::new();
        }
        let Some(root) = nova_project::workspace_root(path) else {
            return Vec::new();
        };

        let Ok(mappers) = discover_mappers_in_source(path, text) else {
            return Vec::new();
        };

        let mut out = Vec::new();
        let mut seen = HashSet::new();

        for mapper in &mappers {
            let Some(impl_path) = generated_mapper_impl_file(&root, mapper) else {
                continue;
            };
            let Some(impl_file) = db.file_id(&impl_path) else {
                continue;
            };
            if !seen.insert(impl_file) {
                continue;
            }
            out.push(nova_framework::NavigationTarget {
                file: impl_file,
                span: None,
                label: format!("Generated {}", mapper.implementation_name),
            });
        }

        out
    }

    fn completions(&self, db: &dyn Database, ctx: &CompletionContext) -> Vec<CompletionItem> {
        let Some(path) = db.file_path(ctx.file) else {
            return Vec::new();
        };
        if path.extension().and_then(|e| e.to_str()) != Some("java") {
            return Vec::new();
        }
        let Some(text) = db.file_text(ctx.file) else {
            return Vec::new();
        };
        if ctx.offset > text.len() {
            return Vec::new();
        }
        if !looks_like_mapstruct_source(text) {
            return Vec::new();
        }

        let workspace = self.workspace.workspace(db, ctx.project);
        let mut items = Vec::new();

        // Find the `@Mapping(...)` string literal value under the cursor, then offer
        // property name completions for the corresponding source/target type.
        for mapper in workspace
            .analysis
            .mappers
            .iter()
            .filter(|m| m.file.as_path() == path)
        {
            for method in &mapper.methods {
                for mapping in &method.mappings {
                    if span_contains_inclusive(mapping.target_span, ctx.offset) {
                        items.extend(mapping_property_completions(
                            db,
                            workspace.as_ref(),
                            text,
                            ctx.offset,
                            mapping.target_span,
                            &method.target_type,
                        ));
                        if !items.is_empty() {
                            return items;
                        }
                    }

                    if let Some(span) = mapping.source_span {
                        if span_contains_inclusive(span, ctx.offset) {
                            items.extend(mapping_property_completions(
                                db,
                                workspace.as_ref(),
                                text,
                                ctx.offset,
                                span,
                                &method.source_type,
                            ));
                            if !items.is_empty() {
                                return items;
                            }
                        }
                    }
                }
            }
        }

        items
    }
}

#[derive(Debug, Default)]
struct FsWorkspaceCache {
    inner: Mutex<HashMap<PathBuf, CachedFsWorkspace>>,
}

#[derive(Clone, Debug)]
struct CachedFsWorkspace {
    fingerprint: u64,
    analysis: Arc<AnalysisResult>,
}

impl FsWorkspaceCache {
    fn new() -> Self {
        Self::default()
    }

    fn analysis_for_root(
        &self,
        root: &Path,
        has_mapstruct_dependency: bool,
    ) -> Option<Arc<AnalysisResult>> {
        let key = root.to_path_buf();
        let fingerprint = fs_cache_fingerprint(root, has_mapstruct_dependency);

        {
            let cache = lock_unpoison(&self.inner);
            if let Some(entry) = cache.get(&key) {
                if entry.fingerprint == fingerprint {
                    return Some(entry.analysis.clone());
                }
            }
        }

        let analysis = analyze_workspace(root, has_mapstruct_dependency).ok()?;
        let analysis = Arc::new(analysis);
        let entry = CachedFsWorkspace {
            fingerprint,
            analysis: analysis.clone(),
        };

        lock_unpoison(&self.inner).insert(key, entry);
        Some(analysis)
    }
}

fn has_mapstruct_dependency(db: &dyn Database, project: ProjectId) -> bool {
    db.has_dependency(project, "org.mapstruct", "mapstruct")
        || db.has_dependency(project, "org.mapstruct", "mapstruct-processor")
        || db.has_class_on_classpath(project, "org.mapstruct.Mapper")
        || db.has_class_on_classpath_prefix(project, "org.mapstruct.")
        || db.has_class_on_classpath_prefix(project, "org/mapstruct/")
}

fn has_mapstruct_build_dependency(db: &dyn Database, project: ProjectId) -> bool {
    db.has_dependency(project, "org.mapstruct", "mapstruct")
        || db.has_dependency(project, "org.mapstruct", "mapstruct-processor")
}

fn fs_cache_fingerprint(root: &Path, has_mapstruct_dependency: bool) -> u64 {
    use std::collections::hash_map::DefaultHasher;

    let mut hasher = DefaultHasher::new();
    build_marker_fingerprint(root).hash(&mut hasher);
    has_mapstruct_dependency.hash(&mut hasher);
    hasher.finish()
}

fn lock_unpoison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}

fn build_marker_fingerprint(root: &Path) -> u64 {
    use std::collections::hash_map::DefaultHasher;

    // Mirror the marker set used by `nova-ide`'s framework cache. The goal here is not perfect
    // invalidation (that would require hashing every source file), but a cheap signal that build
    // context likely changed (dependencies, source roots, etc.).
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
        marker.hash(&mut hasher);
        let path = root.join(marker);
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

    // Include any `.bazelrc.*` fragments at the workspace root.
    let mut bazelrc_fragments = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if file_name.starts_with(".bazelrc.") {
                bazelrc_fragments.push(path);
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

/// Analyze a workspace directory (best-effort).
///
/// `has_mapstruct_dependency` should be set based on build metadata (Maven/Gradle).
/// When false, this function will emit a `MAPSTRUCT_MISSING_DEPENDENCY` error if
/// `@Mapper` usage is detected.
pub fn analyze_workspace(
    project_root: &Path,
    has_mapstruct_dependency: bool,
) -> std::io::Result<AnalysisResult> {
    let roots = source_roots(project_root);
    let mut java_files = Vec::new();
    for root in &roots {
        java_files.extend(collect_java_files(root)?);
    }
    java_files.sort();
    java_files.dedup();

    let mut result = AnalysisResult::default();

    for file in &java_files {
        let text = std::fs::read_to_string(file)?;
        let mappers = discover_mappers_in_source(file, &text)?;
        result.mappers.extend(mappers);
    }

    if !has_mapstruct_dependency && !result.mappers.is_empty() {
        for mapper in &result.mappers {
            result.diagnostics.push(FileDiagnostic {
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
    for mapper in &result.mappers {
        let mut seen: HashMap<(String, String), Span> = HashMap::new();
        for method in &mapper.methods {
            let key = (
                method.source_type.qualified_name(),
                method.target_type.qualified_name(),
            );
            if let Some(prev) = seen.get(&key) {
                result.diagnostics.push(FileDiagnostic {
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

    // Unmapped target properties (best-effort, file-system based).
    for mapper in &result.mappers {
        for method in &mapper.methods {
            let Some(source_props) = properties_for_type(project_root, &roots, &method.source_type)
                .ok()
                .flatten()
            else {
                continue;
            };
            let Some(target_props) = properties_for_type(project_root, &roots, &method.target_type)
                .ok()
                .flatten()
            else {
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

            result.diagnostics.push(FileDiagnostic {
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

    Ok(result)
}

/// Compute MapStruct diagnostics for a single file using the provided in-memory source text.
///
/// This is a best-effort helper intended for IDE usage where `source` may contain
/// unsaved edits. It runs the same diagnostics logic as [`analyze_workspace`], but
/// only for mapper(s) defined in `file`.
///
/// `has_mapstruct_dependency` should be set based on build metadata (Maven/Gradle).
/// When false, this function will emit a `MAPSTRUCT_MISSING_DEPENDENCY` error if
/// `@Mapper` usage is detected in this file.
pub fn diagnostics_for_file(
    project_root: &Path,
    file: &Path,
    source: &str,
    has_mapstruct_dependency: bool,
) -> std::io::Result<Vec<Diagnostic>> {
    let mappers = discover_mappers_in_source(file, source)?;
    if mappers.is_empty() {
        return Ok(Vec::new());
    }

    let mut diagnostics = Vec::new();

    if !has_mapstruct_dependency {
        for mapper in &mappers {
            diagnostics.push(Diagnostic::error(
                "MAPSTRUCT_MISSING_DEPENDENCY",
                "MapStruct annotations are present but no org.mapstruct dependency was detected",
                Some(mapper.name_span),
            ));
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
                diagnostics.push(Diagnostic::error(
                    "MAPSTRUCT_AMBIGUOUS_MAPPING_METHOD",
                    format!(
                        "Ambiguous mapping method for {} -> {} (another candidate at {}..{})",
                        key.0, key.1, prev.start, prev.end
                    ),
                    Some(method.name_span),
                ));
            } else {
                seen.insert(key, method.name_span);
            }
        }
    }

    // Unmapped target properties (best-effort, file-system based).
    let roots = source_roots(project_root);
    for mapper in &mappers {
        for method in &mapper.methods {
            let Some(source_props) = properties_for_type(project_root, &roots, &method.source_type)
                .ok()
                .flatten()
            else {
                continue;
            };
            let Some(target_props) = properties_for_type(project_root, &roots, &method.target_type)
                .ok()
                .flatten()
            else {
                continue;
            };

            if target_props.is_empty() {
                continue;
            }

            let mut mapped: HashSet<String> =
                source_props.intersection(&target_props).cloned().collect();
            for mapping in &method.mappings {
                // MapStruct targets can refer to nested paths (`foo.bar`). For unmapped property
                // diagnostics we only care about the top-level target property name (`foo`).
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

            diagnostics.push(Diagnostic::warning(
                "MAPSTRUCT_UNMAPPED_TARGET_PROPERTIES",
                format!(
                    "Potentially unmapped target properties for {} -> {}: {}",
                    method.source_type.qualified_name(),
                    method.target_type.qualified_name(),
                    unmapped.join(", ")
                ),
                Some(method.name_span),
            ));
        }
    }

    Ok(diagnostics)
}

/// File-system based wrapper for [`diagnostics_for_file`].
pub fn diagnostics_for_file_fs(
    project_root: &Path,
    file: &Path,
    has_mapstruct_dependency: bool,
) -> std::io::Result<Vec<Diagnostic>> {
    let text = std::fs::read_to_string(file)?;
    diagnostics_for_file(project_root, file, &text, has_mapstruct_dependency)
}

/// Go-to-definition support for MapStruct.
///
/// This function is intentionally best-effort and only handles the two most
/// common navigation flows:
/// - mapper method name -> generated implementation method (if present)
/// - `@Mapping(target="...")` value -> target field/getter/setter
pub fn goto_definition(
    project_root: &Path,
    file: &Path,
    offset: usize,
) -> std::io::Result<Vec<NavigationTarget>> {
    let text = std::fs::read_to_string(file)?;
    goto_definition_in_source(project_root, file, &text, offset)
}

/// Go-to-definition support for MapStruct using an in-memory source text snapshot.
///
/// Behavior matches [`goto_definition`], but parses MapStruct constructs from
/// `source` rather than reading `file` from disk. It may still read *other* files
/// from disk as needed (generated sources, target type definitions, etc).
pub fn goto_definition_in_source(
    project_root: &Path,
    file: &Path,
    source: &str,
    offset: usize,
) -> std::io::Result<Vec<NavigationTarget>> {
    let mappers = discover_mappers_in_source(file, source)?;
    if mappers.is_empty() {
        return Ok(Vec::new());
    }

    // 1) Mapper method -> generated method.
    for mapper in &mappers {
        for method in &mapper.methods {
            if span_contains(method.name_span, offset) {
                if let Some(target) = goto_generated_method(project_root, mapper, method)? {
                    return Ok(vec![target]);
                }
                return Ok(Vec::new());
            }
        }
    }

    // 2) @Mapping(target="...") -> target property definition.
    let roots = source_roots(project_root);
    for mapper in &mappers {
        for method in &mapper.methods {
            for mapping in &method.mappings {
                if span_contains(mapping.target_span, offset) {
                    if let Some(target) =
                        goto_target_property(project_root, &roots, mapper, method, mapping)?
                    {
                        return Ok(vec![target]);
                    }
                    return Ok(Vec::new());
                }
            }
        }
    }

    Ok(Vec::new())
}

fn span_contains(span: Span, offset: usize) -> bool {
    span.start <= offset && offset < span.end
}

fn span_contains_inclusive(span: Span, offset: usize) -> bool {
    span.start <= offset && offset <= span.end
}

fn looks_like_mapstruct_source(text: &str) -> bool {
    // Best-effort, cheap guard to avoid building the full workspace model when the
    // file clearly isn't participating in MapStruct.
    //
    // We intentionally keep this conservative (false negatives are acceptable) and bias toward
    // avoiding false positives for other frameworks that also use `@Mapper` (e.g. MyBatis).
    text.contains("org.mapstruct")
}

fn mapping_property_completions(
    db: &dyn Database,
    workspace: &workspace::MapStructWorkspace,
    file_text: &str,
    offset: usize,
    value_span: Span,
    ty: &JavaType,
) -> Vec<CompletionItem> {
    let cursor = offset.min(value_span.end).min(file_text.len());
    if cursor < value_span.start || value_span.start > file_text.len() {
        return Vec::new();
    }

    // Compute the current segment prefix within the string literal value.
    let before_cursor = file_text.get(value_span.start..cursor).unwrap_or_default();
    let segment_start_rel = before_cursor.rfind('.').map(|idx| idx + 1).unwrap_or(0);
    let segment_start = value_span.start + segment_start_rel;
    let prefix = file_text.get(segment_start..cursor).unwrap_or_default();

    // Resolve the type for nested property paths (`foo.bar.<cursor>`).
    let resolved_ty = if segment_start_rel > 0 {
        let path = before_cursor
            .get(..segment_start_rel.saturating_sub(1))
            .unwrap_or_default();
        resolve_property_path_type(db, workspace, ty, path).unwrap_or_else(|| ty.clone())
    } else {
        ty.clone()
    };

    let Some(prop_types) = workspace.property_types_for_type(db, &resolved_ty) else {
        return Vec::new();
    };

    let replace_span = Span::new(segment_start, cursor);
    let mut items: Vec<CompletionItem> = prop_types
        .keys()
        .filter(|name| name.starts_with(prefix))
        .map(|name| CompletionItem {
            label: name.clone(),
            detail: None,
            replace_span: Some(replace_span),
        })
        .collect();
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

fn resolve_property_path_type(
    db: &dyn Database,
    workspace: &workspace::MapStructWorkspace,
    root: &JavaType,
    path: &str,
) -> Option<JavaType> {
    let mut current = root.clone();
    if path.trim().is_empty() {
        return Some(current);
    }

    for seg in path.split('.') {
        let seg = seg.trim();
        if seg.is_empty() {
            return None;
        }
        let map = workspace.property_types_for_type(db, &current)?;
        let next = map.get(seg)?.clone();
        current = next;
    }

    Some(current)
}

fn source_roots(project_root: &Path) -> Vec<PathBuf> {
    let candidates = ["src/main/java", "src/test/java", "src"];
    let mut roots = candidates
        .into_iter()
        .map(|rel| project_root.join(rel))
        .filter(|p| p.is_dir())
        .collect::<Vec<_>>();
    if roots.is_empty() {
        roots.push(project_root.to_path_buf());
    }
    roots
}

fn collect_java_files(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_java_files_inner(root, &mut out)?;
    Ok(out)
}

fn collect_java_files_inner(root: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if !root.exists() {
        return Ok(());
    }
    if root.is_file() {
        if root.extension().and_then(|e| e.to_str()) == Some("java") {
            out.push(root.to_path_buf());
        }
        return Ok(());
    }

    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            // Avoid walking build output roots while scanning sources.
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if matches!(
                name,
                "target" | "build" | "out" | ".git" | ".gradle" | ".idea"
            ) {
                continue;
            }
            collect_java_files_inner(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("java") {
            out.push(path);
        }
    }
    Ok(())
}

fn discover_mappers_in_source(
    file: &Path,
    source: &str,
) -> Result<Vec<MapperModel>, std::io::Error> {
    let tree =
        parse_java(source).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let root = tree.root_node();
    let package = package_of_source(root, source);
    let imports = imports_of_source(root, source);

    Ok(discover_mappers_in_tree(
        file,
        source,
        root,
        package.as_deref(),
        &imports,
    ))
}

fn discover_mappers_in_tree(
    file: &Path,
    source: &str,
    root: Node<'_>,
    default_package: Option<&str>,
    imports: &JavaImports,
) -> Vec<MapperModel> {
    let mut out = Vec::new();
    visit_nodes(root, &mut |node| {
        if node.kind() == "interface_declaration" || node.kind() == "class_declaration" {
            if let Some(mapper) = parse_mapper_decl(file, source, node, default_package, imports) {
                out.push(mapper);
            }
        }
    });
    out
}

fn package_of_source(root: Node<'_>, source: &str) -> Option<String> {
    let mut package = None;
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() == "package_declaration" {
            let name_node = child
                .child_by_field_name("name")
                .or_else(|| find_named_child(child, "scoped_identifier"))
                .or_else(|| find_named_child(child, "identifier"));
            if let Some(name_node) = name_node {
                package = Some(node_text(source, name_node).trim().to_string());
            }
            break;
        }
    }
    package
}

#[derive(Debug, Default, Clone)]
struct JavaImports {
    /// Explicit (non-wildcard) imports mapping simple name -> package.
    explicit: HashMap<String, String>,
}

fn imports_of_source(root: Node<'_>, source: &str) -> JavaImports {
    let mut out = JavaImports::default();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "import_declaration" {
            continue;
        }

        let name_node = child
            .child_by_field_name("name")
            .or_else(|| find_named_child(child, "scoped_identifier"))
            .or_else(|| find_named_child(child, "identifier"));
        let Some(name_node) = name_node else {
            continue;
        };

        let raw = node_text(source, name_node).trim();
        if raw.ends_with(".*") {
            // Wildcard import; we don't have enough information here to resolve a
            // specific type name.
            continue;
        }

        let Some((pkg, name)) = raw.rsplit_once('.') else {
            continue;
        };
        if pkg.is_empty() || name.is_empty() {
            continue;
        }
        out.explicit.insert(name.to_string(), pkg.to_string());
    }
    out
}

fn parse_mapper_decl(
    file: &Path,
    source: &str,
    node: Node<'_>,
    default_package: Option<&str>,
    imports: &JavaImports,
) -> Option<MapperModel> {
    let modifiers = node
        .child_by_field_name("modifiers")
        .or_else(|| find_named_child(node, "modifiers"));
    let annotations = modifiers
        .map(|m| collect_annotations(m, source))
        .unwrap_or_default();
    let mapper_annotation = annotations.iter().find(|a| a.simple_name == "Mapper")?;

    let name_node = node
        .child_by_field_name("name")
        .or_else(|| find_named_child(node, "identifier"))?;
    let name = node_text(source, name_node).to_string();
    let name_span = Span::new(name_node.start_byte(), name_node.end_byte());

    let package = default_package.map(str::to_string);
    let component_model = mapper_annotation
        .args
        .get("componentModel")
        .map(String::as_str)
        .map(parse_component_model)
        .unwrap_or(ComponentModel::Default);

    let implementation_name = mapper_annotation
        .args
        .get("implementationName")
        .map(String::as_str)
        .unwrap_or("<CLASS_NAME>Impl")
        .replace("<CLASS_NAME>", &name);

    let implementation_package = mapper_annotation
        .args
        .get("implementationPackage")
        .map(String::as_str)
        .unwrap_or("<PACKAGE_NAME>");

    let implementation_package =
        apply_package_name_placeholder(implementation_package, package.as_deref());

    let methods = parse_mapper_methods(file, source, node, package.as_deref(), imports);

    Some(MapperModel {
        file: file.to_path_buf(),
        package,
        name,
        name_span,
        component_model,
        implementation_name,
        implementation_package,
        methods,
    })
}

fn parse_component_model(raw: &str) -> ComponentModel {
    // MapStruct allows passing either a literal string ("spring") or one of the
    // `MappingConstants.ComponentModel.*` constants.
    let normalized = raw
        .trim()
        .rsplit('.')
        .next()
        .unwrap_or(raw)
        .trim()
        .to_lowercase();

    match normalized.as_str() {
        "spring" => ComponentModel::Spring,
        "cdi" => ComponentModel::Cdi,
        "default" => ComponentModel::Default,
        other => ComponentModel::Other(other.to_string()),
    }
}

fn apply_package_name_placeholder(pattern: &str, mapper_package: Option<&str>) -> Option<String> {
    let mapper_package = mapper_package.unwrap_or("");
    let mut pkg = pattern.replace("<PACKAGE_NAME>", mapper_package);
    if pkg.starts_with('.') {
        pkg = pkg.trim_start_matches('.').to_string();
    }
    if pkg.ends_with('.') {
        pkg = pkg.trim_end_matches('.').to_string();
    }
    if pkg.is_empty() {
        None
    } else {
        Some(pkg)
    }
}

fn parse_mapper_methods(
    file: &Path,
    source: &str,
    decl: Node<'_>,
    default_package: Option<&str>,
    imports: &JavaImports,
) -> Vec<MappingMethodModel> {
    let body = decl
        .child_by_field_name("body")
        .or_else(|| find_named_child(decl, "interface_body"))
        .or_else(|| find_named_child(decl, "class_body"));
    let Some(body) = body else {
        return Vec::new();
    };

    let mut methods = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() != "method_declaration" {
            continue;
        }
        if let Some(model) = parse_mapping_method(file, source, child, default_package, imports) {
            methods.push(model);
        }
    }
    methods
}

fn parse_mapping_method(
    file: &Path,
    source: &str,
    node: Node<'_>,
    default_package: Option<&str>,
    imports: &JavaImports,
) -> Option<MappingMethodModel> {
    let name_node = node
        .child_by_field_name("name")
        .or_else(|| find_named_child(node, "identifier"))?;
    let name = node_text(source, name_node).to_string();
    let name_span = Span::new(name_node.start_byte(), name_node.end_byte());

    let return_node = node
        .child_by_field_name("type")
        .or_else(|| infer_type_node(node))?;
    let return_type_raw = node_text(source, return_node);
    let return_type = parse_java_type_with_imports(return_type_raw, default_package, imports);

    let params_node = node
        .child_by_field_name("parameters")
        .or_else(|| find_named_child(node, "formal_parameters"))?;
    let params = parse_formal_parameters(params_node, source, default_package, imports);
    let param_types: Vec<JavaType> = params.iter().map(|p| p.ty.clone()).collect();

    let mapping_target_params: Vec<usize> = params
        .iter()
        .enumerate()
        .filter_map(|(idx, param)| {
            if param.is_mapping_target {
                Some(idx)
            } else {
                None
            }
        })
        .collect();
    let source_params: Vec<usize> = params
        .iter()
        .enumerate()
        .filter_map(|(idx, param)| {
            if !param.is_mapping_target && !param.is_context {
                Some(idx)
            } else {
                None
            }
        })
        .collect();

    let (kind, mapping_target_param, source_type, target_type) = if return_type.name == "void" {
        // Update mapping method: `void map(Source src, @MappingTarget Target dst)`
        if mapping_target_params.len() != 1 || source_params.len() != 1 {
            return None;
        }
        let target_idx = mapping_target_params[0];
        let source_idx = source_params[0];

        (
            MappingMethodKind::Update,
            Some(target_idx),
            params[source_idx].ty.clone(),
            params[target_idx].ty.clone(),
        )
    } else {
        // Create mapping method: `Target map(Source src)`
        if !mapping_target_params.is_empty() || source_params.len() != 1 || param_types.len() != 1 {
            return None;
        }
        let source_idx = source_params[0];
        (
            MappingMethodKind::Create,
            None,
            params[source_idx].ty.clone(),
            return_type,
        )
    };

    let modifiers = node
        .child_by_field_name("modifiers")
        .or_else(|| find_named_child(node, "modifiers"));
    let annotations = modifiers
        .map(|m| collect_annotations(m, source))
        .unwrap_or_default();

    let mappings = annotations
        .iter()
        .flat_map(|a| {
            if a.simple_name == "Mapping" {
                return parse_mapping_annotation(a).into_iter().collect::<Vec<_>>();
            }
            if a.simple_name == "Mappings" {
                return parse_mappings_container_annotation(a);
            }
            Vec::new()
        })
        .collect();

    Some(MappingMethodModel {
        file: file.to_path_buf(),
        name,
        name_span,
        kind,
        param_types,
        mapping_target_param,
        source_type,
        target_type,
        mappings,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FormalParameterModel {
    ty: JavaType,
    is_mapping_target: bool,
    is_context: bool,
}

fn parse_formal_parameters(
    params: Node<'_>,
    source: &str,
    default_package: Option<&str>,
    imports: &JavaImports,
) -> Vec<FormalParameterModel> {
    let mut out = Vec::new();
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        if child.kind() != "formal_parameter" {
            continue;
        }

        let Some(ty_node) = child
            .child_by_field_name("type")
            .or_else(|| infer_type_node(child))
        else {
            continue;
        };
        let raw = node_text(source, ty_node);
        let ty = parse_java_type_with_imports(raw, default_package, imports);

        let modifiers = child
            .child_by_field_name("modifiers")
            .or_else(|| find_named_child(child, "modifiers"));
        let annotations = modifiers
            .map(|m| collect_annotations(m, source))
            .unwrap_or_default();

        let is_mapping_target = annotations.iter().any(|a| a.simple_name == "MappingTarget");
        let is_context = annotations.iter().any(|a| a.simple_name == "Context");

        out.push(FormalParameterModel {
            ty,
            is_mapping_target,
            is_context,
        });
    }
    out
}

fn parse_mapping_annotation(annotation: &ParsedAnnotation) -> Option<PropertyMappingModel> {
    let (target, target_span) = annotation_string_value_span(annotation, "target")?;
    let (source, source_span) = match annotation_string_value_span(annotation, "source") {
        Some((value, span)) => (Some(value), Some(span)),
        None => (annotation.args.get("source").cloned(), None),
    };
    Some(PropertyMappingModel {
        source,
        source_span,
        target,
        target_span,
    })
}

fn parse_mappings_container_annotation(annotation: &ParsedAnnotation) -> Vec<PropertyMappingModel> {
    nested_annotations_named(annotation, "Mapping")
        .into_iter()
        .filter_map(|ann| parse_mapping_annotation(&ann))
        .collect()
}

fn nested_annotations_named(
    annotation: &ParsedAnnotation,
    simple_name: &str,
) -> Vec<ParsedAnnotation> {
    let Some(haystack) = annotation.text.as_deref() else {
        return Vec::new();
    };

    let bytes = haystack.as_bytes();
    let mut out = Vec::new();

    let mut i = 0usize;
    let mut in_string = false;
    let mut in_char = false;
    let mut escape = false;

    while i < bytes.len() {
        let b = bytes[i];

        if in_string || in_char {
            if escape {
                escape = false;
                i += 1;
                continue;
            }
            if b == b'\\' {
                escape = true;
                i += 1;
                continue;
            }
            if in_string && b == b'"' {
                in_string = false;
            } else if in_char && b == b'\'' {
                in_char = false;
            }
            i += 1;
            continue;
        }

        if b == b'"' {
            in_string = true;
            i += 1;
            continue;
        }
        if b == b'\'' {
            in_char = true;
            i += 1;
            continue;
        }

        if b != b'@' {
            i += 1;
            continue;
        }

        let start = i;
        i += 1;

        let mut end = i;
        while end < bytes.len() {
            let ch = bytes[end] as char;
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' || ch == '.' {
                end += 1;
            } else {
                break;
            }
        }
        if end == i {
            i = end;
            continue;
        }

        let name = &haystack[i..end];
        let simple = name.rsplit('.').next().unwrap_or(name);
        if simple != simple_name {
            i = end;
            continue;
        }

        // Skip whitespace after the name.
        let mut j = end;
        while j < bytes.len() && (bytes[j] as char).is_ascii_whitespace() {
            j += 1;
        }

        let mut ann_end = j;
        if j < bytes.len() && bytes[j] == b'(' {
            if let Some(close) = find_matching_paren(haystack, j) {
                ann_end = close + 1;
            } else {
                ann_end = bytes.len();
            }
        }

        let snippet = &haystack[start..ann_end];
        let span = Span::new(
            annotation.span.start + start,
            annotation.span.start + ann_end,
        );
        if let Some(parsed) = parse_annotation_text(snippet, span) {
            out.push(parsed);
        }

        i = ann_end;
    }

    out
}

fn find_matching_paren(haystack: &str, open_idx: usize) -> Option<usize> {
    if open_idx >= haystack.len() || !haystack[open_idx..].starts_with('(') {
        return None;
    }

    let mut depth: u32 = 0;
    let mut in_string = false;
    let mut in_char = false;
    let mut escape = false;

    for (rel, ch) in haystack[open_idx..].char_indices() {
        let idx = open_idx + rel;

        if in_string || in_char {
            if escape {
                escape = false;
                continue;
            }
            if ch == '\\' {
                escape = true;
                continue;
            }
            if in_string && ch == '"' {
                in_string = false;
            } else if in_char && ch == '\'' {
                in_char = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '\'' => in_char = true,
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }

    None
}

/// Completion support for MapStruct `@Mapping(source="...")` / `@Mapping(target="...")`.
///
/// This is intentionally best-effort and relies on filesystem-based type discovery, consistent
/// with the non-workspace-scoped MapStruct helpers in this crate (e.g. `goto_definition`).
///
/// Returns an empty list when:
/// - the file does not look like a MapStruct mapper,
/// - the cursor is not within a supported string literal context,
/// - type/property discovery fails.
pub fn completions_for_file(
    project_root: &Path,
    file: &Path,
    source: &str,
    offset: usize,
) -> std::io::Result<Vec<CompletionItem>> {
    if offset > source.len() {
        return Ok(Vec::new());
    }
    if !looks_like_mapstruct_source(source) {
        return Ok(Vec::new());
    }

    let mappers = match discover_mappers_in_source(file, source) {
        Ok(m) => m,
        Err(_) => return Ok(Vec::new()),
    };
    if mappers.is_empty() {
        return Ok(Vec::new());
    }

    let roots = source_roots(project_root);

    for mapper in &mappers {
        for method in &mapper.methods {
            for mapping in &method.mappings {
                if span_contains_inclusive(mapping.target_span, offset) {
                    return Ok(mapping_property_completions_fs(
                        project_root,
                        &roots,
                        source,
                        offset,
                        mapping.target_span,
                        &method.target_type,
                    ));
                }

                if let Some(span) = mapping.source_span {
                    if span_contains_inclusive(span, offset) {
                        return Ok(mapping_property_completions_fs(
                            project_root,
                            &roots,
                            source,
                            offset,
                            span,
                            &method.source_type,
                        ));
                    }
                }
            }
        }
    }

    Ok(Vec::new())
}

fn mapping_property_completions_fs(
    project_root: &Path,
    roots: &[PathBuf],
    file_text: &str,
    offset: usize,
    value_span: Span,
    ty: &JavaType,
) -> Vec<CompletionItem> {
    let cursor = offset.min(value_span.end).min(file_text.len());
    if cursor < value_span.start || value_span.start > file_text.len() {
        return Vec::new();
    }

    let before_cursor = file_text.get(value_span.start..cursor).unwrap_or_default();
    let segment_start_rel = before_cursor.rfind('.').map(|idx| idx + 1).unwrap_or(0);
    let segment_start = value_span.start + segment_start_rel;
    let prefix = file_text.get(segment_start..cursor).unwrap_or_default();

    let Some(props) = properties_for_type(project_root, roots, ty).ok().flatten() else {
        return Vec::new();
    };

    let replace_span = Span::new(segment_start, cursor);
    let mut items: Vec<CompletionItem> = props
        .into_iter()
        .filter(|name| name.starts_with(prefix))
        .map(|name| CompletionItem {
            label: name,
            detail: None,
            replace_span: Some(replace_span),
        })
        .collect();
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

fn parse_formal_parameter_types(
    params: Node<'_>,
    source: &str,
    default_package: Option<&str>,
) -> Vec<JavaType> {
    let mut out = Vec::new();
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        if child.kind() != "formal_parameter" {
            continue;
        }
        let Some(ty_node) = child
            .child_by_field_name("type")
            .or_else(|| infer_type_node(child))
        else {
            continue;
        };
        let raw = node_text(source, ty_node);
        out.push(parse_java_type(raw, default_package));
    }
    out
}

fn parse_java_type(raw: &str, default_package: Option<&str>) -> JavaType {
    let raw = raw.trim();
    if raw.is_empty() {
        return JavaType {
            package: default_package.map(str::to_string),
            name: String::new(),
        };
    }

    let compact = clean_type(raw);
    let no_generics = compact.split('<').next().unwrap_or(&compact);
    let no_array = no_generics.trim_end_matches("[]");

    let (pkg, name) = match no_array.rsplit_once('.') {
        Some((pkg, name)) => (Some(pkg.to_string()), name.to_string()),
        None => (default_package.map(str::to_string), no_array.to_string()),
    };

    JavaType { package: pkg, name }
}

fn parse_java_type_with_imports(
    raw: &str,
    default_package: Option<&str>,
    imports: &JavaImports,
) -> JavaType {
    let raw = raw.trim();
    if raw.is_empty() {
        return JavaType {
            package: default_package.map(str::to_string),
            name: String::new(),
        };
    }

    let compact = clean_type(raw);
    let no_generics = compact.split('<').next().unwrap_or(&compact);
    let no_array = no_generics.trim_end_matches("[]");

    // Qualified type already includes its package.
    if let Some((pkg, name)) = no_array.rsplit_once('.') {
        return JavaType {
            package: Some(pkg.to_string()),
            name: name.to_string(),
        };
    }

    let name = no_array.to_string();
    let pkg = imports
        .explicit
        .get(&name)
        .cloned()
        .or_else(|| default_package.map(str::to_string));

    JavaType { package: pkg, name }
}

fn infer_type_node<'a>(node: Node<'a>) -> Option<Node<'a>> {
    // Best-effort: find the first named child that looks like a type and isn't a modifier/name/params.
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "modifiers" | "identifier" | "formal_parameters" | "parameters" | "type_parameters"
            | "block" => continue,
            _ => {
                if child.kind().contains("type") {
                    return Some(child);
                }
            }
        }
    }
    None
}

fn generated_mapper_impl_file(project_root: &Path, mapper: &MapperModel) -> Option<PathBuf> {
    let impl_name = mapper.implementation_name.as_str();
    let package_path = mapper
        .implementation_package
        .as_deref()
        .unwrap_or("")
        .replace('.', "/");
    let rel_path = if package_path.is_empty() {
        format!("{impl_name}.java")
    } else {
        format!("{package_path}/{impl_name}.java")
    };

    for root in discover_generated_source_roots(project_root) {
        let candidate = root.join(&rel_path);
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    // Fallback: scan generated roots for a file named `<MapperName>Impl.java` (or
    // custom implementation name if configured).
    for root in discover_generated_source_roots(project_root) {
        let Ok(files) = collect_java_files(&root) else {
            continue;
        };
        for file in files {
            if file
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s == impl_name)
            {
                return Some(file);
            }
        }
    }

    None
}

fn goto_generated_method(
    project_root: &Path,
    mapper: &MapperModel,
    method: &MappingMethodModel,
) -> std::io::Result<Option<NavigationTarget>> {
    let impl_name = mapper.implementation_name.as_str();
    let package_path = mapper
        .implementation_package
        .as_deref()
        .unwrap_or("")
        .replace('.', "/");
    let rel_path = if package_path.is_empty() {
        format!("{impl_name}.java")
    } else {
        format!("{package_path}/{impl_name}.java")
    };

    for root in discover_generated_source_roots(project_root) {
        let candidate = root.join(&rel_path);
        if candidate.is_file() {
            if let Some(span) = find_generated_method_span_in_file(&candidate, method)? {
                return Ok(Some(NavigationTarget {
                    file: candidate,
                    span,
                }));
            }
        }
    }

    // Fallback: scan generated roots for a file named `<MapperName>Impl.java` (or
    // custom implementation name if configured).
    for root in discover_generated_source_roots(project_root) {
        for file in collect_java_files(&root)? {
            if file
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s == impl_name)
            {
                if let Some(span) = find_generated_method_span_in_file(&file, method)? {
                    return Ok(Some(NavigationTarget { file, span }));
                }
            }
        }
    }

    Ok(None)
}

fn goto_target_property(
    project_root: &Path,
    roots: &[PathBuf],
    mapper: &MapperModel,
    method: &MappingMethodModel,
    mapping: &PropertyMappingModel,
) -> std::io::Result<Option<NavigationTarget>> {
    let target_pkg = method
        .target_type
        .package
        .as_deref()
        .or(mapper.package.as_deref());
    let target_ty = JavaType {
        package: target_pkg.map(str::to_string),
        name: method.target_type.name.clone(),
    };

    let Some(target_file) = find_type_file(project_root, roots, &target_ty)? else {
        return Ok(None);
    };

    let target_text = std::fs::read_to_string(&target_file)?;
    let tree = parse_java(&target_text)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    let Some(span) = find_property_definition_span(
        tree.root_node(),
        &target_text,
        &target_ty.name,
        &mapping.target,
    ) else {
        return Ok(None);
    };

    Ok(Some(NavigationTarget {
        file: target_file,
        span,
    }))
}

fn find_type_file(
    project_root: &Path,
    roots: &[PathBuf],
    ty: &JavaType,
) -> std::io::Result<Option<PathBuf>> {
    let rel_path = match &ty.package {
        Some(pkg) if !pkg.is_empty() => format!("{}/{}.java", pkg.replace('.', "/"), ty.name),
        _ => format!("{}.java", ty.name),
    };

    for root in roots {
        let candidate = root.join(&rel_path);
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
    }

    // Fallback: brute force search within source roots.
    for root in roots {
        for file in collect_java_files(root)? {
            if file
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s == ty.name)
            {
                return Ok(Some(file));
            }
        }
    }

    let _ = project_root;
    Ok(None)
}

fn find_generated_method_span_in_file(
    path: &Path,
    method: &MappingMethodModel,
) -> std::io::Result<Option<Span>> {
    let text = std::fs::read_to_string(path)?;
    let tree =
        parse_java(&text).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let default_package = package_of_source(tree.root_node(), &text);

    let mut first_by_name = None;
    let mut exact_match = None;
    visit_nodes(tree.root_node(), &mut |node| {
        if exact_match.is_some() {
            return;
        }
        if node.kind() != "method_declaration" {
            return;
        }
        let name_node = node
            .child_by_field_name("name")
            .or_else(|| find_named_child(node, "identifier"));
        let Some(name_node) = name_node else {
            return;
        };
        if node_text(&text, name_node) != method.name {
            return;
        }

        let name_span = Span::new(name_node.start_byte(), name_node.end_byte());
        if first_by_name.is_none() {
            first_by_name = Some(name_span);
        }

        let Some(return_node) = node
            .child_by_field_name("type")
            .or_else(|| infer_type_node(node))
        else {
            return;
        };
        let Some(params_node) = node
            .child_by_field_name("parameters")
            .or_else(|| find_named_child(node, "formal_parameters"))
        else {
            return;
        };

        let return_type =
            parse_java_type(node_text(&text, return_node), default_package.as_deref());
        let param_types =
            parse_formal_parameter_types(params_node, &text, default_package.as_deref());

        if signature_matches(method, &return_type, &param_types) {
            exact_match = Some(name_span);
        }
    });

    Ok(exact_match.or(first_by_name))
}

fn signature_matches(
    method: &MappingMethodModel,
    return_type: &JavaType,
    param_types: &[JavaType],
) -> bool {
    if param_types.len() != method.param_types.len() {
        return false;
    }

    let return_ok = match method.kind {
        MappingMethodKind::Create => return_type.name == method.target_type.name,
        MappingMethodKind::Update => return_type.name == "void",
    };

    if !return_ok {
        return false;
    }

    method
        .param_types
        .iter()
        .zip(param_types.iter())
        .all(|(a, b)| a.name == b.name)
}

fn find_property_definition_span(
    root: Node<'_>,
    source: &str,
    class_name: &str,
    property: &str,
) -> Option<Span> {
    let mut found = None;
    visit_nodes(root, &mut |node| {
        if found.is_some() {
            return;
        }
        let decl_kind = node.kind();
        if !matches!(
            decl_kind,
            "class_declaration" | "interface_declaration" | "record_declaration"
        ) {
            return;
        }
        let name_node = node
            .child_by_field_name("name")
            .or_else(|| find_named_child(node, "identifier"));
        let Some(name_node) = name_node else {
            return;
        };
        if node_text(source, name_node) != class_name {
            return;
        }

        // Record components live in the header parameter list.
        if decl_kind == "record_declaration" {
            if let Some(params) = node
                .child_by_field_name("parameters")
                .or_else(|| find_named_child(node, "formal_parameters"))
            {
                if let Some(span) = find_formal_parameter_name_span(params, source, property) {
                    found = Some(span);
                    return;
                }
            }
        }

        let body = node
            .child_by_field_name("body")
            .or_else(|| match decl_kind {
                "interface_declaration" => find_named_child(node, "interface_body"),
                _ => find_named_child(node, "class_body")
                    .or_else(|| find_named_child(node, "record_body")),
            });
        let Some(body) = body else {
            return;
        };

        // Prefer field.
        if let Some(span) = find_field_name_span(body, source, property) {
            found = Some(span);
            return;
        }

        // Then setter/getter.
        let mut candidates = vec![
            format!("set{}", capitalize(property)),
            format!("get{}", capitalize(property)),
            format!("is{}", capitalize(property)),
        ];
        // Record accessors use the component name directly (`seatCount()`).
        if decl_kind == "record_declaration" {
            candidates.push(property.to_string());
        }
        for name in candidates {
            if let Some(span) = find_method_name_span_in_body(body, source, &name) {
                found = Some(span);
                return;
            }
        }
    });
    found
}

fn find_formal_parameter_name_span(params: Node<'_>, source: &str, name: &str) -> Option<Span> {
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        if child.kind() != "formal_parameter" {
            continue;
        }
        let name_node = child
            .child_by_field_name("name")
            .or_else(|| find_named_child(child, "identifier"))?;
        if node_text(source, name_node) == name {
            return Some(Span::new(name_node.start_byte(), name_node.end_byte()));
        }
    }
    None
}

fn find_field_name_span(body: Node<'_>, source: &str, field_name: &str) -> Option<Span> {
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() != "field_declaration" {
            continue;
        }

        let mut decl_cursor = child.walk();
        for declarator in child.named_children(&mut decl_cursor) {
            if declarator.kind() != "variable_declarator" {
                continue;
            }
            let name_node = declarator.child_by_field_name("name").or_else(|| {
                declarator
                    .named_children(&mut declarator.walk())
                    .find(|n| n.kind() == "identifier")
            });
            let Some(name_node) = name_node else {
                continue;
            };
            if node_text(source, name_node) == field_name {
                return Some(Span::new(name_node.start_byte(), name_node.end_byte()));
            }
        }
    }
    None
}

fn find_method_name_span_in_body(body: Node<'_>, source: &str, method_name: &str) -> Option<Span> {
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() != "method_declaration" {
            continue;
        }
        let name_node = child
            .child_by_field_name("name")
            .or_else(|| find_named_child(child, "identifier"))?;
        if node_text(source, name_node) == method_name {
            return Some(Span::new(name_node.start_byte(), name_node.end_byte()));
        }
    }
    None
}

fn properties_for_type(
    project_root: &Path,
    roots: &[PathBuf],
    ty: &JavaType,
) -> std::io::Result<Option<HashSet<String>>> {
    let Some(file) = find_type_file(project_root, roots, ty)? else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(&file)?;
    let tree =
        parse_java(&text).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    Ok(Some(collect_properties_in_class(
        tree.root_node(),
        &text,
        &ty.name,
    )))
}

fn collect_properties_in_class(root: Node<'_>, source: &str, class_name: &str) -> HashSet<String> {
    let mut props = HashSet::new();
    visit_nodes(root, &mut |node| {
        let decl_kind = node.kind();
        if !matches!(
            decl_kind,
            "class_declaration" | "interface_declaration" | "record_declaration"
        ) {
            return;
        }
        let name_node = node
            .child_by_field_name("name")
            .or_else(|| find_named_child(node, "identifier"));
        let Some(name_node) = name_node else {
            return;
        };
        if node_text(source, name_node) != class_name {
            return;
        }

        // Record components (header params) are also properties.
        if decl_kind == "record_declaration" {
            if let Some(params) = node
                .child_by_field_name("parameters")
                .or_else(|| find_named_child(node, "formal_parameters"))
            {
                let mut cursor = params.walk();
                for child in params.named_children(&mut cursor) {
                    if child.kind() != "formal_parameter" {
                        continue;
                    }
                    let Some(name_node) = child
                        .child_by_field_name("name")
                        .or_else(|| find_named_child(child, "identifier"))
                    else {
                        continue;
                    };
                    props.insert(node_text(source, name_node).to_string());
                }
            }
        }

        let body = node
            .child_by_field_name("body")
            .or_else(|| match decl_kind {
                "interface_declaration" => find_named_child(node, "interface_body"),
                _ => find_named_child(node, "class_body")
                    .or_else(|| find_named_child(node, "record_body")),
            });
        let Some(body) = body else {
            return;
        };

        // Fields.
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            if child.kind() == "field_declaration" {
                let mut decl_cursor = child.walk();
                for declarator in child.named_children(&mut decl_cursor) {
                    if declarator.kind() != "variable_declarator" {
                        continue;
                    }
                    let name_node = declarator.child_by_field_name("name").or_else(|| {
                        declarator
                            .named_children(&mut declarator.walk())
                            .find(|n| n.kind() == "identifier")
                    });
                    if let Some(name_node) = name_node {
                        props.insert(node_text(source, name_node).to_string());
                    }
                }
            } else if child.kind() == "method_declaration" {
                let name_node = child
                    .child_by_field_name("name")
                    .or_else(|| find_named_child(child, "identifier"));
                let Some(name_node) = name_node else {
                    continue;
                };
                if let Some(prop) = property_name_from_accessor(node_text(source, name_node)) {
                    props.insert(prop);
                }
            }
        }
    });
    props
}

fn collect_property_types_in_class(
    root: Node<'_>,
    source: &str,
    class_name: &str,
    default_package: Option<&str>,
    imports: &JavaImports,
) -> HashMap<String, JavaType> {
    let mut props: HashMap<String, JavaType> = HashMap::new();
    visit_nodes(root, &mut |node| {
        let decl_kind = node.kind();
        if !matches!(
            decl_kind,
            "class_declaration" | "interface_declaration" | "record_declaration"
        ) {
            return;
        }
        let name_node = node
            .child_by_field_name("name")
            .or_else(|| find_named_child(node, "identifier"));
        let Some(name_node) = name_node else {
            return;
        };
        if node_text(source, name_node) != class_name {
            return;
        }

        // Record components (header params) provide property types.
        if decl_kind == "record_declaration" {
            if let Some(params) = node
                .child_by_field_name("parameters")
                .or_else(|| find_named_child(node, "formal_parameters"))
            {
                let mut cursor = params.walk();
                for child in params.named_children(&mut cursor) {
                    if child.kind() != "formal_parameter" {
                        continue;
                    }
                    let Some(ty_node) = child
                        .child_by_field_name("type")
                        .or_else(|| infer_type_node(child))
                    else {
                        continue;
                    };
                    let Some(name_node) = child
                        .child_by_field_name("name")
                        .or_else(|| find_named_child(child, "identifier"))
                    else {
                        continue;
                    };
                    let name = node_text(source, name_node).to_string();
                    if name.is_empty() {
                        continue;
                    }
                    let ty = parse_java_type_with_imports(
                        node_text(source, ty_node),
                        default_package,
                        imports,
                    );
                    props.entry(name).or_insert(ty);
                }
            }
        }

        let body = node
            .child_by_field_name("body")
            .or_else(|| match decl_kind {
                "interface_declaration" => find_named_child(node, "interface_body"),
                _ => find_named_child(node, "class_body")
                    .or_else(|| find_named_child(node, "record_body")),
            });
        let Some(body) = body else {
            return;
        };

        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            match child.kind() {
                "field_declaration" => {
                    let Some(ty_node) = child
                        .child_by_field_name("type")
                        .or_else(|| infer_type_node(child))
                    else {
                        continue;
                    };
                    let ty = parse_java_type_with_imports(
                        node_text(source, ty_node),
                        default_package,
                        imports,
                    );

                    let mut decl_cursor = child.walk();
                    for declarator in child.named_children(&mut decl_cursor) {
                        if declarator.kind() != "variable_declarator" {
                            continue;
                        }
                        let name_node = declarator.child_by_field_name("name").or_else(|| {
                            declarator
                                .named_children(&mut declarator.walk())
                                .find(|n| n.kind() == "identifier")
                        });
                        let Some(name_node) = name_node else {
                            continue;
                        };
                        let name = node_text(source, name_node).to_string();
                        if name.is_empty() {
                            continue;
                        }
                        props.entry(name).or_insert_with(|| ty.clone());
                    }
                }
                "method_declaration" => {
                    let name_node = child
                        .child_by_field_name("name")
                        .or_else(|| find_named_child(child, "identifier"));
                    let Some(name_node) = name_node else {
                        continue;
                    };
                    let method_name = node_text(source, name_node);

                    // Getter / boolean accessor.
                    if let Some(prop) = property_name_from_accessor(method_name) {
                        if let Some(return_node) = child
                            .child_by_field_name("type")
                            .or_else(|| infer_type_node(child))
                        {
                            let return_ty = parse_java_type_with_imports(
                                node_text(source, return_node),
                                default_package,
                                imports,
                            );
                            props.entry(prop).or_insert(return_ty);
                        }
                        continue;
                    }

                    // Setter: infer property type from first parameter.
                    if let Some(rest) = method_name.strip_prefix("set") {
                        if rest.is_empty() {
                            continue;
                        }
                        let prop = decapitalize(rest);
                        let params_node = child
                            .child_by_field_name("parameters")
                            .or_else(|| find_named_child(child, "formal_parameters"));
                        let Some(params_node) = params_node else {
                            continue;
                        };
                        let params =
                            parse_formal_parameters(params_node, source, default_package, imports);
                        if let Some(first) = params.first() {
                            props.entry(prop).or_insert(first.ty.clone());
                        }
                    }
                }
                _ => {}
            }
        }
    });
    props
}

fn property_name_from_accessor(name: &str) -> Option<String> {
    if let Some(rest) = name.strip_prefix("get") {
        if rest.is_empty() {
            return None;
        }
        return Some(decapitalize(rest));
    }
    if let Some(rest) = name.strip_prefix("set") {
        if rest.is_empty() {
            return None;
        }
        return Some(decapitalize(rest));
    }
    if let Some(rest) = name.strip_prefix("is") {
        if rest.is_empty() {
            return None;
        }
        return Some(decapitalize(rest));
    }
    None
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

fn decapitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_lowercase().collect::<String>() + chars.as_str(),
    }
}
