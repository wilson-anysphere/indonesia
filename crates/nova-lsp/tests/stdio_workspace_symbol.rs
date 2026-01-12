use nova_core::{path_to_file_uri, AbsPathBuf};
use serde_json::json;
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;

mod support;
use support::{read_response_with_id, write_jsonrpc_message};

fn uri_for_path(path: &Path) -> String {
    let abs = AbsPathBuf::canonicalize(path).expect("abs path");
    path_to_file_uri(&abs).expect("file uri")
}

#[test]
fn stdio_server_supports_workspace_symbol_requests() {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let cache_dir = TempDir::new().expect("cache dir");

    let file_path = root.join("Foo.java");
    std::fs::write(
        &file_path,
        r#"
            package com.example;

            public class Foo {
                public void bar() {}
            }
        "#,
    )
    .expect("write java file");

    let root_uri = uri_for_path(root);
    let file_uri = uri_for_path(&file_path);

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .env("NOVA_CACHE_DIR", cache_dir.path())
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
            "params": { "rootUri": root_uri, "capabilities": {} }
        }),
    );
    let initialize_resp = read_response_with_id(&mut stdout, 1);
    assert!(
        initialize_resp
            .pointer("/result/capabilities/workspaceSymbolProvider")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        "expected workspaceSymbolProvider capability"
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
            "method": "workspace/symbol",
            "params": { "query": "Foo" }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let results = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("workspace/symbol result array");

    assert!(
        results.iter().any(|value| {
            value.get("name").and_then(|v| v.as_str()) == Some("Foo")
                && value
                    .pointer("/location/uri")
                    .and_then(|v| v.as_str())
                    == Some(file_uri.as_str())
        }),
        "expected to find Foo symbol pointing at Foo.java"
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

