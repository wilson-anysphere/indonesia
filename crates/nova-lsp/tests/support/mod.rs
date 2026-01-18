#![allow(dead_code)]

use lsp_types::{InitializeResult, Uri};
use nova_core::{path_to_file_uri, AbsPathBuf};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

static STDIO_SERVER_LOCK: Mutex<()> = Mutex::new(());

#[track_caller]
fn lock_unpoison<'a>(mutex: &'a Mutex<()>) -> std::sync::MutexGuard<'a, ()> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(err) => {
            let loc = std::panic::Location::caller();
            tracing::error!(
                target = "nova.lsp.tests",
                file = loc.file(),
                line = loc.line(),
                column = loc.column(),
                error = %err,
                "mutex poisoned; continuing with recovered guard"
            );
            err.into_inner()
        }
    }
}

/// Serialize tests that spawn the `nova-lsp` stdio server.
///
/// The server binary is large and may spin up multiple helper threads. When the test harness runs
/// these integration tests in parallel (controlled by `RUST_TEST_THREADS`), spawning many server
/// processes at once can exceed the sandbox's memory limits and lead to spurious crashes / EOFs.
pub fn stdio_server_lock() -> std::sync::MutexGuard<'static, ()> {
    lock_unpoison(&STDIO_SERVER_LOCK)
}

/// Build an RFC 8089 `file://` URI string for an absolute filesystem path.
///
/// This uses `nova_core::path_to_file_uri`, which handles Windows drive letters and
/// percent-encoding for special characters.
pub fn file_uri_string(path: &Path) -> String {
    let abs = AbsPathBuf::try_from(path.to_path_buf()).expect("abs path");
    path_to_file_uri(&abs).expect("file uri")
}

/// Build an RFC 8089 `file://` URI for an absolute filesystem path.
pub fn file_uri(path: &Path) -> Uri {
    file_uri_string(path).parse().expect("lsp uri")
}

pub struct TestAiServer {
    base_url: String,
    hits: Arc<AtomicUsize>,
    stop_tx: Option<mpsc::Sender<()>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl TestAiServer {
    pub fn start(response: Value) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        listener.set_nonblocking(true).expect("set_nonblocking");

        let addr = listener.local_addr().expect("local_addr");
        let base_url = format!("http://{addr}");

        let body_bytes = serde_json::to_vec(&response).expect("serialize response");
        let body_bytes = Arc::new(body_bytes);

        let hits = Arc::new(AtomicUsize::new(0));
        let hits_thread = hits.clone();

        let (stop_tx, stop_rx) = mpsc::channel::<()>();

        let handle = thread::spawn(move || loop {
            match stop_rx.try_recv() {
                Ok(()) | Err(mpsc::TryRecvError::Disconnected) => break,
                Err(mpsc::TryRecvError::Empty) => {}
            }

            let (mut stream, _) = match listener.accept() {
                Ok(value) => value,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(_) => break,
            };

            let mut reader = BufReader::new(&mut stream);
            let mut request_line = String::new();
            if reader
                .read_line(&mut request_line)
                .ok()
                .filter(|n| *n > 0)
                .is_none()
            {
                continue;
            }

            let mut parts = request_line.split_whitespace();
            let Some(method) = parts.next() else {
                continue;
            };
            let Some(path) = parts.next() else {
                continue;
            };

            let mut content_length: usize = 0;
            loop {
                let mut line = String::new();
                if reader
                    .read_line(&mut line)
                    .ok()
                    .filter(|n| *n > 0)
                    .is_none()
                {
                    break;
                }
                let line = line.trim_end_matches(['\r', '\n']);
                if line.is_empty() {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    if name.eq_ignore_ascii_case("Content-Length") {
                        if let Ok(len) = value.trim().parse::<usize>() {
                            content_length = len;
                        }
                    }
                }
            }

            if content_length > 0 {
                let mut buf = vec![0u8; content_length];
                let _ = reader.read_exact(&mut buf);
            } else {
                let mut drain = [0u8; 1024];
                let _ = reader.read(&mut drain);
            }

            drop(reader);

            if method == "POST" && path == "/complete" {
                hits_thread.fetch_add(1, Ordering::SeqCst);
                let response_body = body_bytes.as_slice();
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response_body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(response_body);
                let _ = stream.flush();
            } else {
                let body = b"not found";
                let header = format!(
                    "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(body);
                let _ = stream.flush();
            }
        });

        Self {
            base_url,
            hits,
            stop_tx: Some(stop_tx),
            handle: Some(handle),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    pub fn assert_hits(&self, expected: usize) {
        assert_eq!(self.hits(), expected);
    }
}

impl Drop for TestAiServer {
    fn drop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

pub fn write_jsonrpc_message(writer: &mut impl Write, message: &Value) {
    let bytes = serde_json::to_vec(message).expect("serialize");
    write!(writer, "Content-Length: {}\r\n\r\n", bytes.len()).expect("write header");
    writer.write_all(&bytes).expect("write body");
    writer.flush().expect("flush");
}

pub fn read_jsonrpc_message(reader: &mut impl BufRead) -> Value {
    let mut content_length: Option<usize> = None;

    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).expect("read header line");
        assert!(bytes_read > 0, "unexpected EOF while reading headers");

        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }

        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("Content-Length") {
                content_length = value.trim().parse::<usize>().ok();
            }
        }
    }

    let len = content_length.expect("Content-Length header");
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).expect("read body");
    serde_json::from_slice(&buf).expect("parse json")
}

pub fn read_response_with_id(reader: &mut impl BufRead, id: i64) -> Value {
    loop {
        let msg = read_jsonrpc_message(reader);
        if msg.get("method").is_some() {
            continue;
        }
        if msg.get("id").and_then(|v| v.as_i64()) == Some(id) {
            return msg;
        }
    }
}

#[track_caller]
pub fn jsonrpc_result(response: &Value) -> Value {
    response.get("result").cloned().unwrap_or_else(|| {
        panic!("jsonrpc response missing `result` field (or it is not an object): {response:#}")
    })
}

#[track_caller]
pub fn jsonrpc_result_as<T: serde::de::DeserializeOwned>(response: &Value) -> T {
    serde_json::from_value(jsonrpc_result(response)).expect("decode jsonrpc result")
}

pub fn decode_initialize_result(response: &Value) -> InitializeResult {
    serde_json::from_value(response.get("result").cloned().expect("initialize result"))
        .expect("decode InitializeResult")
}

pub fn drain_notifications_until_id(reader: &mut impl BufRead, id: i64) -> (Vec<Value>, Value) {
    let mut notifications = Vec::new();
    loop {
        let msg = read_jsonrpc_message(reader);
        if msg.get("method").is_none() && msg.get("id").and_then(|v| v.as_i64()) == Some(id) {
            return (notifications, msg);
        }

        // JSON-RPC notifications have `method` without `id`. We keep everything else
        // (including server->client requests) for debugging/optional assertions.
        notifications.push(msg);
    }
}

pub fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

fn to_json_value(value: impl serde::Serialize) -> Value {
    serde_json::to_value(value).expect("serialize json value")
}

pub fn json_value(value: impl serde::Serialize) -> Value {
    to_json_value(value)
}

pub fn jsonrpc_request(params: impl serde::Serialize, id: i64, method: &str) -> Value {
    Value::Object({
        let mut value = serde_json::Map::new();
        value.insert("jsonrpc".to_string(), Value::String("2.0".to_string()));
        value.insert("id".to_string(), Value::from(id));
        value.insert("method".to_string(), Value::String(method.to_string()));
        value.insert("params".to_string(), to_json_value(params));
        value
    })
}

pub fn jsonrpc_request_no_params(id: i64, method: &str) -> Value {
    Value::Object({
        let mut value = serde_json::Map::new();
        value.insert("jsonrpc".to_string(), Value::String("2.0".to_string()));
        value.insert("id".to_string(), Value::from(id));
        value.insert("method".to_string(), Value::String(method.to_string()));
        value
    })
}

pub fn jsonrpc_notification(params: impl serde::Serialize, method: &str) -> Value {
    Value::Object({
        let mut value = serde_json::Map::new();
        value.insert("jsonrpc".to_string(), Value::String("2.0".to_string()));
        value.insert("method".to_string(), Value::String(method.to_string()));
        value.insert("params".to_string(), to_json_value(params));
        value
    })
}

pub fn jsonrpc_notification_no_params(method: &str) -> Value {
    Value::Object({
        let mut value = serde_json::Map::new();
        value.insert("jsonrpc".to_string(), Value::String("2.0".to_string()));
        value.insert("method".to_string(), Value::String(method.to_string()));
        value
    })
}

pub fn jsonrpc_response_ok(id: Value, result: impl serde::Serialize) -> Value {
    Value::Object({
        let mut value = serde_json::Map::new();
        value.insert("jsonrpc".to_string(), Value::String("2.0".to_string()));
        value.insert("id".to_string(), id);
        value.insert("result".to_string(), to_json_value(result));
        value
    })
}

pub fn initialize_request_empty(id: i64) -> Value {
    jsonrpc_request(
        Value::Object({
            let mut value = serde_json::Map::new();
            value.insert("capabilities".to_string(), empty_object());
            value
        }),
        id,
        "initialize",
    )
}

pub fn initialize_request_with_root_uri(id: i64, root_uri: String) -> Value {
    jsonrpc_request(
        Value::Object({
            let mut value = serde_json::Map::new();
            value.insert("rootUri".to_string(), Value::String(root_uri));
            value.insert("capabilities".to_string(), empty_object());
            value
        }),
        id,
        "initialize",
    )
}

pub fn initialized_notification() -> Value {
    jsonrpc_notification(empty_object(), "initialized")
}

pub fn shutdown_request(id: i64) -> Value {
    jsonrpc_request_no_params(id, "shutdown")
}

pub fn exit_notification() -> Value {
    jsonrpc_notification_no_params("exit")
}

pub fn did_open_notification(
    uri: impl serde::Serialize,
    language_id: &'static str,
    version: i64,
    text: &str,
) -> Value {
    jsonrpc_notification(
        Value::Object({
            let mut params = serde_json::Map::new();
            params.insert(
                "textDocument".to_string(),
                Value::Object({
                    let mut doc = serde_json::Map::new();
                    doc.insert("uri".to_string(), to_json_value(uri));
                    doc.insert(
                        "languageId".to_string(),
                        Value::String(language_id.to_string()),
                    );
                    doc.insert("version".to_string(), Value::from(version));
                    doc.insert("text".to_string(), Value::String(text.to_string()));
                    doc
                }),
            );
            params
        }),
        "textDocument/didOpen",
    )
}
