use lsp_server::{Message, Notification, Request, RequestId, Response, ResponseError};
use serde_json::Value;
use std::io;

/// Transport-agnostic sink for outgoing JSON-RPC messages.
///
/// The `nova-lsp` binary can run either:
/// - on top of `lsp_server`'s channel model (production), or
/// - against an `io::Write` for unit tests that capture framed output.
pub trait RpcOut {
    fn send_notification(&self, method: &str, params: Value) -> io::Result<()>;
    fn send_request(&self, id: RequestId, method: &str, params: Value) -> io::Result<()>;
    fn send_response(
        &self,
        id: RequestId,
        result: Option<Value>,
        error: Option<ResponseError>,
    ) -> io::Result<()>;
}

pub struct SenderRpcOut {
    sender: crossbeam_channel::Sender<Message>,
}

impl SenderRpcOut {
    pub fn new(sender: crossbeam_channel::Sender<Message>) -> Self {
        Self { sender }
    }
}

impl RpcOut for SenderRpcOut {
    fn send_notification(&self, method: &str, params: Value) -> io::Result<()> {
        self.sender
            .send(Message::Notification(Notification {
                method: method.to_string(),
                params,
            }))
            .map_err(map_send_error)
    }

    fn send_request(&self, id: RequestId, method: &str, params: Value) -> io::Result<()> {
        self.sender
            .send(Message::Request(Request {
                id,
                method: method.to_string(),
                params,
            }))
            .map_err(map_send_error)
    }

    fn send_response(
        &self,
        id: RequestId,
        result: Option<Value>,
        error: Option<ResponseError>,
    ) -> io::Result<()> {
        self.sender
            .send(Message::Response(Response { id, result, error }))
            .map_err(map_send_error)
    }
}

fn map_send_error<T>(err: crossbeam_channel::SendError<T>) -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, err.to_string())
}

/// `RpcOut` adapter that writes Content-Length framed JSON-RPC messages.
///
/// This is primarily intended for unit tests where we want to capture and parse
/// the server output deterministically without creating an `lsp_server::Connection`.
#[cfg(test)]
pub struct WriteRpcOut<W> {
    writer: std::sync::Mutex<W>,
}

#[cfg(test)]
impl<W> WriteRpcOut<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer: std::sync::Mutex::new(writer),
        }
    }

    pub fn into_inner(self) -> W {
        self.writer
            .into_inner()
            .unwrap_or_else(|err| err.into_inner())
    }
}

#[cfg(test)]
impl<W: io::Write> RpcOut for WriteRpcOut<W> {
    fn send_notification(&self, method: &str, params: Value) -> io::Result<()> {
        let mut writer = self.writer.lock().unwrap();
        crate::codec::write_json_message(
            &mut *writer,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
            }),
        )
    }

    fn send_request(&self, id: RequestId, method: &str, params: Value) -> io::Result<()> {
        let mut writer = self.writer.lock().unwrap();
        let id = serde_json::to_value(&id)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err.to_string()))?;
        crate::codec::write_json_message(
            &mut *writer,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            }),
        )
    }

    fn send_response(
        &self,
        id: RequestId,
        result: Option<Value>,
        error: Option<ResponseError>,
    ) -> io::Result<()> {
        let mut writer = self.writer.lock().unwrap();
        let id = serde_json::to_value(&id)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err.to_string()))?;
        let mut message = serde_json::Map::new();
        message.insert("jsonrpc".to_string(), Value::String("2.0".to_string()));
        message.insert("id".to_string(), id);
        if let Some(result) = result {
            message.insert("result".to_string(), result);
        }
        if let Some(error) = error {
            message.insert(
                "error".to_string(),
                serde_json::json!({
                    "code": error.code,
                    "message": error.message,
                    "data": error.data,
                }),
            );
        }

        crate::codec::write_json_message(&mut *writer, &Value::Object(message))
    }
}
