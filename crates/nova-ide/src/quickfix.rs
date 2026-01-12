use std::collections::HashMap;

use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, Position, Range, TextEdit, Uri, WorkspaceEdit,
};
use nova_core::{Name, PackageName, TypeIndex};
use nova_types::{Diagnostic, Span};

/// Generate quick fixes for unresolved type diagnostics.
///
/// Today we only surface:
/// - `Import <fqn>`
/// - `Use fully qualified name '<fqn>'`
pub(crate) fn unresolved_type_quick_fixes(
    uri: &Uri,
    source: &str,
    selection: Span,
    diagnostics: &[Diagnostic],
) -> Vec<CodeActionOrCommand> {
    let mut actions = Vec::new();

    for diagnostic in diagnostics {
        if diagnostic.code != "unresolved-type" {
            continue;
        }

        let Some(diag_span) = diagnostic.span else {
            continue;
        };

        if !spans_intersect(diag_span, selection) {
            continue;
        }

        let Some(span_text) = source.get(diag_span.start..diag_span.end) else {
            continue;
        };

        // If the type is already qualified (e.g. `java.util.List`), don't offer import/FQN fixes.
        if span_text.contains('.') {
            continue;
        }

        for fqn in import_candidates(span_text).into_iter().take(5) {
            if let Some(import_edit) = java_import_text_edit(source, &fqn) {
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: format!("Import {fqn}"),
                    kind: Some(CodeActionKind::QUICKFIX),
                    edit: Some(single_file_workspace_edit(uri, vec![import_edit])),
                    ..CodeAction::default()
                }));
            }

            let replace_edit = TextEdit {
                range: crate::text::span_to_lsp_range(source, diag_span),
                new_text: fqn.clone(),
            };
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: format!("Use fully qualified name '{fqn}'"),
                kind: Some(CodeActionKind::QUICKFIX),
                edit: Some(single_file_workspace_edit(uri, vec![replace_edit])),
                ..CodeAction::default()
            }));
        }
    }

    actions
}

fn spans_intersect(a: Span, b: Span) -> bool {
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

    // Treat zero-length spans as a point (useful for cursor-based LSP code action requests).
    if a_start == a_end {
        return b_start <= a_start && a_start < b_end;
    }
    if b_start == b_end {
        return a_start <= b_start && b_start < a_end;
    }

    a_start < b_end && b_start < a_end
}

fn single_file_workspace_edit(uri: &Uri, edits: Vec<TextEdit>) -> WorkspaceEdit {
    let mut changes = HashMap::new();
    changes.insert(uri.clone(), edits);
    WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    }
}

fn import_candidates(unresolved_name: &str) -> Vec<String> {
    let jdk = crate::code_intelligence::jdk_index();
    import_candidates_with_index(unresolved_name, jdk.as_ref())
}

fn import_candidates_with_index(unresolved_name: &str, index: &dyn TypeIndex) -> Vec<String> {
    let needle = unresolved_name.trim();
    if needle.is_empty() {
        return Vec::new();
    }

    // NOTE: This list is intentionally small and ordered by rough "how likely is a missing import
    // from here?" heuristics. We still sort/dedupe the final output for deterministic results.
    //
    // Keep this bounded: quick-fix code actions run on latency-sensitive paths, and probing the
    // entire JDK index (e.g. by enumerating all class names) can be extremely expensive.
    const COMMON_PACKAGES: &[&str] = &[
        "java.util",
        "java.util.function",
        "java.io",
        "java.time",
        "java.nio",
        "java.net",
        "java.util.concurrent",
        "java.util.stream",
        "java.lang",
    ];

    // Some very common nested types are referred to by their simple inner name (e.g. `Entry`)
    // and can be imported directly (`import java.util.Map.Entry;`). Those types are stored in
    // indices under their binary `$` names (`Map$Entry`), so we probe a small, fixed set of
    // common outers to retain the previous "nested type" coverage without enumerating the
    // entire JDK.
    const JAVA_UTIL_NESTED_OUTERS: &[&str] = &["Map"];
    const JAVA_LANG_NESTED_OUTERS: &[&str] = &["Thread"];

    let name = Name::from(needle);

    let mut out = Vec::new();
    for pkg_str in COMMON_PACKAGES {
        let pkg = PackageName::from_dotted(pkg_str);
        if let Some(ty) = index.resolve_type_in_package(&pkg, &name) {
            // JDK indices use binary names for nested types (`Outer$Inner`). Java imports use source
            // names (`Outer.Inner`), so replace `$` with `.` as a best-effort.
            out.push(ty.as_str().replace('$', "."));
        }

        let nested_outers: &[&str] = match *pkg_str {
            "java.util" => JAVA_UTIL_NESTED_OUTERS,
            "java.lang" => JAVA_LANG_NESTED_OUTERS,
            _ => &[],
        };

        for outer in nested_outers {
            let nested = Name::from(format!("{outer}${needle}"));
            if let Some(ty) = index.resolve_type_in_package(&pkg, &nested) {
                out.push(ty.as_str().replace('$', "."));
            }
        }
    }

    out.sort();
    out.dedup();
    out.truncate(5);
    out
}

// -----------------------------------------------------------------------------
// Java import insertion (best-effort)
// -----------------------------------------------------------------------------

/// Returns `None` when `path` is already imported (either exactly or via a wildcard).
/// Otherwise returns a `TextEdit` inserting the requested import at an appropriate location.
fn java_import_text_edit(text: &str, path: &str) -> Option<TextEdit> {
    let request = normalize_import_request(path)?;

    let line_ending = if text.contains("\r\n") { "\r\n" } else { "\n" };

    let mut package_insert_range: Option<(usize, usize)> = None;
    let mut first_import_start_offset: Option<usize> = None;
    let mut last_non_static_import_insert_range: Option<(usize, usize)> = None;
    let mut last_static_import_insert_range: Option<(usize, usize)> = None;

    let mut offset = 0usize;
    for segment in text.split_inclusive('\n') {
        let line_start = offset;
        let line_end = offset + segment.len();

        let mut line = segment.strip_suffix('\n').unwrap_or(segment);
        line = line.strip_suffix('\r').unwrap_or(line);

        if package_insert_range.is_none() && is_package_declaration(line) {
            if let Some(semi) = line.find(';') {
                let after = &line[semi + 1..];
                if has_code_after_semicolon(after) {
                    let ws = leading_whitespace_len(after);
                    let start = line_start + semi + 1;
                    package_insert_range = Some((start, start + ws));
                } else {
                    package_insert_range = Some((line_end, line_end));
                }
            }
        }

        if let Some((imported, is_static)) = parse_import_path(line) {
            if imported == request.path.as_str()
                || wildcard_import_covers(imported, request.path.as_str())
            {
                return None;
            }

            if first_import_start_offset.is_none() {
                first_import_start_offset = Some(line_start);
            }

            if let Some(semi) = line.find(';') {
                let after = &line[semi + 1..];
                if has_code_after_semicolon(after) {
                    let ws = leading_whitespace_len(after);
                    let start = line_start + semi + 1;
                    let range = (start, start + ws);
                    if is_static {
                        last_static_import_insert_range = Some(range);
                    } else {
                        last_non_static_import_insert_range = Some(range);
                    }
                } else {
                    let range = (line_end, line_end);
                    if is_static {
                        last_static_import_insert_range = Some(range);
                    } else {
                        last_non_static_import_insert_range = Some(range);
                    }
                }
            }
        }

        offset = line_end;
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum InsertionAnchor {
        Top,
        Package,
        Import,
    }

    let (anchor, mut start_offset, mut end_offset) = if request.is_static {
        match last_static_import_insert_range {
            Some((start, end)) => (InsertionAnchor::Import, start, end),
            None => match last_non_static_import_insert_range {
                Some((start, end)) => (InsertionAnchor::Import, start, end),
                None => match first_import_start_offset {
                    Some(offset) => (InsertionAnchor::Import, offset, offset),
                    None => match package_insert_range {
                        Some((start, end)) => (InsertionAnchor::Package, start, end),
                        None => (InsertionAnchor::Top, 0, 0),
                    },
                },
            },
        }
    } else {
        match last_non_static_import_insert_range {
            Some((start, end)) => (InsertionAnchor::Import, start, end),
            None => match first_import_start_offset {
                Some(offset) => (InsertionAnchor::Import, offset, offset),
                None => match package_insert_range {
                    Some((start, end)) => (InsertionAnchor::Package, start, end),
                    None => (InsertionAnchor::Top, 0, 0),
                },
            },
        }
    };
    start_offset = start_offset.min(text.len());
    end_offset = end_offset.min(text.len()).max(start_offset);

    // If we're inserting relative to the `package` declaration, ensure imports land *after*
    // an empty line. If the file already has a blank line after `package ...;`, skip it.
    // Otherwise insert one.
    let mut needs_blank_line = anchor == InsertionAnchor::Package;
    if needs_blank_line && start_offset == end_offset && start_offset < text.len() {
        if let Some(next) = skip_blank_line(text, start_offset) {
            start_offset = next;
            end_offset = next;
            needs_blank_line = false;
        }
    }

    let needs_prefix = start_offset > 0 && text.as_bytes()[start_offset - 1] != b'\n';
    let mut new_text = String::new();
    if needs_prefix {
        new_text.push_str(line_ending);
    }
    if needs_blank_line {
        new_text.push_str(line_ending);
    }
    new_text.push_str("import ");
    if request.is_static {
        new_text.push_str("static ");
    }
    new_text.push_str(&request.path);
    new_text.push(';');
    new_text.push_str(line_ending);

    let start_pos = crate::text::offset_to_position(text, start_offset);
    let end_pos = crate::text::offset_to_position(text, end_offset);
    Some(TextEdit {
        range: Range::new(start_pos, end_pos),
        new_text,
    })
}

#[derive(Debug)]
struct ImportRequest {
    is_static: bool,
    path: String,
}

fn normalize_import_request(raw: &str) -> Option<ImportRequest> {
    let mut path = raw.trim();
    if path.is_empty() {
        return None;
    }

    if let Some(stripped) = path.strip_prefix("import ") {
        path = stripped.trim_start();
    }

    let mut is_static = false;
    if let Some(stripped) = path.strip_prefix("static ") {
        is_static = true;
        path = stripped.trim_start();
    }

    let path = path.trim().trim_end_matches(';').trim();
    if path.is_empty() {
        return None;
    }

    Some(ImportRequest {
        is_static,
        path: path.to_string(),
    })
}

fn is_package_declaration(line: &str) -> bool {
    let trimmed = line.trim_start();
    let rest = match trimmed.strip_prefix("package") {
        Some(rest) => rest,
        None => return false,
    };
    if !rest.starts_with(char::is_whitespace) {
        return false;
    }
    trimmed.contains(';')
}

fn wildcard_import_covers(imported: &str, path: &str) -> bool {
    let Some(prefix) = imported.strip_suffix(".*") else {
        return false;
    };
    let Some(rest) = path.strip_prefix(prefix) else {
        return false;
    };
    let Some(rest) = rest.strip_prefix('.') else {
        return false;
    };
    // `import foo.*;` brings in symbols in package `foo`, not subpackages.
    !rest.contains('.')
}

fn has_code_after_semicolon(after: &str) -> bool {
    let after = after.trim_start();
    if after.is_empty() {
        return false;
    }
    if after.starts_with("//") {
        return false;
    }
    if let Some(after) = after.strip_prefix("/*") {
        let Some(end) = after.find("*/") else {
            return false;
        };
        return !after[end + 2..].trim_start().is_empty();
    }
    true
}

fn leading_whitespace_len(mut s: &str) -> usize {
    let mut consumed = 0usize;
    while let Some(ch) = s.chars().next() {
        if !ch.is_whitespace() {
            break;
        }
        consumed += ch.len_utf8();
        s = &s[ch.len_utf8()..];
    }
    consumed
}

fn parse_import_path(line: &str) -> Option<(&str, bool)> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("import")?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let mut rest = rest.trim_start();
    let mut is_static = false;

    if let Some(after_static) = rest.strip_prefix("static") {
        if after_static.starts_with(char::is_whitespace) {
            is_static = true;
            rest = after_static.trim_start();
        }
    }

    let semi = rest.find(';')?;
    Some((rest[..semi].trim(), is_static))
}

fn skip_blank_line(text: &str, offset: usize) -> Option<usize> {
    if offset >= text.len() {
        return None;
    }

    let slice = &text[offset..];
    let newline_pos = slice.find('\n').unwrap_or(slice.len());
    let mut line = &slice[..newline_pos];
    line = line.strip_suffix('\r').unwrap_or(line);
    if !line.trim().is_empty() {
        return None;
    }

    if newline_pos >= slice.len() {
        Some(text.len())
    } else {
        Some(offset + newline_pos + 1)
    }
}

#[allow(dead_code)]
fn _position_to_offset_utf16(text: &str, position: Position) -> Option<usize> {
    // `nova_core::LineIndex` offers this conversion, but this helper is only used by tests and keeps
    // the import insertion logic self-contained.
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    let mut offset: usize = 0;

    for ch in text.chars() {
        if line == position.line && col_utf16 == position.character {
            return Some(offset);
        }

        offset += ch.len_utf8();
        if ch == '\n' {
            line += 1;
            col_utf16 = 0;
        } else {
            col_utf16 += ch.len_utf16() as u32;
        }
    }

    if line == position.line && col_utf16 == position.character {
        Some(offset)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use nova_jdk::JdkIndex;

    use super::*;

    #[test]
    fn import_candidates_list_includes_java_util_list() {
        // Use the built-in (dependency-free) JDK index so this test stays fast and deterministic.
        let jdk = JdkIndex::new();
        let candidates = import_candidates_with_index("List", &jdk);
        assert_eq!(candidates, vec!["java.util.List".to_string()]);
    }

    #[test]
    fn import_candidates_entry_includes_java_util_map_entry() {
        // `Map.Entry` is a common nested type referenced by its inner name. Ensure we can still
        // suggest it without scanning the full JDK.
        let jdk = JdkIndex::new();
        let candidates = import_candidates_with_index("Entry", &jdk);
        assert_eq!(candidates, vec!["java.util.Map.Entry".to_string()]);
    }

    #[test]
    fn import_candidates_function_includes_java_util_function_function() {
        // `java.util.function` is common in modern Java and included in the built-in JDK index.
        let jdk = JdkIndex::new();
        let candidates = import_candidates_with_index("Function", &jdk);
        assert_eq!(
            candidates,
            vec!["java.util.function.Function".to_string()]
        );
    }
}
