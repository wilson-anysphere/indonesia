use nova_framework_parse::parse_annotation_text;
pub(crate) use nova_framework_parse::{clean_type, simple_name, ParsedAnnotation};
use nova_syntax::{JavaParseResult, SyntaxKind, SyntaxNode, SyntaxToken};
use nova_types::Span;

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
        let text = node_text(source, &child);
        let span = node_span(&child);
        if let Some(ann) = parse_annotation_text(text, span) {
            anns.push(ann);
        }
    }
    anns
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
