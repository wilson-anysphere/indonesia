use lsp_types::{
    CodeAction, CodeActionKind, Command, Diagnostic, NumberOrString, Position, Range, TextEdit,
    Uri, WorkspaceEdit,
};
use nova_core::{LineIndex, Position as CorePosition};
use nova_refactor::extract_method::{
    ExtractMethod, ExtractMethodIssue, InsertionStrategy, Visibility,
};
use nova_refactor::TextRange;
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
/// Today this provides a single quick-fix:
/// - `unresolved-type` → `Create class '<Name>'`
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
        .filter_map(|diag| create_class_quick_fix(source, &uri, &selection, diag))
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

fn position_within_range(start: Position, end: Position, pos: Position) -> bool {
    pos_leq(&start, &pos) && pos_lt(&pos, &end)
}

fn pos_lt(a: &Position, b: &Position) -> bool {
    (a.line, a.character) < (b.line, b.character)
}

fn pos_leq(a: &Position, b: &Position) -> bool {
    (a.line, a.character) <= (b.line, b.character)
}
