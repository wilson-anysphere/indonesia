use lsp_server::{RequestId, Response, ResponseError};
use serde_json::json;

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

pub(super) fn jsonrpc_response_to_response(id: RequestId, response: serde_json::Value) -> Response {
    if let Some(result) = response.get("result") {
        return response_ok(id, result.clone());
    }
    if let Some(error) = response.get("error") {
        let code = error
            .get("code")
            .and_then(|v| v.as_i64())
            .unwrap_or(-32603)
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
        let message = error
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("Internal error")
            .to_string();
        let data = error.get("data").cloned();
        return Response {
            id,
            result: None,
            error: Some(ResponseError {
                code,
                message,
                data,
            }),
        };
    }
    response_error(id, -32603, "Internal error (malformed response)")
}

pub(super) fn server_shutting_down_error(id: serde_json::Value) -> serde_json::Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32600,
            "message": "Server is shutting down"
        }
    })
}

