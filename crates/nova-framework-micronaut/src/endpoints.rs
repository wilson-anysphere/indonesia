use nova_syntax::{SyntaxKind, SyntaxNode};
use nova_types::Span;

use crate::parse::{
    collect_annotations, find_named_child, first_identifier_token, modifier_node, parse_java,
    token_span, visit_nodes,
};
use crate::JavaSource;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandlerLocation {
    pub class_name: String,
    pub method_name: String,
    pub file: String,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub method: String,
    pub path: String,
    pub handler: HandlerLocation,
}

pub fn discover_endpoints(sources: &[JavaSource]) -> Vec<Endpoint> {
    let mut endpoints = Vec::new();

    for src in sources {
        let Ok(parsed) = parse_java(&src.text) else {
            continue;
        };
        let root = parsed.syntax();
        visit_nodes(root, &mut |node| {
            if node.kind() == SyntaxKind::ClassDeclaration {
                endpoints.extend(discover_endpoints_in_class(node, src));
            }
        });
    }

    endpoints.sort_by(|a, b| (&a.path, &a.method).cmp(&(&b.path, &b.method)));
    endpoints
}

fn discover_endpoints_in_class(node: SyntaxNode, src: &JavaSource) -> Vec<Endpoint> {
    let modifiers = modifier_node(&node);
    let class_annotations = modifiers.map_or_else(Vec::new, |m| collect_annotations(m, &src.text));
    let controller = class_annotations
        .iter()
        .find(|a| a.simple_name == "Controller");
    let Some(controller) = controller else {
        return Vec::new();
    };

    let Some(name_token) = first_identifier_token(&node) else {
        return Vec::new();
    };
    let class_name = name_token.text().to_string();

    let base_path = controller
        .args
        .get("value")
        .or_else(|| controller.args.get("uri"))
        .map(String::as_str)
        .unwrap_or("");

    let Some(body) = find_named_child(&node, SyntaxKind::ClassBody) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for child in body
        .children()
        .filter(|c| c.kind() == SyntaxKind::MethodDeclaration)
    {
        out.extend(discover_endpoints_in_method(
            &class_name,
            base_path,
            child,
            src,
        ));
    }

    out
}

fn discover_endpoints_in_method(
    class_name: &str,
    base_path: &str,
    node: SyntaxNode,
    src: &JavaSource,
) -> Vec<Endpoint> {
    let Some(modifiers) = modifier_node(&node) else {
        return Vec::new();
    };
    let annotations = collect_annotations(modifiers, &src.text);

    let Some(name_token) = first_identifier_token(&node) else {
        return Vec::new();
    };
    let method_name = name_token.text().to_string();
    let span = token_span(&name_token);

    let mut out = Vec::new();
    for ann in annotations {
        let Some(http_method) = mapping_http_method(&ann.simple_name) else {
            continue;
        };
        let path = ann
            .args
            .get("value")
            .or_else(|| ann.args.get("uri"))
            .map(String::as_str)
            .unwrap_or("");
        let full_path = join_paths(base_path, path);

        out.push(Endpoint {
            method: http_method.to_string(),
            path: full_path,
            handler: HandlerLocation {
                class_name: class_name.to_string(),
                method_name: method_name.clone(),
                file: src.path.clone(),
                span,
            },
        });
    }

    out
}

fn mapping_http_method(name: &str) -> Option<&'static str> {
    match name {
        "Get" => Some("GET"),
        "Post" => Some("POST"),
        "Put" => Some("PUT"),
        "Delete" => Some("DELETE"),
        "Patch" => Some("PATCH"),
        "Options" => Some("OPTIONS"),
        "Head" => Some("HEAD"),
        "Trace" => Some("TRACE"),
        _ => None,
    }
}

fn join_paths(base: &str, method: &str) -> String {
    let base = base.trim();
    let method = method.trim();

    let mut out = String::new();
    if !base.is_empty() {
        out.push_str(base);
    }
    if !method.is_empty() {
        if !out.ends_with('/') && !method.starts_with('/') {
            out.push('/');
        }
        if out.ends_with('/') && method.starts_with('/') {
            out.pop();
        }
        out.push_str(method);
    }

    if out.is_empty() {
        out.push('/');
    } else if !out.starts_with('/') {
        out.insert(0, '/');
    }

    if out.len() > 1 && out.ends_with('/') {
        out.pop();
    }

    out
}
