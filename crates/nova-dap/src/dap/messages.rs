use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    pub seq: u64,
    #[serde(rename = "type")]
    pub type_: String,
    pub command: String,
    #[serde(default)]
    pub arguments: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Response {
    pub seq: u64,
    #[serde(rename = "type")]
    pub type_: &'static str,
    pub request_seq: u64,
    pub success: bool,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
}

impl Response {
    pub fn success(seq: u64, request: &Request, body: Option<Value>) -> Self {
        Self {
            seq,
            type_: "response",
            request_seq: request.seq,
            success: true,
            command: request.command.clone(),
            message: None,
            body,
        }
    }

    pub fn error(seq: u64, request: &Request, message: impl Into<String>) -> Self {
        Self {
            seq,
            type_: "response",
            request_seq: request.seq,
            success: false,
            command: request.command.clone(),
            message: Some(message.into()),
            body: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub seq: u64,
    #[serde(rename = "type")]
    pub type_: &'static str,
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
}

impl Event {
    pub fn new(seq: u64, event: impl Into<String>, body: Option<Value>) -> Self {
        Self {
            seq,
            type_: "event",
            event: event.into(),
            body,
        }
    }
}
