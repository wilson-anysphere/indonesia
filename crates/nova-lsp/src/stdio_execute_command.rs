use crate::rpc_out::RpcOut;
use crate::stdio_paths::{load_document_text, open_document_files};
use crate::stdio_transport::LspClient;
use crate::ServerState;

use lsp_server::RequestId;
use nova_index::Index;
use nova_ide::{CodeReviewArgs, ExplainErrorArgs, GenerateMethodBodyArgs, GenerateTestsArgs};
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExecuteCommandParams {
    command: String,
    #[serde(default)]
    arguments: Vec<serde_json::Value>,
    /// LSP work-done progress token (if provided by the client).
    #[serde(default)]
    work_done_token: Option<serde_json::Value>,
}

pub(super) fn handle_execute_command(
    params: serde_json::Value,
    state: &mut ServerState,
    client: &LspClient,
    cancel: &CancellationToken,
) -> Result<serde_json::Value, (i32, String)> {
    let params: ExecuteCommandParams =
        serde_json::from_value(params).map_err(|e| (-32602, e.to_string()))?;

    match params.command.as_str() {
        "nova.runTest" => {
            #[derive(Debug, Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct RunTestArgs {
                test_id: String,
            }
            let args: RunTestArgs = parse_first_arg(params.arguments)?;
            let project_root = state.project_root.as_ref().ok_or_else(|| {
                (
                    -32602,
                    "missing project root (initialize.rootUri)".to_string(),
                )
            })?;

            let payload = json!({
                "projectRoot": project_root.to_string_lossy(),
                "buildTool": "auto",
                "tests": [args.test_id],
            });
            let result = nova_lsp::handle_custom_request_cancelable(
                nova_lsp::TEST_RUN_METHOD,
                payload,
                cancel.clone(),
            )
            .map_err(map_nova_lsp_error)?;
            Ok(json!({ "ok": true, "kind": "testRun", "result": result }))
        }
        "nova.debugTest" => {
            #[derive(Debug, Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct DebugTestArgs {
                test_id: String,
            }
            let args: DebugTestArgs = parse_first_arg(params.arguments)?;
            let project_root = state.project_root.as_ref().ok_or_else(|| {
                (
                    -32602,
                    "missing project root (initialize.rootUri)".to_string(),
                )
            })?;
            let payload = json!({
                "projectRoot": project_root.to_string_lossy(),
                "buildTool": "auto",
                "test": args.test_id,
            });
            let result = nova_lsp::handle_custom_request_cancelable(
                nova_lsp::TEST_DEBUG_CONFIGURATION_METHOD,
                payload,
                cancel.clone(),
            )
            .map_err(map_nova_lsp_error)?;
            Ok(json!({ "ok": true, "kind": "testDebugConfiguration", "result": result }))
        }
        "nova.runMain" | "nova.debugMain" => {
            #[derive(Debug, Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct RunMainArgs {
                main_class: String,
            }
            let args: RunMainArgs = parse_first_arg(params.arguments)?;
            let project_root = state.project_root.as_ref().ok_or_else(|| {
                (
                    -32602,
                    "missing project root (initialize.rootUri)".to_string(),
                )
            })?;
            let payload = json!({
                "projectRoot": project_root.to_string_lossy(),
            });
            let configs_value = nova_lsp::handle_custom_request_cancelable(
                nova_lsp::DEBUG_CONFIGURATIONS_METHOD,
                payload,
                cancel.clone(),
            )
            .map_err(map_nova_lsp_error)?;
            let configs: Vec<nova_ide::DebugConfiguration> =
                serde_json::from_value(configs_value).map_err(|e| (-32603, e.to_string()))?;

            let config =
                select_debug_configuration_for_main(&configs, &args.main_class).ok_or_else(|| {
                    (
                        -32602,
                        format!("no debug configuration found for {}", args.main_class),
                    )
                })?;

            let mode = if params.command == "nova.runMain" {
                "run"
            } else {
                "debug"
            };
            Ok(json!({
                "ok": true,
                "kind": "mainConfiguration",
                "mode": mode,
                "configuration": config
            }))
        }
        "nova.extractMethod" => {
            let args: nova_ide::code_action::ExtractMethodCommandArgs =
                parse_first_arg(params.arguments)?;
            let uri = args.uri.clone();
            let source = load_document_text(state, uri.as_str()).ok_or_else(|| {
                (
                    -32603,
                    format!("missing document text for `{}`", uri.as_str()),
                )
            })?;
            let edit = nova_lsp::extract_method::execute(&source, args).map_err(|e| (-32603, e))?;
            serde_json::to_value(edit).map_err(|e| (-32603, e.to_string()))
        }
        nova_ide::COMMAND_EXPLAIN_ERROR => {
            let args: ExplainErrorArgs = parse_first_arg(params.arguments)?;
            crate::stdio_ai::run_ai_explain_error(
                args,
                params.work_done_token,
                state,
                client,
                cancel.clone(),
            )
        }
        nova_ide::COMMAND_GENERATE_METHOD_BODY => {
            let args: GenerateMethodBodyArgs = parse_first_arg(params.arguments)?;
            crate::stdio_ai::run_ai_generate_method_body_apply(
                args,
                params.work_done_token,
                state,
                client,
                cancel.clone(),
            )
        }
        nova_ide::COMMAND_GENERATE_TESTS => {
            let args: GenerateTestsArgs = parse_first_arg(params.arguments)?;
            crate::stdio_ai::run_ai_generate_tests_apply(
                args,
                params.work_done_token,
                state,
                client,
                cancel.clone(),
            )
        }
        nova_ide::COMMAND_CODE_REVIEW => {
            let args: CodeReviewArgs = parse_first_arg(params.arguments)?;
            crate::stdio_ai::run_ai_code_review(
                args.diff,
                args.uri,
                params.work_done_token,
                state,
                client,
                cancel.clone(),
            )
        }
        nova_lsp::SAFE_DELETE_COMMAND => {
            nova_lsp::hardening::record_request();
            if let Err(err) = nova_lsp::hardening::guard_method(nova_lsp::SAFE_DELETE_METHOD) {
                let (code, message) = match err {
                    nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                    nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                };
                return Err((code, message));
            }

            let args: nova_lsp::SafeDeleteParams = parse_first_arg(params.arguments)?;
            let files = open_document_files(state);
            let index = Index::new(files);
            match nova_lsp::handle_safe_delete(&index, args) {
                Ok(result) => {
                    if let nova_lsp::SafeDeleteResult::WorkspaceEdit(edit) = &result {
                        let id: RequestId = serde_json::from_value(json!(state.next_outgoing_id()))
                            .map_err(|e| (-32603, e.to_string()))?;
                        client
                            .send_request(
                                id,
                                "workspace/applyEdit",
                                json!({
                                    "label": "Safe delete",
                                    "edit": edit,
                                }),
                            )
                            .map_err(|e| (-32603, e.to_string()))?;
                    }
                    serde_json::to_value(result).map_err(|e| (-32603, e.to_string()))
                }
                Err(err) => {
                    let (code, message) = match err {
                        nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                        nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                    };
                    Err((code, message))
                }
            }
        }
        _ => Err((-32602, format!("unknown command: {}", params.command))),
    }
}

fn map_nova_lsp_error(err: nova_lsp::NovaLspError) -> (i32, String) {
    match err {
        nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
        nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
    }
}

fn select_debug_configuration_for_main(
    configs: &[nova_ide::DebugConfiguration],
    main_class: &str,
) -> Option<nova_ide::DebugConfiguration> {
    configs
        .iter()
        .filter(|c| c.main_class == main_class)
        .cloned()
        .find(|c| c.name.starts_with("Run "))
        .or_else(|| configs.iter().find(|c| c.main_class == main_class).cloned())
}

fn parse_first_arg<T: serde::de::DeserializeOwned>(
    mut args: Vec<serde_json::Value>,
) -> Result<T, (i32, String)> {
    if args.is_empty() {
        return Err((-32602, "missing command arguments".to_string()));
    }
    let first = args.remove(0);
    serde_json::from_value(first).map_err(|e| (-32602, e.to_string()))
}
