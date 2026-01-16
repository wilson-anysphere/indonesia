use crate::rpc_out::RpcOut;
use lsp_types::{
    LogMessageParams, MessageType, ProgressParams, ProgressParamsValue, ProgressToken,
    WorkDoneProgress, WorkDoneProgressBegin, WorkDoneProgressEnd, WorkDoneProgressReport,
};

pub(super) type RpcError = (i32, String);

pub(super) fn chunk_utf8_by_bytes(text: &str, max_bytes: usize) -> Vec<&str> {
    if text.as_bytes().len() <= max_bytes {
        return vec![text];
    }

    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < text.len() {
        let mut end = (start + max_bytes).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = (start + 1).min(text.len());
            while end < text.len() && !text.is_char_boundary(end) {
                end += 1;
            }
        }
        chunks.push(&text[start..end]);
        start = end;
    }
    chunks
}

pub(super) fn send_log_message(out: &impl RpcOut, message: &str) -> Result<(), RpcError> {
    let params = LogMessageParams {
        typ: MessageType::INFO,
        message: message.to_string(),
    };
    let params = serde_json::to_value(params).map_err(|e| (-32603, e.to_string()))?;
    out.send_notification("window/logMessage", params)
        .map_err(|e| (-32603, e.to_string()))
}

pub(super) fn send_progress_begin(
    out: &impl RpcOut,
    token: Option<&ProgressToken>,
    title: &str,
) -> Result<(), RpcError> {
    let Some(token) = token else {
        return Ok(());
    };

    let params = ProgressParams {
        token: token.clone(),
        value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(WorkDoneProgressBegin {
            title: title.to_string(),
            cancellable: Some(false),
            message: None,
            percentage: None,
        })),
    };
    let params = serde_json::to_value(params).map_err(|e| (-32603, e.to_string()))?;
    out.send_notification("$/progress", params)
        .map_err(|e| (-32603, e.to_string()))
}

pub(super) fn send_progress_report(
    out: &impl RpcOut,
    token: Option<&ProgressToken>,
    message: &str,
    percentage: Option<u32>,
) -> Result<(), RpcError> {
    let Some(token) = token else {
        return Ok(());
    };

    let params = ProgressParams {
        token: token.clone(),
        value: ProgressParamsValue::WorkDone(WorkDoneProgress::Report(WorkDoneProgressReport {
            cancellable: Some(false),
            message: Some(message.to_string()),
            percentage,
        })),
    };
    let params = serde_json::to_value(params).map_err(|e| (-32603, e.to_string()))?;
    out.send_notification("$/progress", params)
        .map_err(|e| (-32603, e.to_string()))
}

pub(super) fn send_progress_end(
    out: &impl RpcOut,
    token: Option<&ProgressToken>,
    message: &str,
) -> Result<(), RpcError> {
    let Some(token) = token else {
        return Ok(());
    };

    let params = ProgressParams {
        token: token.clone(),
        value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(WorkDoneProgressEnd {
            message: Some(message.to_string()),
        })),
    };
    let params = serde_json::to_value(params).map_err(|e| (-32603, e.to_string()))?;
    out.send_notification("$/progress", params)
        .map_err(|e| (-32603, e.to_string()))
}
