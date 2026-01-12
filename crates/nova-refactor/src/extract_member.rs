use std::collections::HashSet;

use nova_format::{format_member_insertion_with_newline, NewlineStyle};
use nova_index::TextRange;
use nova_syntax::ast::{self, AstNode};
use nova_syntax::{parse_java, SyntaxKind};
use thiserror::Error;

use crate::edit::{FileId, TextEdit as WorkspaceTextEdit, WorkspaceEdit};

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ExtractError {
    #[error("failed to parse Java source")]
    ParseError,
    #[error("selection does not resolve to an expression")]
    InvalidSelection,
    #[error("expression has side effects and cannot be extracted safely")]
    SideEffectfulExpression,
    #[error("expression is not contained in a class body")]
    NotInClass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractKind {
    Constant,
    Field,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractOptions {
    pub name: Option<String>,
    pub replace_all: bool,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            name: None,
            replace_all: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractOutcome {
    pub edit: WorkspaceEdit,
    pub name: String,
}

pub fn extract_constant(
    file: &str,
    source: &str,
    selection: TextRange,
    options: ExtractOptions,
) -> Result<ExtractOutcome, ExtractError> {
    extract_impl(file, source, selection, ExtractKind::Constant, options)
}

/// Extracts a selected expression into an instance field.
///
/// Current implementation policy:
/// - Generates an inline-initialized final field:
///   `private final <Type> <name> = <expr>;`
/// - Rejects expressions with potential side effects (method calls, `new`, etc.)
/// - Performs "replace all" using best-effort structural matching (normalized
///   expression text).
pub fn extract_field(
    file: &str,
    source: &str,
    selection: TextRange,
    options: ExtractOptions,
) -> Result<ExtractOutcome, ExtractError> {
    extract_impl(file, source, selection, ExtractKind::Field, options)
}

fn extract_impl(
    file: &str,
    source: &str,
    selection: TextRange,
    kind: ExtractKind,
    options: ExtractOptions,
) -> Result<ExtractOutcome, ExtractError> {
    if selection.end > source.len() {
        return Err(ExtractError::InvalidSelection);
    }

    let selection = trim_range(source, selection);
    if selection.len() == 0 {
        return Err(ExtractError::InvalidSelection);
    }

    let parsed = parse_java(source);
    if !parsed.errors.is_empty() {
        return Err(ExtractError::ParseError);
    }

    let root = parsed.syntax();
    let expr = find_expression(root.clone(), selection).ok_or(ExtractError::InvalidSelection)?;
    if has_side_effects(expr.syntax()) {
        return Err(ExtractError::SideEffectfulExpression);
    }

    let class_body = expr
        .syntax()
        .ancestors()
        .find_map(ast::ClassBody::cast)
        .ok_or(ExtractError::NotInClass)?;

    let expr_range = syntax_range(expr.syntax());
    let expr_text = source[expr_range.start..expr_range.end].to_string();
    let expr_type = infer_expr_type(source, &expr).unwrap_or_else(|| "Object".to_string());

    let existing_names = collect_field_names(&class_body);
    let suggested = match kind {
        ExtractKind::Constant => "VALUE".to_string(),
        ExtractKind::Field => "value".to_string(),
    };
    let mut name = options
        .name
        .as_deref()
        .map(|n| sanitize_identifier(n, kind))
        .filter(|n| !n.is_empty())
        .unwrap_or(suggested);
    name = make_unique(name, &existing_names);

    let occurrences = if options.replace_all {
        find_equivalent_expressions(source, &class_body, &expr)
    } else {
        vec![expr_range]
    };

    let (insert_offset, indent, needs_blank_line_after) = insertion_point(source, &class_body);
    let declaration = match kind {
        ExtractKind::Constant => format!(
            "private static final {} {} = {};",
            expr_type, name, expr_text
        ),
        ExtractKind::Field => format!("private final {} {} = {};", expr_type, name, expr_text),
    };

    let insert_text = format_member_insertion_with_newline(
        &indent,
        &declaration,
        needs_blank_line_after,
        NewlineStyle::detect(source),
    );

    let file_id = FileId::new(file.to_string());
    let mut edit = WorkspaceEdit::new({
        let mut edits = Vec::new();
        edits.push(WorkspaceTextEdit::insert(
            file_id.clone(),
            insert_offset,
            insert_text,
        ));
        for range in occurrences {
            edits.push(WorkspaceTextEdit::replace(
                file_id.clone(),
                range,
                name.clone(),
            ));
        }
        edits
    });
    edit.normalize().map_err(|_| ExtractError::InvalidSelection)?;

    Ok(ExtractOutcome { edit, name })
}

fn trim_range(source: &str, mut range: TextRange) -> TextRange {
    let bytes = source.as_bytes();
    while range.start < range.end && bytes[range.start].is_ascii_whitespace() {
        range.start += 1;
    }
    while range.start < range.end && bytes[range.end - 1].is_ascii_whitespace() {
        range.end -= 1;
    }
    range
}

fn syntax_range(node: &nova_syntax::SyntaxNode) -> TextRange {
    let range = node.text_range();
    TextRange::new(u32::from(range.start()) as usize, u32::from(range.end()) as usize)
}

fn find_expression(root: nova_syntax::SyntaxNode, selection: TextRange) -> Option<ast::Expression> {
    for expr in root.descendants().filter_map(ast::Expression::cast) {
        let range = syntax_range(expr.syntax());
        if range.start == selection.start && range.end == selection.end {
            return Some(expr);
        }
    }
    None
}

fn has_side_effects(expr: &nova_syntax::SyntaxNode) -> bool {
    expr.descendants().any(|node| {
        matches!(
            node.kind(),
            SyntaxKind::MethodCallExpression | SyntaxKind::NewExpression | SyntaxKind::AssignmentExpression
        )
    })
}

fn infer_expr_type(source: &str, expr: &ast::Expression) -> Option<String> {
    match expr {
        ast::Expression::LiteralExpression(lit) => {
            let tok = lit
                .syntax()
                .descendants_with_tokens()
                .filter_map(|el| el.into_token())
                .find(|tok| !tok.kind().is_trivia() && tok.kind() != SyntaxKind::Eof)?;
            match tok.kind() {
                SyntaxKind::IntLiteral => Some("int".to_string()),
                SyntaxKind::StringLiteral => Some("String".to_string()),
                SyntaxKind::CharLiteral => Some("char".to_string()),
                _ => Some("Object".to_string()),
            }
        }
        ast::Expression::BinaryExpression(_)
        | ast::Expression::UnaryExpression(_)
        | ast::Expression::ParenthesizedExpression(_) => {
            // Best-effort: if the expression contains string literals, treat it as `String`,
            // otherwise assume it's numeric (`int`).
            let text = source[syntax_range(expr.syntax()).start..syntax_range(expr.syntax()).end]
                .trim();
            if text.contains('"') {
                Some("String".to_string())
            } else {
                Some("int".to_string())
            }
        }
        _ => None,
    }
}

fn collect_field_names(body: &ast::ClassBody) -> HashSet<String> {
    let mut out = HashSet::new();
    for member in body.members() {
        let ast::ClassMember::FieldDeclaration(field) = member else {
            continue;
        };
        let Some(list) = field.declarator_list() else {
            continue;
        };
        for decl in list.declarators() {
            if let Some(name) = decl.name_token() {
                out.insert(name.text().to_string());
            }
        }
    }
    out
}

fn sanitize_identifier(name: &str, kind: ExtractKind) -> String {
    let mut out = String::new();
    for (idx, ch) in name.chars().enumerate() {
        if idx == 0 {
            if ch.is_ascii_alphabetic() || ch == '_' {
                out.push(ch);
            }
        } else if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        }
    }
    match kind {
        ExtractKind::Constant => out.to_ascii_uppercase(),
        ExtractKind::Field => {
            let mut chars = out.chars();
            match chars.next() {
                Some(first) => first.to_ascii_lowercase().to_string() + chars.as_str(),
                None => out,
            }
        }
    }
}

fn make_unique(mut name: String, existing: &HashSet<String>) -> String {
    if !existing.contains(&name) {
        return name;
    }
    let base = name.clone();
    let mut idx = 1usize;
    loop {
        let candidate = format!("{base}{idx}");
        if !existing.contains(&candidate) {
            name = candidate;
            break;
        }
        idx += 1;
    }
    name
}

fn normalize_expr_text(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

fn find_equivalent_expressions(
    source: &str,
    class_body: &ast::ClassBody,
    selected: &ast::Expression,
) -> Vec<TextRange> {
    let selected_norm = normalize_expr_text(
        source
            .get(syntax_range(selected.syntax()).start..syntax_range(selected.syntax()).end)
            .unwrap_or_default(),
    );

    let mut ranges = Vec::new();
    for expr in class_body.syntax().descendants().filter_map(ast::Expression::cast) {
        if has_side_effects(expr.syntax()) {
            continue;
        }
        let range = syntax_range(expr.syntax());
        let Some(text) = source.get(range.start..range.end) else {
            continue;
        };
        if normalize_expr_text(text) == selected_norm {
            ranges.push(range);
        }
    }
    ranges.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.end.cmp(&b.end)));
    ranges.dedup();
    ranges
}

fn insertion_point(source: &str, body: &ast::ClassBody) -> (usize, String, bool) {
    let newline = NewlineStyle::detect(source);
    let newline_str = newline.as_str();
    let brace_end = body
        .syntax()
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|tok| tok.kind() == SyntaxKind::LBrace)
        .map(|tok| u32::from(tok.text_range().end()) as usize)
        .unwrap_or_else(|| syntax_range(body.syntax()).start);

    // Insert immediately after the first newline following `{`, so we end up at the indentation
    // whitespace for the first existing member (if any).
    let mut offset = brace_end;
    if let Some(rel) = source[offset..].find('\n') {
        offset += rel + 1;
        // If this is a CRLF file, the `\r` will be before the `\n`. Step past it as well.
        if offset >= 2 && source.as_bytes()[offset - 2] == b'\r' && newline_str == "\r\n" {
            // Offset already includes '\n'; nothing extra to do.
        }
    }

    // Determine existing indentation.
    let mut indent_end = offset;
    while indent_end < source.len() {
        match source.as_bytes()[indent_end] {
            b' ' | b'\t' => indent_end += 1,
            _ => break,
        }
    }
    let indent = source[offset..indent_end].to_string();

    // Blank line after when there are already members in the body.
    let needs_blank_line_after = body.members().next().is_some();

    (offset, indent, needs_blank_line_after)
}

