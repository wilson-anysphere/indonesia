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
    parse_java, visit_nodes, ParsedAnnotation,
};
use nova_types::{ClassId, CompletionItem, Diagnostic, Span};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
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
    /// mapper source file.
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
}

impl MapStructAnalyzer {
    pub fn new() -> Self {
        Self {
            workspace: workspace::WorkspaceCache::new(),
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
        // Dependency-based detection.
        if db.has_dependency(project, "org.mapstruct", "mapstruct")
            || db.has_dependency(project, "org.mapstruct", "mapstruct-processor")
        {
            return true;
        }

        // Fallback: detect commonly-used MapStruct types on the classpath.
        db.has_class_on_classpath(project, "org.mapstruct.Mapper")
            || db.has_class_on_classpath_prefix(project, "org.mapstruct.")
            || db.has_class_on_classpath_prefix(project, "org/mapstruct/")
    }

    fn virtual_members(&self, _db: &dyn Database, _class: ClassId) -> Vec<VirtualMember> {
        Vec::new()
    }

    fn diagnostics(&self, db: &dyn Database, file: nova_vfs::FileId) -> Vec<Diagnostic> {
        let Some(path) = db.file_path(file) else {
            return Vec::new();
        };
        if path.extension().and_then(|e| e.to_str()) != Some("java") {
            return Vec::new();
        }
        if db.file_text(file).is_none() {
            return Vec::new();
        }

        let project = db.project_of_file(file);
        let workspace = self.workspace.workspace(db, project);
        workspace.diagnostics_for_file(path)
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
                mapped.insert(mapping.target.clone());
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
    let mappers = discover_mappers_in_source(file, &text)?;
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

fn mapping_property_completions(
    db: &dyn Database,
    workspace: &workspace::MapStructWorkspace,
    file_text: &str,
    offset: usize,
    value_span: Span,
    ty: &JavaType,
) -> Vec<CompletionItem> {
    let Some(props) = workspace.properties_for_type(db, ty) else {
        return Vec::new();
    };

    let cursor = offset.min(value_span.end).min(file_text.len());
    if cursor < value_span.start || value_span.start > file_text.len() {
        return Vec::new();
    }

    // Compute the current segment prefix within the string literal value.
    let before_cursor = file_text
        .get(value_span.start..cursor)
        .unwrap_or_default();
    let segment_start_rel = before_cursor.rfind('.').map(|idx| idx + 1).unwrap_or(0);
    let segment_start = value_span.start + segment_start_rel;
    let prefix = file_text.get(segment_start..cursor).unwrap_or_default();

    let replace_span = Span::new(segment_start, cursor);
    let mut items: Vec<CompletionItem> = props
        .iter()
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
    let package = package_of_source(tree.root_node(), source);

    Ok(discover_mappers_in_tree(
        file,
        source,
        tree.root_node(),
        package.as_deref(),
    ))
}

fn discover_mappers_in_tree(
    file: &Path,
    source: &str,
    root: Node<'_>,
    default_package: Option<&str>,
) -> Vec<MapperModel> {
    let mut out = Vec::new();
    visit_nodes(root, &mut |node| {
        if node.kind() == "interface_declaration" || node.kind() == "class_declaration" {
            if let Some(mapper) = parse_mapper_decl(file, source, node, default_package) {
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

fn parse_mapper_decl(
    file: &Path,
    source: &str,
    node: Node<'_>,
    default_package: Option<&str>,
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

    let methods = parse_mapper_methods(file, source, node, package.as_deref());

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
        if let Some(model) = parse_mapping_method(file, source, child, default_package) {
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
    let return_type = parse_java_type(return_type_raw, default_package);

    let params_node = node
        .child_by_field_name("parameters")
        .or_else(|| find_named_child(node, "formal_parameters"))?;
    let params = parse_formal_parameters(params_node, source, default_package);
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
        .filter(|a| a.simple_name == "Mapping")
        .filter_map(|a| parse_mapping_annotation(a))
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
        let ty = parse_java_type(raw, default_package);

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
        if node.kind() != "class_declaration" {
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

        let body = node
            .child_by_field_name("body")
            .or_else(|| find_named_child(node, "class_body"));
        let Some(body) = body else {
            return;
        };

        // Prefer field.
        if let Some(span) = find_field_name_span(body, source, property) {
            found = Some(span);
            return;
        }

        // Then setter/getter.
        let candidates = [
            format!("set{}", capitalize(property)),
            format!("get{}", capitalize(property)),
            format!("is{}", capitalize(property)),
        ];
        for name in candidates {
            if let Some(span) = find_method_name_span_in_body(body, source, &name) {
                found = Some(span);
                return;
            }
        }
    });
    found
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
        if node.kind() != "class_declaration" {
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

        let body = node
            .child_by_field_name("body")
            .or_else(|| find_named_child(node, "class_body"));
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
