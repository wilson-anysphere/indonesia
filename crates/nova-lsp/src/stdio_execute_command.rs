use crate::stdio_apply_edit::send_workspace_apply_edit;
use crate::stdio_paths::{load_document_text, open_document_files};
use crate::stdio_transport::LspClient;
use crate::ServerState;

use lsp_types::ExecuteCommandParams;
use nova_ide::{ExplainErrorArgs, GenerateMethodBodyArgs, GenerateTestsArgs};
use nova_index::Index;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

pub(super) fn handle_execute_command(
    params: serde_json::Value,
    state: &mut ServerState,
    client: &LspClient,
    cancel: &CancellationToken,
) -> Result<serde_json::Value, (i32, String)> {
    let params: ExecuteCommandParams = crate::stdio_jsonrpc::decode_params_with_code(params)?;

    match params.command.as_str() {
        "nova.runTest" => {
            let args: Map<String, Value> = parse_first_arg(params.arguments)?;
            let test_id = args
                .get("testId")
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "missing required `testId`".to_string()))?
                .to_string();
            let project_root = state.project_root.as_ref().ok_or_else(|| {
                (
                    -32602,
                    "missing project root (initialize.rootUri)".to_string(),
                )
            })?;

            let mut payload = Map::new();
            payload.insert(
                "projectRoot".to_string(),
                Value::String(project_root.to_string_lossy().to_string()),
            );
            payload.insert("buildTool".to_string(), Value::String("auto".to_string()));
            payload.insert(
                "tests".to_string(),
                Value::Array(vec![Value::String(test_id)]),
            );
            let payload = Value::Object(payload);
            let result = nova_lsp::handle_custom_request_cancelable(
                nova_lsp::TEST_RUN_METHOD,
                payload,
                cancel.clone(),
            )
            .map_err(crate::stdio_jsonrpc::nova_lsp_error_code_message)?;
            let mut response = Map::new();
            response.insert("ok".to_string(), Value::Bool(true));
            response.insert("kind".to_string(), Value::String("testRun".to_string()));
            response.insert("result".to_string(), result);
            Ok(Value::Object(response))
        }
        "nova.debugTest" => {
            let args: Map<String, Value> = parse_first_arg(params.arguments)?;
            let test_id = args
                .get("testId")
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "missing required `testId`".to_string()))?
                .to_string();
            let project_root = state.project_root.as_ref().ok_or_else(|| {
                (
                    -32602,
                    "missing project root (initialize.rootUri)".to_string(),
                )
            })?;
            let mut payload = Map::new();
            payload.insert(
                "projectRoot".to_string(),
                Value::String(project_root.to_string_lossy().to_string()),
            );
            payload.insert("buildTool".to_string(), Value::String("auto".to_string()));
            payload.insert("test".to_string(), Value::String(test_id));
            let payload = Value::Object(payload);
            let result = nova_lsp::handle_custom_request_cancelable(
                nova_lsp::TEST_DEBUG_CONFIGURATION_METHOD,
                payload,
                cancel.clone(),
            )
            .map_err(crate::stdio_jsonrpc::nova_lsp_error_code_message)?;
            let mut response = Map::new();
            response.insert("ok".to_string(), Value::Bool(true));
            response.insert(
                "kind".to_string(),
                Value::String("testDebugConfiguration".to_string()),
            );
            response.insert("result".to_string(), result);
            Ok(Value::Object(response))
        }
        "nova.runMain" | "nova.debugMain" => {
            let args: Map<String, Value> = parse_first_arg(params.arguments)?;
            let main_class = args
                .get("mainClass")
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "missing required `mainClass`".to_string()))?
                .to_string();
            let project_root = state.project_root.as_ref().ok_or_else(|| {
                (
                    -32602,
                    "missing project root (initialize.rootUri)".to_string(),
                )
            })?;
            let mut payload = Map::new();
            payload.insert(
                "projectRoot".to_string(),
                Value::String(project_root.to_string_lossy().to_string()),
            );
            let payload = Value::Object(payload);
            let configs_value = nova_lsp::handle_custom_request_cancelable(
                nova_lsp::DEBUG_CONFIGURATIONS_METHOD,
                payload,
                cancel.clone(),
            )
            .map_err(crate::stdio_jsonrpc::nova_lsp_error_code_message)?;
            let configs: Vec<nova_ide::DebugConfiguration> =
                serde_json::from_value(configs_value).map_err(|e| (-32603, e.to_string()))?;

            let config =
                select_debug_configuration_for_main(&configs, &main_class).ok_or_else(|| {
                    (
                        -32602,
                        format!("no debug configuration found for {main_class}"),
                    )
                })?;

            let mode = if params.command == "nova.runMain" {
                "run"
            } else {
                "debug"
            };
            let config_value = serde_json::to_value(config).map_err(|e| (-32603, e.to_string()))?;
            let mut response = Map::new();
            response.insert("ok".to_string(), Value::Bool(true));
            response.insert(
                "kind".to_string(),
                Value::String("mainConfiguration".to_string()),
            );
            response.insert("mode".to_string(), Value::String(mode.to_string()));
            response.insert("configuration".to_string(), config_value);
            Ok(Value::Object(response))
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
            crate::stdio_ai_explain::run_ai_explain_error(
                args,
                params.work_done_progress_params.work_done_token.clone(),
                state,
                client,
                cancel.clone(),
            )
        }
        nova_ide::COMMAND_GENERATE_METHOD_BODY => {
            let args: GenerateMethodBodyArgs = parse_first_arg(params.arguments)?;
            crate::stdio_ai_code_edits::run_ai_generate_method_body_apply(
                args,
                params.work_done_progress_params.work_done_token.clone(),
                state,
                client,
                cancel.clone(),
            )
        }
        nova_ide::COMMAND_GENERATE_TESTS => {
            let args: GenerateTestsArgs = parse_first_arg(params.arguments)?;
            crate::stdio_ai_code_edits::run_ai_generate_tests_apply(
                args,
                params.work_done_progress_params.work_done_token.clone(),
                state,
                client,
                cancel.clone(),
            )
        }
        nova_lsp::SAFE_DELETE_COMMAND => {
            nova_lsp::hardening::record_request();
            if let Err(err) = nova_lsp::hardening::guard_method(nova_lsp::SAFE_DELETE_METHOD) {
                return Err(crate::stdio_jsonrpc::nova_lsp_error_code_message(err));
            }

            let args = parse_first_arg_value(params.arguments)?;
            let (target, mode) = nova_lsp::decode_safe_delete_params(args)
                .map_err(crate::stdio_jsonrpc::nova_lsp_error_code_message)?;
            let files = open_document_files(state);
            let index = Index::new(files);
            let value = nova_lsp::handle_safe_delete(&index, target, mode)
                .map_err(crate::stdio_jsonrpc::nova_lsp_error_code_message)?;
            let is_workspace_edit = value.as_object().is_some_and(|obj| {
                obj.contains_key("changes") || obj.contains_key("documentChanges")
            });
            if is_workspace_edit {
                let edit: lsp_types::WorkspaceEdit =
                    serde_json::from_value(value.clone()).map_err(|e| (-32603, e.to_string()))?;
                send_workspace_apply_edit(state, client, "Safe delete", &edit)?;
            }
            Ok(value)
        }
        _ => Err((-32602, format!("unknown command: {}", params.command))),
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

fn parse_first_arg_value(
    mut args: Vec<serde_json::Value>,
) -> Result<serde_json::Value, (i32, String)> {
    if args.is_empty() {
        return Err((-32602, "missing command arguments".to_string()));
    }
    Ok(args.remove(0))
}
