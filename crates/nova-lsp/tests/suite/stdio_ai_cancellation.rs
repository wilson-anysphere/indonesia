use httpmock::prelude::*;
use httpmock::Mock;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::process::{ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

use nova_lsp::text_pos::TextPos;

use crate::support::{file_uri_string, stdio_server_lock, write_jsonrpc_message};

const PROVIDER_DELAY: Duration = Duration::from_secs(5);
const PROVIDER_HIT_TIMEOUT: Duration = Duration::from_secs(5);
const CANCEL_TIMEOUT: Duration = Duration::from_secs(2);

fn read_jsonrpc_message(reader: &mut impl BufRead) -> Option<Value> {
    let mut content_length: Option<usize> = None;

    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).ok()?;
        if bytes_read == 0 {
            return None;
        }

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

    let len = content_length?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

fn spawn_message_reader(stdout: ChildStdout) -> (mpsc::Receiver<Value>, thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<Value>();
    let handle = thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        while let Some(msg) = read_jsonrpc_message(&mut reader) {
            if tx.send(msg).is_err() {
                break;
            }
        }
    });
    (rx, handle)
}

struct MessagePump {
    rx: mpsc::Receiver<Value>,
    pending: VecDeque<Value>,
}

impl MessagePump {
    fn new(rx: mpsc::Receiver<Value>) -> Self {
        Self {
            rx,
            pending: VecDeque::new(),
        }
    }

    fn recv_any(&mut self, timeout: Duration) -> Option<Value> {
        if let Some(msg) = self.pending.pop_front() {
            return Some(msg);
        }
        self.rx.recv_timeout(timeout).ok()
    }

    fn drain_until_response_with_id(
        &mut self,
        id: i64,
        timeout: Duration,
    ) -> Option<(Vec<Value>, Value)> {
        let deadline = Instant::now() + timeout;
        let mut messages = Vec::new();

        loop {
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let remaining = deadline - now;
            let msg = self.recv_any(remaining)?;

            if msg.get("method").is_none() && msg.get("id").and_then(|v| v.as_i64()) == Some(id) {
                return Some((messages, msg));
            }

            messages.push(msg);
        }
    }
}

fn wait_for_mock_hit(mock: &Mock, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if mock.hits() > 0 {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!(
        "timed out waiting for AI provider request (hits={})",
        mock.hits()
    );
}

fn spawn_stdio_server(config_path: &std::path::Path) -> (std::process::Child, ChildStdin, MessagePump, thread::JoinHandle<()>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(config_path)
        // Ensure env vars don't override the config file.
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env_remove("NOVA_AI_COMPLETIONS_MAX_ITEMS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let (rx, reader_handle) = spawn_message_reader(stdout);
    let pump = MessagePump::new(rx);
    (child, stdin, pump, reader_handle)
}

fn initialize_server(
    stdin: &mut ChildStdin,
    pump: &mut MessagePump,
    root_uri: Option<String>,
) {
    let mut params = json!({ "capabilities": {} });
    if let Some(root_uri) = root_uri {
        params["rootUri"] = json!(root_uri);
    }

    write_jsonrpc_message(
        stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": params,
        }),
    );
    pump.drain_until_response_with_id(1, Duration::from_secs(5))
        .expect("initialize response");
    write_jsonrpc_message(
        stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );
}

fn shutdown_server(mut child: std::process::Child, mut stdin: ChildStdin, mut pump: MessagePump) {
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 99, "method": "shutdown" }),
    );
    pump.drain_until_response_with_id(99, Duration::from_secs(5))
        .expect("shutdown response");
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_cancel_request_cancels_nova_ai_explain_error() {
    let _guard = stdio_server_lock();

    let mock_server = MockServer::start();
    let _mock = mock_server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200)
            .json_body(json!({ "completion": "mock explanation" }))
            .delay(PROVIDER_DELAY);
    });

    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.provider]
kind = "http"
url = "{endpoint}"
model = "default"
timeout_ms = 20000
concurrency = 1

[ai.privacy]
local_only = true
"#,
            endpoint = format!("{}/complete", mock_server.base_url())
        ),
    )
    .expect("write config");

    let (mut child, mut stdin, mut pump, reader_handle) = spawn_stdio_server(&config_path);
    initialize_server(&mut stdin, &mut pump, None);

    // Start an explain-only request that blocks on the delayed HTTP response.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "nova/ai/explainError",
            "params": {
                "diagnosticMessage": "cannot find symbol",
                "code": "unknown()"
            }
        }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "$/cancelRequest",
            "params": { "id": 10 }
        }),
    );

    let (_msgs, resp) = match pump.drain_until_response_with_id(10, CANCEL_TIMEOUT) {
        Some(resp) => resp,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("timed out waiting for explainError cancellation response");
        }
    };

    let code = resp
        .get("error")
        .and_then(|err| err.get("code"))
        .and_then(|v| v.as_i64());
    assert_eq!(
        code,
        Some(-32800),
        "expected cancelled explainError request to return -32800, got: {resp:?}"
    );

    shutdown_server(child, stdin, pump);
    let _ = reader_handle.join();
}

#[test]
fn stdio_server_cancel_request_cancels_nova_ai_generate_method_body_without_apply_edit() {
    let _guard = stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = file_uri_string(root);

    let file_path = root.join("Test.java");
    let file_uri = file_uri_string(&file_path);
    let source = "class Test {\n    int add(int a, int b) {\n    }\n}\n";
    std::fs::write(&file_path, source).expect("write Test.java");

    // If cancellation fails, this patch would insert a return statement in the empty method body.
    let method_line = "    int add(int a, int b) {";
    let open_brace_offset = source
        .find(method_line)
        .expect("method line")
        .saturating_add(method_line.len().saturating_sub(1));
    let close_brace_offset = source
        .find("\n    }\n")
        .expect("method close")
        .saturating_add("\n    ".len());

    let pos = TextPos::new(source);
    let insert_start = pos
        .lsp_position(open_brace_offset + 1)
        .expect("insert start pos");
    let insert_end = pos
        .lsp_position(close_brace_offset)
        .expect("insert end pos");

    let patch = json!({
        "edits": [{
            "file": "Test.java",
            "range": {
                "start": { "line": insert_start.line, "character": insert_start.character },
                "end": { "line": insert_end.line, "character": insert_end.character }
            },
            "text": "\n        return a + b;\n    "
        }]
    });
    let completion = serde_json::to_string(&patch).expect("patch json");

    let mock_server = MockServer::start();
    let _mock = mock_server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200)
            .json_body(json!({ "completion": completion }))
            .delay(PROVIDER_DELAY);
    });

    let config_path = root.join("nova.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.provider]
kind = "http"
url = "{endpoint}"
model = "default"
timeout_ms = 20000
concurrency = 1

[ai.privacy]
local_only = true
"#,
            endpoint = format!("{}/complete", mock_server.base_url())
        ),
    )
    .expect("write config");

    let (mut child, mut stdin, mut pump, reader_handle) = spawn_stdio_server(&config_path);
    initialize_server(&mut stdin, &mut pump, Some(root_uri));

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": file_uri.clone(),
                    "languageId": "java",
                    "version": 1,
                    "text": source
                }
            }
        }),
    );

    let selection_start = pos
        .lsp_position(source.find(method_line).expect("selection start"))
        .expect("selection start pos");
    let selection_end = pos
        .lsp_position(close_brace_offset + 1)
        .expect("selection end pos");

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "nova/ai/generateMethodBody",
            "params": {
                "methodSignature": "int add(int a, int b)",
                "context": null,
                "uri": file_uri,
                "range": { "start": selection_start, "end": selection_end }
            }
        }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "$/cancelRequest",
            "params": { "id": 11 }
        }),
    );

    let (messages, resp) = match pump.drain_until_response_with_id(11, CANCEL_TIMEOUT) {
        Some(resp) => resp,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("timed out waiting for generateMethodBody cancellation response");
        }
    };

    let apply_edits = messages.iter().any(|msg| {
        msg.get("method").and_then(|m| m.as_str()) == Some("workspace/applyEdit")
    });
    assert!(
        !apply_edits,
        "expected cancelled generateMethodBody to emit no workspace/applyEdit, got: {messages:?}"
    );

    let code = resp
        .get("error")
        .and_then(|err| err.get("code"))
        .and_then(|v| v.as_i64());
    assert_eq!(
        code,
        Some(-32800),
        "expected cancelled generateMethodBody request to return -32800, got: {resp:?}"
    );

    shutdown_server(child, stdin, pump);
    let _ = reader_handle.join();
}

#[test]
fn stdio_server_cancel_request_cancels_in_flight_nova_ai_explain_error() {
    let _guard = stdio_server_lock();

    let mock_server = MockServer::start();
    let mock = mock_server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200)
            .json_body(json!({ "completion": "mock explanation" }))
            .delay(PROVIDER_DELAY);
    });

    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.provider]
kind = "http"
url = "{endpoint}"
model = "default"
timeout_ms = 20000
concurrency = 1

[ai.privacy]
local_only = true
"#,
            endpoint = format!("{}/complete", mock_server.base_url())
        ),
    )
    .expect("write config");

    let (mut child, mut stdin, mut pump, reader_handle) = spawn_stdio_server(&config_path);
    initialize_server(&mut stdin, &mut pump, None);

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 20,
            "method": "nova/ai/explainError",
            "params": {
                "diagnosticMessage": "cannot find symbol",
                "code": "unknown()"
            }
        }),
    );

    // Ensure the request reached the provider so we're validating in-flight cancellation (not the
    // handler's early `cancel.is_cancelled()` guard).
    wait_for_mock_hit(&mock, PROVIDER_HIT_TIMEOUT);

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "$/cancelRequest",
            "params": { "id": 20 }
        }),
    );

    let (_msgs, resp) = match pump.drain_until_response_with_id(20, CANCEL_TIMEOUT) {
        Some(resp) => resp,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("timed out waiting for explainError cancellation response");
        }
    };

    let code = resp
        .get("error")
        .and_then(|err| err.get("code"))
        .and_then(|v| v.as_i64());
    assert_eq!(
        code,
        Some(-32800),
        "expected cancelled explainError request to return -32800, got: {resp:?}"
    );

    shutdown_server(child, stdin, pump);
    let _ = reader_handle.join();
}

#[test]
fn stdio_server_cancel_request_cancels_in_flight_nova_ai_generate_method_body_without_apply_edit() {
    let _guard = stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = file_uri_string(root);

    let file_path = root.join("Test.java");
    let file_uri = file_uri_string(&file_path);
    let source = "class Test {\n    int add(int a, int b) {\n    }\n}\n";
    std::fs::write(&file_path, source).expect("write Test.java");

    // If cancellation fails, this patch would insert a return statement in the empty method body.
    let method_line = "    int add(int a, int b) {";
    let open_brace_offset = source
        .find(method_line)
        .expect("method line")
        .saturating_add(method_line.len().saturating_sub(1));
    let close_brace_offset = source
        .find("\n    }\n")
        .expect("method close")
        .saturating_add("\n    ".len());

    let pos = TextPos::new(source);
    let insert_start = pos
        .lsp_position(open_brace_offset + 1)
        .expect("insert start pos");
    let insert_end = pos
        .lsp_position(close_brace_offset)
        .expect("insert end pos");

    let patch = json!({
        "edits": [{
            "file": "Test.java",
            "range": {
                "start": { "line": insert_start.line, "character": insert_start.character },
                "end": { "line": insert_end.line, "character": insert_end.character }
            },
            "text": "\n        return a + b;\n    "
        }]
    });
    let completion = serde_json::to_string(&patch).expect("patch json");

    let mock_server = MockServer::start();
    let mock = mock_server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200)
            .json_body(json!({ "completion": completion }))
            .delay(PROVIDER_DELAY);
    });

    let config_path = root.join("nova.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.provider]
kind = "http"
url = "{endpoint}"
model = "default"
timeout_ms = 20000
concurrency = 1

[ai.privacy]
local_only = true
"#,
            endpoint = format!("{}/complete", mock_server.base_url())
        ),
    )
    .expect("write config");

    let (mut child, mut stdin, mut pump, reader_handle) = spawn_stdio_server(&config_path);
    initialize_server(&mut stdin, &mut pump, Some(root_uri));

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": file_uri.clone(),
                    "languageId": "java",
                    "version": 1,
                    "text": source
                }
            }
        }),
    );

    let selection_start = pos
        .lsp_position(source.find(method_line).expect("selection start"))
        .expect("selection start pos");
    let selection_end = pos
        .lsp_position(close_brace_offset + 1)
        .expect("selection end pos");

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 21,
            "method": "nova/ai/generateMethodBody",
            "params": {
                "methodSignature": "int add(int a, int b)",
                "context": null,
                "uri": file_uri,
                "range": { "start": selection_start, "end": selection_end }
            }
        }),
    );

    wait_for_mock_hit(&mock, PROVIDER_HIT_TIMEOUT);

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "$/cancelRequest",
            "params": { "id": 21 }
        }),
    );

    let (messages, resp) = match pump.drain_until_response_with_id(21, CANCEL_TIMEOUT) {
        Some(resp) => resp,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("timed out waiting for generateMethodBody cancellation response");
        }
    };

    let apply_edits = messages.iter().any(|msg| {
        msg.get("method").and_then(|m| m.as_str()) == Some("workspace/applyEdit")
    });
    assert!(
        !apply_edits,
        "expected cancelled generateMethodBody to emit no workspace/applyEdit, got: {messages:?}"
    );

    let code = resp
        .get("error")
        .and_then(|err| err.get("code"))
        .and_then(|v| v.as_i64());
    assert_eq!(
        code,
        Some(-32800),
        "expected cancelled generateMethodBody request to return -32800, got: {resp:?}"
    );

    shutdown_server(child, stdin, pump);
    let _ = reader_handle.join();
}
