use crate::stdio_paths::open_document_files;
use crate::stdio_text_document;
use crate::stdio_transport::LspClient;
use crate::{
    rpc_out::RpcOut, stdio_ai, stdio_code_action, stdio_code_lens, stdio_completion,
    stdio_execute_command, stdio_extensions, stdio_goto, stdio_hierarchy, stdio_init,
    stdio_jsonrpc, stdio_memory, stdio_organize_imports, stdio_rename, stdio_semantic_tokens,
    stdio_workspace_symbol,
};
use crate::ServerState;

use lsp_server::{Request, Response};
use nova_index::Index;
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use nova_vfs::VfsPath;

pub(super) fn handle_request(
    request: Request,
    cancel: CancellationToken,
    state: &mut ServerState,
    client: &LspClient,
) -> std::io::Result<Response> {
    let Request { id, method, params } = request;
    let id_json = serde_json::to_value(&id).unwrap_or(serde_json::Value::Null);
    let response_json = handle_request_json(&method, id_json, params, &cancel, state, client)?;

    if cancel.is_cancelled() {
        return Ok(stdio_jsonrpc::response_error(id, -32800, "Request cancelled"));
    }

    Ok(stdio_jsonrpc::jsonrpc_response_to_response(id, response_json))
}

fn handle_request_json(
    method: &str,
    id: serde_json::Value,
    params: serde_json::Value,
    cancel: &CancellationToken,
    state: &mut ServerState,
    client: &LspClient,
) -> std::io::Result<serde_json::Value> {
    if cancel.is_cancelled() {
        return Ok(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32800, "message": "Request cancelled" }
        }));
    }

    // LSP lifecycle: after a successful `shutdown` request, the server must not accept
    // any further requests (other than repeated `shutdown`) and should wait for `exit`.
    if state.shutdown_requested && method != "shutdown" {
        return Ok(stdio_jsonrpc::server_shutting_down_error(id));
    }

    match method {
        "initialize" => {
            // Capture workspace root to power CodeLens execute commands.
            stdio_init::apply_initialize_params(params, state);
            Ok(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": stdio_init::initialize_result_json(),
            }))
        }
        "shutdown" => {
            state.shutdown_requested = true;
            state.cancel_semantic_search_workspace_indexing();
            state.shutdown_distributed_router(Duration::from_secs(2));
            Ok(json!({ "jsonrpc": "2.0", "id": id, "result": serde_json::Value::Null }))
        }
        nova_lsp::SEMANTIC_SEARCH_INDEX_STATUS_METHOD => {
            nova_lsp::hardening::record_request();
            if let Err(err) =
                nova_lsp::hardening::guard_method(nova_lsp::SEMANTIC_SEARCH_INDEX_STATUS_METHOD)
            {
                let (code, message) = match err {
                    nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                    nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                };
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": message }
                }));
            }

            Ok(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": state.semantic_search_workspace_index_status_json(),
            }))
        }
        nova_lsp::MEMORY_STATUS_METHOD => {
            nova_lsp::hardening::record_request();
            if let Err(err) = nova_lsp::hardening::guard_method(nova_lsp::MEMORY_STATUS_METHOD) {
                let (code, message) = match err {
                    nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                    nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                };
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": message }
                }));
            }

            Ok(match stdio_memory::memory_status_payload(state) {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Err(err) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } }),
            })
        }
        #[cfg(debug_assertions)]
        nova_lsp::INTERNAL_INTERRUPTIBLE_WORK_METHOD => {
            #[derive(Debug, Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct InterruptibleWorkParams {
                steps: u32,
            }

            let params: InterruptibleWorkParams = match serde_json::from_value(params) {
                Ok(params) => params,
                Err(err) => {
                    return Ok(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32602, "message": err.to_string() }
                    }));
                }
            };

            // NOTE: This request is intentionally only available in debug builds. It is used by
            // integration tests to validate that `$/cancelRequest` triggers Salsa cancellation and
            // that `ra_salsa::Cancelled` is treated as a normal LSP request cancellation.
            use nova_db::NovaIde as _;
            let _ = client.send_notification(
                nova_lsp::INTERNAL_INTERRUPTIBLE_WORK_STARTED_NOTIFICATION,
                json!({ "id": id.clone() }),
            );
            let value = state.analysis.salsa.with_snapshot(|snap| {
                snap.interruptible_work(nova_db::FileId::from_raw(0), params.steps)
            });

            Ok(json!({ "jsonrpc": "2.0", "id": id, "result": { "value": value } }))
        }
        nova_lsp::EXTENSIONS_STATUS_METHOD => {
            nova_lsp::hardening::record_request();
            if let Err(err) = nova_lsp::hardening::guard_method(nova_lsp::EXTENSIONS_STATUS_METHOD)
            {
                let (code, message) = match err {
                    nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                    nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                };
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": message }
                }));
            }

            #[derive(Debug, Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct ExtensionsStatusParams {
                #[serde(default)]
                schema_version: Option<u32>,
            }

            // Allow `params` to be `null` or omitted.
            let params: Option<ExtensionsStatusParams> = match serde_json::from_value(params) {
                Ok(params) => params,
                Err(err) => {
                    return Ok(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32602, "message": err.to_string() }
                    }));
                }
            };
            if let Some(version) = params.and_then(|p| p.schema_version) {
                if version != nova_lsp::EXTENSIONS_STATUS_SCHEMA_VERSION {
                    return Ok(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32602,
                            "message": format!(
                                "unsupported schemaVersion {version} (expected {})",
                                nova_lsp::EXTENSIONS_STATUS_SCHEMA_VERSION
                            )
                        }
                    }));
                }
            }

            Ok(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": stdio_extensions::extensions_status_json(state),
            }))
        }
        nova_lsp::EXTENSIONS_NAVIGATION_METHOD => {
            nova_lsp::hardening::record_request();
            if let Err(err) =
                nova_lsp::hardening::guard_method(nova_lsp::EXTENSIONS_NAVIGATION_METHOD)
            {
                let (code, message) = match err {
                    nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                    nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                };
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": message }
                }));
            }

            let result =
                stdio_extensions::handle_extensions_navigation(params, state, cancel.clone());
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/completion" => {
            let result = stdio_completion::handle_completion(params, state, cancel.clone());
            Ok(match result {
                Ok(list) => json!({ "jsonrpc": "2.0", "id": id, "result": list }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/codeAction" => {
            let result = stdio_code_action::handle_code_action(params, state, cancel.clone());
            Ok(match result {
                Ok(actions) => json!({ "jsonrpc": "2.0", "id": id, "result": actions }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "codeAction/resolve" => {
            let result = stdio_code_action::handle_code_action_resolve(params, state);
            Ok(match result {
                Ok(action) => json!({ "jsonrpc": "2.0", "id": id, "result": action }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/codeLens" => {
            let result = stdio_code_lens::handle_code_lens(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "codeLens/resolve" => {
            let result = stdio_code_lens::handle_code_lens_resolve(params);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/prepareRename" => {
            let result = stdio_rename::handle_prepare_rename(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/rename" => {
            let result = stdio_rename::handle_rename(params, state);
            Ok(match result {
                Ok(edit) => json!({ "jsonrpc": "2.0", "id": id, "result": edit }),
                Err((code, message)) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": message }
                }),
            })
        }
        "textDocument/hover" => {
            let result = stdio_text_document::handle_hover(params, state, cancel.clone());
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err((code, message)) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                }
            })
        }
        "textDocument/signatureHelp" => {
            let result = stdio_text_document::handle_signature_help(params, state, cancel.clone());
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err((code, message)) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                }
            })
        }
        "textDocument/references" => {
            let result = stdio_text_document::handle_references(params, state, cancel.clone());
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err((code, message)) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                }
            })
        }
        "textDocument/definition" => {
            let result = stdio_goto::handle_definition(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/implementation" => {
            let result = stdio_goto::handle_implementation(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/declaration" => {
            let result = stdio_goto::handle_declaration(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/typeDefinition" => {
            let result = stdio_goto::handle_type_definition(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/documentHighlight" => {
            let result = stdio_text_document::handle_document_highlight(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/foldingRange" => {
            let result = stdio_text_document::handle_folding_range(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/selectionRange" => {
            let result = stdio_text_document::handle_selection_range(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/prepareCallHierarchy" => {
            let result = stdio_hierarchy::handle_prepare_call_hierarchy(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "callHierarchy/incomingCalls" => {
            let result = stdio_hierarchy::handle_call_hierarchy_incoming_calls(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "callHierarchy/outgoingCalls" => {
            let result = stdio_hierarchy::handle_call_hierarchy_outgoing_calls(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/prepareTypeHierarchy" => {
            let result = stdio_hierarchy::handle_prepare_type_hierarchy(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "typeHierarchy/supertypes" => {
            let result = stdio_hierarchy::handle_type_hierarchy_supertypes(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "typeHierarchy/subtypes" => {
            let result = stdio_hierarchy::handle_type_hierarchy_subtypes(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/diagnostic" => {
            let result = stdio_text_document::handle_document_diagnostic(params, state, cancel.clone());
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/inlayHint" => {
            let result = stdio_text_document::handle_inlay_hints(params, state, cancel.clone());
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/semanticTokens/full" => {
            let result = stdio_semantic_tokens::handle_semantic_tokens_full(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/semanticTokens/full/delta" => {
            let result = stdio_semantic_tokens::handle_semantic_tokens_full_delta(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/documentSymbol" => {
            let result = stdio_text_document::handle_document_symbol(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "completionItem/resolve" => {
            let result = stdio_completion::handle_completion_item_resolve(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "workspace/symbol" => {
            let result = stdio_workspace_symbol::handle_workspace_symbol(params, state, cancel);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err((code, message)) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                }
            })
        }
        "workspace/executeCommand" => {
            let result = stdio_execute_command::handle_execute_command(params, state, client, cancel);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err((code, message)) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                }
            })
        }
        #[cfg(feature = "ai")]
        nova_lsp::NOVA_COMPLETION_MORE_METHOD => {
            nova_lsp::hardening::record_request();
            if let Err(err) = nova_lsp::hardening::guard_method(nova_lsp::NOVA_COMPLETION_MORE_METHOD)
            {
                let (code, message) = match err {
                    nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                    nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                };
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": message }
                }));
            }
            let result = stdio_completion::handle_completion_more(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        nova_lsp::DOCUMENT_FORMATTING_METHOD
        | nova_lsp::DOCUMENT_RANGE_FORMATTING_METHOD
        | nova_lsp::DOCUMENT_ON_TYPE_FORMATTING_METHOD => {
            let uri = params
                .get("textDocument")
                .and_then(|doc| doc.get("uri"))
                .and_then(|uri| uri.as_str());
            let Some(uri) = uri else {
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32602, "message": "missing textDocument.uri" }
                }));
            };
            let path = VfsPath::uri(uri.to_string());
            let Some(text) = state.analysis.vfs.overlay().document_text(&path) else {
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32602, "message": format!("unknown document: {uri}") }
                }));
            };

            Ok(
                match nova_lsp::handle_formatting_request(method, params, &text) {
                    Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                    Err(err) => {
                        let (code, message) = match err {
                            nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                            nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                        };
                        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                    }
                },
            )
        }
        nova_lsp::JAVA_ORGANIZE_IMPORTS_METHOD => {
            nova_lsp::hardening::record_request();
            if let Err(err) = nova_lsp::hardening::guard_method(nova_lsp::JAVA_ORGANIZE_IMPORTS_METHOD)
            {
                let (code, message) = match err {
                    nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                    nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                };
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": message }
                }));
            }
            let result = stdio_organize_imports::handle_java_organize_imports(params, state, client);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err((code, message)) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                }
            })
        }
        nova_lsp::SAFE_DELETE_METHOD => {
            nova_lsp::hardening::record_request();
            if let Err(err) = nova_lsp::hardening::guard_method(nova_lsp::SAFE_DELETE_METHOD) {
                let (code, message) = match err {
                    nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                    nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                };
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": message }
                }));
            }

            let params: nova_lsp::SafeDeleteParams = match serde_json::from_value(params) {
                Ok(params) => params,
                Err(err) => {
                    return Ok(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32602, "message": err.to_string() }
                    }));
                }
            };

            // Best-effort: build an in-memory index from open documents.
            let files = open_document_files(state);
            let index = Index::new(files);

            Ok(match nova_lsp::handle_safe_delete(&index, params) {
                Ok(result) => match serde_json::to_value(result) {
                    Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                    Err(err) => {
                        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err.to_string() } })
                    }
                },
                Err(err) => {
                    let (code, message) = match err {
                        nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                        nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                    };
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                }
            })
        }
        nova_lsp::CHANGE_SIGNATURE_METHOD => {
            nova_lsp::hardening::record_request();
            if let Err(err) = nova_lsp::hardening::guard_method(nova_lsp::CHANGE_SIGNATURE_METHOD) {
                let (code, message) = match err {
                    nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                    nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                };
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": message }
                }));
            }

            let change: nova_refactor::ChangeSignature = match serde_json::from_value(params) {
                Ok(params) => params,
                Err(err) => {
                    return Ok(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32602, "message": err.to_string() }
                    }));
                }
            };

            // Best-effort: build an in-memory index from open documents.
            let files = open_document_files(state);
            let index = Index::new(files);

            Ok(match nova_lsp::change_signature_workspace_edit(&index, &change) {
                Ok(edit) => match serde_json::to_value(edit) {
                    Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                    Err(err) => {
                        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err.to_string() } })
                    }
                },
                Err(err) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32603, "message": err }
                }),
            })
        }
        nova_lsp::MOVE_METHOD_METHOD => {
            nova_lsp::hardening::record_request();
            if let Err(err) = nova_lsp::hardening::guard_method(nova_lsp::MOVE_METHOD_METHOD) {
                let (code, message) = match err {
                    nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                    nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                };
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": message }
                }));
            }

            let params: nova_lsp::MoveMethodParams = match serde_json::from_value(params) {
                Ok(params) => params,
                Err(err) => {
                    return Ok(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32602, "message": err.to_string() }
                    }));
                }
            };

            let files = open_document_files(state);
            Ok(match nova_lsp::handle_move_method(&files, params) {
                Ok(edit) => match serde_json::to_value(edit) {
                    Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                    Err(err) => {
                        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err.to_string() } })
                    }
                },
                Err(err) => {
                    let (code, message) = match err {
                        nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                        nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                    };
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                }
            })
        }
        nova_lsp::MOVE_STATIC_MEMBER_METHOD => {
            nova_lsp::hardening::record_request();
            if let Err(err) = nova_lsp::hardening::guard_method(nova_lsp::MOVE_STATIC_MEMBER_METHOD)
            {
                let (code, message) = match err {
                    nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                    nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                };
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": message }
                }));
            }

            let params: nova_lsp::MoveStaticMemberParams = match serde_json::from_value(params) {
                Ok(params) => params,
                Err(err) => {
                    return Ok(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32602, "message": err.to_string() }
                    }));
                }
            };

            let files = open_document_files(state);
            Ok(match nova_lsp::handle_move_static_member(&files, params) {
                Ok(edit) => match serde_json::to_value(edit) {
                    Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                    Err(err) => {
                        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err.to_string() } })
                    }
                },
                Err(err) => {
                    let (code, message) = match err {
                        nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                        nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                    };
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                }
            })
        }
        _ => {
            if method.starts_with("nova/ai/") {
                nova_lsp::hardening::record_request();
                if let Err(err) = nova_lsp::hardening::guard_method(method) {
                    let (code, message) = match err {
                        nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                        nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                    };
                    return Ok(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": code, "message": message }
                    }));
                }
                let result =
                    stdio_ai::handle_ai_custom_request(method, params, state, client, cancel);
                Ok(match result {
                    Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                    Err((code, message)) => {
                        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                    }
                })
            } else if method.starts_with("nova/") {
                Ok(match nova_lsp::handle_custom_request_cancelable(method, params, cancel.clone())
                {
                    Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                    Err(err) => {
                        let (code, message) = match err {
                            nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                            nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                        };
                        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                    }
                })
            } else {
                Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32601,
                        "message": format!("Method not found: {method}")
                    }
                }))
            }
        }
    }
}

