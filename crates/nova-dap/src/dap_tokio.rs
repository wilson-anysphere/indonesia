use std::io;

use crate::dap::{MAX_DAP_HEADER_LINE_BYTES, MAX_DAP_MESSAGE_BYTES};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

#[derive(Debug, Error)]
pub enum DapError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error("json error: {message}")]
    Json { message: String },

    #[error("dap protocol error: {0}")]
    Protocol(String),
}

pub type Result<T> = std::result::Result<T, DapError>;

impl From<serde_json::Error> for DapError {
    fn from(err: serde_json::Error) -> Self {
        // `serde_json::Error` display strings can include user-provided scalar values (e.g.
        // `invalid type: string "..."`). Avoid echoing those values because DAP payloads can
        // include secrets (launch args/env, evaluated expressions, etc).
        let message = sanitize_json_error_message(&err.to_string());
        Self::Json { message }
    }
}

fn sanitize_json_error_message(message: &str) -> String {
    // Conservatively redact all double-quoted substrings. This keeps the error actionable (it
    // retains the overall structure + line/column info) without echoing potentially-sensitive
    // content embedded in strings.
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(start) = rest.find('"') {
        // Include the opening quote.
        out.push_str(&rest[..start + 1]);
        rest = &rest[start + 1..];

        let mut end = None;
        let bytes = rest.as_bytes();
        for (idx, &b) in bytes.iter().enumerate() {
            if b != b'"' {
                continue;
            }

            // Treat quotes preceded by an odd number of backslashes as escaped.
            let mut backslashes = 0usize;
            let mut k = idx;
            while k > 0 && bytes[k - 1] == b'\\' {
                backslashes += 1;
                k -= 1;
            }
            if backslashes % 2 == 0 {
                end = Some(idx);
                break;
            }
        }

        let Some(end) = end else {
            // Unterminated quote: redact the remainder and stop.
            out.push_str("<redacted>");
            rest = "";
            break;
        };
        out.push_str("<redacted>\"");
        rest = &rest[end + 1..];
    }
    out.push_str(rest);

    // `serde` wraps unknown fields/variants in backticks:
    // `unknown field `secret`, expected ...`
    //
    // Redact only the first backticked segment so we keep the expected value list actionable.
    if let Some(start) = out.find('`') {
        let after_start = &out[start.saturating_add(1)..];
        let end = if let Some(end_rel) = after_start.rfind("`, expected") {
            Some(start.saturating_add(1).saturating_add(end_rel))
        } else if let Some(end_rel) = after_start.rfind('`') {
            Some(start.saturating_add(1).saturating_add(end_rel))
        } else {
            None
        };
        if let Some(end) = end {
            if start + 1 <= end && end <= out.len() {
                out.replace_range(start + 1..end, "<redacted>");
            }
        }
    }

    out
}

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

pub fn make_response(
    seq: i64,
    request: &Request,
    success: bool,
    body: Option<Value>,
    message: Option<String>,
) -> Response {
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

async fn read_line_limited<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    max_len: usize,
) -> io::Result<Option<String>> {
    let mut buf = Vec::<u8>::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if buf.is_empty() {
                return Ok(None);
            }
            break;
        }

        let newline_pos = available.iter().position(|&b| b == b'\n');
        let take = newline_pos.map(|pos| pos + 1).unwrap_or(available.len());
        if buf.len() + take > max_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("DAP header line exceeds maximum size ({max_len} bytes)"),
            ));
        }

        buf.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline_pos.is_some() {
            break;
        }
    }

    let line = String::from_utf8(buf)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "DAP header line is not UTF-8"))?;
    Ok(Some(line))
}

impl<R: AsyncRead + Unpin> DapReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            reader: BufReader::new(inner),
        }
    }

    pub async fn read_value(&mut self) -> Result<Option<Value>> {
        let mut content_length: Option<usize> = None;
        let mut saw_header_line = false;

        loop {
            let Some(line) = read_line_limited(&mut self.reader, MAX_DAP_HEADER_LINE_BYTES).await?
            else {
                if saw_header_line {
                    return Err(DapError::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "EOF while reading DAP headers",
                    )));
                }
                return Ok(None);
            };
            saw_header_line = true;

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
            return Err(DapError::Protocol(
                "missing Content-Length header".to_string(),
            ));
        };

        if len > MAX_DAP_MESSAGE_BYTES {
            return Err(DapError::Protocol(format!(
                "DAP message Content-Length {len} exceeds maximum allowed size {MAX_DAP_MESSAGE_BYTES}",
            )));
        }

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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{timeout, Duration};

    #[tokio::test]
    async fn rejects_oversized_content_length_without_reading_message_body() {
        let (mut writer, reader) = tokio::io::duplex(1024);

        // Write only the header section. Keep the writer side open so any attempt
        // to read the body would hang.
        let framed = format!("Content-Length: {}\r\n\r\n", MAX_DAP_MESSAGE_BYTES + 1);
        writer.write_all(framed.as_bytes()).await.unwrap();
        writer.flush().await.unwrap();

        let mut reader = DapReader::new(reader);
        let result = timeout(Duration::from_millis(100), reader.read_value()).await;

        let err = result
            .expect("read_value() should return immediately for oversized Content-Length")
            .unwrap_err();

        match err {
            DapError::Protocol(msg) => {
                assert!(msg.contains("exceeds maximum allowed size"), "{msg}");
                assert!(
                    msg.contains(&(MAX_DAP_MESSAGE_BYTES + 1).to_string()),
                    "{msg}"
                );
                assert!(msg.contains(&MAX_DAP_MESSAGE_BYTES.to_string()), "{msg}");
            }
            other => panic!("expected DapError::Protocol, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_overlong_header_lines() {
        let long = "A".repeat(MAX_DAP_HEADER_LINE_BYTES + 1);
        let framed = format!("{long}\n\n");
        let (mut writer, reader) = tokio::io::duplex(framed.len());
        writer.write_all(framed.as_bytes()).await.unwrap();
        writer.shutdown().await.unwrap();
        drop(writer);

        let mut reader = DapReader::new(reader);
        let err = reader.read_value().await.unwrap_err();
        match err {
            DapError::Io(io_err) => {
                assert_eq!(io_err.kind(), io::ErrorKind::InvalidData);
                assert!(io_err
                    .to_string()
                    .contains("header line exceeds maximum size"));
            }
            other => panic!("expected io error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_pathologically_large_content_length_without_attempting_allocation() {
        // Similar to the sync codec test: this should be rejected by the size
        // check, not by attempting to allocate the body buffer.
        let framed = format!("Content-Length: {}\r\n\r\n", usize::MAX);

        let (client, mut server) = tokio::io::duplex(1024);
        server.write_all(framed.as_bytes()).await.unwrap();
        drop(server);

        let mut reader = DapReader::new(client);
        let err = reader.read_value().await.unwrap_err();
        match err {
            DapError::Protocol(msg) => assert!(msg.contains("Content-Length")),
            other => panic!("expected protocol error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn eof_mid_headers_returns_unexpected_eof() {
        let (mut writer, reader) = tokio::io::duplex(1024);

        writer.write_all(b"Content-Length: 2\r\n").await.unwrap();
        writer.shutdown().await.unwrap();
        drop(writer);

        let mut reader = DapReader::new(reader);
        let err = reader.read_value().await.unwrap_err();
        match err {
            DapError::Io(io_err) => {
                assert_eq!(io_err.kind(), io::ErrorKind::UnexpectedEof);
                assert!(io_err.to_string().contains("EOF while reading DAP headers"));
            }
            other => panic!("expected io error, got {other:?}"),
        }
    }

    #[test]
    fn dap_error_json_does_not_echo_string_values() {
        let secret_suffix = "dap-super-secret-token";
        let secret = format!("prefix\"{secret_suffix}");
        let err = serde_json::from_value::<Request>(serde_json::json!({
            "seq": secret,
            "type": "request",
            "command": "initialize",
            "arguments": {}
        }))
        .expect_err("expected type error");

        let dap_err = DapError::from(err);
        let message = dap_err.to_string();
        assert!(
            !message.contains(secret_suffix),
            "expected DapError json message to omit string values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected DapError json message to include redaction marker: {message}"
        );
    }
}
