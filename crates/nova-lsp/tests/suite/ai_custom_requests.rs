use httpmock::prelude::*;
use serde_json::json;
use std::io::{BufRead, BufReader};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

use crate::support;

struct FlakyAiServer {
    base_url: String,
    hits: Arc<AtomicUsize>,
    stop_tx: Option<mpsc::Sender<()>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl FlakyAiServer {
    fn start(success_response: serde_json::Value) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        listener.set_nonblocking(true).expect("set_nonblocking");

        let addr = listener.local_addr().expect("local_addr");
        let base_url = format!("http://{addr}");

        let body_bytes = serde_json::to_vec(&success_response).expect("serialize response");
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

            let mut reader = std::io::BufReader::new(&mut stream);
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
            let method = parts.next().unwrap_or_default();
            let path = parts.next().unwrap_or_default();

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
                        content_length = value.trim().parse::<usize>().unwrap_or(0);
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
                let call = hits_thread.fetch_add(1, Ordering::SeqCst);
                if call == 0 {
                    let body = b"boom";
                    let header = format!(
                        "HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(header.as_bytes());
                    let _ = stream.write_all(body);
                    let _ = stream.flush();
                } else {
                    let response_body = body_bytes.as_slice();
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        response_body.len()
                    );
                    let _ = stream.write_all(header.as_bytes());
                    let _ = stream.write_all(response_body);
                    let _ = stream.flush();
                }
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

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

impl Drop for FlakyAiServer {
    fn drop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn spawn_stdio_server(config_path: &std::path::Path) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(config_path)
        // The test config file should be authoritative; clear any legacy env-var AI wiring that
        // could override `--config` (common in developer shells).
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        // Ensure a developer's environment doesn't disable AI unexpectedly (tests that *do* want
        // these overrides set them explicitly).
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env_remove("NOVA_AI_COMPLETIONS_MAX_ITEMS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp")
}

fn initialize(child: &mut std::process::Child) -> (std::process::ChildStdin, BufReader<std::process::ChildStdout>) {
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "capabilities": {} }
        }),
    );
    let _initialize_resp = support::read_response_with_id(&mut stdout, 1);
    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    (stdin, stdout)
}

fn shutdown(mut child: std::process::Child, mut stdin: std::process::ChildStdin, mut stdout: BufReader<std::process::ChildStdout>) {
    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 99, "method": "shutdown" }),
    );
    let _shutdown_resp = support::read_response_with_id(&mut stdout, 99);
    support::write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

fn code_review_request_omits_excluded_diff(req: &HttpMockRequest) -> bool {
    const SECRET_MARKER: &str = "DO_NOT_LEAK_THIS_SECRET";
    let Some(body) = req.body.as_deref() else {
        return false;
    };
    let body = String::from_utf8_lossy(body);
    body.contains("[diff omitted due to excluded_paths]") && !body.contains(SECRET_MARKER)
}

#[test]
fn stdio_ai_code_review_custom_request_returns_string_and_emits_progress() {
    let _lock = support::stdio_server_lock();

    let server = MockServer::start();
    let long = "Nova AI output ".repeat(20_000 / 14 + 32);
    let completion = long.clone();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200).json_body(json!({ "completion": completion }));
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

[ai.privacy]
local_only = true
"#,
            endpoint = format!("{}/complete", server.base_url())
        ),
    )
    .expect("write config");

    let mut child = spawn_stdio_server(&config_path);
    let (mut stdin, mut stdout) = initialize(&mut child);

    // Send the custom request with a work-done token so we can observe `$/progress`.
    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/ai/codeReview",
            "params": {
                "workDoneToken": "token",
                "diff": "diff --git a/Main.java b/Main.java\n+class Main {}\n",
            }
        }),
    );

    let (notifications, resp) = support::drain_notifications_until_id(&mut stdout, 2);
    let result = resp.get("result").cloned().expect("result");
    let output = result.as_str().expect("string result").to_string();
    assert_eq!(output, long);

    assert!(
        notifications.iter().any(|msg| {
            msg.get("method").and_then(|m| m.as_str()) == Some("$/progress")
                && msg
                    .pointer("/params/value/kind")
                    .and_then(|k| k.as_str())
                    == Some("begin")
        }),
        "expected work-done progress begin notification"
    );
    assert!(
        notifications.iter().any(|msg| {
            msg.get("method").and_then(|m| m.as_str()) == Some("$/progress")
                && msg
                    .pointer("/params/value/kind")
                    .and_then(|k| k.as_str())
                    == Some("end")
        }),
        "expected work-done progress end notification"
    );

    let mut chunks = Vec::<String>::new();
    for msg in &notifications {
        if msg.get("method").and_then(|m| m.as_str()) != Some("window/logMessage") {
            continue;
        }
        let Some(text) = msg.pointer("/params/message").and_then(|m| m.as_str()) else {
            continue;
        };
        if !text.starts_with("AI codeReview") {
            continue;
        }
        let (_, chunk) = text
            .split_once(": ")
            .expect("chunk messages should contain ': ' delimiter");
        chunks.push(chunk.to_string());
    }
    assert!(
        chunks.len() > 1,
        "expected AI output to be chunked into multiple window/logMessage notifications"
    );
    assert_eq!(chunks.join(""), output);

    mock.assert();

    shutdown(child, stdin, stdout);
}

#[test]
fn stdio_ai_code_review_custom_request_omits_diff_for_excluded_paths() {
    let _lock = support::stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let secrets_dir = root.join("src").join("secrets");
    std::fs::create_dir_all(&secrets_dir).expect("create secrets dir");
    let secret_path = secrets_dir.join("Secret.java");
    std::fs::write(&secret_path, "class Secret {}").expect("write Secret.java");
    let secret_uri = support::file_uri_string(&secret_path);

    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/complete")
            .matches(code_review_request_omits_excluded_diff);
        then
            .status(200)
            .json_body(json!({ "completion": "mock review" }));
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

[ai.privacy]
local_only = true
excluded_paths = ["src/secrets/**"]
"#,
            endpoint = format!("{}/complete", server.base_url())
        ),
    )
    .expect("write config");

    let mut child = spawn_stdio_server(&config_path);
    let (mut stdin, mut stdout) = initialize(&mut child);

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/ai/codeReview",
            "params": {
                "diff": "diff --git a/Secret.java b/Secret.java\n+// DO_NOT_LEAK_THIS_SECRET\n",
                "uri": secret_uri,
            }
        }),
    );

    let resp = support::read_response_with_id(&mut stdout, 2);
    let result = resp.get("result").cloned().expect("result");
    assert_eq!(result.as_str(), Some("mock review"));

    mock.assert_hits(1);

    shutdown(child, stdin, stdout);
}

#[test]
fn stdio_ai_models_custom_request_returns_models_payload() {
    let _lock = support::stdio_server_lock();

    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200).json_body(json!({
            "data": [
                { "id": "alpha" },
                { "id": "beta" }
            ]
        }));
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
kind = "open_ai_compatible"
url = "{endpoint}"
model = "alpha"

[ai.privacy]
local_only = true
"#,
            endpoint = server.base_url()
        ),
    )
    .expect("write config");

    let mut child = spawn_stdio_server(&config_path);
    let (mut stdin, mut stdout) = initialize(&mut child);

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/ai/models",
            "params": {}
        }),
    );

    let resp = support::read_response_with_id(&mut stdout, 2);
    let result = resp.get("result").cloned().expect("result");
    let models = result.get("models").and_then(|v| v.as_array()).expect("models array");
    let models = models
        .iter()
        .filter_map(|v| v.as_str())
        .collect::<Vec<_>>();
    assert_eq!(models, vec!["alpha", "beta"]);

    mock.assert_hits(1);

    shutdown(child, stdin, stdout);
}

#[test]
fn stdio_ai_retry_max_retries_zero_disables_retries() {
    let _lock = support::stdio_server_lock();

    let flaky_server = FlakyAiServer::start(json!({ "completion": "mock review" }));
    let endpoint = format!("{}/complete", flaky_server.base_url());

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
retry_max_retries = 0
retry_initial_backoff_ms = 1
retry_max_backoff_ms = 1

[ai.privacy]
local_only = true
"#
        ),
    )
    .expect("write config");

    let mut child = spawn_stdio_server(&config_path);
    let (mut stdin, mut stdout) = initialize(&mut child);

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/ai/codeReview",
            "params": {
                "diff": "diff --git a/Main.java b/Main.java\n+class Main {}\n",
            }
        }),
    );

    let resp = support::read_response_with_id(&mut stdout, 2);
    assert!(
        resp.get("error").is_some(),
        "expected error result when retries are disabled, got: {resp:?}"
    );
    assert_eq!(flaky_server.hits(), 1, "expected a single provider hit");

    shutdown(child, stdin, stdout);
}

#[test]
fn stdio_ai_retry_max_retries_allows_retry_on_500() {
    let _lock = support::stdio_server_lock();

    let flaky_server = FlakyAiServer::start(json!({ "completion": "mock review" }));
    let endpoint = format!("{}/complete", flaky_server.base_url());

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
retry_max_retries = 1
retry_initial_backoff_ms = 1
retry_max_backoff_ms = 1

[ai.privacy]
local_only = true
"#
        ),
    )
    .expect("write config");

    let mut child = spawn_stdio_server(&config_path);
    let (mut stdin, mut stdout) = initialize(&mut child);

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/ai/codeReview",
            "params": {
                "diff": "diff --git a/Main.java b/Main.java\n+class Main {}\n",
            }
        }),
    );

    let resp = support::read_response_with_id(&mut stdout, 2);
    let result = resp.get("result").cloned().expect("result");
    assert_eq!(result.as_str(), Some("mock review"));
    assert_eq!(flaky_server.hits(), 2, "expected one retry after 500");

    shutdown(child, stdin, stdout);
}

#[test]
fn stdio_ai_status_custom_request_reflects_env_override_disable_ai() {
    let _lock = support::stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    std::fs::write(
        &config_path,
        r#"
[ai]
enabled = true
api_key = "supersecret"

[ai.provider]
kind = "http"
url = "http://127.0.0.1:1234/complete"
model = "default"

[ai.features]
multi_token_completion = true
"#,
    )
    .expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env_remove("NOVA_AI_COMPLETIONS_MAX_ITEMS")
        .env("NOVA_DISABLE_AI", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let (mut stdin, mut stdout) = initialize(&mut child);

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/ai/status",
            "params": {}
        }),
    );
    let resp = support::read_response_with_id(&mut stdout, 2);
    let result = resp.get("result").cloned().expect("result");
    assert_eq!(result.get("enabled").and_then(|v| v.as_bool()), Some(false));
    assert_eq!(result.get("configured").and_then(|v| v.as_bool()), Some(false));
    assert_eq!(
        result.pointer("/envOverrides/disableAi").and_then(|v| v.as_bool()),
        Some(true)
    );

    // Must not leak API keys.
    assert!(
        !result.to_string().contains("supersecret"),
        "expected status payload to omit API keys; got: {result:#?}"
    );

    shutdown(child, stdin, stdout);
}

#[test]
fn stdio_ai_status_custom_request_reflects_env_override_disable_ai_completions() {
    let _lock = support::stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    std::fs::write(
        &config_path,
        r#"
[ai]
enabled = true
api_key = "supersecret"

[ai.provider]
kind = "http"
url = "http://127.0.0.1:1234/complete"
model = "default"

[ai.features]
multi_token_completion = true
"#,
    )
    .expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_AI_COMPLETIONS_MAX_ITEMS")
        .env("NOVA_DISABLE_AI_COMPLETIONS", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let (mut stdin, mut stdout) = initialize(&mut child);

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/ai/status",
            "params": {}
        }),
    );
    let resp = support::read_response_with_id(&mut stdout, 2);
    let result = resp.get("result").cloned().expect("result");
    assert_eq!(result.get("enabled").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(result.get("configured").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(
        result
            .pointer("/envOverrides/disableAiCompletions")
            .and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        result
            .pointer("/features/multi_token_completion")
            .and_then(|v| v.as_bool()),
        Some(false)
    );

    // Must not leak API keys.
    assert!(
        !result.to_string().contains("supersecret"),
        "expected status payload to omit API keys; got: {result:#?}"
    );

    shutdown(child, stdin, stdout);
}
