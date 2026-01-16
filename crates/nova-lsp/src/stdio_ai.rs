use crate::rpc_out::RpcOut;
use crate::stdio_ai_code_edits::{run_ai_generate_method_body_apply, run_ai_generate_tests_apply};
use crate::stdio_ai_explain::run_ai_explain_error;
use crate::ServerState;

use lsp_types::ProgressToken;
use nova_ide::{ExplainErrorArgs, GenerateMethodBodyArgs, GenerateTestsArgs};
use nova_scheduler::CancellationToken;
use serde_json::Value;

fn split_work_done_token(params: Value) -> Result<(Option<ProgressToken>, Value), (i32, String)> {
    let mut params = match params {
        Value::Object(obj) => obj,
        _ => return Err((-32602, "params must be an object".to_string())),
    };
    let work_done_token = match params.remove("workDoneToken") {
        None | Some(Value::Null) => None,
        Some(value) => Some(crate::stdio_jsonrpc::decode_params_with_code(value)?),
    };
    Ok((work_done_token, Value::Object(params)))
}

pub(super) fn handle_ai_custom_request<O: RpcOut + Sync>(
    method: &str,
    params: Value,
    state: &mut ServerState,
    rpc_out: &O,
    cancel: &CancellationToken,
) -> Result<serde_json::Value, (i32, String)> {
    match method {
        nova_lsp::AI_EXPLAIN_ERROR_METHOD => {
            let (work_done_token, params) = split_work_done_token(params)?;
            let args: ExplainErrorArgs = crate::stdio_jsonrpc::decode_params_with_code(params)?;
            run_ai_explain_error(args, work_done_token, state, rpc_out, cancel.clone())
        }
        nova_lsp::AI_GENERATE_METHOD_BODY_METHOD => {
            let (work_done_token, params) = split_work_done_token(params)?;
            let args: GenerateMethodBodyArgs =
                crate::stdio_jsonrpc::decode_params_with_code(params)?;
            run_ai_generate_method_body_apply(args, work_done_token, state, rpc_out, cancel.clone())
        }
        nova_lsp::AI_GENERATE_TESTS_METHOD => {
            let (work_done_token, params) = split_work_done_token(params)?;
            let args: GenerateTestsArgs = crate::stdio_jsonrpc::decode_params_with_code(params)?;
            run_ai_generate_tests_apply(args, work_done_token, state, rpc_out, cancel.clone())
        }
        _ => Err((-32601, format!("Method not found: {method}"))),
    }
}
