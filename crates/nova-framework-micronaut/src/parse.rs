use std::collections::HashMap;

use nova_syntax::{JavaParseResult, SyntaxKind, SyntaxNode, SyntaxToken};
use nova_types::Span;

#[derive(Clone, Debug)]
pub(crate) struct ParsedAnnotation {
    pub simple_name: String,
    pub args: HashMap<String, String>,
    pub span: Span,
}

pub(crate) fn parse_java(source: &str) -> Result<JavaParseResult, String> {
    // `nova_syntax::parse_java` always produces a syntax tree and a best-effort
    // list of parse errors; we keep the old `Result` signature to avoid
    // sprawling call-site changes.
    Ok(nova_syntax::parse_java(source))
}

pub(crate) fn visit_nodes<F: FnMut(SyntaxNode)>(node: SyntaxNode, f: &mut F) {
    f(node.clone());
    for child in node.children() {
        visit_nodes(child, f);
    }
}

pub(crate) fn node_text<'a>(source: &'a str, node: &SyntaxNode) -> &'a str {
    let range = node.text_range();
    let start = u32::from(range.start()) as usize;
    let end = u32::from(range.end()) as usize;
    &source[start..end]
}

pub(crate) fn node_span(node: &SyntaxNode) -> Span {
    let range = node.text_range();
    Span::new(
        u32::from(range.start()) as usize,
        u32::from(range.end()) as usize,
    )
}

pub(crate) fn token_span(token: &SyntaxToken) -> Span {
    let range = token.text_range();
    Span::new(
        u32::from(range.start()) as usize,
        u32::from(range.end()) as usize,
    )
}

pub(crate) fn first_identifier_token(node: &SyntaxNode) -> Option<SyntaxToken> {
    node.children_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|token| token.kind() == SyntaxKind::Identifier)
}

pub(crate) fn find_named_child(node: &SyntaxNode, kind: SyntaxKind) -> Option<SyntaxNode> {
    node.children().find(|child| child.kind() == kind)
}

pub(crate) fn collect_annotations(modifiers: SyntaxNode, source: &str) -> Vec<ParsedAnnotation> {
    let mut anns = Vec::new();
    for child in modifiers
        .children()
        .filter(|c| c.kind() == SyntaxKind::Annotation)
    {
        if let Some(ann) = parse_annotation(child, source) {
            anns.push(ann);
        }
    }
    anns
}

pub(crate) fn parse_annotation(node: SyntaxNode, source: &str) -> Option<ParsedAnnotation> {
    let text = node_text(source, &node);
    let span = node_span(&node);
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

pub(crate) fn modifier_node(node: &SyntaxNode) -> Option<SyntaxNode> {
    find_named_child(node, SyntaxKind::Modifiers)
}

pub(crate) fn infer_field_type_node(node: &SyntaxNode) -> Option<SyntaxNode> {
    find_named_child(node, SyntaxKind::Type)
}

pub(crate) fn infer_param_type_node(node: &SyntaxNode) -> Option<SyntaxNode> {
    find_named_child(node, SyntaxKind::Type)
}
