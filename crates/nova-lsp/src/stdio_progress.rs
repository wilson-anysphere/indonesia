use crate::rpc_out::RpcOut;
use serde_json::{json, Map, Value};

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
  out
    .send_notification("window/logMessage", json!({ "type": 3, "message": message }))
    .map_err(|e| (-32603, e.to_string()))
}

pub(super) fn send_progress_begin(
  out: &impl RpcOut,
  token: Option<&Value>,
  title: &str,
) -> Result<(), RpcError> {
  let Some(token) = token else {
    return Ok(());
  };
  out
    .send_notification(
      "$/progress",
      json!({
        "token": token,
        "value": {
          "kind": "begin",
          "title": title,
          "cancellable": false,
          "message": "",
        }
      }),
    )
    .map_err(|e| (-32603, e.to_string()))
}

pub(super) fn send_progress_report(
  out: &impl RpcOut,
  token: Option<&Value>,
  message: &str,
  percentage: Option<u32>,
) -> Result<(), RpcError> {
  let Some(token) = token else {
    return Ok(());
  };
  let mut value = Map::new();
  value.insert("kind".to_string(), json!("report"));
  value.insert("message".to_string(), json!(message));
  if let Some(percentage) = percentage {
    value.insert("percentage".to_string(), json!(percentage));
  }
  out
    .send_notification(
      "$/progress",
      json!({
        "token": token,
        "value": value
      }),
    )
    .map_err(|e| (-32603, e.to_string()))
}

pub(super) fn send_progress_end(
  out: &impl RpcOut,
  token: Option<&Value>,
  message: &str,
) -> Result<(), RpcError> {
  let Some(token) = token else {
    return Ok(());
  };
  out
    .send_notification(
      "$/progress",
      json!({
        "token": token,
        "value": {
          "kind": "end",
          "message": message,
        }
      }),
    )
    .map_err(|e| (-32603, e.to_string()))
}

