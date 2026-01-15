use crate::rpc_out::RpcOut;
use crate::stdio_paths::load_document_text;
use crate::ServerState;

use lsp_server::RequestId;
use lsp_types::{CodeAction, CodeActionKind, Uri as LspUri, WorkspaceEdit as LspWorkspaceEdit};
use nova_refactor::{
    code_action_for_edit, organize_imports, workspace_edit_to_lsp, FileId as RefactorFileId,
    OrganizeImportsParams,
};
use serde::Deserialize;
use serde_json::json;

pub(super) fn organize_imports_workspace_edit(
    state: &mut ServerState,
    uri: &LspUri,
    source: &str,
) -> Option<LspWorkspaceEdit> {
    if !source.contains("import") {
        return None;
    }

    let snapshot = state.refactor_snapshot(uri).ok()?;
    let file = RefactorFileId::new(uri.to_string());
    let edit = organize_imports(
        snapshot.refactor_db(),
        OrganizeImportsParams { file: file.clone() },
    )
    .ok()?;
    if edit.is_empty() {
        return None;
    }

    workspace_edit_to_lsp(snapshot.refactor_db(), &edit).ok()
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JavaOrganizeImportsRequestParams {
    uri: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct JavaOrganizeImportsResponse {
    applied: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    edit: Option<LspWorkspaceEdit>,
}

pub(super) fn handle_java_organize_imports(
    params: serde_json::Value,
    state: &mut ServerState,
    client: &crate::stdio_transport::LspClient,
) -> Result<serde_json::Value, (i32, String)> {
    let params: JavaOrganizeImportsRequestParams =
        serde_json::from_value(params).map_err(|e| (-32602, e.to_string()))?;
    let uri_string = params.uri;
    let uri = uri_string
        .parse::<LspUri>()
        .map_err(|e| (-32602, format!("invalid uri: {e}")))?;

    let Some(source) =
        load_document_text(state, &uri_string).or_else(|| load_document_text(state, uri.as_str()))
    else {
        return Err((-32602, format!("unknown document: {}", uri.as_str())));
    };

    let Some(edit) = organize_imports_workspace_edit(state, &uri, &source) else {
        return serde_json::to_value(JavaOrganizeImportsResponse {
            applied: false,
            edit: None,
        })
        .map_err(|e| (-32603, e.to_string()));
    };

    let id: RequestId = serde_json::from_value(json!(state.next_outgoing_id()))
        .map_err(|e| (-32603, e.to_string()))?;
    client
        .send_request(
            id,
            "workspace/applyEdit",
            json!({
                "label": "Organize imports",
                "edit": edit.clone(),
            }),
        )
        .map_err(|e| (-32603, e.to_string()))?;

    serde_json::to_value(JavaOrganizeImportsResponse {
        applied: true,
        edit: Some(edit),
    })
    .map_err(|e| (-32603, e.to_string()))
}

