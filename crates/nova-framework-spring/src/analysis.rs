use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use nova_framework_parse::{
    collect_annotations, find_named_child, modifier_node, node_text, parse_java, simplify_type,
    visit_nodes, ParsedAnnotation,
};
use nova_types::{Diagnostic, Span};
use tree_sitter::Node;

pub const SPRING_NO_BEAN: &str = "SPRING_NO_BEAN";
pub const SPRING_AMBIGUOUS_BEAN: &str = "SPRING_AMBIGUOUS_BEAN";
pub const SPRING_CIRCULAR_DEP: &str = "SPRING_CIRCULAR_DEP";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SourceSpan {
    pub source: usize,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceDiagnostic {
    pub source: usize,
    pub diagnostic: Diagnostic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BeanKind {
    Component,
    BeanMethod,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bean {
    pub name: String,
    pub ty: String,
    pub qualifiers: Vec<String>,
    pub location: SourceSpan,
    pub kind: BeanKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InjectionKind {
    Field,
    ConstructorParam,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InjectionPoint {
    pub kind: InjectionKind,
    pub owner_class: String,
    pub ty: String,
    pub qualifier: Option<String>,
    pub location: SourceSpan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NavigationTarget {
    pub label: String,
    pub location: SourceSpan,
}

#[derive(Clone, Debug)]
pub struct BeanModel {
    pub beans: Vec<Bean>,
    pub injections: Vec<InjectionPoint>,
    /// `injections[i]` -> candidate bean indices.
    pub injection_candidates: Vec<Vec<usize>>,
    /// `beans[i]` -> injection indices that could resolve to this bean.
    pub bean_usages: Vec<Vec<usize>>,
}

impl BeanModel {
    #[must_use]
    pub fn navigation_from_injection(&self, injection: usize) -> Vec<NavigationTarget> {
        self.injection_candidates
            .get(injection)
            .into_iter()
            .flatten()
            .filter_map(|&bean_idx| self.beans.get(bean_idx))
            .map(|bean| NavigationTarget {
                label: format!("Bean: {}", bean.name),
                location: bean.location,
            })
            .collect()
    }

    #[must_use]
    pub fn navigation_from_bean(&self, bean: usize) -> Vec<NavigationTarget> {
        self.bean_usages
            .get(bean)
            .into_iter()
            .flatten()
            .filter_map(|&inj_idx| self.injections.get(inj_idx))
            .map(|inj| NavigationTarget {
                label: format!("Injected into {}", inj.owner_class),
                location: inj.location,
            })
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct AnalysisResult {
    pub model: BeanModel,
    pub diagnostics: Vec<SourceDiagnostic>,
}

#[derive(Clone, Debug, Default)]
struct ClassInfo {
    super_class: Option<String>,
    interfaces: Vec<String>,
}

#[derive(Clone, Debug, Default)]
struct ClassHierarchy {
    classes: HashMap<String, ClassInfo>,
}

impl ClassHierarchy {
    fn insert(&mut self, class: impl Into<String>, info: ClassInfo) {
        self.classes.insert(class.into(), info);
    }

    fn is_assignable(&self, from: &str, to: &str) -> bool {
        if from == to {
            return true;
        }

        let mut queue = VecDeque::<String>::new();
        queue.push_back(from.to_string());

        let mut visited = HashSet::<String>::new();
        while let Some(current) = queue.pop_front() {
            if !visited.insert(current.clone()) {
                continue;
            }
            let Some(info) = self.classes.get(&current) else {
                continue;
            };
            if let Some(super_class) = info.super_class.as_deref() {
                if super_class == to {
                    return true;
                }
                queue.push_back(super_class.to_string());
            }
            for iface in &info.interfaces {
                if iface == to {
                    return true;
                }
                queue.push_back(iface.clone());
            }
        }

        false
    }
}

/// Analyze a set of Java sources for Spring beans and autowiring issues.
pub fn analyze_java_sources(sources: &[&str]) -> AnalysisResult {
    let mut beans = Vec::<Bean>::new();
    let mut injections = Vec::<InjectionPoint>::new();
    let mut hierarchy = ClassHierarchy::default();

    // Parse sources and discover beans/injections.
    for (source_idx, src) in sources.iter().enumerate() {
        let Ok(tree) = parse_java(src) else {
            continue;
        };
        let root = tree.root_node();
        visit_nodes(root, &mut |node| {
            if node.kind() == "class_declaration" {
                parse_class_declaration(
                    node,
                    source_idx,
                    src,
                    &mut beans,
                    &mut injections,
                    &mut hierarchy,
                );
            }
        });
    }

    // Resolve injection candidates.
    let mut injection_candidates = Vec::with_capacity(injections.len());
    for injection in &injections {
        let mut candidates: Vec<usize> = beans
            .iter()
            .enumerate()
            .filter(|(_, bean)| hierarchy.is_assignable(&bean.ty, &injection.ty))
            .map(|(idx, _)| idx)
            .collect();

        if let Some(qualifier) = injection.qualifier.as_deref() {
            candidates.retain(|&idx| {
                let bean = &beans[idx];
                bean.name == qualifier || bean.qualifiers.iter().any(|q| q == qualifier)
            });
        }

        injection_candidates.push(candidates);
    }

    // Back-link usages (for navigation).
    let mut bean_usages: Vec<Vec<usize>> = vec![Vec::new(); beans.len()];
    for (inj_idx, cands) in injection_candidates.iter().enumerate() {
        for &bean_idx in cands {
            if let Some(list) = bean_usages.get_mut(bean_idx) {
                list.push(inj_idx);
            }
        }
    }

    let model = BeanModel {
        beans,
        injections,
        injection_candidates,
        bean_usages,
    };

    let diagnostics = diagnostics(&model);

    AnalysisResult { model, diagnostics }
}

fn diagnostics(model: &BeanModel) -> Vec<SourceDiagnostic> {
    let mut out = Vec::new();

    // Injection diagnostics.
    for (inj_idx, injection) in model.injections.iter().enumerate() {
        let candidates = &model.injection_candidates[inj_idx];
        match candidates.len() {
            0 => out.push(SourceDiagnostic {
                source: injection.location.source,
                diagnostic: Diagnostic::error(
                    SPRING_NO_BEAN,
                    format!(
                        "No Spring bean of type `{}` found for injection",
                        injection.ty
                    ),
                    Some(injection.location.span),
                ),
            }),
            1 => {}
            _ => {
                if injection.qualifier.is_none() {
                    let names = candidates
                        .iter()
                        .filter_map(|idx| model.beans.get(*idx).map(|b| b.name.as_str()))
                        .collect::<Vec<_>>()
                        .join(", ");
                    out.push(SourceDiagnostic {
                        source: injection.location.source,
                        diagnostic: Diagnostic::error(
                            SPRING_AMBIGUOUS_BEAN,
                            format!(
                                "Multiple Spring beans of type `{}` found ({names}); use @Qualifier to disambiguate",
                                injection.ty
                            ),
                            Some(injection.location.span),
                        ),
                    });
                }
            }
        }
    }

    // Circular dependency diagnostics.
    out.extend(circular_dependency_diagnostics(model));
    out
}

fn circular_dependency_diagnostics(model: &BeanModel) -> Vec<SourceDiagnostic> {
    // Only consider component beans for dependency roots (field/constructor injection).
    let mut bean_by_class: HashMap<&str, usize> = HashMap::new();
    for (idx, bean) in model.beans.iter().enumerate() {
        if bean.kind == BeanKind::Component {
            bean_by_class.insert(bean.ty.as_str(), idx);
        }
    }

    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); model.beans.len()];
    for (inj_idx, injection) in model.injections.iter().enumerate() {
        let Some(&from) = bean_by_class.get(injection.owner_class.as_str()) else {
            continue;
        };
        let candidates = &model.injection_candidates[inj_idx];
        if candidates.len() == 1 {
            edges[from].push(candidates[0]);
        }
    }

    let cycles = find_cycles(&edges);
    if cycles.is_empty() {
        return Vec::new();
    }

    let mut diags = Vec::new();
    for cycle in cycles {
        let names = cycle
            .iter()
            .filter_map(|&idx| model.beans.get(idx).map(|b| b.name.as_str()))
            .collect::<Vec<_>>()
            .join(" -> ");

        for &bean_idx in &cycle {
            let Some(bean) = model.beans.get(bean_idx) else {
                continue;
            };
            diags.push(SourceDiagnostic {
                source: bean.location.source,
                diagnostic: Diagnostic::warning(
                    SPRING_CIRCULAR_DEP,
                    format!("Circular Spring dependency detected: {names}"),
                    Some(bean.location.span),
                ),
            });
        }
    }

    diags
}

fn find_cycles(edges: &[Vec<usize>]) -> Vec<Vec<usize>> {
    fn dfs(
        node: usize,
        edges: &[Vec<usize>],
        stack: &mut Vec<usize>,
        on_stack: &mut Vec<bool>,
        visited: &mut Vec<bool>,
        out: &mut Vec<Vec<usize>>,
    ) {
        visited[node] = true;
        on_stack[node] = true;
        stack.push(node);

        for &next in &edges[node] {
            if !visited[next] {
                dfs(next, edges, stack, on_stack, visited, out);
            } else if on_stack[next] {
                if let Some(pos) = stack.iter().position(|&n| n == next) {
                    out.push(stack[pos..].to_vec());
                }
            }
        }

        stack.pop();
        on_stack[node] = false;
    }

    let mut visited = vec![false; edges.len()];
    let mut on_stack = vec![false; edges.len()];
    let mut stack = Vec::new();
    let mut cycles = Vec::new();

    for node in 0..edges.len() {
        if !visited[node] {
            dfs(
                node,
                edges,
                &mut stack,
                &mut on_stack,
                &mut visited,
                &mut cycles,
            );
        }
    }

    // Normalize + deduplicate cycles by rotating to the smallest element index.
    let mut normalized = BTreeSet::<Vec<usize>>::new();
    for mut cycle in cycles {
        if cycle.is_empty() {
            continue;
        }
        if let Some((min_pos, _)) = cycle.iter().enumerate().min_by_key(|(_, v)| *v) {
            cycle.rotate_left(min_pos);
        }
        normalized.insert(cycle);
    }

    normalized.into_iter().collect()
}

fn parse_class_declaration(
    node: Node<'_>,
    source_idx: usize,
    source: &str,
    beans: &mut Vec<Bean>,
    injections: &mut Vec<InjectionPoint>,
    hierarchy: &mut ClassHierarchy,
) {
    let annotations = modifier_node(node)
        .map(|m| collect_annotations(m, source))
        .unwrap_or_default();

    let name_node = node
        .child_by_field_name("name")
        .or_else(|| find_named_child(node, "identifier"));
    let Some(name_node) = name_node else {
        return;
    };
    let class_name = node_text(source, name_node).to_string();
    let class_span = Span::new(name_node.start_byte(), name_node.end_byte());

    let body = node
        .child_by_field_name("body")
        .or_else(|| find_named_child(node, "class_body"));
    let Some(body) = body else {
        return;
    };

    let (super_class, interfaces) =
        parse_supertypes_from_header(&source[node.start_byte()..body.start_byte()]);
    hierarchy.insert(
        class_name.clone(),
        ClassInfo {
            super_class,
            interfaces,
        },
    );

    if let Some(bean) = parse_component_bean(&annotations, source_idx, class_span, &class_name) {
        beans.push(bean);
    }

    let is_configuration = annotations.iter().any(|a| a.simple_name == "Configuration");

    parse_class_body(
        body,
        source_idx,
        source,
        &class_name,
        is_configuration,
        beans,
        injections,
    );
}

fn parse_component_bean(
    annotations: &[ParsedAnnotation],
    source_idx: usize,
    class_span: Span,
    class_name: &str,
) -> Option<Bean> {
    const STEREOTYPES: &[&str] = &["Component", "Service", "Repository", "Controller"];
    let stereotype = annotations
        .iter()
        .find(|a| STEREOTYPES.contains(&a.simple_name.as_str()))?;

    let explicit_name = stereotype
        .args
        .get("value")
        .or_else(|| stereotype.args.get("name"))
        .cloned()
        .filter(|s| !s.is_empty());

    let name = explicit_name.unwrap_or_else(|| lower_camel_case(class_name));

    let qualifiers = annotations
        .iter()
        .filter(|a| a.simple_name == "Qualifier")
        .filter_map(|a| {
            a.args
                .get("value")
                .cloned()
                .or_else(|| a.args.get("name").cloned())
        })
        .filter(|q| !q.is_empty())
        .collect::<Vec<_>>();

    Some(Bean {
        name,
        ty: class_name.to_string(),
        qualifiers,
        location: SourceSpan {
            source: source_idx,
            span: class_span,
        },
        kind: BeanKind::Component,
    })
}

fn parse_class_body(
    body: Node<'_>,
    source_idx: usize,
    source: &str,
    class_name: &str,
    is_configuration: bool,
    beans: &mut Vec<Bean>,
    injections: &mut Vec<InjectionPoint>,
) {
    // Collect constructors first for the single-constructor heuristic.
    let mut constructors = Vec::<ConstructorData>::new();

    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        match child.kind() {
            "field_declaration" => {
                parse_field_injections(child, source_idx, source, class_name, injections)
            }
            "constructor_declaration" => {
                constructors.push(parse_constructor(child, source_idx, source, class_name))
            }
            "method_declaration" if is_configuration => {
                if let Some(bean) = parse_bean_method(child, source_idx, source) {
                    beans.push(bean);
                }
            }
            _ => {}
        }
    }

    parse_constructor_injections(constructors, injections);
}

fn parse_field_injections(
    node: Node<'_>,
    source_idx: usize,
    source: &str,
    class_name: &str,
    injections: &mut Vec<InjectionPoint>,
) {
    let annotations = modifier_node(node)
        .map(|m| collect_annotations(m, source))
        .unwrap_or_default();

    if !annotations.iter().any(|a| a.simple_name == "Autowired") {
        return;
    }

    let qualifier = annotations
        .iter()
        .find(|a| a.simple_name == "Qualifier")
        .and_then(|a| {
            a.args
                .get("value")
                .cloned()
                .or_else(|| a.args.get("name").cloned())
        })
        .filter(|s| !s.is_empty());

    let ty_node = node
        .child_by_field_name("type")
        .or_else(|| infer_field_type_node(node));
    let ty = ty_node
        .map(|n| simplify_type(node_text(source, n)))
        .unwrap_or_default();

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
        let span = Span::new(name_node.start_byte(), name_node.end_byte());
        injections.push(InjectionPoint {
            kind: InjectionKind::Field,
            owner_class: class_name.to_string(),
            ty: ty.clone(),
            qualifier: qualifier.clone(),
            location: SourceSpan {
                source: source_idx,
                span,
            },
        });
    }
}

#[derive(Clone, Debug)]
struct ConstructorParam {
    ty: String,
    qualifier: Option<String>,
    span: Span,
}

#[derive(Clone, Debug)]
struct ConstructorData {
    owner_class: String,
    source: usize,
    is_autowired: bool,
    params: Vec<ConstructorParam>,
}

fn parse_constructor(
    node: Node<'_>,
    source_idx: usize,
    source: &str,
    class_name: &str,
) -> ConstructorData {
    let annotations = modifier_node(node)
        .map(|m| collect_annotations(m, source))
        .unwrap_or_default();
    let is_autowired = annotations.iter().any(|a| a.simple_name == "Autowired");

    let params_node = node
        .child_by_field_name("parameters")
        .or_else(|| find_named_child(node, "formal_parameters"));

    let mut params = Vec::new();
    if let Some(params_node) = params_node {
        let mut cursor = params_node.walk();
        for child in params_node.named_children(&mut cursor) {
            if child.kind() != "formal_parameter" {
                continue;
            }
            if let Some(param) = parse_constructor_param(child, source) {
                params.push(param);
            }
        }
    }

    ConstructorData {
        owner_class: class_name.to_string(),
        source: source_idx,
        is_autowired,
        params,
    }
}

fn parse_constructor_param(node: Node<'_>, source: &str) -> Option<ConstructorParam> {
    let annotations = modifier_node(node)
        .map(|m| collect_annotations(m, source))
        .unwrap_or_default();
    let qualifier = annotations
        .iter()
        .find(|a| a.simple_name == "Qualifier")
        .and_then(|a| {
            a.args
                .get("value")
                .cloned()
                .or_else(|| a.args.get("name").cloned())
        })
        .filter(|s| !s.is_empty());

    let name_node = node
        .child_by_field_name("name")
        .or_else(|| find_named_child(node, "identifier"))?;
    let span = Span::new(name_node.start_byte(), name_node.end_byte());

    let ty_node = node
        .child_by_field_name("type")
        .or_else(|| infer_param_type_node(node))?;
    let ty = simplify_type(node_text(source, ty_node));

    Some(ConstructorParam {
        ty,
        qualifier,
        span,
    })
}

fn parse_constructor_injections(ctors: Vec<ConstructorData>, injections: &mut Vec<InjectionPoint>) {
    let autowired: Vec<_> = ctors.iter().filter(|c| c.is_autowired).collect();
    let selected: Vec<&ConstructorData> = if !autowired.is_empty() {
        autowired
    } else if ctors.len() == 1 {
        ctors.iter().collect()
    } else {
        Vec::new()
    };

    for ctor in selected {
        for param in &ctor.params {
            injections.push(InjectionPoint {
                kind: InjectionKind::ConstructorParam,
                owner_class: ctor.owner_class.clone(),
                ty: param.ty.clone(),
                qualifier: param.qualifier.clone(),
                location: SourceSpan {
                    source: ctor.source,
                    span: param.span,
                },
            });
        }
    }
}

fn parse_bean_method(node: Node<'_>, source_idx: usize, source: &str) -> Option<Bean> {
    let annotations = modifier_node(node)
        .map(|m| collect_annotations(m, source))
        .unwrap_or_default();

    let bean_ann = annotations.iter().find(|a| a.simple_name == "Bean")?;

    let name_node = node
        .child_by_field_name("name")
        .or_else(|| find_named_child(node, "identifier"))?;
    let method_name = node_text(source, name_node).to_string();
    let span = Span::new(name_node.start_byte(), name_node.end_byte());

    let return_type_node = node
        .child_by_field_name("type")
        .or_else(|| infer_method_return_type_node(node))?;
    let ty = simplify_type(node_text(source, return_type_node));

    let explicit_name = bean_ann
        .args
        .get("name")
        .or_else(|| bean_ann.args.get("value"))
        .cloned()
        .filter(|s| !s.is_empty());
    let name = explicit_name.unwrap_or_else(|| method_name.clone());

    let qualifiers = annotations
        .iter()
        .filter(|a| a.simple_name == "Qualifier")
        .filter_map(|a| {
            a.args
                .get("value")
                .cloned()
                .or_else(|| a.args.get("name").cloned())
        })
        .filter(|q| !q.is_empty())
        .collect::<Vec<_>>();

    Some(Bean {
        name,
        ty,
        qualifiers,
        location: SourceSpan {
            source: source_idx,
            span,
        },
        kind: BeanKind::BeanMethod,
    })
}

fn lower_camel_case(name: &str) -> String {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut out = String::new();
    out.extend(first.to_lowercase());
    out.push_str(chars.as_str());
    out
}

fn parse_supertypes_from_header(header: &str) -> (Option<String>, Vec<String>) {
    // Best-effort string scan. We keep it lightweight (this is framework-level
    // analysis, not a full type solver).
    let mut super_class = None;
    let mut interfaces = Vec::new();

    if let Some(idx) = find_keyword_top_level(header, "extends") {
        let after = header[idx + "extends".len()..].trim();
        let ty = after.split_whitespace().next().unwrap_or("");
        if !ty.is_empty() {
            super_class = Some(simplify_type(ty));
        }
    }

    if let Some(idx) = find_keyword_top_level(header, "implements") {
        let after = header[idx + "implements".len()..].trim();
        // `after` may include type params; stop at `{` if present.
        let after = after.split('{').next().unwrap_or(after);
        for part in after.split(',') {
            let ty = part.trim().split_whitespace().next().unwrap_or("");
            if !ty.is_empty() {
                interfaces.push(simplify_type(ty));
            }
        }
    }

    (super_class, interfaces)
}

fn find_keyword_top_level(haystack: &str, keyword: &str) -> Option<usize> {
    let mut depth: u32 = 0;
    let bytes = haystack.as_bytes();
    let kw = keyword.as_bytes();

    let mut i = 0usize;
    while i + kw.len() <= bytes.len() {
        match bytes[i] {
            b'<' => {
                depth += 1;
                i += 1;
                continue;
            }
            b'>' => {
                depth = depth.saturating_sub(1);
                i += 1;
                continue;
            }
            _ => {}
        }

        if depth == 0 && haystack[i..].starts_with(keyword) {
            let before_ok = i == 0 || !is_ident_continue(bytes[i - 1] as char);
            let after_ok =
                i + kw.len() >= bytes.len() || !is_ident_continue(bytes[i + kw.len()] as char);
            if before_ok && after_ok {
                return Some(i);
            }
        }

        i += 1;
    }
    None
}

fn is_ident_continue(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '$'
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

// (tree-sitter helpers live in `nova-framework-parse`)
