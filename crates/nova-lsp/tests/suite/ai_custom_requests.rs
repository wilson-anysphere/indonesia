use httpmock::prelude::*;
use serde_json::json;
use std::io::BufReader;
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support;

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

fn spawn_stdio_server_with_legacy_env_retry_config(
    config_path: &std::path::Path,
    endpoint: &str,
    max_retries: &str,
) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(config_path)
        // Ensure a developer's environment doesn't disable AI unexpectedly.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env_remove("NOVA_AI_COMPLETIONS_MAX_ITEMS")
        // Reset any legacy AI config so only the values set below apply.
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        .env_remove("NOVA_AI_RETRY_MAX_RETRIES")
        .env_remove("NOVA_AI_RETRY_INITIAL_BACKOFF_MS")
        .env_remove("NOVA_AI_RETRY_MAX_BACKOFF_MS")
        .env("NOVA_AI_PROVIDER", "http")
        .env("NOVA_AI_ENDPOINT", endpoint)
        .env("NOVA_AI_MODEL", "default")
        .env("NOVA_AI_RETRY_MAX_RETRIES", max_retries)
        // Keep the tests fast: avoid the default 200ms backoff.
        .env("NOVA_AI_RETRY_INITIAL_BACKOFF_MS", "1")
        .env("NOVA_AI_RETRY_MAX_BACKOFF_MS", "1")
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

#[test]
fn stdio_ai_custom_request_invalid_params_does_not_echo_secret_string_values() {
    let _lock = support::stdio_server_lock();

    let secret = "NOVA_SECRET_DO_NOT_LEAK";

    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    std::fs::write(&config_path, "[ai]\nenabled = false\n").expect("write config");

    let mut child = spawn_stdio_server(&config_path);
    let (mut stdin, mut stdout) = initialize(&mut child);

    // Pass a scalar instead of the expected params object. `serde_json::Error` would normally echo
    // string values (including secrets) in its display output; ensure the server sanitizes that
    // error before returning it to clients.
    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/ai/codeReview",
            "params": secret,
        }),
    );

    let resp = support::read_response_with_id(&mut stdout, 2);
    let error = resp.get("error").cloned().expect("expected error response");
    assert_eq!(error.get("code").and_then(|v| v.as_i64()), Some(-32602));
    assert!(
        !resp.to_string().contains(secret),
        "expected JSON-RPC error to omit secret string values; got: {resp:?}"
    );

    shutdown(child, stdin, stdout);
}

#[test]
fn stdio_execute_command_invalid_params_does_not_echo_secret_string_values() {
    let _lock = support::stdio_server_lock();

    let secret = "NOVA_EXEC_COMMAND_SECRET_DO_NOT_LEAK";

    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    std::fs::write(&config_path, "[ai]\nenabled = false\n").expect("write config");

    let mut child = spawn_stdio_server(&config_path);
    let (mut stdin, mut stdout) = initialize(&mut child);

    // Pass a scalar instead of the expected params object. `serde_json::Error` would normally echo
    // string values (including secrets) in its display output; ensure the server sanitizes that
    // error before returning it to clients.
    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "workspace/executeCommand",
            "params": secret,
        }),
    );

    let resp = support::read_response_with_id(&mut stdout, 3);
    let error = resp.get("error").cloned().expect("expected error response");
    assert_eq!(error.get("code").and_then(|v| v.as_i64()), Some(-32602));
    assert!(
        !resp.to_string().contains(secret),
        "expected JSON-RPC error to omit secret string values; got: {resp:?}"
    );

    shutdown(child, stdin, stdout);
}

fn code_review_request_omits_excluded_diff(req: &HttpMockRequest) -> bool {
    const SECRET_MARKER: &str = "DO_NOT_LEAK_THIS_SECRET";
    let Some(body) = req.body.as_deref() else {
        return false;
    };
    let body = String::from_utf8_lossy(body);
    body.contains("[diff omitted due to excluded_paths]") && !body.contains(SECRET_MARKER)
}

const CODE_REVIEW_TRUNCATION_HEAD_MARKER: &str = "NOVA_CODE_REVIEW_HEAD_MARKER_8b06c0ad";
const CODE_REVIEW_TRUNCATION_MIDDLE_MARKER: &str = "NOVA_CODE_REVIEW_MIDDLE_MARKER_b0cf6f47";
const CODE_REVIEW_TRUNCATION_TAIL_MARKER: &str = "NOVA_CODE_REVIEW_TAIL_MARKER_7f57f676";

fn code_review_request_truncates_large_diff(req: &HttpMockRequest) -> bool {
    let Some(body) = req.body.as_deref() else {
        return false;
    };
    let body = String::from_utf8_lossy(body);
    body.contains(CODE_REVIEW_TRUNCATION_HEAD_MARKER)
        && body.contains(CODE_REVIEW_TRUNCATION_TAIL_MARKER)
        && body.contains("[diff truncated: omitted")
        && !body.contains(CODE_REVIEW_TRUNCATION_MIDDLE_MARKER)
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
fn stdio_ai_code_review_custom_request_truncates_large_diffs() {
    let _lock = support::stdio_server_lock();

    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/complete")
            .matches(code_review_request_truncates_large_diff);
        then.status(200).json_body(json!({ "completion": "mock review" }));
    });

    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.features]
code_review_max_diff_chars = 200

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

    let diff = format!(
        "diff --git a/Main.java b/Main.java\n+// {head}\n+{a}\n+// {middle}\n+{b}\n+// {tail}\n",
        head = CODE_REVIEW_TRUNCATION_HEAD_MARKER,
        middle = CODE_REVIEW_TRUNCATION_MIDDLE_MARKER,
        tail = CODE_REVIEW_TRUNCATION_TAIL_MARKER,
        a = "A".repeat(2000),
        b = "B".repeat(2000),
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/ai/codeReview",
            "params": { "diff": diff }
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
fn stdio_ai_models_custom_request_returns_empty_list_when_listing_is_unsupported() {
    let _lock = support::stdio_server_lock();

    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(404).json_body(json!({ "error": "not supported" }));
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

    // Allow `params` to be `null` for this endpoint.
    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/ai/models",
            "params": null
        }),
    );

    let resp = support::read_response_with_id(&mut stdout, 2);
    assert!(
        resp.get("error").is_none(),
        "expected success response, got: {resp:?}"
    );
    let models = resp
        .pointer("/result/models")
        .and_then(|v| v.as_array())
        .expect("models array");
    assert!(models.is_empty(), "expected empty models list, got: {models:?}");

    mock.assert_hits(1);

    shutdown(child, stdin, stdout);
}

#[test]
fn stdio_ai_retry_max_retries_zero_disables_retries() {
    let _lock = support::stdio_server_lock();

    let flaky_server = support::TestAiServer::start_flaky(json!({ "completion": "mock review" }));
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
    flaky_server.assert_hits(1);

    shutdown(child, stdin, stdout);
}

#[test]
fn stdio_ai_retry_max_retries_allows_retry_on_500() {
    let _lock = support::stdio_server_lock();

    let flaky_server = support::TestAiServer::start_flaky(json!({ "completion": "mock review" }));
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
    flaky_server.assert_hits(2);

    shutdown(child, stdin, stdout);
}

#[test]
fn stdio_ai_legacy_env_retry_max_retries_zero_disables_retries() {
    let _lock = support::stdio_server_lock();

    let flaky_server = support::TestAiServer::start_flaky(json!({ "completion": "mock review" }));
    let endpoint = format!("{}/complete", flaky_server.base_url());

    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    std::fs::write(&config_path, "[ai]\nenabled = false\n").expect("write config");

    let mut child = spawn_stdio_server_with_legacy_env_retry_config(&config_path, &endpoint, "0");
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
    flaky_server.assert_hits(1);

    shutdown(child, stdin, stdout);
}

#[test]
fn stdio_ai_legacy_env_retry_max_retries_allows_retry_on_500() {
    let _lock = support::stdio_server_lock();

    let flaky_server = support::TestAiServer::start_flaky(json!({ "completion": "mock review" }));
    let endpoint = format!("{}/complete", flaky_server.base_url());

    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    std::fs::write(&config_path, "[ai]\nenabled = false\n").expect("write config");

    let mut child = spawn_stdio_server_with_legacy_env_retry_config(&config_path, &endpoint, "1");
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
    flaky_server.assert_hits(2);

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

#[test]
fn stdio_ai_status_custom_request_reflects_env_override_disable_ai_code_actions() {
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
explain_errors = true
code_actions = true
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
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env_remove("NOVA_DISABLE_AI_CODE_REVIEW")
        .env_remove("NOVA_AI_COMPLETIONS_MAX_ITEMS")
        .env("NOVA_DISABLE_AI_CODE_ACTIONS", "1")
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
            .pointer("/envOverrides/disableAiCodeActions")
            .and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        result
            .pointer("/features/explain_errors")
            .and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        result
            .pointer("/features/code_actions")
            .and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        result
            .pointer("/features/multi_token_completion")
            .and_then(|v| v.as_bool()),
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
fn stdio_ai_status_custom_request_reflects_env_override_disable_ai_code_review() {
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
code_actions = true
code_review = true
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
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env_remove("NOVA_DISABLE_AI_CODE_ACTIONS")
        .env_remove("NOVA_AI_COMPLETIONS_MAX_ITEMS")
        .env("NOVA_DISABLE_AI_CODE_REVIEW", "1")
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
            .pointer("/envOverrides/disableAiCodeReview")
            .and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        result
            .pointer("/features/code_review")
            .and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        result
            .pointer("/features/code_actions")
            .and_then(|v| v.as_bool()),
        Some(true)
    );

    // Must not leak API keys.
    assert!(
        !result.to_string().contains("supersecret"),
        "expected status payload to omit API keys; got: {result:#?}"
    );

    shutdown(child, stdin, stdout);
}
