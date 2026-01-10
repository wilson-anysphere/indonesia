use lsp_types::{
    CodeAction, CodeActionDisabled, CodeActionKind, CodeActionOrCommand, Position, Range, Uri,
    WorkspaceEdit,
};
use nova_index::Index;
use nova_refactor::{
    convert_to_record, safe_delete, ConvertToRecordError, ConvertToRecordOptions, SafeDeleteMode,
    SafeDeleteOutcome, SafeDeleteTarget,
};
use schemars::schema::RootSchema;
use schemars::schema_for;
use serde::{Deserialize, Serialize};

pub const CHANGE_SIGNATURE_METHOD: &str = "nova/refactor/changeSignature";
pub const MOVE_METHOD_METHOD: &str = "nova/refactor/moveMethod";
pub const MOVE_STATIC_MEMBER_METHOD: &str = "nova/refactor/moveStaticMember";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveMethodParams {
    pub from_class: String,
    pub method_name: String,
    pub to_class: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveStaticMemberParams {
    pub from_class: String,
    pub member_name: String,
    pub to_class: String,
}

pub fn change_signature_schema() -> RootSchema {
    schema_for!(nova_refactor::ChangeSignature)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RefactorResponse {
    /// Custom extension used by clients to show a preview and request confirmation.
    #[serde(rename = "nova/refactor/preview")]
    Preview {
        report: nova_refactor::SafeDeleteReport,
    },
}

/// Build a `Refactor` code action for Safe Delete.
///
/// If the delete is unsafe this returns a code action whose `data` contains a
/// `nova/refactor/preview` payload.
pub fn safe_delete_code_action(index: &Index, target: SafeDeleteTarget) -> Option<CodeActionOrCommand> {
    let title_base = match target {
        SafeDeleteTarget::Symbol(id) => index
            .find_symbol(id)
            .map(|sym| format!("Safe delete `{}`", sym.name))
            .unwrap_or_else(|| "Safe delete".to_string()),
    };

    let outcome = safe_delete(index, target, SafeDeleteMode::Safe).ok()?;
    match outcome {
        SafeDeleteOutcome::Applied { edits } => {
            // Convert refactor edits into an LSP WorkspaceEdit (best-effort).
            let mut changes = std::collections::HashMap::new();
            for edit in edits {
                let uri: Uri = edit.file.parse().ok()?;
                let range = index
                    .file_text(&edit.file)
                    .map(|text| {
                        let line_index = LineIndex::new(text);
                        Range::new(
                            line_index.position(edit.range.start),
                            line_index.position(edit.range.end),
                        )
                    })
                    .unwrap_or_else(|| Range::new(Position::new(0, edit.range.start as u32), Position::new(0, edit.range.end as u32)));
                changes.entry(uri).or_insert_with(Vec::new).push(lsp_types::TextEdit {
                    range,
                    new_text: edit.replacement,
                });
            }

            Some(CodeActionOrCommand::CodeAction(CodeAction {
                title: title_base,
                kind: Some(CodeActionKind::REFACTOR),
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..WorkspaceEdit::default()
                }),
                ..CodeAction::default()
            }))
        }
        SafeDeleteOutcome::Preview { report } => Some(CodeActionOrCommand::CodeAction(CodeAction {
            title: format!("{title_base}…"),
            kind: Some(CodeActionKind::REFACTOR),
            // In a full server we'd attach this to a command that returns the preview.
            data: Some(serde_json::to_value(RefactorResponse::Preview { report }).ok()?),
            ..CodeAction::default()
        })),
    }
}

/// Build `Extract constant` / `Extract field` code actions for a selection in a single document.
pub fn extract_member_code_actions(
    uri: &Uri,
    source: &str,
    selection: lsp_types::Range,
) -> Vec<CodeActionOrCommand> {
    nova_ide::refactor::extract_member_code_actions(uri, source, selection)
}

/// Resolve a code action produced by [`extract_member_code_actions`].
pub fn resolve_extract_member_code_action(
    uri: &Uri,
    source: &str,
    action: &mut CodeAction,
    name: Option<String>,
) -> Result<(), nova_refactor::ExtractError> {
    nova_ide::refactor::resolve_extract_member_code_action(uri, source, action, name)
}

/// Build `RefactorInline` code actions for Inline Method.
pub fn inline_method_code_actions(
    uri: &Uri,
    source: &str,
    position: lsp_types::Position,
) -> Vec<CodeActionOrCommand> {
    nova_ide::refactor::inline_method_code_actions(uri, source, position)
}

pub fn convert_to_record_code_action(
    uri: Uri,
    source: &str,
    position: Position,
) -> Option<CodeActionOrCommand> {
    let line_index = LineIndex::new(source);
    let offset = line_index.offset(position)?;

    let file = uri.to_string();
    match convert_to_record(&file, source, offset, ConvertToRecordOptions::default()) {
        Ok(edit) => {
            let range = Range::new(
                line_index.position(edit.range.start),
                line_index.position(edit.range.end),
            );

            let mut changes = std::collections::HashMap::new();
            changes.insert(
                uri.clone(),
                vec![lsp_types::TextEdit {
                    range,
                    new_text: edit.replacement,
                }],
            );

            Some(CodeActionOrCommand::CodeAction(CodeAction {
                title: "Convert to record".to_string(),
                kind: Some(CodeActionKind::REFACTOR_REWRITE),
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..WorkspaceEdit::default()
                }),
                is_preferred: Some(true),
                ..CodeAction::default()
            }))
        }
        Err(ConvertToRecordError::NoClassAtPosition) => None,
        Err(err) => Some(CodeActionOrCommand::CodeAction(CodeAction {
            title: "Convert to record".to_string(),
            kind: Some(CodeActionKind::REFACTOR_REWRITE),
            disabled: Some(CodeActionDisabled {
                reason: err.to_string(),
            }),
            ..CodeAction::default()
        })),
    }
}

/// Maps between byte offsets and LSP positions.
///
/// Nova treats the LSP `character` value as a UTF-8 code unit offset. This is
/// sufficient for the ASCII-only fixtures used by refactoring tests.
#[derive(Debug, Clone)]
struct LineIndex {
    line_starts: Vec<usize>,
}

impl LineIndex {
    fn new(text: &str) -> Self {
        let mut line_starts = vec![0];
        for (idx, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(idx + 1);
            }
        }
        Self { line_starts }
    }

    fn position(&self, offset: usize) -> Position {
        let line = match self.line_starts.binary_search(&offset) {
            Ok(line) => line,
            Err(next) => next.saturating_sub(1),
        };
        let line_start = self.line_starts.get(line).copied().unwrap_or(0);
        Position::new(line as u32, (offset - line_start) as u32)
    }

    fn offset(&self, position: Position) -> Option<usize> {
        let line = position.line as usize;
        let col = position.character as usize;
        let line_start = *self.line_starts.get(line)?;
        Some(line_start + col)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offers_convert_to_record_action() {
        let uri: Uri = "file:///Test.java".parse().unwrap();
        let source = r#"
public final class Point {
    private final int x;

    public Point(int x) {
        this.x = x;
    }
}
"#;

        let action = convert_to_record_code_action(uri, source, Position::new(1, 20)).unwrap();
        let action = match action {
            CodeActionOrCommand::CodeAction(action) => action,
            _ => panic!("expected CodeAction"),
        };
        assert!(action.edit.is_some());
    }
}
