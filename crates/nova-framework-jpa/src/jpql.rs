//! Minimal JPQL support.
//!
//! JPQL is a fairly complex language. For editor features we can get away with
//! a tokenizer + some heuristics to understand the most common query patterns.

use std::collections::HashMap;

use nova_syntax::{
    parse_java, Annotation, AnnotationElementValuePairList, AstNode, SyntaxKind, SyntaxNode,
    SyntaxToken,
};
use nova_types::{CompletionItem, Diagnostic, Span};

use crate::entity::EntityModel;

pub const JPQL_UNKNOWN_ENTITY: &str = "JPQL_UNKNOWN_ENTITY";
pub const JPQL_UNKNOWN_ALIAS: &str = "JPQL_UNKNOWN_ALIAS";
pub const JPQL_UNKNOWN_FIELD: &str = "JPQL_UNKNOWN_FIELD";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Ident(String),
    Keyword(String),
    Dot,
    Comma,
    LParen,
    RParen,
    StringLiteral(String),
    Number(String),
    Operator(char),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

pub fn tokenize_jpql(input: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b' ' | b'\t' | b'\n' | b'\r' => {
                i += 1;
            }
            b'.' => {
                tokens.push(Token {
                    kind: TokenKind::Dot,
                    span: Span::new(i, i + 1),
                });
                i += 1;
            }
            b',' => {
                tokens.push(Token {
                    kind: TokenKind::Comma,
                    span: Span::new(i, i + 1),
                });
                i += 1;
            }
            b'(' => {
                tokens.push(Token {
                    kind: TokenKind::LParen,
                    span: Span::new(i, i + 1),
                });
                i += 1;
            }
            b')' => {
                tokens.push(Token {
                    kind: TokenKind::RParen,
                    span: Span::new(i, i + 1),
                });
                i += 1;
            }
            b'\'' | b'"' => {
                let quote = b;
                let start = i;
                i += 1;
                while i < bytes.len() {
                    // JPQL escapes quotes by doubling them ('')
                    if bytes[i] == quote {
                        if i + 1 < bytes.len() && bytes[i + 1] == quote {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                let raw = &input[start..i];
                let unquoted = unquote_jpql_string_literal(raw, quote as char);
                tokens.push(Token {
                    kind: TokenKind::StringLiteral(unquoted),
                    span: Span::new(start, i),
                });
            }
            b'0'..=b'9' => {
                let start = i;
                i += 1;
                while i < bytes.len() && matches!(bytes[i], b'0'..=b'9') {
                    i += 1;
                }
                tokens.push(Token {
                    kind: TokenKind::Number(input[start..i].to_string()),
                    span: Span::new(start, i),
                });
            }
            b'=' | b'<' | b'>' | b'+' | b'-' | b'*' | b'/' => {
                tokens.push(Token {
                    kind: TokenKind::Operator(b as char),
                    span: Span::new(i, i + 1),
                });
                i += 1;
            }
            _ if is_ident_start(b as char) => {
                let start = i;
                i += 1;
                while i < bytes.len() && is_ident_continue(bytes[i] as char) {
                    i += 1;
                }
                let text = &input[start..i];
                let upper = text.to_ascii_uppercase();
                let kind = if is_keyword(&upper) {
                    TokenKind::Keyword(upper)
                } else {
                    TokenKind::Ident(text.to_string())
                };
                tokens.push(Token {
                    kind,
                    span: Span::new(start, i),
                });
            }
            _ => {
                // Unknown char; skip.
                i += 1;
            }
        }
    }
    tokens
}

fn unquote_jpql_string_literal(raw: &str, quote: char) -> String {
    // JPQL uses the SQL-style escaping mechanism where the quote character is
    // escaped by doubling it (`''` inside `'...'`).
    //
    // We intentionally accept unterminated string literals (common while typing)
    // and unquote best-effort.
    let mut inner = raw.strip_prefix(quote).unwrap_or(raw);
    if inner.ends_with(quote) && inner.len() >= quote.len_utf8() {
        inner = &inner[..inner.len() - quote.len_utf8()];
    }
    inner.replace(&format!("{quote}{quote}"), &quote.to_string())
}

fn is_ident_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_' || ch == '$'
}

fn is_ident_continue(ch: char) -> bool {
    is_ident_start(ch) || ch.is_ascii_digit()
}

fn is_keyword(upper: &str) -> bool {
    matches!(
        upper,
        "SELECT"
            | "FROM"
            | "WHERE"
            | "JOIN"
            | "INNER"
            | "LEFT"
            | "RIGHT"
            | "OUTER"
            | "FETCH"
            | "AS"
            | "ON"
            | "GROUP"
            | "BY"
            | "ORDER"
            | "HAVING"
            | "UPDATE"
            | "DELETE"
            | "INSERT"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_string_literal_allows_escaped_quotes() {
        let query = "SELECT u FROM User u WHERE u.name = 'O''Reilly'";
        let start = query.find("'O''Reilly'").unwrap();
        let end = start + "'O''Reilly'".len();
        let tokens = tokenize_jpql(query);

        assert!(
            tokens.iter().any(|t| {
                t.kind == TokenKind::StringLiteral("O'Reilly".to_string())
                    && t.span == Span::new(start, end)
            }),
            "expected a single StringLiteral token spanning the full escaped literal"
        );
    }

    #[test]
    fn tokenize_string_literal_with_only_escaped_quote() {
        let tokens = tokenize_jpql("''''");
        assert_eq!(
            tokens,
            vec![Token {
                kind: TokenKind::StringLiteral("'".to_string()),
                span: Span::new(0, 4),
            }]
        );
    }

    #[test]
    fn tokenize_unterminated_string_literal_is_best_effort() {
        let tokens = tokenize_jpql("'abc");
        assert_eq!(
            tokens,
            vec![Token {
                kind: TokenKind::StringLiteral("abc".to_string()),
                span: Span::new(0, 4),
            }]
        );
    }
}

/// Extract JPQL strings from Java source annotations (`@Query`, `@NamedQuery`).
pub fn extract_jpql_strings(java_source: &str) -> Vec<(String, Span)> {
    // Fast path: avoid parsing Java sources that clearly cannot contain JPQL
    // strings.
    if !(java_source.contains("@Query") || java_source.contains("@NamedQuery")) {
        return Vec::new();
    }

    let mut out = Vec::new();
    let parse = parse_java(java_source);
    let root = parse.syntax();
    for annotation in root
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::Annotation)
    {
        let Some(simple_name) = annotation_simple_name(&annotation) else {
            continue;
        };

        match simple_name.as_str() {
            "Query" => {
                if let Some((value, span)) = jpql_string_from_annotation(&annotation, "value", true)
                {
                    out.push((value, span));
                }
            }
            "NamedQuery" => {
                if let Some((value, span)) =
                    jpql_string_from_annotation(&annotation, "query", false)
                {
                    out.push((value, span));
                }
            }
            _ => {}
        }
    }

    out
}

fn strip_quotes(lit: &str) -> &str {
    let lit = lit.trim();
    if lit.starts_with("\"\"\"") && lit.ends_with("\"\"\"") && lit.len() >= 6 {
        &lit[3..lit.len() - 3]
    } else if (lit.starts_with('"') && lit.ends_with('"'))
        || (lit.starts_with('\'') && lit.ends_with('\''))
    {
        &lit[1..lit.len() - 1]
    } else {
        lit
    }
}

fn literal_content_bounds(source: &str, lit_span: Span) -> (usize, usize) {
    let Some(lit) = source.get(lit_span.start..lit_span.end) else {
        return (lit_span.start, lit_span.end);
    };

    if lit.starts_with("\"\"\"") && lit.ends_with("\"\"\"") && lit.len() >= 6 {
        (
            lit_span.start.saturating_add(3),
            lit_span.end.saturating_sub(3),
        )
    } else {
        (
            lit_span.start.saturating_add(1),
            lit_span.end.saturating_sub(1),
        )
    }
}

fn annotation_simple_name(annotation: &SyntaxNode) -> Option<String> {
    let name = annotation
        .children()
        .find(|n| n.kind() == SyntaxKind::Name)?;
    let last = name
        .children_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| t.kind().is_identifier_like())
        .last()?;
    Some(last.text().to_string())
}

fn jpql_string_from_annotation(
    annotation: &SyntaxNode,
    key: &str,
    allow_positional: bool,
) -> Option<(String, Span)> {
    let annotation = Annotation::cast(annotation.clone())?;
    let args = annotation.arguments()?;

    // Prefer a named argument (`key = "..."`).
    if let Some((value, span)) = string_literal_for_named_arg(&args, key) {
        return Some((value, span));
    }

    // Otherwise, support a single positional string literal argument (used by `@Query("...")`).
    if allow_positional {
        let value = args.value()?;
        if let Some((value, span)) = first_string_literal_token(value.syntax()) {
            return Some((value, span));
        }
    }

    None
}

fn string_literal_for_named_arg(
    args: &AnnotationElementValuePairList,
    key: &str,
) -> Option<(String, Span)> {
    for pair in args.pairs() {
        let name = pair.name_token()?;
        if name.text() != key {
            continue;
        }

        let value = pair.value()?;
        if let Some((value, span)) = first_string_literal_token(value.syntax()) {
            return Some((value, span));
        }
    }
    None
}

fn first_string_literal_token(expr: &SyntaxNode) -> Option<(String, Span)> {
    let token = expr
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| matches!(t.kind(), SyntaxKind::StringLiteral | SyntaxKind::TextBlock))?;
    let span = span_of_token(&token);
    Some((strip_quotes(token.text()).to_string(), span))
}

fn span_of_token(token: &SyntaxToken) -> Span {
    let range = token.text_range();
    let start: usize = u32::from(range.start()) as usize;
    let end: usize = u32::from(range.end()) as usize;
    Span::new(start, end)
}

/// Diagnose all JPQL query strings found in the provided Java source.
///
/// This is a best-effort mapper: JPQL diagnostics are produced relative to the
/// query string, then translated into byte spans within the Java source string.
pub fn jpql_diagnostics_in_java_source(java_source: &str, model: &EntityModel) -> Vec<Diagnostic> {
    let mut diags = Vec::new();

    for (query, lit_span) in extract_jpql_strings(java_source) {
        // `lit_span` covers the quote characters. We map query spans to the
        // underlying literal content (start after the opening quote).
        let (content_start, _content_end) = literal_content_bounds(java_source, lit_span);

        for mut diag in jpql_diagnostics(&query, model) {
            if let Some(span) = diag.span {
                diag.span = Some(Span::new(
                    content_start.saturating_add(span.start),
                    content_start.saturating_add(span.end),
                ));
            } else {
                diag.span = Some(lit_span);
            }
            diags.push(diag);
        }
    }

    diags
}

/// Provide JPQL completions for a cursor located inside a Java string literal
/// used in `@Query(...)` / `@NamedQuery(query=...)`.
pub fn jpql_completions_in_java_source(
    java_source: &str,
    cursor: usize,
    model: &EntityModel,
) -> Vec<CompletionItem> {
    for (query, lit_span) in extract_jpql_strings(java_source) {
        let (content_start, content_end_inclusive) = literal_content_bounds(java_source, lit_span);

        if cursor >= content_start && cursor <= content_end_inclusive {
            let query_cursor = cursor.saturating_sub(content_start);
            return jpql_completions(&query, query_cursor, model);
        }
    }

    Vec::new()
}

pub fn jpql_completions(query: &str, cursor: usize, model: &EntityModel) -> Vec<CompletionItem> {
    let tokens = tokenize_jpql(query);
    jpql_completions_tokens(&tokens, cursor, model)
}

fn jpql_completions_tokens(
    tokens: &[Token],
    cursor: usize,
    model: &EntityModel,
) -> Vec<CompletionItem> {
    if let Some((root_alias, path)) = path_context(tokens, cursor) {
        let alias_map = build_alias_map(tokens, model);

        let Some(mut entity) = alias_map
            .get(&root_alias)
            .and_then(|entity_name| model.entity(entity_name))
        else {
            return Vec::new();
        };

        for segment in &path {
            let Some(field) = entity.field_named(segment) else {
                return Vec::new();
            };
            let Some(rel) = &field.relationship else {
                return Vec::new();
            };
            let Some(target) = &rel.target_entity else {
                return Vec::new();
            };
            let Some(next_entity) = model.entity(target) else {
                return Vec::new();
            };
            entity = next_entity;
        }

        let mut items: Vec<_> = entity
            .fields
            .iter()
            .filter(|f| !f.is_transient && !f.is_static)
            .map(|f| CompletionItem::new(f.name.clone()))
            .collect();
        items.sort_by(|a, b| a.label.cmp(&b.label));
        return items;
    }

    if entity_context(tokens, cursor) {
        let mut items: Vec<_> = model
            .jpql_entity_names()
            .map(|name| CompletionItem::new(name.clone()))
            .collect();
        items.sort_by(|a, b| a.label.cmp(&b.label));
        return items;
    }

    Vec::new()
}

fn path_context(tokens: &[Token], cursor: usize) -> Option<(String, Vec<String>)> {
    // Find the most recent `ident ( . ident )* .` before the cursor.
    let mut dot_idx = None;
    for (idx, tok) in tokens.iter().enumerate() {
        if tok.span.end > cursor {
            break;
        }
        if tok.kind == TokenKind::Dot {
            dot_idx = Some(idx);
        }
    }
    let dot_idx = dot_idx?;

    let mut idents_rev = Vec::new();
    let mut idx = dot_idx.checked_sub(1)?;

    loop {
        let tok = tokens.get(idx)?;
        let TokenKind::Ident(ident) = &tok.kind else {
            return None;
        };
        idents_rev.push(ident.clone());

        let Some(dot_pos) = idx.checked_sub(1) else {
            break;
        };
        if tokens.get(dot_pos).map(|t| &t.kind) != Some(&TokenKind::Dot) {
            break;
        }
        idx = dot_pos.checked_sub(1)?;
    }

    idents_rev.reverse();
    let Some((root, rest)) = idents_rev.split_first() else {
        return None;
    };
    Some((root.clone(), rest.to_vec()))
}

fn entity_context(tokens: &[Token], cursor: usize) -> bool {
    // Heuristic: cursor is in a position where an entity name is expected.
    //
    // This includes:
    // - immediately after `FROM` / `JOIN`
    // - immediately after a comma in a multi-`FROM` clause
    // - within the entity identifier itself (partial typing)
    let mut current = None;
    for (idx, tok) in tokens.iter().enumerate() {
        if tok.span.start > cursor {
            break;
        }
        current = Some(idx);
        if tok.span.end >= cursor {
            break;
        }
    }
    let Some(idx) = current else {
        return false;
    };

    match &tokens[idx].kind {
        TokenKind::Keyword(k) if k == "FROM" || k == "JOIN" => true,
        TokenKind::Comma => true,
        TokenKind::Ident(_) if cursor <= tokens[idx].span.end => {
            matches!(
                tokens.get(idx.wrapping_sub(1)).map(|t| &t.kind),
                Some(TokenKind::Keyword(k)) if k == "FROM" || k == "JOIN"
            ) || matches!(
                tokens.get(idx.wrapping_sub(1)).map(|t| &t.kind),
                Some(TokenKind::Comma)
            )
        }
        _ => false,
    }
}

pub fn jpql_diagnostics(query: &str, model: &EntityModel) -> Vec<Diagnostic> {
    let tokens = tokenize_jpql(query);
    let alias_map = build_alias_map(&tokens, model);
    let mut diags = Vec::new();

    // Validate entity references (FROM + entity JOINs).
    for (idx, tok) in tokens.iter().enumerate() {
        match &tok.kind {
            TokenKind::Keyword(k) if k == "FROM" => {
                let start = idx + 1;
                if let Some(entity_tok) = tokens.get(start) {
                    if let TokenKind::Ident(entity_name) = &entity_tok.kind {
                        let entity_name = simple_name(entity_name);
                        validate_entity_name(&entity_name, entity_tok.span, model, &mut diags);
                    }
                }

                if let Some((_entity, _alias, mut next_i)) = parse_entity_alias(&tokens, start) {
                    while tokens
                        .get(next_i)
                        .is_some_and(|t| matches!(&t.kind, TokenKind::Comma))
                    {
                        let item_start = next_i + 1;
                        let Some((entity, _alias, item_end)) =
                            parse_entity_alias(&tokens, item_start)
                        else {
                            break;
                        };
                        let entity_name = simple_name(&entity);
                        let span = tokens
                            .get(item_start)
                            .map(|t| t.span)
                            .unwrap_or_else(|| Span::new(0, 0));
                        validate_entity_name(&entity_name, span, model, &mut diags);
                        next_i = item_end;
                    }
                }
            }
            TokenKind::Keyword(k) if k == "JOIN" => {
                let mut i = idx + 1;
                while let Some(tok) = tokens.get(i) {
                    match &tok.kind {
                        TokenKind::Keyword(k)
                            if matches!(
                                k.as_str(),
                                "INNER" | "LEFT" | "RIGHT" | "OUTER" | "FETCH"
                            ) =>
                        {
                            i += 1;
                            continue;
                        }
                        _ => break,
                    }
                }

                let Some(entity_tok) = tokens.get(i) else {
                    continue;
                };
                let TokenKind::Ident(entity_name) = &entity_tok.kind else {
                    continue;
                };
                if tokens.get(i + 1).map(|t| &t.kind) == Some(&TokenKind::Dot) {
                    // Path join: `JOIN u.posts ...`
                    continue;
                }
                let entity_name = simple_name(entity_name);
                validate_entity_name(&entity_name, entity_tok.span, model, &mut diags);
            }
            _ => {}
        }
    }

    // Validate dotted path expressions (`alias.field` and `alias.rel.field`).
    let mut i = 0usize;
    while i < tokens.len() {
        let Some(tok) = tokens.get(i) else {
            break;
        };
        let TokenKind::Ident(alias) = &tok.kind else {
            i += 1;
            continue;
        };
        if tokens.get(i + 1).map(|t| &t.kind) != Some(&TokenKind::Dot) {
            i += 1;
            continue;
        }
        let Some(field_tok) = tokens.get(i + 2) else {
            i += 1;
            continue;
        };
        let TokenKind::Ident(_) = &field_tok.kind else {
            i += 1;
            continue;
        };

        // Only consider alias lookups at the start of a path expression. This
        // avoids treating `u.user.name` as two independent `alias.field`
        // expressions (`u.user` and `user.name`).
        let start_of_path = i == 0 || tokens.get(i - 1).map(|t| &t.kind) != Some(&TokenKind::Dot);
        if !start_of_path {
            i += 1;
            continue;
        }

        let Some(entity_name) = alias_map.get(alias) else {
            diags.push(Diagnostic::error(
                JPQL_UNKNOWN_ALIAS,
                format!("Unknown JPQL alias `{}`", alias),
                Some(tok.span),
            ));
            i += 1;
            continue;
        };

        let mut current_entity = model.entity(entity_name);
        let mut j = i + 2;
        while j < tokens.len() {
            let Some(seg_tok) = tokens.get(j) else {
                break;
            };
            let TokenKind::Ident(segment) = &seg_tok.kind else {
                break;
            };

            let Some(entity) = current_entity else {
                break;
            };

            let Some(field) = entity.field_named(segment) else {
                diags.push(Diagnostic::error(
                    JPQL_UNKNOWN_FIELD,
                    format!("Unknown field `{}` on entity `{}`", segment, entity.name),
                    Some(seg_tok.span),
                ));
                break;
            };

            // If this is the start of a longer path, try to resolve the segment
            // as a relationship and continue.
            if tokens.get(j + 1).map(|t| &t.kind) == Some(&TokenKind::Dot) {
                let Some(rel) = &field.relationship else {
                    break;
                };
                let Some(target) = &rel.target_entity else {
                    break;
                };
                current_entity = model.entity(target);
                j += 2;
                continue;
            }

            break;
        }

        i = j;
    }

    diags
}

fn simple_name(ty: &str) -> String {
    ty.rsplit('.').next().unwrap_or(ty).to_string()
}

fn validate_entity_name(
    entity_name: &str,
    span: Span,
    model: &EntityModel,
    diags: &mut Vec<Diagnostic>,
) {
    if model.entity_by_jpql_name(entity_name).is_none() {
        diags.push(Diagnostic::error(
            JPQL_UNKNOWN_ENTITY,
            format!("Unknown JPQL entity `{}`", entity_name),
            Some(span),
        ));
    }
}

fn build_alias_map(tokens: &[Token], model: &EntityModel) -> HashMap<String, String> {
    let mut map = HashMap::new();

    let mut i = 0usize;
    while i < tokens.len() {
        match &tokens[i].kind {
            TokenKind::Keyword(k) if k == "FROM" => {
                i += 1;
                if let Some((entity, alias, mut next_i)) = parse_entity_alias(tokens, i) {
                    let entity = simple_name(&entity);
                    let class_name = model
                        .entity_by_jpql_name(&entity)
                        .map(|e| e.name.clone())
                        .unwrap_or(entity);
                    map.insert(alias, class_name);

                    // Support comma-separated from items: `FROM User u, Post p`.
                    while tokens
                        .get(next_i)
                        .is_some_and(|t| matches!(&t.kind, TokenKind::Comma))
                    {
                        let item_start = next_i + 1;
                        let Some((entity, alias, item_end)) =
                            parse_entity_alias(tokens, item_start)
                        else {
                            break;
                        };
                        let entity = simple_name(&entity);
                        let class_name = model
                            .entity_by_jpql_name(&entity)
                            .map(|e| e.name.clone())
                            .unwrap_or(entity);
                        map.insert(alias, class_name);
                        next_i = item_end;
                    }

                    i = next_i;
                    continue;
                }
            }
            TokenKind::Keyword(k) if k == "JOIN" => {
                i += 1;
                // Skip join modifiers.
                while let Some(tok) = tokens.get(i) {
                    match &tok.kind {
                        TokenKind::Keyword(k)
                            if matches!(
                                k.as_str(),
                                "INNER" | "LEFT" | "RIGHT" | "OUTER" | "FETCH" | "AS"
                            ) =>
                        {
                            i += 1;
                            continue;
                        }
                        _ => break,
                    }
                }
                if let Some((target_entity, alias, next_i)) = parse_join(tokens, i, &map, model) {
                    map.insert(alias, target_entity);
                    i = next_i;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }

    map
}

fn parse_entity_alias(tokens: &[Token], start: usize) -> Option<(String, String, usize)> {
    let entity_tok = tokens.get(start)?;
    let TokenKind::Ident(entity) = &entity_tok.kind else {
        return None;
    };
    let mut idx = start + 1;
    if matches!(
        tokens.get(idx).map(|t| &t.kind),
        Some(TokenKind::Keyword(k)) if k == "AS"
    ) {
        idx += 1;
    }
    let alias_tok = tokens.get(idx)?;
    let TokenKind::Ident(alias) = &alias_tok.kind else {
        return None;
    };
    Some((entity.clone(), alias.clone(), idx + 1))
}

fn parse_join(
    tokens: &[Token],
    start: usize,
    alias_map: &HashMap<String, String>,
    model: &EntityModel,
) -> Option<(String, String, usize)> {
    let first_tok = tokens.get(start)?;
    let TokenKind::Ident(first_ident) = &first_tok.kind else {
        return None;
    };

    // Path join: alias . field alias2
    if tokens.get(start + 1).map(|t| t.kind.clone()) == Some(TokenKind::Dot) {
        let field_tok = tokens.get(start + 2)?;
        let TokenKind::Ident(field_name) = &field_tok.kind else {
            return None;
        };
        let mut alias_idx = start + 3;
        if matches!(
            tokens.get(alias_idx).map(|t| &t.kind),
            Some(TokenKind::Keyword(k)) if k == "AS"
        ) {
            alias_idx += 1;
        }
        let join_alias_tok = tokens.get(alias_idx)?;
        let TokenKind::Ident(join_alias) = &join_alias_tok.kind else {
            return None;
        };

        let entity_name = alias_map.get(first_ident)?;
        let entity = model.entity(entity_name)?;
        let field = entity.field_named(field_name)?;
        let target = field
            .relationship
            .as_ref()
            .and_then(|rel| rel.target_entity.clone());

        let Some(target) = target else {
            return None;
        };
        return Some((target, join_alias.clone(), alias_idx + 1));
    }

    // Entity join: Entity alias
    let mut alias_idx = start + 1;
    if matches!(
        tokens.get(alias_idx).map(|t| &t.kind),
        Some(TokenKind::Keyword(k)) if k == "AS"
    ) {
        alias_idx += 1;
    }
    let alias_tok = tokens.get(alias_idx)?;
    let TokenKind::Ident(alias) = &alias_tok.kind else {
        return None;
    };
    let entity_name = simple_name(first_ident);
    let class_name = model
        .entity_by_jpql_name(&entity_name)
        .map(|e| e.name.clone())
        .unwrap_or(entity_name);
    Some((class_name, alias.clone(), alias_idx + 1))
}

// TODO: Could support deeper semantic understanding (function calls, nested
// expressions, etc.) as Nova grows.
