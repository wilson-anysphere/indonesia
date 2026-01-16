use crate::ServerState;

use std::sync::atomic::{AtomicU64, Ordering};

static SEMANTIC_TOKENS_RESULT_ID: AtomicU64 = AtomicU64::new(1);

fn next_semantic_tokens_result_id() -> String {
    let id = SEMANTIC_TOKENS_RESULT_ID.fetch_add(1, Ordering::Relaxed);
    format!("nova-lsp-semantic-tokens:{id}")
}

pub(super) fn handle_semantic_tokens_full(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: lsp_types::SemanticTokensParams = crate::stdio_jsonrpc::decode_params(params)?;
    let uri = params.text_document.uri;

    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return Ok(serde_json::Value::Null);
    }

    let tokens = nova_ide::semantic_tokens(&state.analysis, file_id);
    let result = lsp_types::SemanticTokens {
        result_id: Some(next_semantic_tokens_result_id()),
        data: tokens,
    };
    serde_json::to_value(result).map_err(|e| e.to_string())
}

pub(super) fn handle_semantic_tokens_full_delta(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: lsp_types::SemanticTokensDeltaParams = crate::stdio_jsonrpc::decode_params(params)?;
    let uri = params.text_document.uri;

    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return Ok(serde_json::Value::Null);
    }

    let tokens = nova_ide::semantic_tokens(&state.analysis, file_id);
    let result = lsp_types::SemanticTokens {
        result_id: Some(next_semantic_tokens_result_id()),
        data: tokens,
    };
    serde_json::to_value(result).map_err(|e| e.to_string())
}
