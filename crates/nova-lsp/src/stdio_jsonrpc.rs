use lsp_server::{RequestId, Response, ResponseError};
use serde::de::DeserializeOwned;

pub(super) fn nova_lsp_error_code_message(err: nova_lsp::NovaLspError) -> (i32, String) {
    match err {
        nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
        nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
    }
}

pub(super) fn decode_params_with_code<T: DeserializeOwned>(
    params: serde_json::Value,
) -> Result<T, (i32, String)> {
    decode_params(params).map_err(|message| (-32602, message))
}

pub(super) fn decode_params<T: DeserializeOwned>(params: serde_json::Value) -> Result<T, String> {
    serde_json::from_value(params).map_err(|e| e.to_string())
}

pub(super) fn response_ok(id: RequestId, result: serde_json::Value) -> Response {
    Response {
        id,
        result: Some(result),
        error: None,
    }
}

pub(super) fn response_error(id: RequestId, code: i32, message: impl Into<String>) -> Response {
    Response {
        id,
        result: None,
        error: Some(ResponseError {
            code,
            message: message.into(),
            data: None,
        }),
    }
}
