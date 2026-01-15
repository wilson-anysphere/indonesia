use lsp_server::RequestId;
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
}
