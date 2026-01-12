use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use nova_types::{Diagnostic, Severity, Span};
use tree_sitter::{Node, Parser, Tree};

pub const CDI_UNSATISFIED_CODE: &str = "QUARKUS_CDI_UNSATISFIED_DEPENDENCY";
pub const CDI_AMBIGUOUS_CODE: &str = "QUARKUS_CDI_AMBIGUOUS_DEPENDENCY";
pub const CDI_CIRCULAR_CODE: &str = "QUARKUS_CDI_CIRCULAR_DEPENDENCY";

#[derive(Debug, Clone)]
pub struct CdiAnalysis {
    pub model: CdiModel,
    pub diagnostics: Vec<Diagnostic>,
}

/// Like [`CdiAnalysis`], but retains the source index for each diagnostic.
#[derive(Debug, Clone)]
pub struct CdiAnalysisWithSources {
    pub model: CdiModel,
    pub diagnostics: Vec<SourceDiagnostic>,
}

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

#[derive(Debug, Clone, Default)]
pub struct CdiModel {
    pub beans: Vec<CdiBean>,
    pub injection_points: Vec<CdiInjectionPoint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeanKind {
    Class,
    ProducerMethod,
}

#[derive(Debug, Clone)]
pub struct CdiBean {
    pub name: String,
    pub kind: BeanKind,
    pub provided_types: Vec<String>,
    pub qualifiers: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CdiInjectionPoint {
    pub required_type: String,
    pub qualifiers: Vec<String>,
}

#[derive(Clone, Debug)]
struct Bean {
    name: String,
    provided_types: HashSet<String>,
    qualifiers: Qualifiers,
    kind: BeanKind,
    dependencies: Vec<InjectionPoint>,
    location: SourceSpan,
}

#[derive(Clone, Debug)]
struct InjectionPoint {
    required_type: String,
    qualifiers: Qualifiers,
    location: SourceSpan,
}

#[derive(Clone, Debug, Default)]
struct Qualifiers {
    named: Option<String>,
    custom: HashSet<String>,
}

impl Qualifiers {
    fn is_empty(&self) -> bool {
        self.named.is_none() && self.custom.is_empty()
    }
}

pub fn analyze_cdi(sources: &[&str]) -> CdiAnalysis {
    let analysis = analyze_cdi_with_sources(sources);
    let diagnostics = analysis
        .diagnostics
        .iter()
        .map(|sd| sd.diagnostic.clone())
        .collect();
    CdiAnalysis {
        model: analysis.model,
        diagnostics,
    }
}

pub fn analyze_cdi_with_sources(sources: &[&str]) -> CdiAnalysisWithSources {
    let index = build_index(sources);
    let diagnostics = compute_diagnostics(&index);

    let model = CdiModel {
        beans: index
            .beans
            .iter()
            .map(|bean| {
                let mut provided_types: Vec<String> = bean.provided_types.iter().cloned().collect();
                provided_types.sort();
                CdiBean {
                    name: bean.name.clone(),
                    kind: bean.kind.clone(),
                    provided_types,
                    qualifiers: format_qualifiers(&bean.qualifiers),
                }
            })
            .collect(),
        injection_points: index
            .injections
            .iter()
            .map(|ip| CdiInjectionPoint {
                required_type: ip.required_type.clone(),
                qualifiers: format_qualifiers(&ip.qualifiers),
            })
            .collect(),
    };

    CdiAnalysisWithSources { model, diagnostics }
}

struct CdiIndex {
    beans: Vec<Bean>,
    injections: Vec<InjectionPoint>,
}

fn build_index(sources: &[&str]) -> CdiIndex {
    let mut beans = Vec::new();
    let mut injections = Vec::new();

    for (source_idx, src) in sources.iter().enumerate() {
        let Ok(tree) = parse_java(src) else {
            continue;
        };
        let root = tree.root_node();
        visit_nodes(root, &mut |node| {
            if node.kind() == "class_declaration" {
                parse_class_declaration(node, source_idx, src, &mut beans, &mut injections);
            }
        });
    }

    CdiIndex { beans, injections }
}

fn parse_class_declaration(
    node: Node<'_>,
    source_idx: usize,
    source: &str,
    beans: &mut Vec<Bean>,
    injections: &mut Vec<InjectionPoint>,
) {
    let modifiers = modifier_node(node);
    let class_annotations = modifiers
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

    let header = &source[node.start_byte()..body.start_byte()];
    let (super_class, interfaces) = parse_supertypes_from_header(header);

    let mut provided_types = HashSet::new();
    provided_types.insert(class_name.clone());
    if let Some(super_class) = super_class {
        provided_types.insert(super_class);
    }
    provided_types.extend(interfaces);

    let class_qualifiers = parse_qualifiers(&class_annotations);
    let is_bean = is_bean_class(&class_annotations);

    if is_bean {
        let mut class_deps = Vec::new();
        parse_class_body_for_injections(body, source_idx, source, injections, &mut class_deps);

        beans.push(Bean {
            name: class_name.clone(),
            kind: BeanKind::Class,
            provided_types,
            qualifiers: class_qualifiers.clone(),
            dependencies: class_deps,
            location: SourceSpan {
                source: source_idx,
                span: class_span,
            },
        });
    }

    parse_class_body_for_producers(
        body,
        source_idx,
        source,
        &class_name,
        &class_qualifiers,
        beans,
    );
}

fn parse_class_body_for_injections(
    body: Node<'_>,
    source_idx: usize,
    source: &str,
    injections: &mut Vec<InjectionPoint>,
    deps: &mut Vec<InjectionPoint>,
) {
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        match child.kind() {
            "field_declaration" => {
                let points = parse_field_injections(child, source_idx, source);
                for ip in points {
                    injections.push(ip.clone());
                    deps.push(ip);
                }
            }
            "constructor_declaration" => {
                let points = parse_constructor_injections(child, source_idx, source);
                for ip in points {
                    injections.push(ip.clone());
                    deps.push(ip);
                }
            }
            _ => {}
        }
    }
}

fn parse_field_injections(node: Node<'_>, source_idx: usize, source: &str) -> Vec<InjectionPoint> {
    let Some(modifiers) = modifier_node(node) else {
        return Vec::new();
    };
    let annotations = collect_annotations(modifiers, source);
    if !annotations.iter().any(|a| a.simple_name == "Inject") {
        return Vec::new();
    }

    let ty_node = node
        .child_by_field_name("type")
        .or_else(|| infer_field_type_node(node));
    let required_type = ty_node
        .map(|n| simplify_type(node_text(source, n)))
        .unwrap_or_default();

    let qualifiers = parse_qualifiers(&annotations);

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
        let span = Span::new(name_node.start_byte(), name_node.end_byte());
        out.push(InjectionPoint {
            required_type: required_type.clone(),
            qualifiers: qualifiers.clone(),
            location: SourceSpan {
                source: source_idx,
                span,
            },
        });
    }

    out
}

fn parse_constructor_injections(
    node: Node<'_>,
    source_idx: usize,
    source: &str,
) -> Vec<InjectionPoint> {
    let modifiers = modifier_node(node);
    let annotations = modifiers
        .map(|m| collect_annotations(m, source))
        .unwrap_or_default();
    if !annotations.iter().any(|a| a.simple_name == "Inject") {
        return Vec::new();
    }

    let params = node
        .child_by_field_name("parameters")
        .or_else(|| find_named_child(node, "formal_parameters"));
    let Some(params) = params else {
        return Vec::new();
    };

    let mut out = Vec::new();
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        if child.kind() != "formal_parameter" {
            continue;
        }
        if let Some(ip) = parse_constructor_param_injection(child, source_idx, source) {
            out.push(ip);
        }
    }
    out
}

fn parse_constructor_param_injection(
    node: Node<'_>,
    source_idx: usize,
    source: &str,
) -> Option<InjectionPoint> {
    let name_node = node
        .child_by_field_name("name")
        .or_else(|| find_named_child(node, "identifier"))?;
    let span = Span::new(name_node.start_byte(), name_node.end_byte());

    let ty_node = node
        .child_by_field_name("type")
        .or_else(|| infer_param_type_node(node))?;
    let required_type = simplify_type(node_text(source, ty_node));

    let qualifiers = modifier_node(node)
        .map(|m| parse_qualifiers(&collect_annotations(m, source)))
        .unwrap_or_default();

    Some(InjectionPoint {
        required_type,
        qualifiers,
        location: SourceSpan {
            source: source_idx,
            span,
        },
    })
}

fn parse_class_body_for_producers(
    body: Node<'_>,
    source_idx: usize,
    source: &str,
    class_name: &str,
    class_qualifiers: &Qualifiers,
    beans: &mut Vec<Bean>,
) {
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() != "method_declaration" {
            continue;
        }

        let modifiers = modifier_node(child);
        let annotations = modifiers
            .map(|m| collect_annotations(m, source))
            .unwrap_or_default();
        if !annotations.iter().any(|a| a.simple_name == "Produces") {
            continue;
        }

        let name_node = child
            .child_by_field_name("name")
            .or_else(|| find_named_child(child, "identifier"));
        let Some(name_node) = name_node else {
            continue;
        };
        let method_name = node_text(source, name_node).to_string();
        let method_span = Span::new(name_node.start_byte(), name_node.end_byte());

        let return_ty_node = child
            .child_by_field_name("type")
            .or_else(|| infer_method_return_type_node(child));
        let return_ty = return_ty_node
            .map(|n| simplify_type(node_text(source, n)))
            .unwrap_or_default();

        let member_qualifiers = parse_qualifiers(&annotations);
        let mut qualifiers = class_qualifiers.clone();
        if member_qualifiers.named.is_some() {
            qualifiers.named = member_qualifiers.named.clone();
        }
        qualifiers.custom.extend(member_qualifiers.custom);

        let mut provided_types = HashSet::new();
        if !return_ty.is_empty() {
            provided_types.insert(return_ty);
        }

        beans.push(Bean {
            name: format!("{class_name}::{method_name}"),
            kind: BeanKind::ProducerMethod,
            provided_types,
            qualifiers,
            dependencies: Vec::new(),
            location: SourceSpan {
                source: source_idx,
                span: method_span,
            },
        });
    }
}

fn compute_diagnostics(index: &CdiIndex) -> Vec<SourceDiagnostic> {
    let mut diagnostics = Vec::new();

    for ip in &index.injections {
        let matches = resolve_injection(index, ip);
        if matches.is_empty() {
            diagnostics.push(SourceDiagnostic {
                source: ip.location.source,
                diagnostic: Diagnostic::error(
                    CDI_UNSATISFIED_CODE,
                    format!("Unsatisfied dependency: {}", ip.required_type),
                    Some(ip.location.span),
                ),
            });
        } else if matches.len() > 1 {
            diagnostics.push(SourceDiagnostic {
                source: ip.location.source,
                diagnostic: Diagnostic::error(
                    CDI_AMBIGUOUS_CODE,
                    format!(
                        "Ambiguous dependency: {} ({} candidates)",
                        ip.required_type,
                        matches.len()
                    ),
                    Some(ip.location.span),
                ),
            });
        }
    }

    diagnostics.extend(detect_circular_dependencies(index));
    diagnostics
}

fn resolve_injection<'a>(index: &'a CdiIndex, ip: &InjectionPoint) -> Vec<&'a Bean> {
    index
        .beans
        .iter()
        .filter(|bean| bean.provided_types.contains(&ip.required_type))
        .filter(|bean| qualifiers_match(&bean.qualifiers, &ip.qualifiers))
        .collect()
}

fn qualifiers_match(bean: &Qualifiers, required: &Qualifiers) -> bool {
    if required.is_empty() {
        // No qualifiers at injection point => @Default.
        // A bean with custom qualifiers (other than @Named) does *not* have @Default.
        return bean.custom.is_empty();
    }

    if let Some(named) = &required.named {
        match &bean.named {
            Some(bn) if bn == named => {}
            Some(_) | None => return false,
        }
    }

    required.custom.is_subset(&bean.custom)
}

fn detect_circular_dependencies(index: &CdiIndex) -> Vec<SourceDiagnostic> {
    let class_beans: Vec<&Bean> = index
        .beans
        .iter()
        .filter(|b| matches!(b.kind, BeanKind::Class))
        .collect();
    if class_beans.is_empty() {
        return Vec::new();
    }

    let mut bean_by_name: HashMap<&str, usize> = HashMap::new();
    for (idx, bean) in class_beans.iter().enumerate() {
        bean_by_name.insert(bean.name.as_str(), idx);
    }

    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); class_beans.len()];
    for (idx, bean) in class_beans.iter().enumerate() {
        for dep in &bean.dependencies {
            let matches: Vec<&Bean> = resolve_injection(index, dep)
                .into_iter()
                .filter(|b| matches!(b.kind, BeanKind::Class))
                .collect();
            if matches.len() != 1 {
                continue;
            }
            let target = matches[0];
            if let Some(&target_idx) = bean_by_name.get(target.name.as_str()) {
                edges[idx].push(target_idx);
            }
        }
    }

    let cycles = find_cycles(&edges);
    if cycles.is_empty() {
        return Vec::new();
    }

    let mut diagnostics = Vec::new();
    let mut seen = HashSet::<Vec<usize>>::new();

    for cycle in cycles {
        let canonical = canonical_cycle(&cycle);
        if !seen.insert(canonical.clone()) {
            continue;
        }

        let names: Vec<&str> = canonical
            .iter()
            .map(|idx| class_beans[*idx].name.as_str())
            .collect();
        let msg = format!("Circular dependency detected: {}", names.join(" -> "));

        for idx in canonical {
            let bean = class_beans[idx];
            diagnostics.push(SourceDiagnostic {
                source: bean.location.source,
                diagnostic: Diagnostic {
                    severity: Severity::Warning,
                    code: CDI_CIRCULAR_CODE.into(),
                    message: msg.clone(),
                    span: Some(bean.location.span),
                },
            });
        }
    }

    diagnostics
}

fn find_cycles(edges: &[Vec<usize>]) -> Vec<Vec<usize>> {
    fn dfs(
        node: usize,
        edges: &[Vec<usize>],
        state: &mut [u8],
        stack: &mut Vec<usize>,
        out: &mut Vec<Vec<usize>>,
    ) {
        state[node] = 1;
        stack.push(node);

        for &next in &edges[node] {
            if state[next] == 0 {
                dfs(next, edges, state, stack, out);
            } else if state[next] == 1 {
                if let Some(pos) = stack.iter().position(|n| *n == next) {
                    out.push(stack[pos..].to_vec());
                }
            }
        }

        stack.pop();
        state[node] = 2;
    }

    let mut out = Vec::new();
    let mut state = vec![0u8; edges.len()];
    let mut stack = Vec::new();

    for node in 0..edges.len() {
        if state[node] == 0 {
            dfs(node, edges, &mut state, &mut stack, &mut out);
        }
    }

    out
}

fn canonical_cycle(cycle: &[usize]) -> Vec<usize> {
    if cycle.is_empty() {
        return Vec::new();
    }
    let mut best = cycle.to_vec();
    for start in 0..cycle.len() {
        let rotated: Vec<usize> = cycle[start..]
            .iter()
            .chain(cycle[..start].iter())
            .copied()
            .collect();
        if rotated < best {
            best = rotated;
        }
    }
    best
}

fn parse_qualifiers(annotations: &[ParsedAnnotation]) -> Qualifiers {
    let mut q = Qualifiers::default();
    for ann in annotations {
        match ann.simple_name.as_str() {
            "Named" => {
                q.named = ann.args.get("value").cloned().filter(|s| !s.is_empty());
            }
            n if is_non_qualifier_annotation(n) => {}
            n => {
                q.custom.insert(n.to_string());
            }
        }
    }
    q
}

fn format_qualifiers(qualifiers: &Qualifiers) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(name) = &qualifiers.named {
        out.push(format!("Named({name})"));
    }
    let mut custom: Vec<_> = qualifiers.custom.iter().cloned().collect();
    custom.sort();
    out.extend(custom);
    out
}

fn is_non_qualifier_annotation(name: &str) -> bool {
    matches!(
        name,
        "Inject"
            | "Produces"
            | "ApplicationScoped"
            | "Singleton"
            | "RequestScoped"
            | "SessionScoped"
            | "Dependent"
            | "Path"
            | "GET"
            | "POST"
            | "PUT"
            | "DELETE"
            | "PATCH"
            | "HEAD"
            | "OPTIONS"
            | "ConfigProperty"
    )
}

fn is_bean_class(annotations: &[ParsedAnnotation]) -> bool {
    annotations.iter().any(|ann| {
        matches!(
            ann.simple_name.as_str(),
            "ApplicationScoped"
                | "Singleton"
                | "RequestScoped"
                | "SessionScoped"
                | "Dependent"
                | "Path"
        )
    })
}

// -----------------------------------------------------------------------------
// tree-sitter-java helpers (local copy; keep CDI analysis self-contained)
// -----------------------------------------------------------------------------

thread_local! {
    static JAVA_PARSER: RefCell<Result<Parser, String>> = RefCell::new({
        let mut parser = Parser::new();
        match parser.set_language(tree_sitter_java::language()) {
            Ok(()) => Ok(parser),
            Err(_) => Err("tree-sitter-java language load failed".to_string()),
        }
    });
}

fn parse_java(source: &str) -> Result<Tree, String> {
    JAVA_PARSER.with(|parser_cell| {
        let mut parser = parser_cell
            .try_borrow_mut()
            .map_err(|_| "tree-sitter parser is already in use".to_string())?;
        let parser = match parser.as_mut() {
            Ok(parser) => parser,
            Err(err) => return Err(err.clone()),
        };

        parser
            .parse(source, None)
            .ok_or_else(|| "tree-sitter failed to produce a syntax tree".to_string())
    })
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

fn find_named_child<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    // Avoid the `named_children` iterator here: in tree-sitter 0.20 its cursor
    // borrow must outlive `'a`, which doesn't work for a local cursor variable.
    for idx in 0..node.named_child_count() {
        let child = node.named_child(idx)?;
        if child.kind() == kind {
            return Some(child);
        }
    }
    None
}

fn node_text<'a>(source: &'a str, node: Node<'_>) -> &'a str {
    &source[node.byte_range()]
}

fn modifier_node(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("modifiers")
        .or_else(|| find_named_child(node, "modifiers"))
}

fn infer_field_type_node<'a>(node: Node<'a>) -> Option<Node<'a>> {
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

fn simplify_type(raw: &str) -> String {
    let raw = raw.trim();
    let base = strip_generic_args(raw);
    let base = base.trim_end_matches("[]").trim();
    base.rsplit('.').next().unwrap_or(base).to_string()
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

#[derive(Clone, Debug)]
struct ParsedAnnotation {
    simple_name: String,
    args: HashMap<String, String>,
}

fn collect_annotations(modifiers: Node<'_>, source: &str) -> Vec<ParsedAnnotation> {
    let mut anns = Vec::new();
    let mut cursor = modifiers.walk();
    for child in modifiers.named_children(&mut cursor) {
        if child.kind().ends_with("annotation") {
            if let Some(ann) = parse_annotation(child, source) {
                anns.push(ann);
            }
        }
    }
    anns
}

fn parse_annotation(node: Node<'_>, source: &str) -> Option<ParsedAnnotation> {
    parse_annotation_text(node_text(source, node))
}

fn parse_annotation_text(text: &str) -> Option<ParsedAnnotation> {
    let text = text.trim();
    if !text.starts_with('@') {
        return None;
    }
    let rest = &text[1..];
    let (name_part, args_part) = match rest.split_once('(') {
        Some((name, args)) => (name.trim(), Some(args)),
        None => (rest.trim(), None),
    };

    let simple_name = name_part
        .rsplit('.')
        .next()
        .unwrap_or(name_part)
        .trim()
        .to_string();

    let mut args = HashMap::new();
    if let Some(args_part) = args_part {
        let args_part = args_part.trim_end_matches(')').trim();
        parse_annotation_args(args_part, &mut args);
    }

    Some(ParsedAnnotation { simple_name, args })
}

fn parse_annotation_args(args_part: &str, out: &mut HashMap<String, String>) {
    for segment in split_top_level_commas(args_part) {
        let seg = segment.trim();
        if seg.is_empty() {
            continue;
        }

        // Single positional argument => `value`.
        if !seg.contains('=') {
            if let Some(value) = parse_literal(seg) {
                out.insert("value".to_string(), value);
            }
            continue;
        }

        let Some((key, value)) = seg.split_once('=') else {
            continue;
        };
        let key = key.trim().to_string();
        let value = value.trim();
        if let Some(parsed) = parse_literal(value) {
            out.insert(key, parsed);
        }
    }
}

fn split_top_level_commas(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0u32;
    let mut in_string = false;
    let mut current = String::new();

    for ch in input.chars() {
        match ch {
            '"' => {
                in_string = !in_string;
                current.push(ch);
            }
            '(' if !in_string => {
                depth += 1;
                current.push(ch);
            }
            ')' if !in_string => {
                depth = depth.saturating_sub(1);
                current.push(ch);
            }
            ',' if !in_string && depth == 0 => {
                out.push(current);
                current = String::new();
            }
            _ => current.push(ch),
        }
    }

    out.push(current);
    out
}

fn parse_literal(input: &str) -> Option<String> {
    let input = input.trim();
    if input.starts_with('"') && input.ends_with('"') && input.len() >= 2 {
        return Some(input[1..input.len() - 1].to_string());
    }
    if input.starts_with('\'') && input.ends_with('\'') && input.len() >= 2 {
        return Some(input[1..input.len() - 1].to_string());
    }
    Some(input.to_string())
}

fn parse_supertypes_from_header(header: &str) -> (Option<String>, HashSet<String>) {
    let mut super_class = None;
    let mut interfaces = HashSet::new();

    if let Some(idx) = find_keyword_top_level(header, "extends") {
        let after = header[idx + "extends".len()..].trim();
        let ty = after.split_whitespace().next().unwrap_or("");
        if !ty.is_empty() {
            super_class = Some(simplify_type(ty));
        }
    }

    if let Some(idx) = find_keyword_top_level(header, "implements") {
        let after = header[idx + "implements".len()..].trim();
        let after = after.split('{').next().unwrap_or(after);
        for part in after.split(',') {
            let ty = part.split_whitespace().next().unwrap_or("");
            if !ty.is_empty() {
                interfaces.insert(simplify_type(ty));
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
