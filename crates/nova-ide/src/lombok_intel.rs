//! Lightweight Lombok-backed member completion support.
//!
//! Nova's full semantic engine is still under development, but we already have a
//! `nova-framework-lombok` analyzer that can synthesize Lombok-generated virtual
//! members (getters, setters, builders, ...).
//!
//! This module wires that analyzer into `nova-ide` member completions by:
//! - building a best-effort workspace index of classes using tree-sitter-java
//! - feeding those classes into a `nova_framework::MemoryDatabase`
//! - running `nova_resolve::complete_member_names` to include virtual members
//! - caching the result per (guessed) project root to avoid reparsing on every
//!   completion request.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;
use tree_sitter::{Node, Parser};

use nova_db::{Database as TextDatabase, FileId};
use nova_framework::{AnalyzerRegistry, Database as FrameworkDatabase, MemoryDatabase, VirtualMember};
use nova_framework_lombok::LombokAnalyzer;
use nova_hir::framework::{Annotation, ClassData, ConstructorData, FieldData, MethodData};
use nova_types::{ClassId, Parameter, PrimitiveType, Type};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemberKind {
    Field,
    Method,
    Class,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemberCompletion {
    pub label: String,
    pub kind: MemberKind,
}

struct WorkspaceLombokIntel {
    db: MemoryDatabase,
    registry: AnalyzerRegistry,
    classes_by_name: HashMap<String, ClassId>,
}

#[derive(Clone)]
struct CachedIntel {
    fingerprint: u64,
    intel: Arc<WorkspaceLombokIntel>,
}

static CACHE: Lazy<Mutex<HashMap<PathBuf, CachedIntel>>> = Lazy::new(|| Mutex::new(HashMap::new()));

pub(crate) fn complete_members(
    db: &dyn TextDatabase,
    file: FileId,
    receiver_type: &str,
) -> Vec<MemberCompletion> {
    let Some(path) = db.file_path(file) else {
        return Vec::new();
    };
    let root = guess_project_root(path);

    let Some(intel) = workspace_intel(db, &root) else {
        return Vec::new();
    };

    let receiver_type = simplify_type_name(receiver_type);
    let Some(&class_id) = intel.classes_by_name.get(receiver_type.as_str()) else {
        return Vec::new();
    };

    let receiver_ty = Type::class(class_id, vec![]);

    // Use `nova-resolve` to combine explicit + framework-generated members.
    let names = nova_resolve::complete_member_names(&intel.db, &intel.registry, &receiver_ty);

    // Build a best-effort kind map so we can emit reasonable LSP kinds.
    let mut kind_by_name: HashMap<String, MemberKind> = HashMap::new();
    let class_data = intel.db.class(class_id);
    for field in &class_data.fields {
        kind_by_name.insert(field.name.clone(), MemberKind::Field);
    }
    for method in &class_data.methods {
        kind_by_name.insert(method.name.clone(), MemberKind::Method);
    }
    for vm in intel.registry.virtual_members_for_class(&intel.db, class_id) {
        match vm {
            VirtualMember::Field(f) => {
                kind_by_name.insert(f.name, MemberKind::Field);
            }
            VirtualMember::Method(m) => {
                kind_by_name.insert(m.name, MemberKind::Method);
            }
            VirtualMember::InnerClass(c) => {
                kind_by_name.insert(c.name, MemberKind::Class);
            }
            VirtualMember::Constructor(_) => {}
        }
    }

    let mut seen = HashSet::<String>::new();
    let mut out = Vec::new();
    for name in names {
        if !seen.insert(name.clone()) {
            continue;
        }
        let kind = kind_by_name.get(&name).copied().unwrap_or(MemberKind::Method);
        out.push(MemberCompletion { label: name, kind });
    }
    out
}

fn workspace_intel(db: &dyn TextDatabase, root: &Path) -> Option<Arc<WorkspaceLombokIntel>> {
    let root = root.to_path_buf();
    let fingerprint = workspace_fingerprint(db, &root);

    {
        let guard = CACHE.lock().ok()?;
        if let Some(cached) = guard.get(&root) {
            if cached.fingerprint == fingerprint {
                return Some(cached.intel.clone());
            }
        }
    }

    let intel = Arc::new(build_workspace_intel(db, &root)?);

    let mut guard = CACHE.lock().ok()?;
    guard.insert(
        root,
        CachedIntel {
            fingerprint,
            intel: intel.clone(),
        },
    );
    Some(intel)
}

fn workspace_fingerprint(db: &dyn TextDatabase, root: &Path) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    // Use (path, content) for all files under `root`. This is intentionally
    // coarse-grained but good enough to avoid reparsing on every completion.
    for file_id in db.all_file_ids() {
        let Some(path) = db.file_path(file_id) else {
            continue;
        };
        if !path.starts_with(root) {
            continue;
        }
        path.to_string_lossy().hash(&mut hasher);
        db.file_content(file_id).hash(&mut hasher);
    }
    hasher.finish()
}

fn build_workspace_intel(db: &dyn TextDatabase, root: &Path) -> Option<WorkspaceLombokIntel> {
    let mut mem_db = MemoryDatabase::new();
    let project = mem_db.add_project();

    // Detect Lombok based on:
    // - build files present in the host DB (`pom.xml`, `build.gradle`, ...)
    // - presence of Lombok annotations/imports in sources.
    let mut enable_lombok = false;

    let mut classes = Vec::<ClassData>::new();

    for file_id in db.all_file_ids() {
        let Some(path) = db.file_path(file_id) else {
            continue;
        };
        if !path.starts_with(root) {
            continue;
        }

        let text = db.file_content(file_id);

        if path.extension().and_then(|e| e.to_str()) == Some("java") {
            let (mut parsed, saw_lombok) = extract_classes_from_source(text);
            enable_lombok |= saw_lombok;
            classes.append(&mut parsed);
            continue;
        }

        // Best-effort dependency detection (when build files are loaded into the DB).
        if is_build_file(path) && text.contains("org.projectlombok") && text.contains("lombok") {
            enable_lombok = true;
        }
    }

    if enable_lombok {
        // `LombokAnalyzer::applies_to` is dependency/classpath based, so we mark
        // the project as having Lombok when we detect it in sources.
        mem_db.add_dependency(project, "org.projectlombok", "lombok");
    }

    let mut classes_by_name: HashMap<String, ClassId> = HashMap::new();
    for class in classes {
        let name = class.name.clone();
        let id = mem_db.add_class(project, class);
        classes_by_name.entry(name).or_insert(id);
    }

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(LombokAnalyzer::new()));

    Some(WorkspaceLombokIntel {
        db: mem_db,
        registry,
        classes_by_name,
    })
}

fn is_build_file(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|n| n.to_str()),
        Some(
            "pom.xml"
                | "build.gradle"
                | "build.gradle.kts"
                | "settings.gradle"
                | "settings.gradle.kts"
        )
    )
}

fn extract_classes_from_source(source: &str) -> (Vec<ClassData>, bool) {
    // Cheap fallback for Lombok detection (import or fully-qualified reference).
    let mut saw_lombok = source.contains("lombok.");

    let Ok(tree) = parse_java(source) else {
        return (Vec::new(), saw_lombok);
    };

    let mut classes = Vec::new();
    let root = tree.root_node();
    visit_nodes(root, &mut |node| {
        if node.kind() == "class_declaration" {
            if let Some(class) = parse_class_declaration(node, source, &mut saw_lombok) {
                classes.push(class);
            }
        }
    });

    (classes, saw_lombok)
}

fn parse_class_declaration(node: Node<'_>, source: &str, saw_lombok: &mut bool) -> Option<ClassData> {
    let modifiers = modifier_node(node);
    let annotations = modifiers
        .map(|m| collect_annotations(m, source))
        .unwrap_or_default();
    if annotations.iter().any(|a| is_lombok_annotation(&a.name)) {
        *saw_lombok = true;
    }

    let name_node = node
        .child_by_field_name("name")
        .or_else(|| find_named_child(node, "identifier"))?;
    let class_name = node_text(source, name_node).to_string();

    let body = node
        .child_by_field_name("body")
        .or_else(|| find_named_child(node, "class_body"))?;

    let mut fields = Vec::new();
    let mut methods = Vec::new();
    let mut constructors = Vec::new();

    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        match child.kind() {
            "field_declaration" => {
                let mut parsed = parse_field_declaration(child, source, saw_lombok);
                fields.append(&mut parsed);
            }
            "method_declaration" => {
                if let Some(method) = parse_method_declaration(child, source) {
                    methods.push(method);
                }
            }
            "constructor_declaration" => {
                if let Some(ctor) = parse_constructor_declaration(child, source) {
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

fn parse_field_declaration(node: Node<'_>, source: &str, saw_lombok: &mut bool) -> Vec<FieldData> {
    let modifiers = modifier_node(node);
    let annotations = modifiers
        .map(|m| collect_annotations(m, source))
        .unwrap_or_default();
    if annotations.iter().any(|a| is_lombok_annotation(&a.name)) {
        *saw_lombok = true;
    }

    let (is_static, is_final) = modifiers
        .map(|m| modifier_flags(m, source))
        .unwrap_or((false, false));

    let ty_node = node
        .child_by_field_name("type")
        .or_else(|| infer_field_type_node(node));
    let ty = ty_node
        .map(|n| parse_type(node_text(source, n)))
        .unwrap_or(Type::Unknown);

    let mut out = Vec::new();
    let mut cursor = node.walk();
    for declarator in node.named_children(&mut cursor) {
        if declarator.kind() != "variable_declarator" {
            continue;
        }
        let name_node = declarator
            .child_by_field_name("name")
            .or_else(|| find_named_child(declarator, "identifier"));
        let Some(name_node) = name_node else {
            continue;
        };
        let name = node_text(source, name_node).to_string();
        out.push(FieldData {
            name,
            ty: ty.clone(),
            is_static,
            is_final,
            annotations: annotations.clone(),
        });
    }
    out
}

fn parse_method_declaration(node: Node<'_>, source: &str) -> Option<MethodData> {
    let modifiers = modifier_node(node);
    let is_static = modifiers
        .map(|m| modifier_contains_keyword(m, source, "static"))
        .unwrap_or(false);

    let name_node = node
        .child_by_field_name("name")
        .or_else(|| find_named_child(node, "identifier"))?;
    let name = node_text(source, name_node).to_string();

    let return_type_node = node
        .child_by_field_name("type")
        .or_else(|| infer_method_return_type_node(node));
    let return_type = return_type_node
        .map(|n| parse_type(node_text(source, n)))
        .unwrap_or(Type::Unknown);

    let params = node
        .child_by_field_name("parameters")
        .or_else(|| find_named_child(node, "formal_parameters"))
        .map(|n| parse_formal_parameters(n, source))
        .unwrap_or_default();

    Some(MethodData {
        name,
        return_type,
        params,
        is_static,
    })
}

fn parse_constructor_declaration(node: Node<'_>, source: &str) -> Option<ConstructorData> {
    let params = node
        .child_by_field_name("parameters")
        .or_else(|| find_named_child(node, "formal_parameters"))
        .map(|n| parse_formal_parameters(n, source))
        .unwrap_or_default();

    Some(ConstructorData { params })
}

fn parse_formal_parameters(node: Node<'_>, source: &str) -> Vec<Parameter> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "formal_parameter" {
            continue;
        }
        let name_node = child
            .child_by_field_name("name")
            .or_else(|| find_named_child(child, "identifier"));
        let Some(name_node) = name_node else {
            continue;
        };
        let name = node_text(source, name_node).to_string();

        let ty_node = child
            .child_by_field_name("type")
            .or_else(|| infer_param_type_node(child));
        let ty = ty_node
            .map(|n| parse_type(node_text(source, n)))
            .unwrap_or(Type::Unknown);

        out.push(Parameter::new(name, ty));
    }
    out
}

fn parse_java(source: &str) -> Result<tree_sitter::Tree, String> {
    let mut parser = Parser::new();
    parser
        .set_language(tree_sitter_java::language())
        .map_err(|_| "tree-sitter-java language load failed".to_string())?;
    parser
        .parse(source, None)
        .ok_or_else(|| "tree-sitter failed to produce a syntax tree".to_string())
}

fn visit_nodes<'a, F: FnMut(Node<'a>)>(node: Node<'a>, f: &mut F) {
    f(node);
    if node.child_count() == 0 {
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_nodes(child, f);
    }
}

fn node_text<'a>(source: &'a str, node: Node<'_>) -> &'a str {
    &source[node.byte_range()]
}

fn find_named_child<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    let result = node
        .named_children(&mut cursor)
        .find(|child| child.kind() == kind);
    result
}

fn modifier_node(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("modifiers")
        .or_else(|| find_named_child(node, "modifiers"))
}

fn collect_annotations(modifiers: Node<'_>, source: &str) -> Vec<Annotation> {
    let mut out = Vec::new();
    let mut cursor = modifiers.walk();
    for child in modifiers.named_children(&mut cursor) {
        if !child.kind().ends_with("annotation") {
            continue;
        }
        if let Some(name) = parse_annotation_name(node_text(source, child)) {
            out.push(Annotation::new(name));
        }
    }
    out
}

fn parse_annotation_name(text: &str) -> Option<String> {
    let text = text.trim();
    if !text.starts_with('@') {
        return None;
    }
    let rest = &text[1..];
    let name_part = rest.split_once('(').map(|(n, _)| n).unwrap_or(rest);
    let name_part = name_part.trim();
    let simple = name_part
        .rsplit('.')
        .next()
        .unwrap_or(name_part)
        .trim()
        .to_string();
    if simple.is_empty() { None } else { Some(simple) }
}

fn is_lombok_annotation(name: &str) -> bool {
    matches!(
        name,
        "Getter"
            | "Setter"
            | "Data"
            | "Value"
            | "Builder"
            | "NoArgsConstructor"
            | "AllArgsConstructor"
            | "RequiredArgsConstructor"
            | "ToString"
            | "EqualsAndHashCode"
            | "Slf4j"
            | "Log4j2"
    )
}

fn modifier_flags(modifiers: Node<'_>, source: &str) -> (bool, bool) {
    (
        modifier_contains_keyword(modifiers, source, "static"),
        modifier_contains_keyword(modifiers, source, "final"),
    )
}

fn modifier_contains_keyword(modifiers: Node<'_>, source: &str, keyword: &str) -> bool {
    node_text(source, modifiers)
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '$'))
        .any(|tok| tok == keyword)
}

fn parse_type(raw: &str) -> Type {
    let mut raw = raw.trim().to_string();
    if raw.is_empty() {
        return Type::Unknown;
    }

    // Drop whitespace (tree-sitter type nodes may include spaces in generics).
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

fn infer_field_type_node<'a>(node: Node<'a>) -> Option<Node<'a>> {
    // Field declarations are roughly: [modifiers] <type> <declarator> ...
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            k if k == "modifiers" || k.ends_with("annotation") => continue,
            "variable_declarator" => break,
            _ => return Some(child),
        }
    }
    None
}

fn infer_param_type_node<'a>(node: Node<'a>) -> Option<Node<'a>> {
    // formal_parameter: [modifiers] <type> <name>
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            k if k == "modifiers" || k.ends_with("annotation") => continue,
            "identifier" => break,
            _ => return Some(child),
        }
    }
    None
}

fn infer_method_return_type_node<'a>(node: Node<'a>) -> Option<Node<'a>> {
    // method_declaration: [modifiers] <type> <name> ...
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            k if k == "modifiers" || k.ends_with("annotation") => continue,
            "identifier" => break,
            _ => return Some(child),
        }
    }
    None
}

fn simplify_type_name(raw: &str) -> String {
    let raw = raw.trim();
    let raw = raw.split('<').next().unwrap_or(raw).trim();
    let raw = raw.trim_end_matches("[]").trim();
    raw.rsplit('.').next().unwrap_or(raw).to_string()
}

fn guess_project_root(file_path: &Path) -> PathBuf {
    // Best-effort heuristic that works well for Maven/Gradle layouts:
    // `<root>/src/...` -> `<root>`.
    //
    // If the path doesn't contain `src`, fall back to the file's parent directory.
    let dir = file_path.parent().unwrap_or(file_path);

    let mut out = PathBuf::new();
    for comp in dir.components() {
        match comp {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(comp.as_os_str()),
            Component::Normal(name) if name == "src" => break,
            Component::Normal(name) => out.push(name),
            Component::CurDir | Component::ParentDir => {}
        }
    }

    if out.as_os_str().is_empty() {
        dir.to_path_buf()
    } else {
        out
    }
}
