use std::collections::HashMap;

use nova_types::Span;
use tree_sitter::{Node, Parser, Tree};

#[derive(Clone, Debug)]
pub(crate) struct ParsedAnnotation {
    pub simple_name: String,
    pub args: HashMap<String, String>,
    pub span: Span,
}

pub(crate) fn parse_java(source: &str) -> Result<Tree, String> {
    let mut parser = Parser::new();
    parser
        .set_language(tree_sitter_java::language())
        .map_err(|_| "tree-sitter-java language load failed".to_string())?;
    parser
        .parse(source, None)
        .ok_or_else(|| "tree-sitter failed to produce a syntax tree".to_string())
}

pub(crate) fn visit_nodes<'a, F: FnMut(Node<'a>)>(node: Node<'a>, f: &mut F) {
    f(node);
    if node.child_count() == 0 {
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_nodes(child, f);
    }
}

pub(crate) fn node_text<'a>(source: &'a str, node: Node<'_>) -> &'a str {
    &source[node.byte_range()]
}

pub(crate) fn find_named_child<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    let result = node
        .named_children(&mut cursor)
        .find(|child| child.kind() == kind);
    result
}

pub(crate) fn collect_annotations(modifiers: Node<'_>, source: &str) -> Vec<ParsedAnnotation> {
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

pub(crate) fn parse_annotation(node: Node<'_>, source: &str) -> Option<ParsedAnnotation> {
    let text = node_text(source, node);
    let span = Span::new(node.start_byte(), node.end_byte());
    parse_annotation_text(text, span)
}

pub(crate) fn parse_annotation_text(text: &str, span: Span) -> Option<ParsedAnnotation> {
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

    Some(ParsedAnnotation {
        simple_name,
        args,
        span,
    })
}

pub(crate) fn parse_annotation_args(args_part: &str, out: &mut HashMap<String, String>) {
    for segment in split_top_level_commas(args_part) {
        let seg = segment.trim();
        if seg.is_empty() {
            continue;
        }

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

pub(crate) fn clean_type(raw: &str) -> String {
    raw.split_whitespace().collect::<String>()
}

pub(crate) fn simple_name(raw: &str) -> String {
    let raw = strip_generic_args(raw);
    raw.rsplit('.').next().unwrap_or(&raw).to_string()
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

pub(crate) fn modifier_node(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("modifiers")
        .or_else(|| find_named_child(node, "modifiers"))
}

pub(crate) fn infer_field_type_node<'a>(node: Node<'a>) -> Option<Node<'a>> {
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

pub(crate) fn infer_param_type_node<'a>(node: Node<'a>) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            k if k == "modifiers" || k.ends_with("annotation") => continue,
            // Parameter name.
            "identifier" => break,
            _ => return Some(child),
        }
    }
    None
}
