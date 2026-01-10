use lsp_types::{CodeAction, CodeActionKind, CodeActionOrCommand, Uri, WorkspaceEdit};
use nova_index::Index;
use nova_refactor::{safe_delete, SafeDeleteMode, SafeDeleteOutcome, SafeDeleteTarget};
use serde::{Deserialize, Serialize};

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
                changes.entry(uri).or_insert_with(Vec::new).push(lsp_types::TextEdit {
                    range: lsp_types::Range {
                        start: lsp_types::Position {
                            line: 0,
                            character: edit.range.start as u32,
                        },
                        end: lsp_types::Position {
                            line: 0,
                            character: edit.range.end as u32,
                        },
                    },
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
