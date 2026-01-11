use serde_json::json;
use std::io::BufReader;
use std::process::{Command, Stdio};

mod support;
use support::{read_response_with_id, write_jsonrpc_message};

#[test]
fn stdio_exit_without_shutdown_returns_failure_status() {
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
