use std::collections::HashMap;

use lsp_types::{CodeAction, CodeActionKind, CodeActionOrCommand, Range, TextEdit, Uri, WorkspaceEdit};
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
        if diag.code.as_ref() != "unresolved-name" {
            continue;
        }

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

    actions
}

fn spans_intersect(a: Span, b: Span) -> bool {
    a.start < b.end && a.end > b.start
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
    let insert_offset = line_start_offset(source, close_brace_offset)?;
    let close_indent = line_indent(source, insert_offset);
    let new_text = format!("{close_indent}  private Object {name};\n");

    Some(CodeAction {
        title: format!("Create field '{name}'"),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(single_edit(uri, source, insert_offset, new_text)),
        ..Default::default()
    })
}

fn single_edit(uri: &Uri, source: &str, insert_offset: usize, new_text: String) -> WorkspaceEdit {
    let pos = crate::text::offset_to_position(source, insert_offset);
    let range = Range {
        start: pos,
        end: pos,
    };
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
