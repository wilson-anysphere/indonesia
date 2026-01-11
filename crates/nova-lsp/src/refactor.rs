use lsp_types::{
    CodeAction, CodeActionDisabled, CodeActionKind, CodeActionOrCommand, Position, Range, Uri,
    WorkspaceEdit,
};
use nova_core::{LineIndex, Position as CorePosition, TextSize};
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
pub fn safe_delete_code_action(
    index: &Index,
    target: SafeDeleteTarget,
) -> Option<CodeActionOrCommand> {
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
                    .and_then(|text| {
                        let start = u32::try_from(edit.range.start).ok()?;
                        let end = u32::try_from(edit.range.end).ok()?;
                        let line_index = LineIndex::new(text);
                        let start = line_index.position(text, TextSize::from(start));
                        let end = line_index.position(text, TextSize::from(end));
                        Some(Range::new(
                            Position::new(start.line, start.character),
                            Position::new(end.line, end.character),
                        ))
                    })
                    .unwrap_or_else(|| {
                        Range::new(
                            Position::new(0, edit.range.start as u32),
                            Position::new(0, edit.range.end as u32),
                        )
                    });
                changes
                    .entry(uri)
                    .or_insert_with(Vec::new)
                    .push(lsp_types::TextEdit {
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
        SafeDeleteOutcome::Preview { report } => {
            Some(CodeActionOrCommand::CodeAction(CodeAction {
                title: format!("{title_base}…"),
                kind: Some(CodeActionKind::REFACTOR),
                // In a full server we'd attach this to a command that returns the preview.
                data: Some(serde_json::to_value(RefactorResponse::Preview { report }).ok()?),
                ..CodeAction::default()
            }))
        }
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
    let offset = line_index
        .offset_of_position(source, CorePosition::new(position.line, position.character))?;
    let offset: usize = u32::from(offset) as usize;

    let file = uri.to_string();
    match convert_to_record(&file, source, offset, ConvertToRecordOptions::default()) {
        Ok(edit) => {
            let start = TextSize::from(u32::try_from(edit.range.start).ok()?);
            let end = TextSize::from(u32::try_from(edit.range.end).ok()?);
            let start = line_index.position(source, start);
            let end = line_index.position(source, end);
            let range = Range::new(
                Position::new(start.line, start.character),
                Position::new(end.line, end.character),
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
