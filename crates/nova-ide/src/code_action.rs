use lsp_types::{
    CodeAction, CodeActionKind, Command, Diagnostic, NumberOrString, Position, Range, TextEdit,
    Uri, WorkspaceEdit,
};
use nova_core::{LineIndex, Position as CorePosition};
use nova_refactor::extract_method::{
    ExtractMethod, ExtractMethodIssue, InsertionStrategy, Visibility,
};
use nova_refactor::TextRange;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractMethodCommandArgs {
    pub uri: Uri,
    pub range: Range,
    pub name: String,
    pub visibility: Visibility,
    pub insertion_strategy: InsertionStrategy,
}

/// Produces an Extract Method code action if the selected region is extractable.
///
/// The action is surfaced as a command because the client typically needs to
/// collect additional input (method name, visibility) before the edit can be
/// generated.
pub fn extract_method_code_action(source: &str, uri: Uri, lsp_range: Range) -> Option<CodeAction> {
    let index = LineIndex::new(source);
    let range = TextRange::new(
        index
            .offset_of_position(
                source,
                CorePosition::new(lsp_range.start.line, lsp_range.start.character),
            )?
            .into(),
        index
            .offset_of_position(
                source,
                CorePosition::new(lsp_range.end.line, lsp_range.end.character),
            )?
            .into(),
    );

    // Probe analysis to see if extraction is possible; use a placeholder name.
    let probe = ExtractMethod {
        file: uri.to_string(),
        selection: range,
        name: "extracted".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let analysis = probe.analyze(source).ok()?;
    let extractable = analysis
        .issues
        .iter()
        .all(|issue| matches!(issue, ExtractMethodIssue::NameCollision { .. }));

    if extractable {
        let args = ExtractMethodCommandArgs {
            uri,
            range: lsp_range,
            name: probe.name,
            visibility: probe.visibility,
            insertion_strategy: probe.insertion_strategy,
        };

        Some(CodeAction {
            title: "Extract method…".to_string(),
            kind: Some(CodeActionKind::REFACTOR_EXTRACT),
            command: Some(Command {
                title: "Extract method".to_string(),
                command: "nova.extractMethod".to_string(),
                arguments: Some(vec![serde_json::to_value(args).ok()?]),
            }),
            ..Default::default()
        })
    } else {
        None
    }
}

/// Generate quick fixes for a code-action request based on the supplied diagnostics.
///
/// This is designed to be used by LSP layers that already have a list of diagnostics relevant to
/// the requested selection range (e.g. `CodeActionParams.context.diagnostics`).
///
/// Today this provides quick-fixes:
/// - `unresolved-type` → `Create class '<Name>'`
/// - `FLOW_UNREACHABLE` → `Remove unreachable code`
/// - `FLOW_UNASSIGNED` → `Initialize '<name>'`
/// - `type-mismatch` → `Cast to <expected>` / `Convert to String`
/// - `unresolved-import` → `Remove unresolved import`
/// - `unused-import` → `Remove unused import`
pub fn diagnostic_quick_fixes(
    source: &str,
    uri: Option<Uri>,
    selection: Range,
    diagnostics: &[Diagnostic],
) -> Vec<CodeAction> {
    let Some(uri) = uri else {
        return Vec::new();
    };

    diagnostics
        .iter()
        .flat_map(|diag| {
            create_class_quick_fix(source, &uri, &selection, diag)
                .into_iter()
                .chain(
                    remove_unreachable_code_quick_fix(source, &uri, &selection, diag).into_iter(),
                )
                .chain(
                    initialize_unassigned_local_quick_fix(source, &uri, &selection, diag)
                        .into_iter(),
                )
                .chain(remove_unused_import_quick_fix(source, &uri, &selection, diag).into_iter())
                .chain(
                    remove_unresolved_import_quick_fix(source, &uri, &selection, diag).into_iter(),
                )
                .chain(type_mismatch_quick_fixes(source, &uri, &selection, diag).into_iter())
        })
        .collect()
}

fn create_class_quick_fix(
    source: &str,
    uri: &Uri,
    selection: &Range,
    diagnostic: &Diagnostic,
) -> Option<CodeAction> {
    if diagnostic_code(diagnostic)? != "unresolved-type" {
        return None;
    }

    if !ranges_intersect(selection, &diagnostic.range) {
        return None;
    }

    let name = unresolved_type_name(&diagnostic.message)?;
    if !is_simple_type_identifier(name) {
        return None;
    }

    let insert_pos = crate::text::offset_to_position(source, source.len());
    let insert_range = Range {
        start: insert_pos,
        end: insert_pos,
    };

    let prefix = if source.ends_with('\n') { "\n" } else { "\n\n" };
    let new_text = format!("{prefix}class {name} {{\n}}\n");

    let mut changes = HashMap::new();
    changes.insert(
        uri.clone(),
        vec![TextEdit {
            range: insert_range,
            new_text,
        }],
    );

    Some(CodeAction {
        title: format!("Create class '{name}'"),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }),
        diagnostics: Some(vec![diagnostic.clone()]),
        ..CodeAction::default()
    })
}

fn remove_unreachable_code_quick_fix(
    source: &str,
    uri: &Uri,
    selection: &Range,
    diagnostic: &Diagnostic,
) -> Option<CodeAction> {
    if diagnostic_code(diagnostic)? != "FLOW_UNREACHABLE" {
        return None;
    }

    if !ranges_intersect(selection, &diagnostic.range) {
        return None;
    }

    let delete_range = full_line_range(source, &diagnostic.range)?;

    let mut changes = HashMap::new();
    changes.insert(
        uri.clone(),
        vec![TextEdit {
            range: delete_range,
            new_text: String::new(),
        }],
    );

    Some(CodeAction {
        title: "Remove unreachable code".to_string(),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }),
        diagnostics: Some(vec![diagnostic.clone()]),
        ..CodeAction::default()
    })
}

fn initialize_unassigned_local_quick_fix(
    source: &str,
    uri: &Uri,
    selection: &Range,
    diagnostic: &Diagnostic,
) -> Option<CodeAction> {
    if diagnostic_code(diagnostic)? != "FLOW_UNASSIGNED" {
        return None;
    }

    if !ranges_intersect(selection, &diagnostic.range) {
        return None;
    }

    let name = backticked_name(&diagnostic.message)?;

    let start_offset = crate::text::position_to_offset(source, diagnostic.range.start)?;
    let (line_start, indent) = line_start_and_indent(source, start_offset)?;

    let default_value = infer_default_value_for_local(source, name, start_offset);
    let line_ending = if source.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let new_text = format!("{indent}{name} = {default_value};{line_ending}");

    let insert_pos = crate::text::offset_to_position(source, line_start);
    let insert_range = Range {
        start: insert_pos,
        end: insert_pos,
    };

    let mut changes = HashMap::new();
    changes.insert(
        uri.clone(),
        vec![TextEdit {
            range: insert_range,
            new_text,
        }],
    );

    Some(CodeAction {
        title: format!("Initialize '{name}'"),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }),
        diagnostics: Some(vec![diagnostic.clone()]),
        ..CodeAction::default()
    })
}

fn remove_unused_import_quick_fix(
    source: &str,
    uri: &Uri,
    selection: &Range,
    diagnostic: &Diagnostic,
) -> Option<CodeAction> {
    if diagnostic_code(diagnostic)? != "unused-import" {
        return None;
    }

    if !ranges_intersect(selection, &diagnostic.range) {
        return None;
    }

    let delete_range = full_line_range(source, &diagnostic.range)?;

    let mut changes = HashMap::new();
    changes.insert(
        uri.clone(),
        vec![TextEdit {
            range: delete_range,
            new_text: String::new(),
        }],
    );

    Some(CodeAction {
        title: "Remove unused import".to_string(),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }),
        diagnostics: Some(vec![diagnostic.clone()]),
        ..CodeAction::default()
    })
}

fn remove_unresolved_import_quick_fix(
    source: &str,
    uri: &Uri,
    selection: &Range,
    diagnostic: &Diagnostic,
) -> Option<CodeAction> {
    if diagnostic_code(diagnostic)? != "unresolved-import" {
        return None;
    }

    if !ranges_intersect(selection, &diagnostic.range) {
        return None;
    }

    let delete_range = full_line_range(source, &diagnostic.range)?;

    let mut changes = HashMap::new();
    changes.insert(
        uri.clone(),
        vec![TextEdit {
            range: delete_range,
            new_text: String::new(),
        }],
    );

    Some(CodeAction {
        title: "Remove unresolved import".to_string(),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }),
        diagnostics: Some(vec![diagnostic.clone()]),
        ..CodeAction::default()
    })
}

fn type_mismatch_quick_fixes(
    source: &str,
    uri: &Uri,
    selection: &Range,
    diagnostic: &Diagnostic,
) -> Vec<CodeAction> {
    if diagnostic_code(diagnostic) != Some("type-mismatch") {
        return Vec::new();
    }

    if !ranges_intersect(selection, &diagnostic.range) {
        return Vec::new();
    }

    let Some((expected, _found)) = parse_type_mismatch(&diagnostic.message) else {
        return Vec::new();
    };

    let (start_pos, end_pos) = normalize_range(&diagnostic.range);
    let start_offset = crate::text::position_to_offset(source, start_pos);
    let end_offset = crate::text::position_to_offset(source, end_pos);
    let (Some(start_offset), Some(end_offset)) = (start_offset, end_offset) else {
        return Vec::new();
    };
    let (start_offset, end_offset) = (start_offset.min(end_offset), start_offset.max(end_offset));

    let expr = source
        .get(start_offset..end_offset)
        .unwrap_or_default()
        .trim();
    if expr.is_empty() {
        return Vec::new();
    }

    let range = Range {
        start: start_pos,
        end: end_pos,
    };

    fn single_replace_edit(uri: &Uri, range: Range, new_text: String) -> WorkspaceEdit {
        let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
        changes.insert(uri.clone(), vec![TextEdit { range, new_text }]);
        WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }
    }

    let mut actions = Vec::new();

    if expected == "String" {
        actions.push(CodeAction {
            title: "Convert to String".to_string(),
            kind: Some(CodeActionKind::QUICKFIX),
            edit: Some(single_replace_edit(
                uri,
                range,
                format!("String.valueOf({expr})"),
            )),
            diagnostics: Some(vec![diagnostic.clone()]),
            is_preferred: Some(true),
            ..CodeAction::default()
        });
    }

    actions.push(CodeAction {
        title: format!("Cast to {expected}"),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(single_replace_edit(uri, range, format!("({expected}) {expr}"))),
        diagnostics: Some(vec![diagnostic.clone()]),
        is_preferred: Some(expected != "String"),
        ..CodeAction::default()
    });

    actions
}

fn diagnostic_code(diagnostic: &Diagnostic) -> Option<&str> {
    match diagnostic.code.as_ref()? {
        NumberOrString::String(code) => Some(code.as_str()),
        NumberOrString::Number(_) => None,
    }
}

fn unresolved_type_name(message: &str) -> Option<&str> {
    let rest = message.strip_prefix("unresolved type `")?;
    rest.strip_suffix('`')
}

fn backticked_name(message: &str) -> Option<&str> {
    // Flow diagnostics for unassigned locals use the format:
    // `use of local `<name>` before definite assignment`
    let start = message.find('`')?;
    let rest = &message[start + 1..];
    let end = rest.find('`')?;
    Some(rest[..end].trim())
}

fn parse_type_mismatch(message: &str) -> Option<(String, String)> {
    let message = message.strip_prefix("type mismatch: expected ")?;
    let (expected, found) = message.split_once(", found ")?;
    Some((expected.trim().to_string(), found.trim().to_string()))
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

fn ranges_intersect(a: &Range, b: &Range) -> bool {
    let (a_start, a_end) = normalize_range(a);
    let (b_start, b_end) = normalize_range(b);

    if a_start == a_end {
        return position_within_range(b_start, b_end, a_start);
    }
    if b_start == b_end {
        return position_within_range(a_start, a_end, b_start);
    }

    pos_lt(&a_start, &b_end) && pos_lt(&b_start, &a_end)
}

fn normalize_range(range: &Range) -> (Position, Position) {
    if pos_leq(&range.start, &range.end) {
        (range.start, range.end)
    } else {
        (range.end, range.start)
    }
}

fn full_line_range(source: &str, range: &Range) -> Option<Range> {
    let (start, end) = normalize_range(range);
    let start_offset = crate::text::position_to_offset(source, start)?;
    let end_offset = crate::text::position_to_offset(source, end)?;

    let start_offset = start_offset.min(source.len());
    let end_offset = end_offset.min(source.len());

    let line_start = source[..start_offset]
        .rfind('\n')
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let line_end = source[end_offset..]
        .find('\n')
        .map(|idx| end_offset + idx + 1)
        .unwrap_or(source.len());

    Some(Range {
        start: crate::text::offset_to_position(source, line_start),
        end: crate::text::offset_to_position(source, line_end),
    })
}

fn line_start_and_indent(source: &str, offset: usize) -> Option<(usize, &str)> {
    let offset = offset.min(source.len());
    let line_start = source[..offset].rfind('\n').map(|idx| idx + 1).unwrap_or(0);

    let line = source.get(line_start..)?;
    let indent_len: usize = line
        .chars()
        .take_while(|ch| *ch == ' ' || *ch == '\t')
        .map(char::len_utf8)
        .sum();
    let indent = line.get(..indent_len)?;

    Some((line_start, indent))
}

fn infer_default_value_for_local(source: &str, name: &str, before_offset: usize) -> &'static str {
    let before_offset = before_offset.min(source.len());
    let prefix = &source[..before_offset];

    // Best-effort: detect primitive local declarations. For all non-primitives we default to
    // `null` anyway, so we can keep the type parsing narrow and avoid false positives.
    let pat = format!(
        r"^\s*(?:@\w+(?:\([^)]*\))?\s+)*(?:final\s+)?(?P<ty>byte|short|int|long|float|double|boolean|char)(?P<array1>(?:\[\])*)\s+{}\b",
        regex::escape(name)
    );
    let re = Regex::new(&pat).ok();

    if let Some(re) = re {
        for line in prefix.lines().rev() {
            let Some(caps) = re.captures(line) else {
                continue;
            };
            let ty = caps.name("ty").map(|m| m.as_str()).unwrap_or("");
            let array1 = caps.name("array1").map(|m| m.as_str()).unwrap_or("");

            // Handle the alternative Java array syntax: `int x[];` (brackets after the name).
            let array2 = line.contains(&format!("{name}[]"));

            if !array1.is_empty() || array2 {
                return "null";
            }

            return match ty {
                "boolean" => "false",
                "char" => "'\\0'",
                "byte" | "short" | "int" | "long" | "float" | "double" => "0",
                _ => "null",
            };
        }
    }

    "null"
}

fn position_within_range(start: Position, end: Position, pos: Position) -> bool {
    pos_leq(&start, &pos) && pos_lt(&pos, &end)
}

fn pos_lt(a: &Position, b: &Position) -> bool {
    (a.line, a.character) < (b.line, b.character)
}

fn pos_leq(a: &Position, b: &Position) -> bool {
    (a.line, a.character) <= (b.line, b.character)
}
