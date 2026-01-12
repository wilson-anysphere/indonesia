use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use tempfile::TempDir;

#[test]
fn stdio_server_exposes_extensions_status_and_navigation_requests() {
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
    let initialize_resp = read_jsonrpc_response(&mut stdout, 1);

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
    let status_resp = read_jsonrpc_response(&mut stdout, 2);
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
    let navigation_resp = read_jsonrpc_response(&mut stdout, 3);
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
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "shutdown" }),
    );
    let _shutdown_resp = read_jsonrpc_response(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

fn write_jsonrpc_message(writer: &mut impl Write, message: &serde_json::Value) {
    let bytes = serde_json::to_vec(message).expect("serialize");
    write!(writer, "Content-Length: {}\r\n\r\n", bytes.len()).expect("write header");
    writer.write_all(&bytes).expect("write body");
    writer.flush().expect("flush");
}

fn read_jsonrpc_response(reader: &mut impl BufRead, expected_id: i64) -> serde_json::Value {
    loop {
        let message = read_jsonrpc_message(reader);
        if message.get("id").and_then(|v| v.as_i64()) == Some(expected_id) {
            return message;
        }
    }
}

fn read_jsonrpc_message(reader: &mut impl BufRead) -> serde_json::Value {
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
