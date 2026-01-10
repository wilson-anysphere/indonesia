use std::io;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

#[derive(Debug, Error)]
pub enum DapError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("dap protocol error: {0}")]
    Protocol(String),
}

pub type Result<T> = std::result::Result<T, DapError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub seq: i64,
    #[serde(rename = "type")]
    pub message_type: String,
    pub command: String,
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub seq: i64,
    #[serde(rename = "type")]
    pub message_type: String,
    pub request_seq: i64,
    pub success: bool,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub seq: i64,
    #[serde(rename = "type")]
    pub message_type: String,
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
}

pub fn make_response(seq: i64, request: &Request, success: bool, body: Option<Value>, message: Option<String>) -> Response {
    Response {
        seq,
        message_type: "response".to_string(),
        request_seq: request.seq,
        success,
        command: request.command.clone(),
        message,
        body,
    }
}

pub fn make_event(seq: i64, event: impl Into<String>, body: Option<Value>) -> Event {
    Event {
        seq,
        message_type: "event".to_string(),
        event: event.into(),
        body,
    }
}

pub struct DapReader<R> {
    reader: BufReader<R>,
}

impl<R: AsyncRead + Unpin> DapReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            reader: BufReader::new(inner),
        }
    }

    pub async fn read_value(&mut self) -> Result<Option<Value>> {
        let mut content_length: Option<usize> = None;
        let mut line = String::new();

        loop {
            line.clear();
            let n = self.reader.read_line(&mut line).await?;
            if n == 0 {
                return Ok(None);
            }

            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }

            let Some((name, value)) = trimmed.split_once(':') else {
                continue;
            };

            if name.eq_ignore_ascii_case("Content-Length") {
                let value = value.trim();
                content_length = Some(value.parse::<usize>().map_err(|e| {
                    DapError::Protocol(format!("invalid Content-Length {value:?}: {e}"))
                })?);
            }
        }

        let Some(len) = content_length else {
            return Err(DapError::Protocol("missing Content-Length header".to_string()));
        };

        let mut buf = vec![0u8; len];
        self.reader.read_exact(&mut buf).await?;
        Ok(Some(serde_json::from_slice::<Value>(&buf)?))
    }

    pub async fn read_request(&mut self) -> Result<Option<Request>> {
        let Some(value) = self.read_value().await? else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_value::<Request>(value)?))
    }
}

pub struct DapWriter<W> {
    writer: W,
}

impl<W: AsyncWrite + Unpin> DapWriter<W> {
    pub fn new(writer: W) -> Self {
        Self { writer }
    }

    pub async fn write_value(&mut self, value: &Value) -> Result<()> {
        let bytes = serde_json::to_vec(value)?;
        self.writer
            .write_all(format!("Content-Length: {}\r\n\r\n", bytes.len()).as_bytes())
            .await?;
        self.writer.write_all(&bytes).await?;
        self.writer.flush().await?;
        Ok(())
    }

    pub async fn write_response(&mut self, response: &Response) -> Result<()> {
        let value = serde_json::to_value(response)?;
        self.write_value(&value).await
    }

    pub async fn write_event(&mut self, event: &Event) -> Result<()> {
        let value = serde_json::to_value(event)?;
        self.write_value(&value).await
    }
}
