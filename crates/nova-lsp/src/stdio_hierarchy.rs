use crate::ServerState;

use serde_json::json;

pub(super) fn handle_prepare_call_hierarchy(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: lsp_types::CallHierarchyPrepareParams =
        serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = &params.text_document_position_params.text_document.uri;

    let file_id = state.analysis.ensure_loaded(uri);
    if !state.analysis.exists(file_id) {
        return Ok(json!([]));
    }

    let items = nova_ide::code_intelligence::prepare_call_hierarchy(
        &state.analysis,
        file_id,
        params.text_document_position_params.position,
    )
    .unwrap_or_default();
    serde_json::to_value(items).map_err(|e| e.to_string())
}

pub(super) fn handle_call_hierarchy_incoming_calls(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: lsp_types::CallHierarchyIncomingCallsParams =
        serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = &params.item.uri;
    let file_id = state.analysis.ensure_loaded(uri);
    if !state.analysis.exists(file_id) {
        return Ok(json!([]));
    }

    let calls = nova_ide::code_intelligence::call_hierarchy_incoming_calls_for_item(
        &state.analysis,
        file_id,
        &params.item,
    );
    serde_json::to_value(calls).map_err(|e| e.to_string())
}

pub(super) fn handle_call_hierarchy_outgoing_calls(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: lsp_types::CallHierarchyOutgoingCallsParams =
        serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = &params.item.uri;
    let file_id = state.analysis.ensure_loaded(uri);
    if !state.analysis.exists(file_id) {
        return Ok(json!([]));
    }

    let calls = nova_ide::code_intelligence::call_hierarchy_outgoing_calls_for_item(
        &state.analysis,
        file_id,
        &params.item,
    );
    serde_json::to_value(calls).map_err(|e| e.to_string())
}

pub(super) fn handle_prepare_type_hierarchy(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: lsp_types::TypeHierarchyPrepareParams =
        serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = &params.text_document_position_params.text_document.uri;

    let file_id = state.analysis.ensure_loaded(uri);
    if !state.analysis.exists(file_id) {
        return Ok(json!([]));
    }

    let items = nova_ide::code_intelligence::prepare_type_hierarchy(
        &state.analysis,
        file_id,
        params.text_document_position_params.position,
    )
    .unwrap_or_default();
    serde_json::to_value(items).map_err(|e| e.to_string())
}

pub(super) fn handle_type_hierarchy_supertypes(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: lsp_types::TypeHierarchySupertypesParams =
        serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = &params.item.uri;
    let file_id = state.analysis.ensure_loaded(uri);
    if !state.analysis.exists(file_id) {
        return Ok(json!([]));
    }

    let items = nova_ide::code_intelligence::type_hierarchy_supertypes(
        &state.analysis,
        file_id,
        params.item.name.as_str(),
    );
    serde_json::to_value(items).map_err(|e| e.to_string())
}

pub(super) fn handle_type_hierarchy_subtypes(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: lsp_types::TypeHierarchySubtypesParams =
        serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = &params.item.uri;
    let file_id = state.analysis.ensure_loaded(uri);
    if !state.analysis.exists(file_id) {
        return Ok(json!([]));
    }

    let items = nova_ide::code_intelligence::type_hierarchy_subtypes(
        &state.analysis,
        file_id,
        params.item.name.as_str(),
    );
    serde_json::to_value(items).map_err(|e| e.to_string())
}

