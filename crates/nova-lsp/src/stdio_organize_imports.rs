use crate::stdio_apply_edit::send_workspace_apply_edit;
use crate::stdio_paths::load_document_text;
use crate::ServerState;

use lsp_types::{CodeAction, CodeActionKind, Uri as LspUri, WorkspaceEdit as LspWorkspaceEdit};
use nova_refactor::{
    code_action_for_edit, organize_imports, workspace_edit_to_lsp, FileId as RefactorFileId,
    OrganizeImportsParams,
};
use serde_json::{Map, Value};

pub(super) fn organize_imports_workspace_edit(
    state: &mut ServerState,
    uri: &LspUri,
    source: &str,
) -> Option<LspWorkspaceEdit> {
    if !source.contains("import") {
        return None;
    }

    let snapshot = match state.refactor_snapshot(uri) {
        Ok(snapshot) => snapshot,
        Err(err) => {
            tracing::debug!(
                target = "nova.lsp",
                uri = uri.as_str(),
                err = ?err,
                "organize imports failed to build refactor snapshot"
            );
            return None;
        }
    };
    let file = RefactorFileId::new(uri.to_string());
    let edit = match organize_imports(
        snapshot.refactor_db(),
        OrganizeImportsParams { file: file.clone() },
    ) {
        Ok(edit) => edit,
        Err(err) => {
            tracing::debug!(
                target = "nova.lsp",
                uri = uri.as_str(),
                err = ?err,
                "organize imports failed"
            );
            return None;
        }
    };
    if edit.is_empty() {
        return None;
    }

    match workspace_edit_to_lsp(snapshot.refactor_db(), &edit) {
        Ok(edit) => Some(edit),
        Err(err) => {
            tracing::debug!(
                target = "nova.lsp",
                uri = uri.as_str(),
                err = ?err,
                "organize imports produced a non-convertible workspace edit"
            );
            None
        }
    }
}

pub(super) fn organize_imports_code_action(
    state: &mut ServerState,
    uri: &LspUri,
    source: &str,
) -> Option<CodeAction> {
    let edit = organize_imports_workspace_edit(state, uri, source)?;
    Some(code_action_for_edit(
        "Organize imports",
        CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
        edit,
    ))
}

pub(super) fn handle_java_organize_imports(
    params: serde_json::Value,
    state: &mut ServerState,
    client: &crate::stdio_transport::LspClient,
) -> Result<serde_json::Value, (i32, String)> {
    let params: Map<String, Value> = crate::stdio_jsonrpc::decode_params_with_code(params)?;
    let uri_string = params
        .get("uri")
        .and_then(|v| v.as_str())
        .ok_or_else(|| (-32602, "missing required `uri`".to_string()))?
        .to_string();
    let uri = uri_string
        .parse::<LspUri>()
        .map_err(|e| (-32602, format!("invalid uri: {e}")))?;

    let Some(source) =
        load_document_text(state, &uri_string).or_else(|| load_document_text(state, uri.as_str()))
    else {
        return Err((-32602, format!("unknown document: {}", uri.as_str())));
    };

    let Some(edit) = organize_imports_workspace_edit(state, &uri, &source) else {
        let mut response = Map::new();
        response.insert("applied".to_string(), Value::Bool(false));
        return Ok(Value::Object(response));
    };

    send_workspace_apply_edit(state, client, "Organize imports", &edit)?;

    let mut response = Map::new();
    response.insert("applied".to_string(), Value::Bool(true));
    response.insert(
        "edit".to_string(),
        serde_json::to_value(edit).map_err(|e| (-32603, e.to_string()))?,
    );
    Ok(Value::Object(response))
}
