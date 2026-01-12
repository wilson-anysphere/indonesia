use crate::text_pos::TextPos;
use lsp_types::{
    CodeAction, CodeActionDisabled, CodeActionKind, CodeActionOrCommand, Position, Range, Uri,
    WorkspaceEdit,
};
use nova_index::Index;
use nova_index::SymbolId;
use nova_refactor::{
    change_signature as refactor_change_signature, convert_to_record, extract_variable,
    inline_variable, move_method as refactor_move_method,
    move_static_member as refactor_move_static_member, safe_delete, workspace_edit_to_lsp,
    workspace_edit_to_lsp_with_uri_mapper, ChangeSignature, ConvertToRecordError,
    ConvertToRecordOptions, ExtractVariableParams, FileId, InlineVariableParams, JavaSymbolKind,
    MoveMethodParams as RefactorMoveMethodParams,
    MoveStaticMemberParams as RefactorMoveStaticMemberParams, RefactorDatabase,
    RefactorJavaDatabase, SafeDeleteMode, SafeDeleteOutcome, SafeDeleteTarget,
    SemanticRefactorError, TextDatabase, WorkspaceTextRange,
};
use schemars::schema::RootSchema;
use schemars::schema_for;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::str::FromStr;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
enum CodeActionData {
    ExtractVariable {
        start: usize,
        end: usize,
        use_var: bool,
        name: Option<String>,
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
    let SafeDeleteTarget::Symbol(target_id) = target;
    let title_base = index
        .find_symbol(target_id)
        .map(|sym| format!("Safe delete `{}`", sym.name))
        .unwrap_or_else(|| "Safe delete".to_string());

    let outcome = safe_delete(
        index,
        SafeDeleteTarget::Symbol(target_id),
        SafeDeleteMode::Safe,
    )
    .ok()?;
    match outcome {
        SafeDeleteOutcome::Applied { edit } => Some(CodeActionOrCommand::CodeAction(CodeAction {
            title: title_base,
            kind: Some(CodeActionKind::REFACTOR),
            edit: Some(workspace_edit_to_lsp(index, &edit).ok()?),
            ..CodeAction::default()
        })),
        SafeDeleteOutcome::Preview { report } => {
            let command = lsp_types::Command {
                title: format!("{title_base}…"),
                command: SAFE_DELETE_COMMAND.to_string(),
                arguments: Some(vec![serde_json::to_value(SafeDeleteParams {
                    target: SafeDeleteTargetParam::SymbolId(target_id),
                    mode: SafeDeleteMode::Safe,
                })
                .ok()?]),
            };
            Some(CodeActionOrCommand::CodeAction(CodeAction {
                title: format!("{title_base}…"),
                kind: Some(CodeActionKind::REFACTOR),
                // Attach a command so the code action is actionable even without `codeAction/resolve`.
                data: Some(serde_json::to_value(RefactorResponse::Preview { report }).ok()?),
                command: Some(command),
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

/// Build `Extract variable…` code actions for a selection in a single document.
///
/// The returned action is unresolved (it only carries `data`). Clients can
/// resolve it through [`resolve_extract_variable_code_action`], supplying a
/// variable name.
pub fn extract_variable_code_actions(
    uri: &Uri,
    source: &str,
    selection: Range,
) -> Vec<CodeActionOrCommand> {
    if !is_java_uri(uri) {
        return Vec::new();
    }

    let pos = TextPos::new(source);
    let Some(expr_range) = pos.byte_range(selection) else {
        return Vec::new();
    };
    if expr_range.len() == 0 {
        return Vec::new();
    }
    let expr_range = WorkspaceTextRange::new(expr_range.start, expr_range.end);

    let selected = source
        .get(expr_range.start..expr_range.end)
        .unwrap_or_default()
        .trim();
    if selected.is_empty() {
        return Vec::new();
    }

    // Probe the refactoring with a placeholder name to avoid offering the action
    // when it can't be applied.
    let file_path = uri.to_string();
    let file = FileId::new(file_path.clone());
    let db = TextDatabase::new([(file.clone(), source.to_string())]);

    fn probe_extract_variable_placeholder_name(
        db: &TextDatabase,
        file: &FileId,
        expr_range: WorkspaceTextRange,
        use_var: bool,
    ) -> Option<String> {
        for attempt in 0usize..100 {
            let name = if attempt == 0 {
                "extracted".to_string()
            } else {
                format!("extracted{attempt}")
            };

            match extract_variable(
                db,
                ExtractVariableParams {
                    file: file.clone(),
                    expr_range,
                    name: name.clone(),
                    use_var,
                    replace_all: false,
                },
            ) {
                Ok(_) => return Some(name),
                Err(SemanticRefactorError::InvalidIdentifier { .. }) => continue,
                // Treat name conflicts as a recoverable probe failure by trying another placeholder
                // name. The semantic refactor engine reports these as conflicts (not
                // ExtractNotSupported), so matching on the structured error keeps this resilient to
                // changes in error wording.
                Err(SemanticRefactorError::Conflicts(_)) => continue,
                Err(_) => return None,
            }
        }

        None
    }

    let mut actions = Vec::new();
    // Only offer Extract Variable when the `var` extraction variant is applicable. This preserves
    // the refactoring's "safe by default" behavior (e.g. we do not offer extraction for
    // side-effectful expressions).
    let Some(placeholder_name) =
        probe_extract_variable_placeholder_name(&db, &file, expr_range, true)
    else {
        return Vec::new();
    };

    actions.push(CodeActionOrCommand::CodeAction(CodeAction {
        title: "Extract variable…".to_string(),
        kind: Some(CodeActionKind::REFACTOR_EXTRACT),
        data: Some(
            serde_json::to_value(CodeActionData::ExtractVariable {
                start: expr_range.start,
                end: expr_range.end,
                use_var: true,
                name: Some(placeholder_name.clone()),
            })
            .expect("serializable"),
        ),
        ..CodeAction::default()
    }));

    // Offer the explicit-type extraction variant when type inference is available.
    if extract_variable(
        &db,
        ExtractVariableParams {
            file: file.clone(),
            expr_range,
            name: placeholder_name.clone(),
            use_var: false,
            replace_all: false,
        },
    )
    .is_ok()
    {
        actions.push(CodeActionOrCommand::CodeAction(CodeAction {
            title: "Extract variable… (explicit type)".to_string(),
            kind: Some(CodeActionKind::REFACTOR_EXTRACT),
            data: Some(
                serde_json::to_value(CodeActionData::ExtractVariable {
                    start: expr_range.start,
                    end: expr_range.end,
                    use_var: false,
                    name: Some(placeholder_name),
                })
                .expect("serializable"),
            ),
            ..CodeAction::default()
        }));
    }

    actions
}

/// Resolve a code action produced by [`extract_variable_code_actions`].
///
/// If `name` is provided, it overrides the stored name and enables a simple
/// "extract + rename" integration via a custom request.
pub fn resolve_extract_variable_code_action(
    uri: &Uri,
    source: &str,
    action: &mut CodeAction,
    name: Option<String>,
) -> Result<(), SemanticRefactorError> {
    let Some(data) = action.data.take() else {
        return Ok(());
    };

    let Ok(parsed) = serde_json::from_value::<CodeActionData>(data) else {
        // Not our code action; leave it unresolved.
        return Ok(());
    };

    let CodeActionData::ExtractVariable {
        start,
        end,
        use_var,
        name: stored_name,
    } = parsed;

    if start > end {
        action.disabled = Some(CodeActionDisabled {
            reason: "invalid extraction range".to_string(),
        });
        return Ok(());
    }

    let expr_range = WorkspaceTextRange::new(start, end);
    let file_path = uri.to_string();
    let file = FileId::new(file_path.clone());
    let db = TextDatabase::new([(file.clone(), source.to_string())]);

    let edit = match extract_variable(
        &db,
        ExtractVariableParams {
            file,
            expr_range,
            name: name
                .or(stored_name)
                .unwrap_or_else(|| "extracted".to_string()),
            use_var,
            replace_all: false,
        },
    ) {
        Ok(edit) => edit,
        Err(err) => {
            action.disabled = Some(CodeActionDisabled {
                reason: err.to_string(),
            });
            return Ok(());
        }
    };

    match workspace_edit_to_lsp(&db, &edit) {
        Ok(lsp_edit) => action.edit = Some(lsp_edit),
        Err(err) => {
            action.disabled = Some(CodeActionDisabled {
                reason: err.to_string(),
            });
        }
    }

    Ok(())
}

/// Build `RefactorInline` code actions for Inline Method.
pub fn inline_method_code_actions(
    uri: &Uri,
    source: &str,
    position: lsp_types::Position,
) -> Vec<CodeActionOrCommand> {
    nova_ide::refactor::inline_method_code_actions(uri, source, position)
}

/// Build `RefactorInline` code actions for Inline Variable.
pub fn inline_variable_code_actions(
    uri: &Uri,
    source: &str,
    position: Position,
) -> Vec<CodeActionOrCommand> {
    if !is_java_uri(uri) {
        return Vec::new();
    }

    let Some(offset) = TextPos::new(source).byte_offset(position) else {
        return Vec::new();
    };

    let file_path = uri.to_string();
    let file = FileId::new(file_path.clone());
    let db = RefactorJavaDatabase::single_file(file_path, source);

    let Some(symbol) = db.symbol_at(&file, offset) else {
        return Vec::new();
    };

    if db.symbol_kind(symbol) != Some(JavaSymbolKind::Local) {
        return Vec::new();
    }

    let usage_range = db.find_references(symbol).into_iter().find_map(|r| {
        if r.file != file {
            return None;
        }
        if r.range.start <= offset && offset <= r.range.end {
            Some(r.range)
        } else {
            None
        }
    });

    let mut actions = Vec::new();
    for (inline_all, title) in [
        (false, "Inline variable"),
        (true, "Inline variable (all usages)"),
    ] {
        // The single-usage variant only makes sense when the cursor is on a usage site.
        // When we're on the declaration (or otherwise not on a reference), avoid emitting a
        // disabled "Inline variable" action (InlineNoUsageAtCursor) and only offer the "all
        // usages" variant.
        if !inline_all && usage_range.is_none() {
            continue;
        }

        let edit = match inline_variable(
            &db,
            InlineVariableParams {
                symbol,
                inline_all,
                usage_range: if inline_all { None } else { usage_range },
            },
        ) {
            Ok(edit) => edit,
            Err(SemanticRefactorError::InlineNotSupported) => continue,
            Err(err) => {
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: title.to_string(),
                    kind: Some(CodeActionKind::REFACTOR_INLINE),
                    disabled: Some(CodeActionDisabled {
                        reason: err.to_string(),
                    }),
                    ..CodeAction::default()
                }));
                continue;
            }
        };

        if edit.is_empty() {
            continue;
        }

        match workspace_edit_to_lsp(&db, &edit) {
            Ok(lsp_edit) => actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: title.to_string(),
                kind: Some(CodeActionKind::REFACTOR_INLINE),
                edit: Some(lsp_edit),
                is_preferred: Some(!inline_all),
                ..CodeAction::default()
            })),
            Err(err) => actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: title.to_string(),
                kind: Some(CodeActionKind::REFACTOR_INLINE),
                disabled: Some(CodeActionDisabled {
                    reason: err.to_string(),
                }),
                ..CodeAction::default()
            })),
        }
    }

    actions
}

pub fn convert_to_record_code_action(
    uri: Uri,
    source: &str,
    position: Position,
) -> Option<CodeActionOrCommand> {
    let offset = TextPos::new(source).byte_offset(position)?;

    let file = uri.to_string();
    match convert_to_record(&file, source, offset, ConvertToRecordOptions::default()) {
        Ok(edit) => {
            let file_id = FileId::new(file.clone());
            let db = TextDatabase::new([(file_id, source.to_string())]);
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

fn is_java_uri(uri: &Uri) -> bool {
    uri.as_str().ends_with(".java")
}

fn move_refactor_workspace(
    open_files: &BTreeMap<String, String>,
) -> (
    BTreeMap<PathBuf, String>,
    TextDatabase,
    HashMap<FileId, Uri>,
) {
    let mut files = BTreeMap::new();
    let mut db_files = Vec::new();
    let mut uri_by_id = HashMap::new();

    for (uri_string, text) in open_files {
        let Ok(path) = nova_core::file_uri_to_path(uri_string) else {
            continue;
        };
        let path = path.into_path_buf();
        let file_id = FileId::new(path.to_string_lossy().into_owned());
        files.insert(path, text.clone());
        db_files.push((file_id.clone(), text.clone()));

        if let Ok(uri) = Uri::from_str(uri_string) {
            uri_by_id.insert(file_id, uri);
        }
    }

    (files, TextDatabase::new(db_files), uri_by_id)
}

pub fn handle_move_method(
    open_files: &BTreeMap<String, String>,
    params: MoveMethodParams,
) -> crate::Result<WorkspaceEdit> {
    let (files, db, uri_by_id) = move_refactor_workspace(open_files);
    let edit = refactor_move_method(
        &files,
        RefactorMoveMethodParams {
            from_class: params.from_class,
            method_name: params.method_name,
            to_class: params.to_class,
        },
    )
    .map_err(|err| crate::NovaLspError::InvalidParams(err.to_string()))?;

    workspace_edit_to_lsp_with_uri_mapper(&db, &edit, |file| {
        Ok(uri_by_id
            .get(file)
            .cloned()
            .expect("move refactor edit only touches known open files"))
    })
    .map_err(|err| crate::NovaLspError::InvalidParams(err.to_string()))
}

pub fn handle_move_static_member(
    open_files: &BTreeMap<String, String>,
    params: MoveStaticMemberParams,
) -> crate::Result<WorkspaceEdit> {
    let (files, db, uri_by_id) = move_refactor_workspace(open_files);
    let edit = refactor_move_static_member(
        &files,
        RefactorMoveStaticMemberParams {
            from_class: params.from_class,
            member_name: params.member_name,
            to_class: params.to_class,
        },
    )
    .map_err(|err| crate::NovaLspError::InvalidParams(err.to_string()))?;

    workspace_edit_to_lsp_with_uri_mapper(&db, &edit, |file| {
        Ok(uri_by_id
            .get(file)
            .cloned()
            .expect("move refactor edit only touches known open files"))
    })
    .map_err(|err| crate::NovaLspError::InvalidParams(err.to_string()))
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
    use nova_core::{LineIndex, Position as CorePosition};
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

    fn apply_workspace_edit(
        files: &BTreeMap<Uri, String>,
        edit: &WorkspaceEdit,
    ) -> BTreeMap<Uri, String> {
        let mut out = files.clone();
        let Some(changes) = &edit.changes else {
            return out;
        };

        for (uri, edits) in changes {
            let text = out.get(uri).cloned().unwrap_or_default();
            let updated = apply_lsp_edits(&text, edits);
            out.insert(uri.clone(), updated);
        }

        out
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

        assert_eq!(
            lsp_edit.range.start,
            position_utf16(source, sym.decl_range.start)
        );
        assert_eq!(
            lsp_edit.range.end,
            position_utf16(source, sym.decl_range.end)
        );
    }

    #[test]
    fn safe_delete_request_returns_edits_for_all_files() {
        let uri_a: Uri = "file:///A.java".parse().unwrap();
        let uri_b: Uri = "file:///B.java".parse().unwrap();

        let source_a = r#"
class A {
    public void used() {
    }
}
"#;

        let source_b = r#"
class B {
    public void entry() {
        new A().used();
    }
}
"#;

        let mut files = BTreeMap::new();
        files.insert(uri_a.to_string(), source_a.to_string());
        files.insert(uri_b.to_string(), source_b.to_string());
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
        assert_eq!(report.usages[0].file, uri_b.to_string());

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

        let changes = edit.changes.expect("workspace edit changes present");
        let updated_a = apply_lsp_edits(source_a, changes.get(&uri_a).expect("edits for A"));
        let updated_b = apply_lsp_edits(source_b, changes.get(&uri_b).expect("edits for B"));

        assert!(
            !updated_a.contains("void used"),
            "declaration should be removed"
        );
        assert!(!updated_b.contains("used();"), "usage should be removed");
    }

    fn test_uri(file: &str) -> Uri {
        #[cfg(windows)]
        {
            format!("file:///C:/{file}").parse().unwrap()
        }

        #[cfg(not(windows))]
        {
            format!("file:///{file}").parse().unwrap()
        }
    }

    #[test]
    fn extract_variable_not_offered_for_expression_bodied_lambda() {
        let uri = test_uri("Test.java");
        let source = r#"
import java.util.function.Function;
class C {
  void m() {
    Function<Integer,Integer> f = x -> x + 1;
  }
}
"#;
        let start = source.find("x + 1").expect("selection exists");
        let end = start + "x + 1".len();
        let selection = Range::new(position_utf16(source, start), position_utf16(source, end));

        let actions = extract_variable_code_actions(&uri, source, selection);
        assert!(
            actions.is_empty(),
            "expected no extract-variable action for expression-bodied lambda, got: {actions:?}"
        );
    }

    #[test]
    fn move_static_member_request_returns_workspace_edit() {
        let uri_a = test_uri("A.java");
        let uri_b = test_uri("B.java");
        let uri_use = test_uri("Use.java");

        let before_a = "public class A {\n    public static int add(int a, int b) {\n        return a + b;\n    }\n}\n";
        let before_b = "public class B {\n}\n";
        let before_use =
            "public class Use {\n    public int f() {\n        return A.add(1, 2);\n    }\n}\n";

        let after_a = "public class A {\n}\n";
        let after_b = "public class B {\n    public static int add(int a, int b) {\n        return a + b;\n    }\n}\n";
        let after_use =
            "public class Use {\n    public int f() {\n        return B.add(1, 2);\n    }\n}\n";

        let mut files = BTreeMap::new();
        files.insert(uri_a.clone(), before_a.to_string());
        files.insert(uri_b.clone(), before_b.to_string());
        files.insert(uri_use.clone(), before_use.to_string());

        let open_files: BTreeMap<String, String> = files
            .iter()
            .map(|(uri, text)| (uri.to_string(), text.clone()))
            .collect();

        let edit = handle_move_static_member(
            &open_files,
            MoveStaticMemberParams {
                from_class: "A".into(),
                member_name: "add".into(),
                to_class: "B".into(),
            },
        )
        .expect("move static member workspace edit");

        let updated = apply_workspace_edit(&files, &edit);
        assert_eq!(updated.get(&uri_a).map(String::as_str), Some(after_a));
        assert_eq!(updated.get(&uri_b).map(String::as_str), Some(after_b));
        assert_eq!(updated.get(&uri_use).map(String::as_str), Some(after_use));
    }

    #[test]
    fn move_method_request_returns_workspace_edit() {
        let uri_a = test_uri("A.java");
        let uri_b = test_uri("B.java");
        let uri_use = test_uri("Use.java");

        let before_a = "public class A {\n    public B b = new B();\n    int base = 10;\n\n    public int compute(int x) {\n        return base + b.inc(x);\n    }\n}\n";
        let before_b =
            "public class B {\n    public int inc(int x) {\n        return x + 1;\n    }\n}\n";
        let before_use =
            "public class Use {\n    public int f(A a) {\n        return a.compute(5);\n    }\n}\n";

        let after_a = "public class A {\n    public B b = new B();\n    int base = 10;\n}\n";
        let after_b = "public class B {\n    public int inc(int x) {\n        return x + 1;\n    }\n\n    public int compute(A a, int x) {\n        return a.base + this.inc(x);\n    }\n}\n";
        let after_use =
            "public class Use {\n    public int f(A a) {\n        return a.b.compute(a, 5);\n    }\n}\n";

        let mut files = BTreeMap::new();
        files.insert(uri_a.clone(), before_a.to_string());
        files.insert(uri_b.clone(), before_b.to_string());
        files.insert(uri_use.clone(), before_use.to_string());

        let open_files: BTreeMap<String, String> = files
            .iter()
            .map(|(uri, text)| (uri.to_string(), text.clone()))
            .collect();

        let edit = handle_move_method(
            &open_files,
            MoveMethodParams {
                from_class: "A".into(),
                method_name: "compute".into(),
                to_class: "B".into(),
            },
        )
        .expect("move method workspace edit");

        let updated = apply_workspace_edit(&files, &edit);
        assert_eq!(updated.get(&uri_a).map(String::as_str), Some(after_a));
        assert_eq!(updated.get(&uri_b).map(String::as_str), Some(after_b));
        assert_eq!(updated.get(&uri_use).map(String::as_str), Some(after_use));
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
