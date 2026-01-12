use std::collections::HashMap;

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

    actions.extend(crate::quickfix::unresolved_type_quick_fixes(
        uri,
        source,
        selection,
        diagnostics,
    ));

    for diag in diagnostics {
        match diag.code.as_ref() {
            "unresolved-name" => {
                let Some(diag_span) = diag.span else {
                    continue;
                };

                if !spans_intersect(diag_span, selection) {
                    continue;
                }

                let Some(name) = extract_unresolved_name(diag, source) else {
                    continue;
                };

                if !looks_like_value_ident(&name) {
                    continue;
                }

                if let Some(action) = create_local_variable_action(uri, source, diag_span, &name) {
                    actions.push(CodeActionOrCommand::CodeAction(action));
                }

                if let Some(action) = create_field_action(uri, source, &name) {
                    actions.push(CodeActionOrCommand::CodeAction(action));
                }
            }
            "unused-import" => {
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
                let Some(diag_span) = diag.span else {
                    continue;
                };

                if !spans_intersect(diag_span, selection) {
                    continue;
                }

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
            _ => {}
        }
    }

    actions
}

pub(crate) fn spans_intersect(a: Span, b: Span) -> bool {
    if a.start == a.end {
        return b.start <= a.start && a.start < b.end;
    }
    if b.start == b.end {
        return a.start <= b.start && b.start < a.end;
    }
    a.start < b.end && b.start < a.end
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
