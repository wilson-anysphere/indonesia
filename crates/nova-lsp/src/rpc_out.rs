use lsp_server::RequestId;
use serde_json::Value;
use std::io;

#[cfg(test)]
fn jsonrpc_notification(method: &str, params: Value) -> Value {
    let mut msg = serde_json::Map::new();
    msg.insert("jsonrpc".to_string(), Value::String("2.0".to_string()));
    msg.insert("method".to_string(), Value::String(method.to_string()));
    msg.insert("params".to_string(), params);
    Value::Object(msg)
}

#[cfg(test)]
fn jsonrpc_request(id: Value, method: &str, params: Value) -> Value {
    let mut msg = serde_json::Map::new();
    msg.insert("jsonrpc".to_string(), Value::String("2.0".to_string()));
    msg.insert("id".to_string(), id);
    msg.insert("method".to_string(), Value::String(method.to_string()));
    msg.insert("params".to_string(), params);
    Value::Object(msg)
}

/// Transport-agnostic sink for outgoing JSON-RPC messages.
///
/// The `nova-lsp` binary can run either:
/// - on top of `lsp_server`'s channel model (production), or
/// - against an `io::Write` for unit tests that capture framed output.
pub trait RpcOut {
    fn send_notification(&self, method: &str, params: Value) -> io::Result<()>;
    fn send_request(&self, id: RequestId, method: &str, params: Value) -> io::Result<()>;
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
        crate::codec::write_json_message(&mut *writer, &jsonrpc_notification(method, params))
    }

    fn send_request(&self, id: RequestId, method: &str, params: Value) -> io::Result<()> {
        let mut writer = self.writer.lock().unwrap();
        let id = serde_json::to_value(&id)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err.to_string()))?;
        crate::codec::write_json_message(&mut *writer, &jsonrpc_request(id, method, params))
    }
}
