use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use lsp_types::{CodeAction, CodeActionKind, CodeActionOrCommand, TextEdit, WorkspaceEdit};
use nova_db::{Database, FileId, NovaTypeck};
use nova_types::Span;

/// Diagnostic-driven quick fixes.
///
/// These are best-effort edits derived from high-signal diagnostics emitted by the
/// Salsa-backed type checker.
pub fn create_symbol_quick_fixes(
    db: &dyn Database,
    file: FileId,
    selection: Option<Span>,
) -> Vec<CodeActionOrCommand> {
    let Some(selection) = selection else {
        return Vec::new();
    };

    let source = db.file_content(file);
    let uri = file_uri(db, file);

    let is_java = db
        .file_path(file)
        .is_some_and(|path| path.extension().and_then(|e| e.to_str()) == Some("java"));
    if !is_java {
        return Vec::new();
    }

    // All create-symbol fixes insert into the same best-effort location.
    let (insert_offset, indent) = insertion_point(source);
    let insert_position = crate::text::offset_to_position(source, insert_offset);
    let insert_range = lsp_types::Range {
        start: insert_position,
        end: insert_position,
    };

    let mut actions = Vec::new();
    let mut seen_titles: HashSet<String> = HashSet::new();

    // Collect just the type-checking diagnostics; create-symbol quick fixes are
    // driven by (high-signal) type errors, and this avoids the overhead of also
    // running control-flow diagnostics during `textDocument/codeAction`.
    let diagnostics =
        crate::code_intelligence::with_salsa_snapshot_for_single_file(db, file, source, |snap| {
            snap.type_diagnostics(file)
        });
    for diagnostic in diagnostics {
        let Some(span) = diagnostic.span else {
            continue;
        };

        if !spans_intersect(span, selection) {
            continue;
        }

        match diagnostic.code.as_ref() {
            "unresolved-method" => {
                let Some(name) = unresolved_member_name(&diagnostic.message, source, span) else {
                    continue;
                };

                let snippet = source.get(span.start..span.end).unwrap_or_default();
                if !looks_like_enclosing_member_access(snippet) {
                    continue;
                }
                let is_static = diagnostic.message.contains("static context")
                    || looks_like_static_receiver(snippet)
                    || is_within_static_block(source, span.start);

                let title = format!("Create method '{name}'");
                if !seen_titles.insert(title.clone()) {
                    continue;
                }

                let stub = method_stub(&name, &indent, is_static);
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title,
                    kind: Some(CodeActionKind::QUICKFIX),
                    edit: Some(single_file_insert_edit(uri.clone(), insert_range, stub)),
                    ..CodeAction::default()
                }));
            }
            "unresolved-field" => {
                let Some(name) = unresolved_member_name(&diagnostic.message, source, span) else {
                    continue;
                };

                let snippet = source.get(span.start..span.end).unwrap_or_default();
                if !looks_like_enclosing_member_access(snippet) {
                    continue;
                }
                let is_static = looks_like_static_receiver(snippet);

                let title = format!("Create field '{name}'");
                if !seen_titles.insert(title.clone()) {
                    continue;
                }

                let stub = field_stub(&name, &indent, is_static);
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title,
                    kind: Some(CodeActionKind::QUICKFIX),
                    edit: Some(single_file_insert_edit(uri.clone(), insert_range, stub)),
                    ..CodeAction::default()
                }));
            }
            _ => {}
        }
    }

    actions
}

fn spans_intersect(a: Span, b: Span) -> bool {
    if b.start == b.end {
        return a.start <= b.start && b.start <= a.end;
    }
    a.start < b.end && b.start < a.end
}

fn file_uri(db: &dyn Database, file: FileId) -> lsp_types::Uri {
    if let Some(path) = db.file_path(file) {
        if let Ok(abs) = nova_core::AbsPathBuf::new(path.to_path_buf()) {
            if let Ok(uri) = nova_core::path_to_file_uri(&abs) {
                if let Ok(parsed) = lsp_types::Uri::from_str(&uri) {
                    return parsed;
                }
            }
        }
    }
    lsp_types::Uri::from_str("file:///unknown.java").expect("static URI is valid")
}

pub(crate) fn insertion_point(source: &str) -> (usize, String) {
    let Some(close_brace) = source.rfind('}') else {
        return (source.len(), "  ".to_string());
    };

    let line_start = source[..close_brace]
        .rfind('\n')
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let before_brace_on_line = &source[line_start..close_brace];
    let close_indent: String = before_brace_on_line
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect();

    let indent = if before_brace_on_line.trim().is_empty() {
        format!("{close_indent}  ")
    } else {
        "  ".to_string()
    };

    (close_brace, indent)
}

fn single_file_insert_edit(
    uri: lsp_types::Uri,
    range: lsp_types::Range,
    new_text: String,
) -> WorkspaceEdit {
    let mut changes: HashMap<lsp_types::Uri, Vec<TextEdit>> = HashMap::new();
    changes.insert(uri, vec![TextEdit { range, new_text }]);
    WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    }
}

pub(crate) fn method_stub(name: &str, indent: &str, is_static: bool) -> String {
    let static_kw = if is_static { "static " } else { "" };
    format!("\n\n{indent}private {static_kw}Object {name}(Object... args) {{ return null; }}\n")
}

fn field_stub(name: &str, indent: &str, is_static: bool) -> String {
    let static_kw = if is_static { "static " } else { "" };
    format!("\n\n{indent}private {static_kw}Object {name};\n")
}

fn unresolved_member_name(message: &str, source: &str, span: Span) -> Option<String> {
    extract_backticked_ident(message).or_else(|| {
        let snippet = source.get(span.start..span.end)?;
        extract_identifier_from_snippet(snippet)
    })
}

fn extract_backticked_ident(message: &str) -> Option<String> {
    let start = message.find('`')?;
    let rest = message.get(start + 1..)?;
    let end_rel = rest.find('`')?;
    let candidate = rest[..end_rel].trim();
    if is_java_identifier(candidate) {
        Some(candidate.to_string())
    } else {
        None
    }
}

fn extract_identifier_from_snippet(snippet: &str) -> Option<String> {
    let trimmed = snippet.trim();
    // For call expressions, drop `(...)`. For field accesses, drop receiver prefixes.
    let before_paren = trimmed.split('(').next().unwrap_or(trimmed).trim();
    let tail = before_paren
        .rsplit('.')
        .next()
        .unwrap_or(before_paren)
        .trim();
    if is_java_identifier(tail) {
        Some(tail.to_string())
    } else {
        None
    }
}

fn is_java_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !is_ident_start(first) {
        return false;
    }
    chars.all(is_ident_continue)
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c == '$' || c.is_ascii_alphabetic()
}

fn is_ident_continue(c: char) -> bool {
    is_ident_start(c) || c.is_ascii_digit()
}

fn looks_like_static_receiver(snippet: &str) -> bool {
    let trimmed = snippet.trim();
    let Some((receiver, _)) = trimmed.split_once('.') else {
        return false;
    };
    let receiver = receiver.trim();
    let receiver = receiver.rsplit('.').next().unwrap_or(receiver).trim();
    receiver
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase())
}

fn looks_like_enclosing_member_access(snippet: &str) -> bool {
    let trimmed = snippet.trim_start();
    let Some(dot) = trimmed.find('.') else {
        // Unqualified access like `foo()`/`bar` is assumed to refer to the enclosing type.
        return true;
    };
    let receiver = trimmed[..dot].trim();
    receiver == "this" || receiver == "super"
}

fn contains_word(haystack: &str, needle: &str) -> bool {
    for (idx, _) in haystack.match_indices(needle) {
        let before = haystack[..idx].chars().next_back();
        let after = haystack[idx + needle.len()..].chars().next();
        let ok_before = before.map_or(true, |c| !is_ident_continue(c));
        let ok_after = after.map_or(true, |c| !is_ident_continue(c));
        if ok_before && ok_after {
            return true;
        }
    }
    false
}

fn is_within_static_block(source: &str, offset: usize) -> bool {
    let bytes = source.as_bytes();
    let mut depth: i32 = 0;
    let mut i = offset.min(bytes.len());
    while i > 0 {
        i -= 1;
        match bytes[i] {
            b'}' => depth += 1,
            b'{' => {
                if depth == 0 {
                    let window_start = i.saturating_sub(200);
                    let before = &source[window_start..i];
                    if contains_word(before, "static") {
                        return true;
                    }
                } else {
                    depth -= 1;
                }
            }
            _ => {}
        }
    }
    false
}
