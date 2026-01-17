use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use lsp_types::{CodeAction, CodeActionKind, CodeActionOrCommand, TextEdit, WorkspaceEdit};
use nova_core::{Name, TypeIndex, TypeName};
use nova_db::{Database, FileId, NovaTypeck};
use nova_jdk::JdkIndex;
use nova_types::{Diagnostic, Span};

use crate::imports::java_import_text_edit;
use crate::text::span_to_lsp_range;

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

    // Collect just the type-checking diagnostics; create-symbol quick fixes are
    // driven by (high-signal) type errors, and this avoids the overhead of also
    // running control-flow diagnostics during `textDocument/codeAction`.
    let diagnostics =
        crate::code_intelligence::with_salsa_snapshot_for_single_file(db, file, source, |snap| {
            snap.type_diagnostics(file)
        });
    create_symbol_quick_fixes_from_diagnostics(&uri, source, Some(selection), &diagnostics)
}

pub(crate) fn create_symbol_quick_fixes_from_diagnostics(
    uri: &lsp_types::Uri,
    source: &str,
    selection: Option<Span>,
    diagnostics: &[Diagnostic],
) -> Vec<CodeActionOrCommand> {
    let Some(selection) = selection else {
        return Vec::new();
    };

    // All create-symbol fixes insert into the same best-effort location.
    let (insert_offset, indent) = insertion_point(source);
    let insert_position = crate::text::offset_to_position(source, insert_offset);
    let insert_range = lsp_types::Range {
        start: insert_position,
        end: insert_position,
    };

    let mut actions = Vec::new();
    let mut seen_titles: HashSet<String> = HashSet::new();

    for diagnostic in diagnostics {
        let Some(span) = diagnostic.span else {
            continue;
        };

        if !crate::quick_fix::spans_intersect(span, selection) {
            continue;
        }

        match diagnostic.code.as_ref() {
            "unresolved-method" => {
                let Some(name) = unresolved_member_name(&diagnostic.message, source, span) else {
                    continue;
                };

                let Some(snippet) = source.get(span.start..span.end) else {
                    continue;
                };
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

                let Some(snippet) = source.get(span.start..span.end) else {
                    continue;
                };
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

    let on_own_line = before_brace_on_line.trim().is_empty();

    // If the closing brace is on its own indented line (e.g. `    }`), inserting at the brace
    // offset would split the indentation from the brace and leave trailing whitespace on the
    // previous line. Insert at the line start so the brace stays aligned.
    let insert_offset = if on_own_line && !close_indent.is_empty() {
        line_start
    } else {
        close_brace
    };

    let indent = if on_own_line {
        format!("{close_indent}  ")
    } else {
        "  ".to_string()
    };

    (insert_offset, indent)
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

pub(crate) fn field_stub(name: &str, indent: &str, is_static: bool) -> String {
    let static_kw = if is_static { "static " } else { "" };
    format!("\n\n{indent}private {static_kw}Object {name};\n")
}

pub(crate) fn unresolved_member_name(message: &str, source: &str, span: Span) -> Option<String> {
    extract_backticked_ident(message)
        .or_else(|| extract_quoted_ident(message, '\''))
        .or_else(|| {
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

fn extract_quoted_ident(message: &str, quote: char) -> Option<String> {
    let start = message.find(quote)?;
    let rest = message.get(start + quote.len_utf8()..)?;
    let end_rel = rest.find(quote)?;
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

pub(crate) fn is_java_identifier(s: &str) -> bool {
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

pub(crate) fn looks_like_static_receiver(snippet: &str) -> bool {
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

pub(crate) fn looks_like_enclosing_member_access(snippet: &str) -> bool {
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

pub(crate) fn is_within_static_block(source: &str, offset: usize) -> bool {
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

// -----------------------------------------------------------------------------
// Unresolved static member quick fixes (JDK)
// -----------------------------------------------------------------------------

const STATIC_MEMBER_OWNERS: [&str; 4] = [
    "java.lang.Math",
    "java.util.Objects",
    "java.util.Collections",
    "java.util.stream.Collectors",
];

pub(crate) fn unresolved_static_member_quick_fixes(
    source: &str,
    uri: &lsp_types::Uri,
    selection: Span,
    diagnostics: &[Diagnostic],
) -> Vec<CodeAction> {
    let mut out = Vec::new();
    let mut seen: HashSet<(String, Option<(u32, u32, u32, u32)>, String)> = HashSet::new();

    let jdk = crate::code_intelligence::jdk_index();

    let mut candidates: Vec<Span> = diagnostics
        .iter()
        .filter(|diag| {
            matches!(
                diag.code.as_ref(),
                "unresolved-method" | "unresolved-name" | "UNRESOLVED_REFERENCE"
            )
        })
        .filter_map(|diag| diag.span)
        .filter(|span| crate::quick_fix::spans_intersect(*span, selection))
        .collect();

    // Best-effort fallback: some file regions (e.g. field initializers) are not yet part of the
    // unresolved-reference diagnostics. In those cases, treat the selected identifier as
    // unresolved if it isn't declared locally.
    if candidates.is_empty() && selection.start < selection.end && selection.end <= source.len() {
        if let Some(ident) = source.get(selection.start..selection.end) {
            if is_java_identifier(ident)
                && !is_qualified(source, selection.start)
                && !crate::code_intelligence::is_declared_name(source, ident)
            {
                candidates.push(selection);
            }
        }
    }

    candidates.sort_by_key(|span| (span.start, span.end));
    candidates.dedup();

    for span in candidates {
        if is_qualified(source, span.start) {
            continue;
        }
        let ident = match source.get(span.start..span.end) {
            Some(s) => s,
            None => continue,
        };

        for owner in STATIC_MEMBER_OWNERS {
            if !static_member_exists(&jdk, owner, ident) {
                continue;
            }

            let simple_owner = owner.rsplit('.').next().unwrap_or(owner);

            // 1) Qualify fix.
            let qualify_title = format!("Qualify with {simple_owner}");
            let qualify_edit = TextEdit {
                range: span_to_lsp_range(source, span),
                new_text: format!("{simple_owner}.{ident}"),
            };
            push_code_action(&mut out, &mut seen, uri, qualify_title, qualify_edit);

            // 2) Static import fix.
            let import_path = format!("static {owner}.{ident}");
            if let Some(import_edit) = java_import_text_edit(source, &import_path) {
                let import_title = format!("Add static import {owner}.{ident}");
                push_code_action(&mut out, &mut seen, uri, import_title, import_edit);
            }
        }
    }

    out
}

fn is_qualified(text: &str, ident_start: usize) -> bool {
    let bytes = text.as_bytes();
    let mut i = ident_start;
    while i > 0 {
        i -= 1;
        let b = bytes[i];
        if (b as char).is_ascii_whitespace() {
            continue;
        }
        if b == b'.' {
            return true;
        }
        if b == b':' {
            // Handle `::` (best-effort, ignoring trivia between colons).
            let mut j = i;
            while j > 0 {
                j -= 1;
                let prev = bytes[j];
                if (prev as char).is_ascii_whitespace() {
                    continue;
                }
                return prev == b':';
            }
        }
        break;
    }
    false
}

fn static_member_exists(jdk: &JdkIndex, owner: &str, name: &str) -> bool {
    let owner = TypeName::new(owner);
    let name = Name::from(name);
    jdk.resolve_static_member(&owner, &name).is_some()
}

fn push_code_action(
    out: &mut Vec<CodeAction>,
    seen: &mut HashSet<(String, Option<(u32, u32, u32, u32)>, String)>,
    uri: &lsp_types::Uri,
    title: String,
    edit: TextEdit,
) {
    let key_range = (
        edit.range.start.line,
        edit.range.start.character,
        edit.range.end.line,
        edit.range.end.character,
    );
    let key = (title.clone(), Some(key_range), edit.new_text.clone());
    if !seen.insert(key) {
        return;
    }

    let mut changes: HashMap<lsp_types::Uri, Vec<TextEdit>> = HashMap::new();
    changes.insert(uri.clone(), vec![edit]);

    out.push(CodeAction {
        title,
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }),
        ..CodeAction::default()
    });
}
