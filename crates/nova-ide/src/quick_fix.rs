use std::collections::{HashMap, HashSet};

use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, Range, TextEdit, Uri, WorkspaceEdit,
};
use nova_types::{Diagnostic, Span};

/// Produce quick-fix code actions for a selection span given diagnostics.
///
/// This is intentionally deterministic and purely text-based: it only looks at the provided
/// diagnostics, selection span, and current file text.
pub(crate) fn quick_fixes_for_diagnostics(
    uri: &Uri,
    source: &str,
    selection: Span,
    diagnostics: &[Diagnostic],
) -> Vec<CodeActionOrCommand> {
    let mut actions = Vec::new();
    let mut seen_create_method: HashSet<String> = HashSet::new();

    actions.extend(crate::quickfix::unresolved_type_quick_fixes(
        uri,
        source,
        selection,
        diagnostics,
    ));

    for diag in diagnostics {
        let Some(diag_span) = diag.span else {
            continue;
        };

        if !spans_intersect(diag_span, selection) {
            continue;
        }

        match diag.code.as_ref() {
            "unresolved-name" => {
                let Some(name) = extract_unresolved_name(diag, source) else {
                    continue;
                };

                // Lowercase identifiers are assumed to be missing values (locals/fields).
                if looks_like_value_ident(&name) {
                    if let Some(action) =
                        create_local_variable_action(uri, source, diag_span, &name)
                    {
                        actions.push(CodeActionOrCommand::CodeAction(action));
                    }

                    if let Some(action) = create_field_action(uri, source, &name) {
                        actions.push(CodeActionOrCommand::CodeAction(action));
                    }
                }

                // Uppercase identifiers are often missing types used as qualifiers (e.g.
                // `List.of(...)`).
                if looks_like_type_ident(&name) {
                    actions.extend(import_and_qualify_type_actions(
                        uri, source, diag_span, &name,
                    ));
                }
            }
            "unused-import" => {
                let Some(line_start) = line_start_offset(source, diag_span.start) else {
                    continue;
                };
                let line_end = line_end_offset(source, diag_span.end);

                let edit =
                    single_replace_range_edit(uri, source, line_start, line_end, String::new());
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: "Remove unused import".to_string(),
                    kind: Some(CodeActionKind::QUICKFIX),
                    edit: Some(edit),
                    is_preferred: Some(true),
                    ..Default::default()
                }));
            }
            "unresolved-import" => {
                let Some(diag_span) = diag.span else {
                    continue;
                };

                if !spans_intersect(diag_span, selection) {
                    continue;
                }

                let Some(line_start) = line_start_offset(source, diag_span.start) else {
                    continue;
                };
                let line_end = line_end_offset(source, diag_span.end);

                let edit =
                    single_replace_range_edit(uri, source, line_start, line_end, String::new());
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: "Remove unresolved import".to_string(),
                    kind: Some(CodeActionKind::QUICKFIX),
                    edit: Some(edit),
                    is_preferred: Some(true),
                    ..Default::default()
                }));
            }
            "unresolved-type" => {
                let name = source
                    .get(diag_span.start..diag_span.end)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .or_else(|| unresolved_type_name(&diag.message).map(|s| s.to_string()));
                let Some(name) = name else {
                    continue;
                };

                if !is_simple_type_identifier(&name) {
                    continue;
                }

                if let Some(action) = create_class_action(uri, source, &name) {
                    actions.push(CodeActionOrCommand::CodeAction(action));
                }
            }
            "FLOW_UNREACHABLE" => {
                let Some(diag_span) = diag.span else {
                    continue;
                };

                if !spans_intersect(diag_span, selection) {
                    continue;
                }

                if let Some(action) = remove_unreachable_code_action(uri, source, diag_span) {
                    actions.push(CodeActionOrCommand::CodeAction(action));
                }
            }
            "return-mismatch" => {
                let Some(diag_span) = diag.span else {
                    continue;
                };

                if !spans_intersect(diag_span, selection) {
                    continue;
                }

                if diag
                    .message
                    .contains("cannot return a value from a `void` method")
                {
                    actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                        title: "Remove returned value".to_string(),
                        kind: Some(CodeActionKind::QUICKFIX),
                        edit: Some(single_replace_edit(uri, source, diag_span, String::new())),
                        ..Default::default()
                    }));
                    continue;
                }

                let Some((expected, found)) = parse_return_mismatch(&diag.message) else {
                    continue;
                };
                if found == "void" {
                    continue;
                }

                let Some(expr_text) = source.get(diag_span.start..diag_span.end) else {
                    continue;
                };
                let expr = expr_text.trim();
                if expr.is_empty() {
                    continue;
                }

                let replacement = format!("({expected}) ({expr})");
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: format!("Cast to {expected}"),
                    kind: Some(CodeActionKind::QUICKFIX),
                    edit: Some(single_replace_edit(uri, source, diag_span, replacement)),
                    ..Default::default()
                }));
            }
            // Lightweight lexer-based diagnostics.
            "UNRESOLVED_REFERENCE" => {
                let Some(diag_span) = diag.span else {
                    continue;
                };

                if !spans_intersect(diag_span, selection) {
                    continue;
                }

                let Some(name) = extract_unresolved_member_name(diag, source, diag_span) else {
                    continue;
                };

                let title = format!("Create method '{name}'");
                if !seen_create_method.insert(title.clone()) {
                    continue;
                }

                let (insert_offset, indent) = crate::quick_fixes::insertion_point(source);
                let new_text = crate::quick_fixes::method_stub(&name, &indent, false);
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title,
                    kind: Some(CodeActionKind::QUICKFIX),
                    edit: Some(single_edit(uri, source, insert_offset, new_text)),
                    ..Default::default()
                }));
            }
            _ => {}
        }
    }

    actions
}

pub(crate) fn spans_intersect(a: Span, b: Span) -> bool {
    let (a_start, a_end) = if a.start <= a.end {
        (a.start, a.end)
    } else {
        (a.end, a.start)
    };
    let (b_start, b_end) = if b.start <= b.end {
        (b.start, b.end)
    } else {
        (b.end, b.start)
    };

    // For cursor-based selections (zero-length spans), treat "touching" either end of a diagnostic
    // span as intersecting. This mirrors common LSP client behavior (cursor is often positioned
    // *after* the token) and avoids missing quick fixes at span boundaries.
    if a_start == a_end {
        return b_start <= a_start && a_start <= b_end;
    }
    if b_start == b_end {
        return a_start <= b_start && b_start <= a_end;
    }

    a_start < b_end && b_start < a_end
}

fn parse_return_mismatch(message: &str) -> Option<(String, String)> {
    // Current format (from Salsa typeck):
    // `return type mismatch: expected {expected}, found {found}`
    let rest = message.strip_prefix("return type mismatch: expected ")?;
    let (expected, found) = rest.split_once(", found ")?;
    Some((expected.trim().to_string(), found.trim().to_string()))
}

fn looks_like_value_ident(name: &str) -> bool {
    name.as_bytes()
        .first()
        .is_some_and(|b| matches!(b, b'a'..=b'z'))
}

fn looks_like_type_ident(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_uppercase() {
        return false;
    }
    name.chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '$')
}

fn import_and_qualify_type_actions(
    uri: &Uri,
    source: &str,
    diag_span: Span,
    name: &str,
) -> Vec<CodeActionOrCommand> {
    let span_text = source
        .get(diag_span.start..diag_span.end)
        .unwrap_or_default();
    if span_text.contains('.') {
        return Vec::new();
    }

    // Reuse the same (small) JDK package probe used by `unresolved-type` quick fixes.
    let candidates = crate::quickfix::import_candidates(name);
    let mut actions = Vec::new();
    for fqn in candidates {
        if let Some(edit) = java_import_workspace_edit(uri, source, &fqn) {
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: format!("Import '{fqn}'"),
                kind: Some(CodeActionKind::QUICKFIX),
                edit: Some(edit),
                ..CodeAction::default()
            }));
        }

        // Replace the unresolved identifier with a fully qualified name.
        actions.push(CodeActionOrCommand::CodeAction(CodeAction {
            title: format!("Use fully qualified name '{fqn}'"),
            kind: Some(CodeActionKind::QUICKFIX),
            edit: Some(single_replace_edit(uri, source, diag_span, fqn)),
            ..CodeAction::default()
        }));
    }

    actions
}

fn java_import_workspace_edit(uri: &Uri, source: &str, fqn: &str) -> Option<WorkspaceEdit> {
    let insert_offset = java_import_insertion_offset(source);

    // Avoid suggesting duplicate imports (including star-import coverage).
    if has_java_import(source, fqn) {
        return None;
    }
    if let Some((pkg, _)) = fqn.rsplit_once('.') {
        if has_java_import(source, &format!("{pkg}.*")) {
            return None;
        }
    }

    let line_ending = if source.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let new_text = format!("import {fqn};{line_ending}");
    Some(single_edit(uri, source, insert_offset, new_text))
}

fn java_import_insertion_offset(text: &str) -> usize {
    let mut package_line_end: Option<usize> = None;
    let mut last_import_line_end: Option<usize> = None;

    let mut offset = 0usize;
    for segment in text.split_inclusive('\n') {
        let line_end = offset + segment.len();
        let mut line = segment.strip_suffix('\n').unwrap_or(segment);
        line = line.strip_suffix('\r').unwrap_or(line);
        let trimmed = line.trim_start();

        if package_line_end.is_none() && trimmed.starts_with("package ") {
            package_line_end = Some(line_end);
        }
        if trimmed.starts_with("import ") {
            last_import_line_end = Some(line_end);
        }

        offset = line_end;
    }

    last_import_line_end.or(package_line_end).unwrap_or(0)
}

fn has_java_import(source: &str, path: &str) -> bool {
    for line in source.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("import ") {
            continue;
        }

        let mut rest = trimmed["import ".len()..].trim_start();
        // Ignore static imports for type import checks.
        if let Some(after_static) = rest.strip_prefix("static") {
            if after_static.starts_with(char::is_whitespace) {
                rest = after_static.trim_start();
            }
        }

        let rest = rest.trim_end();
        let rest = rest.strip_suffix(';').unwrap_or(rest).trim_end();
        if rest == path {
            return true;
        }
    }
    false
}

fn extract_unresolved_name(diag: &Diagnostic, source: &str) -> Option<String> {
    // Prefer extracting from backticks in the message: `foo`
    if let Some(name) = extract_backticked(&diag.message) {
        return Some(name);
    }

    // Fallback to the diagnostic span text.
    let span = diag.span?;
    source.get(span.start..span.end).map(|s| s.to_string())
}

fn extract_backticked(message: &str) -> Option<String> {
    let start = message.find('`')?;
    let rest = &message[start + 1..];
    let end_rel = rest.find('`')?;
    let name = &rest[..end_rel];
    (!name.is_empty()).then_some(name.to_string())
}

fn unresolved_type_name(message: &str) -> Option<&str> {
    let rest = message.strip_prefix("unresolved type `")?;
    rest.strip_suffix('`')
}

fn is_simple_type_identifier(name: &str) -> bool {
    if name.is_empty() || name.contains('.') || name.contains('$') {
        return false;
    }

    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn extract_unresolved_member_name(diag: &Diagnostic, source: &str, span: Span) -> Option<String> {
    extract_quoted(&diag.message, '\'')
        .or_else(|| extract_backticked(&diag.message))
        .or_else(|| {
            let snippet = source.get(span.start..span.end)?;
            extract_method_name_from_snippet(snippet)
        })
}

fn extract_quoted(message: &str, quote: char) -> Option<String> {
    let start = message.find(quote)?;
    let rest = &message[start + quote.len_utf8()..];
    let end_rel = rest.find(quote)?;
    let value = &rest[..end_rel];
    (!value.is_empty()).then_some(value.to_string())
}

fn extract_method_name_from_snippet(snippet: &str) -> Option<String> {
    let trimmed = snippet.trim();
    if trimmed.is_empty() {
        return None;
    }
    let prefix = trimmed.split('(').next().unwrap_or(trimmed);
    extract_last_identifier(prefix)
}

fn extract_last_identifier(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut i = bytes.len();
    while i > 0 {
        let ch = bytes[i - 1] as char;
        if is_ident_continue(ch) {
            break;
        }
        i -= 1;
    }
    if i == 0 {
        return None;
    }
    let end = i;
    while i > 0 {
        let ch = bytes[i - 1] as char;
        if is_ident_continue(ch) {
            i -= 1;
            continue;
        }
        break;
    }
    let start = i;
    let ident = text.get(start..end)?;
    if ident.is_empty() || !is_ident_start(ident.as_bytes()[0] as char) {
        return None;
    }
    Some(ident.to_string())
}

fn is_ident_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || matches!(ch, '_' | '$')
}

fn is_ident_continue(ch: char) -> bool {
    is_ident_start(ch) || ch.is_ascii_digit()
}

fn create_local_variable_action(
    uri: &Uri,
    source: &str,
    diag_span: Span,
    name: &str,
) -> Option<CodeAction> {
    let insert_offset = line_start_offset(source, diag_span.start)?;
    let indent = line_indent(source, insert_offset);
    let new_text = format!("{indent}Object {name} = null;\n");

    Some(CodeAction {
        title: format!("Create local variable '{name}'"),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(single_edit(uri, source, insert_offset, new_text)),
        ..Default::default()
    })
}

fn create_field_action(uri: &Uri, source: &str, name: &str) -> Option<CodeAction> {
    let close_brace_offset = source.rfind('}')?;
    let line_start = line_start_offset(source, close_brace_offset)?;
    let prefix = source.get(line_start..close_brace_offset)?;
    let (insert_offset, new_text) = if prefix.trim().is_empty() {
        // `}` is on its own (possibly indented) line. Insert before the indentation so the closing
        // brace remains aligned, and indent the new field one level deeper.
        let close_indent = line_indent(source, line_start);
        (
            line_start,
            format!("{close_indent}  private Object {name};\n"),
        )
    } else {
        // Single-line files (or brace-with-code-on-the-same-line): insert before the final `}`.
        // Use a fixed 2-space indent, per requirements.
        (close_brace_offset, format!("\n  private Object {name};\n"))
    };

    Some(CodeAction {
        title: format!("Create field '{name}'"),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(single_edit(uri, source, insert_offset, new_text)),
        ..Default::default()
    })
}

fn create_class_action(uri: &Uri, source: &str, name: &str) -> Option<CodeAction> {
    let insert_offset = source.len();
    let prefix = if source.ends_with('\n') { "\n" } else { "\n\n" };
    let new_text = format!("{prefix}class {name} {{\n}}\n");

    Some(CodeAction {
        title: format!("Create class '{name}'"),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(single_edit(uri, source, insert_offset, new_text)),
        ..Default::default()
    })
}

fn single_edit(uri: &Uri, source: &str, insert_offset: usize, new_text: String) -> WorkspaceEdit {
    single_replace_range_edit(uri, source, insert_offset, insert_offset, new_text)
}

fn single_replace_range_edit(
    uri: &Uri,
    source: &str,
    start_offset: usize,
    end_offset: usize,
    new_text: String,
) -> WorkspaceEdit {
    let start = crate::text::offset_to_position(source, start_offset);
    let end = crate::text::offset_to_position(source, end_offset);
    let range = Range { start, end };
    let edit = TextEdit { range, new_text };

    let mut changes = HashMap::new();
    changes.insert(uri.clone(), vec![edit]);
    WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    }
}

fn single_replace_edit(uri: &Uri, source: &str, span: Span, new_text: String) -> WorkspaceEdit {
    let range = crate::text::span_to_lsp_range(source, span);
    let edit = TextEdit { range, new_text };

    let mut changes = HashMap::new();
    changes.insert(uri.clone(), vec![edit]);
    WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    }
}

fn line_start_offset(text: &str, offset: usize) -> Option<usize> {
    let offset = offset.min(text.len());
    if offset == 0 {
        return Some(0);
    }
    let prefix = text.get(..offset)?;
    match prefix.rfind('\n') {
        Some(idx) => Some(idx + 1),
        None => Some(0),
    }
}

fn line_end_offset(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    let Some(rest) = text.get(offset..) else {
        return text.len();
    };
    match rest.find('\n') {
        Some(rel) => offset + rel + 1,
        None => text.len(),
    }
}

fn line_indent<'a>(text: &'a str, line_start: usize) -> &'a str {
    let bytes = text.as_bytes();
    let mut end = line_start.min(bytes.len());
    while end < bytes.len() {
        match bytes[end] {
            b' ' | b'\t' => end += 1,
            _ => break,
        }
    }
    // SAFETY: we only advance on ASCII bytes, which are always char boundaries.
    &text[line_start..end]
}

fn remove_unreachable_code_action(uri: &Uri, source: &str, diag_span: Span) -> Option<CodeAction> {
    // Best-effort: remove the entire line containing the unreachable statement, rather than just
    // the diagnostic span. The span may not cover the full statement text (e.g. `x = 1` inside
    // `int x = 1;`), and deleting whole lines is deterministic and avoids leaving behind broken
    // syntax.
    let start = line_start_offset(source, diag_span.start)?;
    let end = line_end_offset(source, diag_span.end);

    Some(CodeAction {
        title: "Remove unreachable code".to_string(),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(single_replace_range_edit(uri, source, start, end, String::new())),
        ..Default::default()
    })
}
