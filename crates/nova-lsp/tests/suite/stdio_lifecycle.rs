use serde_json::json;
use std::io::BufReader;
use std::process::{Command, Stdio};

use crate::support::{read_response_with_id, write_jsonrpc_message};

#[test]
fn stdio_initialize_parse_errors_do_not_echo_secret_string_values() {
    let _lock = crate::support::stdio_server_lock();

    // The secret intentionally contains an embedded quote. When `lsp_server` fails to deserialize
    // an incoming JSON-RPC message, it can surface a `serde_json::Error` display string that echoes
    // scalar values (e.g. `invalid type: string "..."`). Ensure the server sanitizes those error
    // strings before they hit stderr/logs.
    let secret_suffix = "NOVA_SECRET_DO_NOT_LEAK_FROM_INIT_PARSE";
    let secret = format!("prefix\"{secret_suffix}");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    {
        let mut stdin = child.stdin.take().expect("stdin");
        write_jsonrpc_message(&mut stdin, &json!(secret));
    }

    let output = child.wait_with_output().expect("wait_with_output");
    assert!(
        !output.status.success(),
        "expected invalid initialize message to exit non-zero, got {output:?}"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains(secret_suffix),
        "expected initialize parse error output to omit secret string values; got: {stderr}"
    );
}

#[test]
fn stdio_exit_without_shutdown_returns_failure_status() {
    let _lock = crate::support::stdio_server_lock();
    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "capabilities": {} }
        }),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    // Exit without a shutdown request: per LSP the server should exit non-zero.
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert_eq!(
        status.code(),
        Some(1),
        "expected LSP exit without shutdown to return status=1, got {status:?}"
    );
}

#[test]
fn initialize_advertises_nova_experimental_capabilities() {
    let _lock = crate::support::stdio_server_lock();
    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "capabilities": {} }
        }),
    );
    let initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    let requests = initialize_resp
        .get("result")
        .and_then(|r| r.get("capabilities"))
        .and_then(|c| c.get("experimental"))
        .and_then(|e| e.get("nova"))
        .and_then(|n| n.get("requests"))
        .and_then(|v| v.as_array())
        .expect("initializeResult.capabilities.experimental.nova.requests");

    let has_metrics = requests
        .iter()
        .any(|v| v.as_str() == Some(nova_lsp::METRICS_METHOD));
    assert!(
        has_metrics,
        "expected capabilities.experimental.nova.requests to include nova/metrics, got: {requests:?}"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 2);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_requests_after_shutdown_are_rejected() {
    let _lock = crate::support::stdio_server_lock();
    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "capabilities": {} }
        }),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 2);

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "textDocument/completion", "params": {} }),
    );
    let completion_resp = read_response_with_id(&mut stdout, 3);
    assert_eq!(
        completion_resp
            .get("error")
            .and_then(|err| err.get("code"))
            .and_then(|code| code.as_i64()),
        Some(-32600),
        "expected requests after shutdown to be rejected, got: {completion_resp:?}"
    );

    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn stdio_cancel_request_cancels_inflight_request_by_id() {
    let _lock = crate::support::stdio_server_lock();
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().join("gradle-project");
    fs::create_dir_all(&root).expect("create project dir");

    // Minimal Gradle project layout so `nova/buildProject` will attempt to run the wrapper script.
    fs::write(root.join("settings.gradle"), "rootProject.name = 'demo'\n").expect("write settings");
    fs::write(root.join("build.gradle"), "plugins { id 'java' }\n").expect("write build.gradle");

    let java_dir = root.join("src/main/java/com/example");
    fs::create_dir_all(&java_dir).expect("create java dir");
    fs::write(
        java_dir.join("Foo.java"),
        "package com.example; public class Foo {}",
    )
    .expect("write Foo.java");

    let gradlew_path = root.join("gradlew");
    fs::write(
        &gradlew_path,
        r#"#!/bin/sh
sentinel="${0}.did_sleep"
if [ ! -f "$sentinel" ]; then
  : > "$sentinel"
  sleep 1
fi
exit 0
"#,
    )
    .expect("write fake gradlew");

    let mut perms = fs::metadata(&gradlew_path)
        .expect("stat gradlew")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&gradlew_path, perms).expect("chmod gradlew");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // Initialize + initialized
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "capabilities": {} }
        }),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    // Send a long-running request to occupy the main loop.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/buildProject",
            "params": { "projectRoot": root.to_string_lossy(), "buildTool": "gradle" }
        }),
    );

    // Queue a second request behind it, then cancel it. Cancellation should be tracked by request id
    // and must not crash the server.
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "$/cancelRequest", "params": { "id": 3 } }),
    );

    let _build_resp = read_response_with_id(&mut stdout, 2);

    let cancelled_shutdown = read_response_with_id(&mut stdout, 3);
    let code = cancelled_shutdown
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|v| v.as_i64());
    assert_eq!(
        code,
        Some(-32800),
        "expected cancelled request to return code -32800, got: {cancelled_shutdown:?}"
    );

    // Proper shutdown + exit should still succeed.
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
