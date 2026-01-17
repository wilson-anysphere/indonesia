use crate::stdio_paths::open_document_files;
use crate::stdio_text_document;
use crate::stdio_transport::LspClient;
use crate::ServerState;
use crate::{
    rpc_out::RpcOut, stdio_ai, stdio_code_action, stdio_code_lens, stdio_completion,
    stdio_execute_command, stdio_extensions, stdio_goto, stdio_hierarchy, stdio_init,
    stdio_jsonrpc, stdio_memory, stdio_organize_imports, stdio_rename, stdio_semantic_tokens,
    stdio_workspace_symbol,
};

use lsp_server::{Request, RequestId, Response, ResponseError};
use nova_index::Index;
use nova_vfs::VfsPath;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn response_error(code: i32, message: impl Into<String>) -> ResponseError {
    ResponseError {
        code,
        message: message.into(),
        data: None,
    }
}

fn invalid_params_err(message: String) -> ResponseError {
    response_error(-32602, message)
}

fn internal_err(message: String) -> ResponseError {
    response_error(-32603, message)
}

fn response_error_from_code(err: (i32, String)) -> ResponseError {
    let (code, message) = err;
    response_error(code, message)
}

fn to_value(value: impl serde::Serialize) -> Result<serde_json::Value, ResponseError> {
    serde_json::to_value(value).map_err(|err| response_error(-32603, err.to_string()))
}

fn map_nova_lsp_error(err: nova_lsp::NovaLspError) -> ResponseError {
    let (code, message) = stdio_jsonrpc::nova_lsp_error_code_message(err);
    response_error(code, message)
}

fn parse_params<T: DeserializeOwned>(params: serde_json::Value) -> Result<T, ResponseError> {
    stdio_jsonrpc::decode_params(params).map_err(invalid_params_err)
}

pub(super) fn handle_request(
    request: Request,
    cancel: CancellationToken,
    state: &mut ServerState,
    client: &LspClient,
) -> std::io::Result<Response> {
    let Request { id, method, params } = request;
    let result = handle_request_value(&method, &id, params, &cancel, state, client);

    if cancel.is_cancelled() {
        return Ok(stdio_jsonrpc::response_error(
            id,
            -32800,
            "Request cancelled",
        ));
    }

    Ok(match result {
        Ok(value) => stdio_jsonrpc::response_ok(id, value),
        Err(err) => Response {
            id,
            result: None,
            error: Some(err),
        },
    })
}

fn hardening_guard_or_error(method: &str) -> Option<ResponseError> {
    nova_lsp::hardening::record_request();
    match nova_lsp::hardening::guard_method(method) {
        Ok(()) => None,
        Err(err) => Some(map_nova_lsp_error(err)),
    }
}

fn handle_request_value(
    method: &str,
    id: &RequestId,
    params: serde_json::Value,
    cancel: &CancellationToken,
    state: &mut ServerState,
    client: &LspClient,
) -> Result<serde_json::Value, ResponseError> {
    if cancel.is_cancelled() {
        return Err(response_error(-32800, "Request cancelled"));
    }

    // LSP lifecycle: after a successful `shutdown` request, the server must not accept
    // any further requests (other than repeated `shutdown`) and should wait for `exit`.
    if state.shutdown_requested && method != "shutdown" {
        return Err(response_error(-32600, "Server is shutting down"));
    }

    match method {
        "initialize" => {
            // Capture workspace root to power CodeLens execute commands.
            stdio_init::apply_initialize_params(params, state).map_err(invalid_params_err)?;
            stdio_init::initialize_result_json().map_err(internal_err)
        }
        "shutdown" => {
            state.shutdown_requested = true;
            state.cancel_semantic_search_workspace_indexing();
            state.shutdown_distributed_router(Duration::from_secs(2));
            Ok(serde_json::Value::Null)
        }
        nova_lsp::SEMANTIC_SEARCH_INDEX_STATUS_METHOD => {
            if let Some(err) =
                hardening_guard_or_error(nova_lsp::SEMANTIC_SEARCH_INDEX_STATUS_METHOD)
            {
                return Err(err);
            }

            Ok(state.semantic_search_workspace_index_status_json())
        }
        nova_lsp::MEMORY_STATUS_METHOD => {
            if let Some(err) = hardening_guard_or_error(nova_lsp::MEMORY_STATUS_METHOD) {
                return Err(err);
            }

            stdio_memory::memory_status_payload(state).map_err(internal_err)
        }
        #[cfg(debug_assertions)]
        nova_lsp::INTERNAL_INTERRUPTIBLE_WORK_METHOD => {
            let params: Map<String, Value> = parse_params(params)?;
            let steps = match params.get("steps").and_then(|v| v.as_u64()) {
                None => return Err(response_error(-32602, "missing or invalid `steps`")),
                Some(raw) => match u32::try_from(raw) {
                    Ok(value) => value,
                    Err(err) => {
                        tracing::debug!(
                            target = "nova.lsp",
                            steps = raw,
                            error = %err,
                            "interruptibleWork steps out of range; rejecting"
                        );
                        return Err(response_error(-32602, "missing or invalid `steps`"));
                    }
                },
            };

            // NOTE: This request is intentionally only available in debug builds. It is used by
            // integration tests to validate that `$/cancelRequest` triggers Salsa cancellation and
            // that `ra_salsa::Cancelled` is treated as a normal LSP request cancellation.
            use nova_db::NovaIde as _;
            let id_json = to_value(id)?;
            let mut started_params = serde_json::Map::new();
            started_params.insert("id".to_string(), id_json);
            let started_params = serde_json::Value::Object(started_params);
            if let Err(err) = client.send_notification(
                nova_lsp::INTERNAL_INTERRUPTIBLE_WORK_STARTED_NOTIFICATION,
                started_params,
            ) {
                tracing::debug!(
                    target = "nova.lsp",
                    error = %err,
                    "failed to send interruptibleWork started notification"
                );
            }
            let value = state
                .analysis
                .salsa
                .with_snapshot(|snap| snap.interruptible_work(nova_db::FileId::from_raw(0), steps));

            let value = to_value(value)?;
            let mut result = serde_json::Map::new();
            result.insert("value".to_string(), value);
            Ok(serde_json::Value::Object(result))
        }
        nova_lsp::EXTENSIONS_STATUS_METHOD => {
            if let Some(err) = hardening_guard_or_error(nova_lsp::EXTENSIONS_STATUS_METHOD) {
                return Err(err);
            }

            // Allow `params` to be `null` or omitted.
            let params: Option<Map<String, Value>> = parse_params(params)?;
            let schema_version = match params
                .as_ref()
                .and_then(|p| p.get("schemaVersion"))
                .and_then(|v| v.as_u64())
            {
                None => None,
                Some(raw) => match u32::try_from(raw) {
                    Ok(value) => Some(value),
                    Err(err) => {
                        tracing::debug!(
                            target = "nova.lsp",
                            schema_version = raw,
                            error = %err,
                            "extensions/status schemaVersion is out of range; ignoring"
                        );
                        None
                    }
                },
            };
            if let Some(version) = schema_version {
                if version != nova_lsp::EXTENSIONS_STATUS_SCHEMA_VERSION {
                    return Err(response_error(
                        -32602,
                        format!(
                            "unsupported schemaVersion {version} (expected {})",
                            nova_lsp::EXTENSIONS_STATUS_SCHEMA_VERSION
                        ),
                    ));
                }
            }

            Ok(stdio_extensions::extensions_status_json(state))
        }
        nova_lsp::EXTENSIONS_NAVIGATION_METHOD => {
            if let Some(err) = hardening_guard_or_error(nova_lsp::EXTENSIONS_NAVIGATION_METHOD) {
                return Err(err);
            }

            stdio_extensions::handle_extensions_navigation(params, state, cancel.clone())
                .map_err(internal_err)
        }
        "textDocument/completion" => {
            stdio_completion::handle_completion(params, state, cancel.clone()).map_err(internal_err)
        }
        "textDocument/codeAction" => {
            stdio_code_action::handle_code_action(params, state, cancel.clone())
                .map_err(internal_err)
        }
        "codeAction/resolve" => {
            stdio_code_action::handle_code_action_resolve(params, state).map_err(internal_err)
        }
        "textDocument/codeLens" => {
            stdio_code_lens::handle_code_lens(params, state).map_err(internal_err)
        }
        "codeLens/resolve" => {
            stdio_code_lens::handle_code_lens_resolve(params).map_err(internal_err)
        }
        "textDocument/prepareRename" => {
            stdio_rename::handle_prepare_rename(params, state).map_err(internal_err)
        }
        "textDocument/rename" => {
            let result = stdio_rename::handle_rename(params, state);
            match result {
                Ok(value) => to_value(value),
                Err((code, message)) => Err(response_error(code, message)),
            }
        }
        "textDocument/hover" => stdio_text_document::handle_hover(params, state, cancel.clone())
            .map_err(response_error_from_code),
        "textDocument/signatureHelp" => {
            stdio_text_document::handle_signature_help(params, state, cancel.clone())
                .map_err(response_error_from_code)
        }
        "textDocument/references" => {
            stdio_text_document::handle_references(params, state, cancel.clone())
                .map_err(response_error_from_code)
        }
        "textDocument/definition" => {
            stdio_goto::handle_definition(params, state).map_err(internal_err)
        }
        "textDocument/implementation" => {
            stdio_goto::handle_implementation(params, state).map_err(internal_err)
        }
        "textDocument/declaration" => {
            stdio_goto::handle_declaration(params, state).map_err(internal_err)
        }
        "textDocument/typeDefinition" => {
            stdio_goto::handle_type_definition(params, state).map_err(internal_err)
        }
        "textDocument/documentHighlight" => {
            stdio_text_document::handle_document_highlight(params, state).map_err(internal_err)
        }
        "textDocument/foldingRange" => {
            stdio_text_document::handle_folding_range(params, state).map_err(internal_err)
        }
        "textDocument/selectionRange" => {
            stdio_text_document::handle_selection_range(params, state).map_err(internal_err)
        }
        "textDocument/prepareCallHierarchy" => {
            stdio_hierarchy::handle_prepare_call_hierarchy(params, state).map_err(internal_err)
        }
        "callHierarchy/incomingCalls" => {
            stdio_hierarchy::handle_call_hierarchy_incoming_calls(params, state)
                .map_err(internal_err)
        }
        "callHierarchy/outgoingCalls" => {
            stdio_hierarchy::handle_call_hierarchy_outgoing_calls(params, state)
                .map_err(internal_err)
        }
        "textDocument/prepareTypeHierarchy" => {
            stdio_hierarchy::handle_prepare_type_hierarchy(params, state).map_err(internal_err)
        }
        "typeHierarchy/supertypes" => {
            stdio_hierarchy::handle_type_hierarchy_supertypes(params, state).map_err(internal_err)
        }
        "typeHierarchy/subtypes" => {
            stdio_hierarchy::handle_type_hierarchy_subtypes(params, state).map_err(internal_err)
        }
        "textDocument/diagnostic" => {
            stdio_text_document::handle_document_diagnostic(params, state, cancel.clone())
                .map_err(internal_err)
        }
        "textDocument/inlayHint" => {
            stdio_text_document::handle_inlay_hints(params, state, cancel.clone())
                .map_err(internal_err)
        }
        "textDocument/semanticTokens/full" => {
            stdio_semantic_tokens::handle_semantic_tokens_full(params, state).map_err(internal_err)
        }
        "textDocument/semanticTokens/full/delta" => {
            stdio_semantic_tokens::handle_semantic_tokens_full_delta(params, state)
                .map_err(internal_err)
        }
        "textDocument/documentSymbol" => {
            stdio_text_document::handle_document_symbol(params, state).map_err(internal_err)
        }
        "completionItem/resolve" => {
            stdio_completion::handle_completion_item_resolve(params, state).map_err(internal_err)
        }
        "workspace/symbol" => {
            stdio_workspace_symbol::handle_workspace_symbol(params, state, cancel)
                .map_err(response_error_from_code)
        }
        "workspace/executeCommand" => {
            stdio_execute_command::handle_execute_command(params, state, client, cancel)
                .map_err(response_error_from_code)
        }
        #[cfg(feature = "ai")]
        nova_lsp::NOVA_COMPLETION_MORE_METHOD => {
            if let Some(err) = hardening_guard_or_error(nova_lsp::NOVA_COMPLETION_MORE_METHOD) {
                return Err(err);
            }
            stdio_completion::handle_completion_more(params, state).map_err(internal_err)
        }
        nova_lsp::DOCUMENT_FORMATTING_METHOD
        | nova_lsp::DOCUMENT_RANGE_FORMATTING_METHOD
        | nova_lsp::DOCUMENT_ON_TYPE_FORMATTING_METHOD => {
            let uri = params
                .get("textDocument")
                .and_then(|doc| doc.get("uri"))
                .and_then(|uri| uri.as_str());
            let Some(uri) = uri else {
                return Err(response_error(-32602, "missing textDocument.uri"));
            };
            let path = VfsPath::uri(uri.to_string());
            let Some(text) = state.analysis.vfs.overlay().document_text(&path) else {
                return Err(response_error(-32602, format!("unknown document: {uri}")));
            };

            match nova_lsp::handle_formatting_request(method, params, &text) {
                Ok(value) => Ok(value),
                Err(err) => Err(map_nova_lsp_error(err)),
            }
        }
        nova_lsp::JAVA_ORGANIZE_IMPORTS_METHOD => {
            if let Some(err) = hardening_guard_or_error(nova_lsp::JAVA_ORGANIZE_IMPORTS_METHOD) {
                return Err(err);
            }

            stdio_organize_imports::handle_java_organize_imports(params, state, client)
                .map_err(response_error_from_code)
        }
        nova_lsp::SAFE_DELETE_METHOD => {
            if let Some(err) = hardening_guard_or_error(nova_lsp::SAFE_DELETE_METHOD) {
                return Err(err);
            }

            let (target, mode) =
                nova_lsp::decode_safe_delete_params(params).map_err(map_nova_lsp_error)?;

            // Best-effort: build an in-memory index from open documents.
            let files = open_document_files(state);
            let index = Index::new(files);

            match nova_lsp::handle_safe_delete(&index, target, mode) {
                Ok(value) => Ok(value),
                Err(err) => Err(map_nova_lsp_error(err)),
            }
        }
        nova_lsp::CHANGE_SIGNATURE_METHOD => {
            if let Some(err) = hardening_guard_or_error(nova_lsp::CHANGE_SIGNATURE_METHOD) {
                return Err(err);
            }

            let change: nova_refactor::ChangeSignature = parse_params(params)?;

            // Best-effort: build an in-memory index from open documents.
            let files = open_document_files(state);
            let index = Index::new(files);

            match nova_lsp::change_signature_workspace_edit(&index, &change) {
                Ok(value) => to_value(value),
                Err(err) => Err(response_error(-32603, err)),
            }
        }
        nova_lsp::MOVE_METHOD_METHOD => {
            if let Some(err) = hardening_guard_or_error(nova_lsp::MOVE_METHOD_METHOD) {
                return Err(err);
            }

            let obj = params
                .as_object()
                .ok_or_else(|| response_error(-32602, "params must be an object"))?;
            let from_class = obj
                .get("fromClass")
                .and_then(|v| v.as_str())
                .ok_or_else(|| response_error(-32602, "missing required `fromClass`"))?
                .to_string();
            let method_name = obj
                .get("methodName")
                .and_then(|v| v.as_str())
                .ok_or_else(|| response_error(-32602, "missing required `methodName`"))?
                .to_string();
            let to_class = obj
                .get("toClass")
                .and_then(|v| v.as_str())
                .ok_or_else(|| response_error(-32602, "missing required `toClass`"))?
                .to_string();

            let files = open_document_files(state);
            match nova_lsp::handle_move_method(&files, from_class, method_name, to_class) {
                Ok(value) => to_value(value),
                Err(err) => Err(map_nova_lsp_error(err)),
            }
        }
        nova_lsp::MOVE_STATIC_MEMBER_METHOD => {
            if let Some(err) = hardening_guard_or_error(nova_lsp::MOVE_STATIC_MEMBER_METHOD) {
                return Err(err);
            }

            let obj = params
                .as_object()
                .ok_or_else(|| response_error(-32602, "params must be an object"))?;
            let from_class = obj
                .get("fromClass")
                .and_then(|v| v.as_str())
                .ok_or_else(|| response_error(-32602, "missing required `fromClass`"))?
                .to_string();
            let member_name = obj
                .get("memberName")
                .and_then(|v| v.as_str())
                .ok_or_else(|| response_error(-32602, "missing required `memberName`"))?
                .to_string();
            let to_class = obj
                .get("toClass")
                .and_then(|v| v.as_str())
                .ok_or_else(|| response_error(-32602, "missing required `toClass`"))?
                .to_string();

            let files = open_document_files(state);
            match nova_lsp::handle_move_static_member(&files, from_class, member_name, to_class) {
                Ok(value) => to_value(value),
                Err(err) => Err(map_nova_lsp_error(err)),
            }
        }
        _ => {
            if method.starts_with("nova/ai/") {
                if let Some(err) = hardening_guard_or_error(method) {
                    return Err(err);
                }
                stdio_ai::handle_ai_custom_request(method, params, state, client, cancel)
                    .map_err(response_error_from_code)
            } else if method.starts_with("nova/") {
                match nova_lsp::handle_custom_request_cancelable(method, params, cancel.clone()) {
                    Ok(value) => Ok(value),
                    Err(err) => Err(map_nova_lsp_error(err)),
                }
            } else {
                Err(response_error(
                    -32601,
                    format!("Method not found: {method}"),
                ))
            }
        }
    }
}
