use lsp_types::{
    CodeAction, CodeActionDisabled, CodeActionKind, CodeActionOrCommand, Position, Range, Uri,
    WorkspaceEdit,
};
use nova_core::{LineIndex, Position as CorePosition, TextSize};
use nova_index::Index;
use nova_index::SymbolId;
use nova_refactor::{
    change_signature as refactor_change_signature, convert_to_record, safe_delete,
    workspace_edit_to_lsp, ChangeSignature, ConvertToRecordError, ConvertToRecordOptions, FileId,
    InMemoryJavaDatabase, SafeDeleteMode, SafeDeleteOutcome, SafeDeleteTarget,
};
use schemars::schema::RootSchema;
use schemars::schema_for;
use serde::{Deserialize, Serialize};

pub const CHANGE_SIGNATURE_METHOD: &str = "nova/refactor/changeSignature";
pub const MOVE_METHOD_METHOD: &str = "nova/refactor/moveMethod";
pub const MOVE_STATIC_MEMBER_METHOD: &str = "nova/refactor/moveStaticMember";
pub const SAFE_DELETE_METHOD: &str = "nova/refactor/safeDelete";
/// `workspace/executeCommand` identifier for Safe Delete.
///
/// This is primarily intended for editor integrations that prefer `executeCommand` over custom
/// request methods.
pub const SAFE_DELETE_COMMAND: &str = "nova.safeDelete";

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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SafeDeleteParams {
    pub target: SafeDeleteTargetParam,
    pub mode: SafeDeleteMode,
}

/// Safe delete target passed over the wire.
///
/// Clients may pass either:
/// - a raw `SymbolId` (serialized as a JSON number), or
/// - a fully-tagged [`SafeDeleteTarget`] (for forward compatibility).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SafeDeleteTargetParam {
    SymbolId(SymbolId),
    Target(SafeDeleteTarget),
}

impl SafeDeleteTargetParam {
    fn into_target(self) -> SafeDeleteTarget {
        match self {
            SafeDeleteTargetParam::SymbolId(id) => SafeDeleteTarget::Symbol(id),
            SafeDeleteTargetParam::Target(target) => target,
        }
    }
}

/// Result payload for `nova/refactor/safeDelete`.
///
/// - In `safe` mode, the server returns a preview payload when usages exist.
/// - Otherwise, the server returns a standard LSP `WorkspaceEdit`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SafeDeleteResult {
    Preview(RefactorResponse),
    WorkspaceEdit(WorkspaceEdit),
}

pub fn change_signature_schema() -> RootSchema {
    schema_for!(nova_refactor::ChangeSignature)
}

/// Run Change Signature and convert the resulting edit into an LSP `WorkspaceEdit`.
///
/// The refactoring itself returns Nova's canonical [`nova_refactor::WorkspaceEdit`], which stores
/// edits as byte offsets. This helper uses the shared `workspace_edit_to_lsp` conversion to map
/// byte offsets to UTF-16 LSP positions.
pub fn change_signature_workspace_edit(
    index: &Index,
    change: &ChangeSignature,
) -> Result<WorkspaceEdit, String> {
    let edit = refactor_change_signature(index, change).map_err(|err| err.to_string())?;

    // `workspace_edit_to_lsp` needs file contents to map byte offsets to LSP UTF-16 positions.
    // Until Nova's real semantic database is available in the LSP layer, we use the small
    // in-memory database shipped with `nova-refactor`.
    let db = InMemoryJavaDatabase::new(
        index
            .files()
            .iter()
            .map(|(file, text)| (FileId::new(file.clone()), text.clone())),
    );

    workspace_edit_to_lsp(&db, &edit).map_err(|err| err.to_string())
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
        SafeDeleteOutcome::Applied { edits } => Some(CodeActionOrCommand::CodeAction(CodeAction {
            title: title_base,
            kind: Some(CodeActionKind::REFACTOR),
            edit: Some(workspace_edit_from_safe_delete(index, &edits).ok()?),
            ..CodeAction::default()
        })),
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

pub fn handle_safe_delete(
    index: &Index,
    params: SafeDeleteParams,
) -> crate::Result<SafeDeleteResult> {
    let outcome = safe_delete(index, params.target.into_target(), params.mode)
        .map_err(|err| crate::NovaLspError::InvalidParams(err.to_string()))?;

    match outcome {
        SafeDeleteOutcome::Preview { report } => {
            Ok(SafeDeleteResult::Preview(RefactorResponse::Preview {
                report,
            }))
        }
        SafeDeleteOutcome::Applied { edits } => Ok(SafeDeleteResult::WorkspaceEdit(
            workspace_edit_from_safe_delete(index, &edits)?,
        )),
    }
}

fn workspace_edit_from_safe_delete(
    index: &Index,
    edits: &[nova_refactor::TextEdit],
) -> crate::Result<WorkspaceEdit> {
    let mut changes: std::collections::HashMap<Uri, Vec<lsp_types::TextEdit>> =
        std::collections::HashMap::new();

    for edit in edits {
        let text = index.file_text(&edit.file).ok_or_else(|| {
            crate::NovaLspError::InvalidParams(format!("file `{}` not found in index", edit.file))
        })?;
        let uri: Uri = edit.file.parse().map_err(|_| {
            crate::NovaLspError::InvalidParams(format!("invalid uri: `{}`", edit.file))
        })?;

        let start = u32::try_from(edit.range.start).map_err(|_| {
            crate::NovaLspError::InvalidParams("edit range start out of bounds".into())
        })?;
        let end = u32::try_from(edit.range.end).map_err(|_| {
            crate::NovaLspError::InvalidParams("edit range end out of bounds".into())
        })?;

        let line_index = LineIndex::new(text);
        let start = line_index.position(text, TextSize::from(start));
        let end = line_index.position(text, TextSize::from(end));
        let range = Range::new(
            Position::new(start.line, start.character),
            Position::new(end.line, end.character),
        );

        changes.entry(uri).or_default().push(lsp_types::TextEdit {
            range,
            new_text: edit.replacement.clone(),
        });
    }

    // LSP clients tend to apply edits sequentially. Provide them in reverse
    // order to avoid offset shifting even if a client ignores the spec.
    for edits in changes.values_mut() {
        edits.sort_by(|a, b| {
            b.range
                .start
                .line
                .cmp(&a.range.start.line)
                .then_with(|| b.range.start.character.cmp(&a.range.start.character))
                .then_with(|| b.range.end.line.cmp(&a.range.end.line))
                .then_with(|| b.range.end.character.cmp(&a.range.end.character))
        });
    }

    Ok(WorkspaceEdit {
        changes: Some(changes),
        ..WorkspaceEdit::default()
    })
}
#[cfg(test)]
mod tests {
    use super::*;

    fn apply_lsp_edits(text: &str, edits: &[lsp_types::TextEdit]) -> String {
        let index = LineIndex::new(text);
        let core_edits: Vec<nova_core::TextEdit> = edits
            .iter()
            .map(|edit| {
                let range = nova_core::Range::new(
                    CorePosition::new(edit.range.start.line, edit.range.start.character),
                    CorePosition::new(edit.range.end.line, edit.range.end.character),
                );
                let range = index.text_range(text, range).expect("valid range");
                nova_core::TextEdit::new(range, edit.new_text.clone())
            })
            .collect();

        nova_core::apply_text_edits(text, &core_edits).expect("apply edits")
    }

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

    #[test]
    fn safe_delete_request_returns_preview_then_workspace_edit() {
        let mut files = std::collections::BTreeMap::new();
        let uri: Uri = "file:///A.java".parse().unwrap();
        let source = r#"
class A {
    public void used() {
    }

    public void entry() {
        if ("𝄞".isEmpty() && used()) {
        }
    }
}
"#;
        files.insert(uri.to_string(), source.to_string());
        let index = Index::new(files);
        let target = index.find_method("A", "used").expect("method exists").id;

        let preview = handle_safe_delete(
            &index,
            SafeDeleteParams {
                target: SafeDeleteTargetParam::SymbolId(target),
                mode: SafeDeleteMode::Safe,
            },
        )
        .expect("safe delete preview");
        let report = match preview {
            SafeDeleteResult::Preview(RefactorResponse::Preview { report }) => report,
            other => panic!("expected preview result, got {other:?}"),
        };
        assert_eq!(report.usages.len(), 1);

        let applied = handle_safe_delete(
            &index,
            SafeDeleteParams {
                target: SafeDeleteTargetParam::SymbolId(target),
                mode: SafeDeleteMode::DeleteAnyway,
            },
        )
        .expect("safe delete apply");
        let edit = match applied {
            SafeDeleteResult::WorkspaceEdit(edit) => edit,
            other => panic!("expected workspace edit result, got {other:?}"),
        };

        let Some(changes) = edit.changes else {
            panic!("expected changes map");
        };
        let edits = changes.get(&uri).expect("expected edits for A.java");
        assert!(
            edits.len() >= 2,
            "expected at least usage + declaration edit, got {}",
            edits.len()
        );

        // Ensure our LSP range conversion uses UTF-16 code units (not UTF-8 bytes).
        let usage_offset = source.find("&& used").expect("usage site") + "&& ".len();
        let prefix = &source[..usage_offset];
        let expected_line = prefix.chars().filter(|&ch| ch == '\n').count() as u32;
        let line_start = prefix.rfind('\n').map(|p| p + 1).unwrap_or(0);
        let expected_character_utf16 =
            source[line_start..usage_offset].encode_utf16().count() as u32;
        let expected_character_bytes = (usage_offset - line_start) as u32;
        assert_ne!(
            expected_character_utf16, expected_character_bytes,
            "fixture should include a non-BMP character so UTF-16 != byte offsets"
        );

        let usage_edit = edits
            .iter()
            .find(|e| e.range.start.line == expected_line && e.new_text.is_empty())
            .expect("usage delete edit");
        assert_eq!(usage_edit.range.start.character, expected_character_utf16);
        assert_ne!(usage_edit.range.start.character, expected_character_bytes);
    }

    #[test]
    fn safe_delete_request_applies_workspace_edit_when_unused() {
        let mut files = std::collections::BTreeMap::new();
        let uri: Uri = "file:///A.java".parse().unwrap();
        let source = r#"
class A {
    public void unused() {
    }

    public void entry() {
    }
}
"#;
        files.insert(uri.to_string(), source.to_string());
        let index = Index::new(files);
        let target = index.find_method("A", "unused").expect("method exists").id;

        let result = handle_safe_delete(
            &index,
            SafeDeleteParams {
                target: SafeDeleteTargetParam::Target(SafeDeleteTarget::Symbol(target)),
                mode: SafeDeleteMode::Safe,
            },
        )
        .expect("safe delete apply");
        let edit = match result {
            SafeDeleteResult::WorkspaceEdit(edit) => edit,
            other => panic!("expected workspace edit result, got {other:?}"),
        };

        let Some(changes) = edit.changes else {
            panic!("expected changes map");
        };
        let edits = changes.get(&uri).expect("expected edits for A.java");
        let updated = apply_lsp_edits(source, edits);

        assert!(!updated.contains("unused()"), "method should be removed");
        assert!(
            updated.contains("void entry()"),
            "other methods should remain"
        );
    }
}
