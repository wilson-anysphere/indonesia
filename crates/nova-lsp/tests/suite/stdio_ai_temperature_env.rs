use httpmock::prelude::*;
use serde_json::json;
use std::io::BufReader;
use std::process::{Command, Stdio};

use crate::support;

fn request_body_not_contains(req: &HttpMockRequest, needle: &str) -> bool {
    let Some(body) = req.body.as_deref() else {
        return true;
    };
    let body = String::from_utf8_lossy(body);
    !body.contains(needle)
}

fn spawn_stdio_server(server: &MockServer, temperature: Option<&str>) -> std::process::Child {
    let endpoint = format!("{}/complete", server.base_url());

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nova-lsp"));
    cmd.arg("--stdio")
        // Avoid inheriting config discovery overrides from developer shells.
        .env_remove("NOVA_CONFIG_PATH")
        // Avoid inheriting server-side AI disables from developer shells.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env_remove("NOVA_DISABLE_AI_CODE_ACTIONS")
        .env_remove("NOVA_DISABLE_AI_CODE_REVIEW")
        // Avoid inheriting unrelated AI overrides.
        .env_remove("NOVA_AI_COMPLETIONS_MAX_ITEMS")
        .env_remove("NOVA_AI_MAX_TOKENS")
        .env_remove("NOVA_AI_CONCURRENCY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        .env_remove("NOVA_AI_CACHE_ENABLED")
        .env_remove("NOVA_AI_CACHE_MAX_ENTRIES")
        .env_remove("NOVA_AI_CACHE_TTL_SECS")
        .env_remove("NOVA_AI_TIMEOUT_SECS")
        // Configure legacy env-var based AI provider wiring.
        .env("NOVA_AI_PROVIDER", "http")
        .env("NOVA_AI_ENDPOINT", &endpoint)
        .env("NOVA_AI_MODEL", "default");

    match temperature {
        Some(value) => {
            cmd.env("NOVA_AI_TEMPERATURE", value);
        }
        None => {
            cmd.env_remove("NOVA_AI_TEMPERATURE");
        }
    }

    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp")
}

fn initialize(
    child: &mut std::process::Child,
) -> (
    std::process::ChildStdin,
    BufReader<std::process::ChildStdout>,
) {
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

fn shutdown(
    mut child: std::process::Child,
    mut stdin: std::process::ChildStdin,
    mut stdout: BufReader<std::process::ChildStdout>,
) {
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
fn stdio_ai_env_temperature_is_sent_when_configured() {
    let _lock = support::stdio_server_lock();

    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/complete")
            .body_contains("\"temperature\":0.2");
        then.status(200)
            .json_body(json!({ "completion": "mock explanation" }));
    });

    let mut child = spawn_stdio_server(&server, Some("0.2"));
    let (mut stdin, mut stdout) = initialize(&mut child);

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/ai/explainError",
            "params": {
                "diagnosticMessage": "cannot find symbol",
                "code": "unknown()"
            }
        }),
    );
    let explain_resp = support::read_response_with_id(&mut stdout, 2);
    assert_eq!(
        explain_resp.get("result").and_then(|v| v.as_str()),
        Some("mock explanation"),
        "expected explainError to return provider completion, got: {explain_resp:#?}"
    );

    shutdown(child, stdin, stdout);
    mock.assert_hits(1);
}

#[test]
fn stdio_ai_env_temperature_is_omitted_when_unset() {
    let _lock = support::stdio_server_lock();

    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/complete")
            .matches(|req| request_body_not_contains(req, "\"temperature\":"));
        then.status(200)
            .json_body(json!({ "completion": "mock explanation" }));
    });

    let mut child = spawn_stdio_server(&server, None);
    let (mut stdin, mut stdout) = initialize(&mut child);

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/ai/explainError",
            "params": {
                "diagnosticMessage": "cannot find symbol",
                "code": "unknown()"
            }
        }),
    );
    let explain_resp = support::read_response_with_id(&mut stdout, 2);
    assert_eq!(
        explain_resp.get("result").and_then(|v| v.as_str()),
        Some("mock explanation"),
        "expected explainError to return provider completion, got: {explain_resp:#?}"
    );

    shutdown(child, stdin, stdout);
    mock.assert_hits(1);
}
