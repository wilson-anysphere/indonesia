use std::collections::{HashMap, HashSet};

use nova_types::{Diagnostic, Span};
use tree_sitter::Node;

use crate::parse::{
    clean_type, collect_annotations, find_named_child, infer_field_type_node,
    infer_param_type_node, modifier_node, node_text, parse_java, simple_name, visit_nodes,
    ParsedAnnotation,
};
use crate::FileDiagnostic;
use crate::JavaSource;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BeanKind {
    Class,
    FactoryMethod,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Qualifier {
    Named(String),
    Annotation(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InjectionPoint {
    pub label: String,
    pub ty: String,
    pub qualifiers: Vec<Qualifier>,
    pub file: String,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bean {
    pub id: String,
    pub name: String,
    pub ty: String,
    pub kind: BeanKind,
    pub qualifiers: Vec<Qualifier>,
    pub file: String,
    pub span: Span,
    pub injection_points: Vec<InjectionPoint>,
    pub assignable_types: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InjectionResolution {
    pub requesting_bean_id: String,
    pub injection_point: InjectionPoint,
    pub candidates: Vec<String>, // bean ids
}

#[derive(Clone, Debug, Default)]
pub struct BeanAnalysis {
    pub beans: Vec<Bean>,
    pub injection_resolutions: Vec<InjectionResolution>,
    pub diagnostics: Vec<Diagnostic>,
    pub file_diagnostics: Vec<FileDiagnostic>,
}

pub fn analyze_beans(sources: &[JavaSource]) -> BeanAnalysis {
    let qualifier_annotations = discover_custom_qualifiers(sources);
    let type_hierarchy = build_type_hierarchy(sources);

    let mut beans = Vec::new();

    for src in sources {
        if let Ok(tree) = parse_java(&src.text) {
            let root = tree.root_node();
            visit_nodes(root, &mut |node| {
                if node.kind() == "class_declaration" {
                    if let Some(mut discovered) =
                        discover_beans_in_class(node, src, &qualifier_annotations)
                    {
                        beans.append(&mut discovered);
                    }
                }
            });
        }
    }

    // Compute assignable types for each bean.
    for bean in &mut beans {
        let mut set = HashSet::<String>::new();
        collect_assignable_types(&bean.ty, &type_hierarchy, &mut set);
        set.insert(bean.ty.clone());
        let mut types: Vec<_> = set.into_iter().collect();
        types.sort();
        bean.assignable_types = types;
    }

    beans.sort_by(|a, b| a.id.cmp(&b.id));

    let mut injection_resolutions = Vec::new();
    let mut diagnostics = Vec::new();
    let mut file_diagnostics = Vec::new();

    for bean in &beans {
        for ip in &bean.injection_points {
            let candidates = resolve_injection(&beans, ip);
            let candidate_ids: Vec<String> = candidates.iter().map(|b| b.id.clone()).collect();

            injection_resolutions.push(InjectionResolution {
                requesting_bean_id: bean.id.clone(),
                injection_point: ip.clone(),
                candidates: candidate_ids.clone(),
            });

            match candidate_ids.len() {
                0 => {
                    let diag = Diagnostic::error(
                        "MICRONAUT_NO_BEAN",
                        format!("No bean of type `{}` found for injection", ip.ty),
                        Some(ip.span),
                    );
                    diagnostics.push(diag.clone());
                    file_diagnostics.push(FileDiagnostic::new(ip.file.clone(), diag));
                }
                1 => {}
                _ => {
                    let diag = Diagnostic::error(
                        "MICRONAUT_AMBIGUOUS_BEAN",
                        format!("Multiple beans of type `{}` found for injection", ip.ty),
                        Some(ip.span),
                    );
                    diagnostics.push(diag.clone());
                    file_diagnostics.push(FileDiagnostic::new(ip.file.clone(), diag));
                }
            }
        }
    }

    detect_circular_dependencies(
        &beans,
        &injection_resolutions,
        &mut diagnostics,
        &mut file_diagnostics,
    );

    BeanAnalysis {
        beans,
        injection_resolutions,
        diagnostics,
        file_diagnostics,
    }
}

fn discover_custom_qualifiers(sources: &[JavaSource]) -> HashSet<String> {
    let mut out = HashSet::new();
    for src in sources {
        let Ok(tree) = parse_java(&src.text) else {
            continue;
        };
        let root = tree.root_node();
        visit_nodes(root, &mut |node| {
            if node.kind() != "annotation_type_declaration" {
                return;
            }
            let Some(modifiers) = modifier_node(node) else {
                return;
            };
            let anns = collect_annotations(modifiers, &src.text);
            if !anns.iter().any(|a| a.simple_name == "Qualifier") {
                return;
            }
            let name_node = node
                .child_by_field_name("name")
                .or_else(|| find_named_child(node, "identifier"));
            let Some(name_node) = name_node else {
                return;
            };
            out.insert(node_text(&src.text, name_node).to_string());
        });
    }
    out
}

fn build_type_hierarchy(sources: &[JavaSource]) -> HashMap<String, Vec<String>> {
    let mut parents: HashMap<String, Vec<String>> = HashMap::new();

    for src in sources {
        let Ok(tree) = parse_java(&src.text) else {
            continue;
        };
        let root = tree.root_node();
        visit_nodes(root, &mut |node| match node.kind() {
            "class_declaration" | "interface_declaration" => {
                let Some(name_node) = node
                    .child_by_field_name("name")
                    .or_else(|| find_named_child(node, "identifier"))
                else {
                    return;
                };
                let name = node_text(&src.text, name_node).to_string();
                let body = node
                    .child_by_field_name("body")
                    .or_else(|| find_named_child(node, "class_body"))
                    .or_else(|| find_named_child(node, "interface_body"));
                let Some(body) = body else {
                    return;
                };
                let header = &src.text[node.start_byte()..body.start_byte()];
                let supers = parse_supertypes(header);
                if !supers.is_empty() {
                    parents.insert(name, supers);
                } else {
                    parents.entry(name).or_default();
                }
            }
            _ => {}
        });
    }

    parents
}

fn parse_supertypes(header: &str) -> Vec<String> {
    let mut cleaned = String::with_capacity(header.len());
    let mut angle = 0u32;
    for ch in header.chars() {
        match ch {
            '<' => angle += 1,
            '>' => angle = angle.saturating_sub(1),
            _ if angle == 0 => {
                if ch == '{' || ch == ',' {
                    cleaned.push(' ');
                } else {
                    cleaned.push(ch);
                }
            }
            _ => {}
        }
    }

    let tokens: Vec<&str> = cleaned.split_whitespace().collect();
    let mut out = Vec::new();

    if let Some(idx) = tokens.iter().position(|t| *t == "extends") {
        if let Some(ty) = tokens.get(idx + 1) {
            out.push(simple_name(ty));
        }
    }

    if let Some(idx) = tokens.iter().position(|t| *t == "implements") {
        for ty in tokens.iter().skip(idx + 1) {
            if *ty == "{" {
                break;
            }
            out.push(simple_name(ty));
        }
    }

    out.retain(|t| !t.is_empty());
    out.sort();
    out.dedup();
    out
}

fn collect_assignable_types(
    ty: &str,
    parents: &HashMap<String, Vec<String>>,
    out: &mut HashSet<String>,
) {
    let Some(supers) = parents.get(ty) else {
        return;
    };
    for sup in supers {
        if out.insert(sup.clone()) {
            collect_assignable_types(sup, parents, out);
        }
    }
}

fn discover_beans_in_class(
    node: Node<'_>,
    src: &JavaSource,
    qualifier_annotations: &HashSet<String>,
) -> Option<Vec<Bean>> {
    let modifiers = modifier_node(node);
    let class_annotations = modifiers
        .map(|m| collect_annotations(m, &src.text))
        .unwrap_or_default();

    let is_factory = class_annotations.iter().any(|a| a.simple_name == "Factory");
    let is_class_bean = class_annotations.iter().any(|a| {
        matches!(
            a.simple_name.as_str(),
            "Singleton" | "Prototype" | "Controller"
        )
    });

    if !is_factory && !is_class_bean {
        return None;
    }

    let name_node = node
        .child_by_field_name("name")
        .or_else(|| find_named_child(node, "identifier"))?;
    let class_name = node_text(&src.text, name_node).to_string();

    let class_span = Span::new(node.start_byte(), node.end_byte());
    let class_qualifiers = extract_qualifiers(&class_annotations, qualifier_annotations);
    let class_named = extract_named(&class_annotations);

    let body = node
        .child_by_field_name("body")
        .or_else(|| find_named_child(node, "class_body"))?;

    let mut beans = Vec::new();

    if is_class_bean {
        let injection_points =
            discover_injection_points_in_class_body(&class_name, body, src, qualifier_annotations);

        let bean_name = class_named
            .clone()
            .unwrap_or_else(|| decapitalize(&class_name));
        let mut qualifiers = class_qualifiers.clone();
        if let Some(named) = class_named.clone() {
            qualifiers.push(Qualifier::Named(named));
        }
        qualifiers.sort();
        qualifiers.dedup();

        beans.push(Bean {
            id: format!("{}::{}", src.path, class_name),
            name: bean_name,
            ty: class_name.clone(),
            kind: BeanKind::Class,
            qualifiers,
            file: src.path.clone(),
            span: class_span,
            injection_points,
            assignable_types: Vec::new(),
        });
    }

    if is_factory {
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            if child.kind() != "method_declaration" {
                continue;
            }
            if let Some(bean) = discover_factory_method_bean(
                &class_name,
                child,
                src,
                qualifier_annotations,
                &class_qualifiers,
            ) {
                beans.push(bean);
            }
        }
    }

    Some(beans)
}

fn discover_injection_points_in_class_body(
    class_name: &str,
    body: Node<'_>,
    src: &JavaSource,
    qualifier_annotations: &HashSet<String>,
) -> Vec<InjectionPoint> {
    let mut points = Vec::new();

    // Field injection.
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() == "field_declaration" {
            points.extend(discover_field_injections(child, src, qualifier_annotations));
        }
    }

    // Constructor injection: best-effort, first @Inject constructor wins.
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() != "constructor_declaration" {
            continue;
        }
        let Some(modifiers) = modifier_node(child) else {
            continue;
        };
        let anns = collect_annotations(modifiers, &src.text);
        if !anns.iter().any(|a| a.simple_name == "Inject") {
            continue;
        }

        points.extend(discover_callable_params_as_injections(
            class_name,
            child,
            src,
            qualifier_annotations,
            true,
        ));
        break;
    }

    points
}

fn discover_field_injections(
    node: Node<'_>,
    src: &JavaSource,
    qualifier_annotations: &HashSet<String>,
) -> Vec<InjectionPoint> {
    let Some(modifiers) = modifier_node(node) else {
        return Vec::new();
    };
    let anns = collect_annotations(modifiers, &src.text);
    if !anns.iter().any(|a| a.simple_name == "Inject") {
        return Vec::new();
    }

    let ty_node = node
        .child_by_field_name("type")
        .or_else(|| infer_field_type_node(node));
    let ty = ty_node
        .map(|n| simple_name(&clean_type(node_text(&src.text, n))))
        .unwrap_or_default();

    let qualifiers = extract_qualifiers(&anns, qualifier_annotations);

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
        let name = node_text(&src.text, name_node).to_string();
        let span = Span::new(name_node.start_byte(), name_node.end_byte());

        out.push(InjectionPoint {
            label: name,
            ty: ty.clone(),
            qualifiers: qualifiers.clone(),
            file: src.path.clone(),
            span,
        });
    }

    out
}

fn discover_factory_method_bean(
    factory_class: &str,
    node: Node<'_>,
    src: &JavaSource,
    qualifier_annotations: &HashSet<String>,
    factory_qualifiers: &[Qualifier],
) -> Option<Bean> {
    let modifiers = modifier_node(node);
    let annotations = modifiers
        .map(|m| collect_annotations(m, &src.text))
        .unwrap_or_default();
    if !annotations.iter().any(|a| a.simple_name == "Bean") {
        return None;
    }

    let name_node = node
        .child_by_field_name("name")
        .or_else(|| find_named_child(node, "identifier"))?;
    let method_name = node_text(&src.text, name_node).to_string();
    let span = Span::new(node.start_byte(), node.end_byte());

    let return_ty_node = node.child_by_field_name("type");
    let return_ty = return_ty_node
        .map(|n| simple_name(&clean_type(node_text(&src.text, n))))
        .unwrap_or_default();

    let method_named = extract_named(&annotations);
    let bean_name = method_named.clone().unwrap_or_else(|| method_name.clone());

    let mut qualifiers = factory_qualifiers.to_vec();
    qualifiers.extend(extract_qualifiers(&annotations, qualifier_annotations));
    if let Some(named) = method_named {
        qualifiers.push(Qualifier::Named(named));
    }
    qualifiers.sort();
    qualifiers.dedup();

    let injection_points = discover_callable_params_as_injections(
        factory_class,
        node,
        src,
        qualifier_annotations,
        false,
    );

    Some(Bean {
        id: format!("{}::{}#{}", src.path, factory_class, method_name),
        name: bean_name,
        ty: return_ty,
        kind: BeanKind::FactoryMethod,
        qualifiers,
        file: src.path.clone(),
        span,
        injection_points,
        assignable_types: Vec::new(),
    })
}

fn discover_callable_params_as_injections(
    _owner: &str,
    node: Node<'_>,
    src: &JavaSource,
    qualifier_annotations: &HashSet<String>,
    require_inject: bool,
) -> Vec<InjectionPoint> {
    if require_inject {
        // `require_inject` is handled by the caller for constructors (annotations already checked).
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
        let name_node = child
            .child_by_field_name("name")
            .or_else(|| find_named_child(child, "identifier"));
        let Some(name_node) = name_node else {
            continue;
        };
        let name = node_text(&src.text, name_node).to_string();
        let span = Span::new(name_node.start_byte(), name_node.end_byte());

        let type_node = child
            .child_by_field_name("type")
            .or_else(|| infer_param_type_node(child));
        let ty = type_node
            .map(|n| simple_name(&clean_type(node_text(&src.text, n))))
            .unwrap_or_default();

        let qualifiers = modifier_node(child)
            .map(|m| {
                let anns = collect_annotations(m, &src.text);
                extract_qualifiers(&anns, qualifier_annotations)
            })
            .unwrap_or_default();

        out.push(InjectionPoint {
            label: name,
            ty,
            qualifiers,
            file: src.path.clone(),
            span,
        });
    }

    out
}

fn extract_named(annotations: &[ParsedAnnotation]) -> Option<String> {
    annotations
        .iter()
        .find(|a| a.simple_name == "Named")
        .and_then(|a| a.args.get("value").cloned())
}

fn extract_qualifiers(
    annotations: &[ParsedAnnotation],
    qualifier_annotations: &HashSet<String>,
) -> Vec<Qualifier> {
    let mut out = Vec::new();

    if let Some(named) = extract_named(annotations) {
        out.push(Qualifier::Named(named));
    }

    for ann in annotations {
        if qualifier_annotations.contains(&ann.simple_name) {
            out.push(Qualifier::Annotation(ann.simple_name.clone()));
        }
    }

    out.sort();
    out.dedup();
    out
}

fn resolve_injection<'a>(beans: &'a [Bean], ip: &InjectionPoint) -> Vec<&'a Bean> {
    beans
        .iter()
        .filter(|b| b.assignable_types.iter().any(|t| t == &ip.ty))
        .filter(|b| qualifiers_match(&ip.qualifiers, &b.qualifiers))
        .collect()
}

fn qualifiers_match(injection: &[Qualifier], bean: &[Qualifier]) -> bool {
    injection.iter().all(|q| bean.iter().any(|bq| bq == q))
}

fn detect_circular_dependencies_with_file_diags(
    beans: &[Bean],
    injection_resolutions: &[InjectionResolution],
    diags: &mut Vec<Diagnostic>,
    file_diags: &mut Vec<FileDiagnostic>,
) {
    let mut by_id = HashMap::<&str, usize>::new();
    for (idx, bean) in beans.iter().enumerate() {
        by_id.insert(bean.id.as_str(), idx);
    }

    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); beans.len()];
    for res in injection_resolutions {
        if res.candidates.len() != 1 {
            continue;
        }
        let Some(&from) = by_id.get(res.requesting_bean_id.as_str()) else {
            continue;
        };
        let Some(&to) = by_id.get(res.candidates[0].as_str()) else {
            continue;
        };
        edges[from].push(to);
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    enum Mark {
        Temporary,
        Permanent,
    }

    let mut marks: Vec<Option<Mark>> = vec![None; beans.len()];
    let mut stack: Vec<usize> = Vec::new();
    let mut reported = HashSet::<usize>::new();

    fn visit(
        node: usize,
        edges: &[Vec<usize>],
        marks: &mut [Option<Mark>],
        stack: &mut Vec<usize>,
        reported: &mut HashSet<usize>,
        beans: &[Bean],
        diags: &mut Vec<Diagnostic>,
        file_diags: &mut Vec<FileDiagnostic>,
    ) {
        if marks[node] == Some(Mark::Permanent) {
            return;
        }
        if marks[node] == Some(Mark::Temporary) {
            if let Some(pos) = stack.iter().position(|n| *n == node) {
                let cycle = &stack[pos..];
                let cycle_names = cycle
                    .iter()
                    .map(|idx| beans[*idx].ty.as_str())
                    .collect::<Vec<_>>()
                    .join(" -> ");
                for idx in cycle {
                    if reported.insert(*idx) {
                        let diag = Diagnostic::warning(
                            "MICRONAUT_CIRCULAR_DEPENDENCY",
                            format!("Circular dependency detected: {cycle_names}"),
                            Some(beans[*idx].span),
                        );
                        diags.push(diag.clone());
                        file_diags.push(FileDiagnostic::new(beans[*idx].file.clone(), diag));
                    }
                }
            }
            return;
        }

        marks[node] = Some(Mark::Temporary);
        stack.push(node);
        for &next in &edges[node] {
            visit(
                next, edges, marks, stack, reported, beans, diags, file_diags,
            );
        }
        stack.pop();
        marks[node] = Some(Mark::Permanent);
    }

    for idx in 0..beans.len() {
        visit(
            idx,
            &edges,
            &mut marks,
            &mut stack,
            &mut reported,
            beans,
            diags,
            file_diags,
        );
    }
}

fn detect_circular_dependencies(
    beans: &[Bean],
    injection_resolutions: &[InjectionResolution],
    diags: &mut Vec<Diagnostic>,
    file_diags: &mut Vec<FileDiagnostic>,
) {
    detect_circular_dependencies_with_file_diags(beans, injection_resolutions, diags, file_diags);
}

fn decapitalize(name: &str) -> String {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    first.to_lowercase().collect::<String>() + chars.as_str()
}
