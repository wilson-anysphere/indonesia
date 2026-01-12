//! Lightweight Lombok-backed member completion support.
//!
//! Nova's full semantic engine is still under development, but we already have a
//! `nova-framework-lombok` analyzer that can synthesize Lombok-generated virtual
//! members (getters, setters, builders, ...).
//!
//! This module wires that analyzer into `nova-ide` member completions by:
//! - building a best-effort workspace index of classes using `nova-syntax`
//! - feeding those classes into a `nova_framework::MemoryDatabase`
//! - running `nova_resolve::complete_member_names` to include virtual members
//! - caching the result per project root to avoid reparsing on every
//!   completion request.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;

use nova_db::{Database as TextDatabase, FileId};
use nova_framework::{
    AnalyzerRegistry, Database as FrameworkDatabase, MemoryDatabase, VirtualMember,
};
use nova_framework_lombok::LombokAnalyzer;
use nova_hir::framework::{Annotation, ClassData, ConstructorData, FieldData, MethodData};
use nova_syntax::ast::{self as syntax_ast, AstNode};
use nova_syntax::SyntaxKind;
use nova_types::{ClassId, Parameter, PrimitiveType, Span, Type};

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
    class_files: HashMap<ClassId, FileId>,
    inner_classes_by_name: HashMap<String, Vec<ClassId>>,
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
    let root = crate::framework_cache::project_root_for_path(path);

    let Some(intel) = workspace_intel(db, &root) else {
        return Vec::new();
    };

    let Some(receiver_ty) = resolve_receiver_type(&intel, receiver_type) else {
        return Vec::new();
    };

    // Use `nova-resolve` to combine explicit + framework-generated members.
    let names = nova_resolve::complete_member_names(&intel.db, &intel.registry, &receiver_ty);

    // Build a best-effort kind map so we can emit reasonable LSP kinds.
    let mut kind_by_name: HashMap<String, MemberKind> = HashMap::new();
    match &receiver_ty {
        Type::Class(nova_types::ClassType { def, .. }) => {
            let class_data = intel.db.class(*def);
            for field in &class_data.fields {
                kind_by_name.insert(field.name.clone(), MemberKind::Field);
            }
            for method in &class_data.methods {
                kind_by_name.insert(method.name.clone(), MemberKind::Method);
            }
            for vm in intel.registry.virtual_members_for_class(&intel.db, *def) {
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
        }
        Type::VirtualInner { owner, name } => {
            for vm in intel.registry.virtual_members_for_class(&intel.db, *owner) {
                let VirtualMember::InnerClass(inner) = vm else {
                    continue;
                };
                if inner.name != *name {
                    continue;
                }
                for member in inner.members {
                    match member {
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
                break;
            }
        }
        _ => {}
    }

    let mut seen = HashSet::<String>::new();
    let mut out = Vec::new();
    for name in names {
        if !seen.insert(name.clone()) {
            continue;
        }
        let kind = kind_by_name
            .get(&name)
            .copied()
            .unwrap_or(MemberKind::Method);
        out.push(MemberCompletion { label: name, kind });
    }
    out
}

pub(crate) fn goto_virtual_member_definition(
    db: &dyn TextDatabase,
    file: FileId,
    receiver_type: &str,
    member_name: &str,
) -> Option<(FileId, Span)> {
    let path = db.file_path(file)?;
    let root = crate::framework_cache::project_root_for_path(path);

    let intel = workspace_intel(db, &root)?;
    let receiver_ty = resolve_receiver_type(&intel, receiver_type)?;

    match receiver_ty {
        Type::Class(nova_types::ClassType { def, .. }) => {
            let span = find_virtual_member_span_in_class(&intel, def, member_name)?;
            let file = *intel.class_files.get(&def)?;
            Some((file, span))
        }
        Type::VirtualInner { owner, name } => {
            let span = find_virtual_member_span_in_inner(&intel, owner, &name, member_name)?;
            let file = *intel.class_files.get(&owner)?;
            Some((file, span))
        }
        _ => None,
    }
}

fn resolve_receiver_type(intel: &WorkspaceLombokIntel, receiver_type: &str) -> Option<Type> {
    let normalized = normalize_type_name(receiver_type);

    // Try resolving `Outer.Inner` (or `Outer$Inner`) into a Lombok virtual inner class.
    if let Some((outer_raw, inner)) = normalized
        .rsplit_once('$')
        .or_else(|| normalized.rsplit_once('.'))
    {
        let outer_simple = outer_raw.rsplit('.').next().unwrap_or(outer_raw);
        if let Some(&outer_id) = intel.classes_by_name.get(outer_simple) {
            if intel
                .inner_classes_by_name
                .get(inner)
                .is_some_and(|owners| owners.contains(&outer_id))
            {
                return Some(Type::VirtualInner {
                    owner: outer_id,
                    name: inner.to_string(),
                });
            }
        }
    }

    let simple = simplify_type_name(&normalized);
    if let Some(&class_id) = intel.classes_by_name.get(simple.as_str()) {
        return Some(Type::class(class_id, vec![]));
    }

    // Fall back to resolving an unqualified `FooBuilder` inner name when unique.
    if let Some(owners) = intel.inner_classes_by_name.get(simple.as_str()) {
        if let Some(&owner) = owners.first() {
            return Some(Type::VirtualInner {
                owner,
                name: simple,
            });
        }
    }

    None
}

fn find_virtual_member_span_in_class(
    intel: &WorkspaceLombokIntel,
    class_id: ClassId,
    member_name: &str,
) -> Option<Span> {
    for vm in intel
        .registry
        .virtual_members_for_class(&intel.db, class_id)
    {
        match vm {
            VirtualMember::Field(f) if f.name == member_name => return f.span,
            VirtualMember::Method(m) if m.name == member_name => return m.span,
            VirtualMember::InnerClass(c) if c.name == member_name => return c.span,
            _ => {}
        }
    }
    None
}

fn find_virtual_member_span_in_inner(
    intel: &WorkspaceLombokIntel,
    owner: ClassId,
    inner_name: &str,
    member_name: &str,
) -> Option<Span> {
    for vm in intel.registry.virtual_members_for_class(&intel.db, owner) {
        let VirtualMember::InnerClass(inner) = vm else {
            continue;
        };
        if inner.name != inner_name {
            continue;
        }
        return find_virtual_member_span_in_inner_members(&inner.members, member_name);
    }
    None
}

fn find_virtual_member_span_in_inner_members(
    members: &[VirtualMember],
    member_name: &str,
) -> Option<Span> {
    for member in members {
        match member {
            VirtualMember::Field(f) if f.name == member_name => return f.span,
            VirtualMember::Method(m) if m.name == member_name => return m.span,
            VirtualMember::InnerClass(c) if c.name == member_name => return c.span,
            VirtualMember::InnerClass(c) => {
                if let Some(span) =
                    find_virtual_member_span_in_inner_members(&c.members, member_name)
                {
                    return Some(span);
                }
            }
            _ => {}
        }
    }
    None
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

    let mut classes = Vec::<(ClassData, FileId)>::new();

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
            classes.extend(parsed.drain(..).map(|c| (c, file_id)));
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

    let mut registry = AnalyzerRegistry::new();
    registry.register(Box::new(LombokAnalyzer::new()));

    let mut classes_by_name: HashMap<String, ClassId> = HashMap::new();
    let mut class_files: HashMap<ClassId, FileId> = HashMap::new();
    let mut all_class_ids = Vec::new();
    for (class, file_id) in classes {
        let name = class.name.clone();
        let id = mem_db.add_class(project, class);
        all_class_ids.push(id);
        class_files.insert(id, file_id);
        classes_by_name.entry(name).or_insert(id);
    }

    let mut inner_classes_by_name: HashMap<String, Vec<ClassId>> = HashMap::new();
    for class_id in all_class_ids {
        for vm in registry.virtual_members_for_class(&mem_db, class_id) {
            let VirtualMember::InnerClass(inner) = vm else {
                continue;
            };
            inner_classes_by_name
                .entry(inner.name)
                .or_default()
                .push(class_id);
        }
    }

    Some(WorkspaceLombokIntel {
        db: mem_db,
        registry,
        classes_by_name,
        class_files,
        inner_classes_by_name,
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

    let mut classes = Vec::new();
    let parse = nova_syntax::parse_java(source);
    for node in parse.syntax().descendants() {
        let Some(class) = syntax_ast::ClassDeclaration::cast(node) else {
            continue;
        };
        if let Some(class) = parse_class_declaration(class, source, &mut saw_lombok) {
            classes.push(class);
        }
    }

    (classes, saw_lombok)
}

fn parse_class_declaration(
    node: syntax_ast::ClassDeclaration,
    source: &str,
    saw_lombok: &mut bool,
) -> Option<ClassData> {
    let modifiers = node.modifiers();
    let annotations = modifiers
        .as_ref()
        .map(|m| collect_annotations(m, saw_lombok))
        .unwrap_or_default();

    let class_name = node.name_token()?.text().to_string();

    let body = node.body()?;
    let mut fields = Vec::new();
    let mut methods = Vec::new();
    let mut constructors = Vec::new();

    for member in body.members() {
        match member {
            syntax_ast::ClassMember::FieldDeclaration(field) => {
                let mut parsed = parse_field_declaration(field, source, saw_lombok);
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

fn parse_field_declaration(
    node: syntax_ast::FieldDeclaration,
    source: &str,
    saw_lombok: &mut bool,
) -> Vec<FieldData> {
    let modifiers = node.modifiers();
    let annotations = modifiers
        .as_ref()
        .map(|m| collect_annotations(m, saw_lombok))
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
        let name = name_node.text().to_string();
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

fn parse_method_declaration(
    node: syntax_ast::MethodDeclaration,
    source: &str,
) -> Option<MethodData> {
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

fn parse_formal_parameters(
    node: Option<syntax_ast::ParameterList>,
    source: &str,
) -> Vec<Parameter> {
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

fn collect_annotations(
    modifiers: &syntax_ast::Modifiers,
    saw_lombok: &mut bool,
) -> Vec<Annotation> {
    let mut out = Vec::new();
    for annotation in modifiers.annotations() {
        let Some(name) = annotation.name().map(|name| name.text()) else {
            continue;
        };
        let simple = name.rsplit('.').next().unwrap_or(name.as_str()).trim();
        if simple.is_empty() {
            continue;
        }

        if is_lombok_annotation(simple) {
            *saw_lombok = true;
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

fn is_lombok_annotation(name: &str) -> bool {
    matches!(
        name,
        "Getter"
            | "Setter"
            | "Data"
            | "Value"
            | "Builder"
            | "SuperBuilder"
            | "NoArgsConstructor"
            | "AllArgsConstructor"
            | "RequiredArgsConstructor"
            | "ToString"
            | "EqualsAndHashCode"
            | "Slf4j"
            | "Log4j2"
    )
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

fn simplify_type_name(raw: &str) -> String {
    let raw = normalize_type_name(raw);
    let raw = raw.trim();
    raw.rsplit('.').next().unwrap_or(raw).to_string()
}

fn normalize_type_name(raw: &str) -> String {
    let raw = raw.trim();
    let raw = raw.split('<').next().unwrap_or(raw).trim();
    let raw = raw.trim_end_matches("[]").trim();
    raw.to_string()
}
