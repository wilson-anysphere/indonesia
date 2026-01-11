use lsp_types::{
    CodeAction, CodeActionDisabled, CodeActionKind, CodeActionOrCommand, Position, Range, Uri,
    WorkspaceEdit,
};
use nova_core::{LineIndex, Position as CorePosition};
use nova_index::Index;
use nova_index::SymbolId;
use nova_refactor::{
    change_signature as refactor_change_signature, convert_to_record, safe_delete, workspace_edit_to_lsp,
    ChangeSignature, ConvertToRecordError, ConvertToRecordOptions, FileId, SafeDeleteMode,
    SafeDeleteOutcome, SafeDeleteTarget, TextDatabase, WorkspaceEdit as RefactorWorkspaceEdit,
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
    workspace_edit_to_lsp(index, &edit).map_err(|err| err.to_string())
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
    let title_base = match &target {
        SafeDeleteTarget::Symbol(id) => index
            .find_symbol(*id)
            .map(|sym| format!("Safe delete `{}`", sym.name))
            .unwrap_or_else(|| "Safe delete".to_string()),
    };

    let outcome = safe_delete(index, target, SafeDeleteMode::Safe).ok()?;
    match outcome {
        SafeDeleteOutcome::Applied { edit } => Some(CodeActionOrCommand::CodeAction(CodeAction {
            title: title_base,
            kind: Some(CodeActionKind::REFACTOR),
            edit: Some(workspace_edit_to_lsp(index, &edit).ok()?),
            ..CodeAction::default()
        })),
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
    let offset = line_index
        .offset_of_position(source, CorePosition::new(position.line, position.character))?;
    let offset: usize = u32::from(offset) as usize;

    let file = uri.to_string();
    match convert_to_record(&file, source, offset, ConvertToRecordOptions::default()) {
        Ok(edit) => {
            // Convert legacy (safe-delete style) `TextEdit` into Nova's canonical `WorkspaceEdit`
            // and then reuse the shared LSP conversion helper for UTF-16 correctness.
            let file_id = FileId::new(file.clone());
            let db = TextDatabase::new([(file_id, source.to_string())]);
            let edit = RefactorWorkspaceEdit::new(vec![edit.into()]);
            let lsp_edit = workspace_edit_to_lsp(&db, &edit).ok()?;

            Some(CodeActionOrCommand::CodeAction(CodeAction {
                title: "Convert to record".to_string(),
                kind: Some(CodeActionKind::REFACTOR_REWRITE),
                edit: Some(lsp_edit),
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
        SafeDeleteOutcome::Applied { edit } => {
            let edit = workspace_edit_to_lsp(index, &edit)
                .map_err(|err| crate::NovaLspError::InvalidParams(err.to_string()))?;
            Ok(SafeDeleteResult::WorkspaceEdit(edit))
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn position_utf16(text: &str, offset: usize) -> Position {
        let offset = offset.min(text.len());
        let prefix = &text[..offset];
        let line = prefix.bytes().filter(|b| *b == b'\n').count() as u32;
        let line_start = prefix.rfind('\n').map(|p| p + 1).unwrap_or(0);
        let col = text[line_start..offset].encode_utf16().count() as u32;
        Position::new(line, col)
    }

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

    #[test]
    fn safe_delete_lsp_workspace_edit_uses_utf16_character_offsets() {
        let uri: Uri = "file:///Test.java".parse().unwrap();
        let source = concat!(
            "class A {\n",
            "  private void unused() { String s = \"😀\"; }\n",
            "}\n",
        );

        let mut files = BTreeMap::new();
        files.insert(uri.to_string(), source.to_string());
        let index = Index::new(files);

        let target = index.find_method("A", "unused").expect("method exists").id;
        let sym = index.find_symbol(target).expect("symbol exists");

        let action = safe_delete_code_action(&index, SafeDeleteTarget::Symbol(target))
            .expect("code action emitted");
        let action = match action {
            CodeActionOrCommand::CodeAction(action) => action,
            _ => panic!("expected CodeAction"),
        };

        let edit = action.edit.expect("workspace edit present");
        let changes = edit.changes.expect("workspace edit changes present");
        let edits = changes.get(&uri).expect("edits for uri present");
        assert_eq!(edits.len(), 1);

        let lsp_edit = &edits[0];
        assert_eq!(lsp_edit.new_text, "");

        assert_eq!(lsp_edit.range.start, position_utf16(source, sym.decl_range.start));
        assert_eq!(lsp_edit.range.end, position_utf16(source, sym.decl_range.end));
    }

    #[test]
    fn change_signature_workspace_edit_uses_utf16_character_offsets() {
        let uri: Uri = "file:///A.java".parse().unwrap();
        let source = concat!(
            "class A {\n",
            "    int sum(int a, int b) {\n",
            "        return a + b;\n",
            "    }\n",
            "\n",
            "    void test() {\n",
            "        int 𝒂 = sum(1, 2);\n",
            "    }\n",
            "}\n",
        );

        let mut files = BTreeMap::new();
        files.insert(uri.to_string(), source.to_string());
        let index = Index::new(files);
        let target = index.find_method("A", "sum").expect("method exists").id;

        let change = ChangeSignature {
            target: nova_types::MethodId(target.0),
            new_name: None,
            parameters: vec![
                nova_refactor::ParameterOperation::Existing {
                    old_index: 1,
                    new_name: None,
                    new_type: None,
                },
                nova_refactor::ParameterOperation::Existing {
                    old_index: 0,
                    new_name: None,
                    new_type: None,
                },
            ],
            new_return_type: None,
            new_throws: None,
            propagate_hierarchy: nova_refactor::HierarchyPropagation::None,
        };

        let edit = change_signature_workspace_edit(&index, &change).expect("workspace edit");
        let changes = edit.changes.expect("expected changes map");
        let edits = changes.get(&uri).expect("edits for uri present");

        let call_offset = source.find("sum(1, 2)").expect("call exists");
        let call_end = call_offset + "sum(1, 2)".len();
        let expected_start = position_utf16(source, call_offset);
        let expected_end = position_utf16(source, call_end);

        let call_edit = edits
            .iter()
            .find(|edit| edit.new_text == "sum(2, 1)")
            .expect("call edit");
        assert_eq!(call_edit.range.start, expected_start);
        assert_eq!(call_edit.range.end, expected_end);

        // Sanity check: applying the edits should update both signature and call site.
        let updated = apply_lsp_edits(source, edits);
        assert!(updated.contains("int sum(int b, int a)"));
        assert!(updated.contains("sum(2, 1)"));
    }
}
