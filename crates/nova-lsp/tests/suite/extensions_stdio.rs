use serde_json::json;
use std::io::BufReader;
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support::{read_response_with_id, write_jsonrpc_message};

#[test]
fn stdio_server_exposes_extensions_status_and_navigation_requests() {
    let _lock = crate::support::stdio_server_lock();
    let tmp = TempDir::new().expect("tempdir");
    let file_path = tmp.path().join("Foo.java");
    std::fs::write(&file_path, "class Foo {}\n").expect("write temp Foo.java");
    let uri = url::Url::from_file_path(&file_path)
        .expect("file uri")
        .to_string();

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

    let requests = initialize_resp
        .pointer("/result/capabilities/experimental/nova/requests")
        .and_then(|v| v.as_array())
        .expect("initializeResult.capabilities.experimental.nova.requests must be an array");

    assert!(
        requests
            .iter()
            .filter_map(|v| v.as_str())
            .any(|m| m == nova_lsp::EXTENSIONS_STATUS_METHOD),
        "expected {} to be advertised in experimental.nova.requests; got {requests:?}",
        nova_lsp::EXTENSIONS_STATUS_METHOD
    );
    assert!(
        requests
            .iter()
            .filter_map(|v| v.as_str())
            .any(|m| m == nova_lsp::EXTENSIONS_NAVIGATION_METHOD),
        "expected {} to be advertised in experimental.nova.requests; got {requests:?}",
        nova_lsp::EXTENSIONS_NAVIGATION_METHOD
    );
    assert!(
        requests
            .iter()
            .filter_map(|v| v.as_str())
            .any(|m| m == nova_lsp::BUILD_FILE_CLASSPATH_METHOD),
        "expected {} to be advertised in experimental.nova.requests; got {requests:?}",
        nova_lsp::BUILD_FILE_CLASSPATH_METHOD
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": nova_lsp::EXTENSIONS_STATUS_METHOD,
            "params": null
        }),
    );
    let status_resp = read_response_with_id(&mut stdout, 2);
    let status_result = status_resp.get("result").cloned().expect("result");

    assert_eq!(
        status_result.get("schemaVersion").and_then(|v| v.as_u64()),
        Some(nova_lsp::EXTENSIONS_STATUS_SCHEMA_VERSION as u64)
    );
    assert!(
        status_result
            .get("enabled")
            .and_then(|v| v.as_bool())
            .is_some(),
        "expected status.enabled to be a bool; got {status_result}"
    );
    assert!(
        status_result
            .get("wasmPaths")
            .and_then(|v| v.as_array())
            .is_some(),
        "expected status.wasmPaths to be an array; got {status_result}"
    );
    assert!(
        matches!(
            status_result.get("allow"),
            Some(serde_json::Value::Null) | Some(serde_json::Value::Array(_))
        ),
        "expected status.allow to be null or an array; got {status_result}"
    );
    assert!(
        status_result
            .get("deny")
            .and_then(|v| v.as_array())
            .is_some(),
        "expected status.deny to be an array; got {status_result}"
    );
    assert!(
        status_result
            .get("loadedExtensions")
            .and_then(|v| v.as_array())
            .is_some(),
        "expected status.loadedExtensions to be an array; got {status_result}"
    );
    assert!(
        status_result
            .get("stats")
            .and_then(|v| v.as_object())
            .is_some_and(|stats| {
                ["diagnostic", "completion", "codeAction", "navigation", "inlayHint"]
                    .iter()
                    .all(|k| stats.get(*k).is_some())
            }),
        "expected status.stats to contain diagnostic/completion/codeAction/navigation/inlayHint; got {status_result}"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": nova_lsp::EXTENSIONS_NAVIGATION_METHOD,
            "params": { "textDocument": { "uri": uri } }
        }),
    );
    let navigation_resp = read_response_with_id(&mut stdout, 3);
    let navigation_result = navigation_resp.get("result").cloned().expect("result");

    assert_eq!(
        navigation_result
            .get("schemaVersion")
            .and_then(|v| v.as_u64()),
        Some(nova_lsp::EXTENSIONS_NAVIGATION_SCHEMA_VERSION as u64)
    );
    let targets = navigation_result
        .get("targets")
        .and_then(|v| v.as_array())
        .expect("targets array");

    for target in targets {
        let target = target.as_object().expect("target object");
        assert!(
            target.get("label").and_then(|v| v.as_str()).is_some(),
            "expected target.label to be a string; got {target:?}"
        );
        assert!(
            target.get("uri").and_then(|v| v.as_str()).is_some(),
            "expected target.uri to be a string; got {target:?}"
        );
        assert!(
            target.get("fileId").and_then(|v| v.as_u64()).is_some(),
            "expected target.fileId to be a number; got {target:?}"
        );

        if let Some(range) = target.get("range") {
            if !range.is_null() {
                assert!(
                    range.get("start").is_some() && range.get("end").is_some(),
                    "expected target.range to look like an LSP Range; got {range:?}"
                );
            }
        }

        if let Some(span) = target.get("span") {
            if !span.is_null() {
                assert!(
                    span.get("start").and_then(|v| v.as_u64()).is_some()
                        && span.get("end").and_then(|v| v.as_u64()).is_some(),
                    "expected target.span to have numeric start/end; got {span:?}"
                );
            }
        }
    }

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": nova_lsp::EXTENSIONS_NAVIGATION_METHOD,
            "params": { "schemaVersion": 2, "textDocument": { "uri": uri } }
        }),
    );
    let navigation_bad_schema = read_response_with_id(&mut stdout, 4);
    let error = navigation_bad_schema.get("error").cloned().expect("error");
    assert_eq!(error.get("code").and_then(|v| v.as_i64()), Some(-32603));
    assert!(
        error
            .get("message")
            .and_then(|v| v.as_str())
            .is_some_and(|msg| msg.contains("unsupported schemaVersion 2 (expected 1)")),
        "expected schema version mismatch error message; got {error}"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": nova_lsp::EXTENSIONS_STATUS_METHOD,
            "params": { "schemaVersion": 2 }
        }),
    );
    let status_bad_schema = read_response_with_id(&mut stdout, 5);
    let error = status_bad_schema.get("error").cloned().expect("error");
    assert_eq!(error.get("code").and_then(|v| v.as_i64()), Some(-32602));
    assert!(
        error
            .get("message")
            .and_then(|v| v.as_str())
            .is_some_and(|msg| msg.contains("unsupported schemaVersion 2 (expected 1)")),
        "expected schema version mismatch error message; got {error}"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 6, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 6);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_invalid_params_errors_do_not_echo_secret_string_values() {
    let _lock = crate::support::stdio_server_lock();

    // The secret intentionally contains an embedded quote. `serde_json::Error` display strings
    // escape that quote (e.g. `string "prefix\"secret"`). Make sure the server sanitizes the full
    // string even in the presence of escapes.
    let secret_suffix = "NOVA_SECRET_DO_NOT_LEAK_FROM_INVALID_PARAMS";
    let secret = format!("prefix\"{secret_suffix}");

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

    // Exercise a stateless `nova/*` endpoint implemented in the library crate (so the
    // `NovaLspError::InvalidParams` path is used) with an invalid scalar payload.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": nova_lsp::TEST_DISCOVER_METHOD,
            "params": secret,
        }),
    );

    let resp = read_response_with_id(&mut stdout, 2);
    let error = resp.get("error").cloned().expect("expected error response");
    assert_eq!(error.get("code").and_then(|v| v.as_i64()), Some(-32602));
    assert!(
        !resp.to_string().contains(secret_suffix),
        "expected JSON-RPC error to omit secret string values; got: {resp:?}"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
