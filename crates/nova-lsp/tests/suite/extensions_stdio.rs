use serde_json::Value;
use std::io::BufReader;
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support::{
    decode_initialize_result, exit_notification, initialize_request_empty,
    initialized_notification, jsonrpc_request, read_response_with_id, shutdown_request,
    stdio_server_lock, write_jsonrpc_message,
};

#[test]
fn stdio_server_exposes_extensions_status_and_navigation_requests() {
    let _lock = stdio_server_lock();
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

    write_jsonrpc_message(&mut stdin, &initialize_request_empty(1));
    let initialize_resp = read_response_with_id(&mut stdout, 1);

    let init = decode_initialize_result(&initialize_resp);
    let experimental = init
        .capabilities
        .experimental
        .as_ref()
        .expect("initializeResult.capabilities.experimental");
    let nova = experimental
        .get("nova")
        .and_then(|v| v.as_object())
        .expect("initializeResult.capabilities.experimental.nova");
    let requests = nova
        .get("requests")
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

    write_jsonrpc_message(&mut stdin, &initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(Value::Null, 2, nova_lsp::EXTENSIONS_STATUS_METHOD),
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
        &jsonrpc_request(
            Value::Object({
                let mut params = serde_json::Map::new();
                params.insert(
                    "textDocument".to_string(),
                    Value::Object({
                        let mut doc = serde_json::Map::new();
                        doc.insert("uri".to_string(), Value::String(uri.clone()));
                        doc
                    }),
                );
                params
            }),
            3,
            nova_lsp::EXTENSIONS_NAVIGATION_METHOD,
        ),
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
        &jsonrpc_request(
            Value::Object({
                let mut params = serde_json::Map::new();
                params.insert(
                    "schemaVersion".to_string(),
                    Value::Number(serde_json::Number::from(2u64)),
                );
                params.insert(
                    "textDocument".to_string(),
                    Value::Object({
                        let mut doc = serde_json::Map::new();
                        doc.insert("uri".to_string(), Value::String(uri.clone()));
                        doc
                    }),
                );
                params
            }),
            4,
            nova_lsp::EXTENSIONS_NAVIGATION_METHOD,
        ),
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
        &jsonrpc_request(
            Value::Object({
                let mut params = serde_json::Map::new();
                params.insert(
                    "schemaVersion".to_string(),
                    Value::Number(serde_json::Number::from(2u64)),
                );
                params
            }),
            5,
            nova_lsp::EXTENSIONS_STATUS_METHOD,
        ),
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

    write_jsonrpc_message(&mut stdin, &shutdown_request(6));
    let _shutdown_resp = read_response_with_id(&mut stdout, 6);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
