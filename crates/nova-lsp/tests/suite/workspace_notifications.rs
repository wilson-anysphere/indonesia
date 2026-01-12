use nova_core::{path_to_file_uri, AbsPathBuf};
use serde_json::json;
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support::{read_response_with_id, stdio_server_lock, write_jsonrpc_message};

fn uri_for_path(path: &Path) -> String {
    let abs = AbsPathBuf::canonicalize(path).expect("abs path");
    path_to_file_uri(&abs).expect("file uri")
}

#[test]
fn stdio_workspace_folder_change_updates_project_root_and_keeps_server_responsive() {
    let _lock = stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root1 = temp.path().join("ws1");
    let root2 = temp.path().join("ws2");
    std::fs::create_dir_all(&root1).expect("create ws1");
    std::fs::create_dir_all(&root2).expect("create ws2");

    let cache_dir = TempDir::new().expect("cache dir");

    let foo_path = root1.join("Foo.java");
    std::fs::write(
        &foo_path,
        r#"
            package com.example;
            public class Foo { public void bar() {} }
        "#,
    )
    .expect("write Foo.java");

    let bar_path = root2.join("Bar.java");
    std::fs::write(
        &bar_path,
        r#"
            package com.example;
            public class Bar { public void baz() {} }
        "#,
    )
    .expect("write Bar.java");

    let root1_uri = uri_for_path(&root1);
    let root2_uri = uri_for_path(&root2);
    let foo_uri = uri_for_path(&foo_path);
    let bar_uri = uri_for_path(&bar_path);

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
            "params": { "rootUri": root1_uri.clone(), "capabilities": {} }
        }),
    );
    let initialize_resp = read_response_with_id(&mut stdout, 1);
    assert!(
        initialize_resp
            .pointer("/result/capabilities/workspace/workspaceFolders/supported")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        "expected workspaceFolders capability to be advertised, got: {initialize_resp:?}"
    );
    assert!(
        initialize_resp
            .pointer("/result/capabilities/workspace/fileOperations/didCreate/filters")
            .and_then(|v| v.as_array())
            .is_some(),
        "expected fileOperations capability to be advertised, got: {initialize_resp:?}"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    // First request ensures the workspace is loaded for root1.
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
                && value.pointer("/location/uri").and_then(|v| v.as_str()) == Some(foo_uri.as_str())
        }),
        "expected Foo symbol in initial workspace, got: {resp:?}"
    );

    // Switch to root2 and ensure subsequent requests use the new project root.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "workspace/didChangeWorkspaceFolders",
            "params": {
                "event": {
                    "added": [{ "uri": root2_uri.clone(), "name": "ws2" }],
                    "removed": [{ "uri": root1_uri, "name": "ws1" }]
                }
            }
        }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "workspace/symbol",
            "params": { "query": "Bar" }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    let results = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("workspace/symbol result array");
    assert!(
        results.iter().any(|value| {
            value.get("name").and_then(|v| v.as_str()) == Some("Bar")
                && value.pointer("/location/uri").and_then(|v| v.as_str()) == Some(bar_uri.as_str())
        }),
        "expected Bar symbol after workspace folder change, got: {resp:?}"
    );

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

#[test]
fn stdio_did_change_configuration_reloads_nova_config_and_keeps_server_responsive() {
    let _lock = stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    std::fs::write(&config_path, "[extensions]\nenabled = false\n").expect("write config");

    let cache_dir = TempDir::new().expect("cache dir");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        // Ensure a developer's legacy AI env-var wiring can't override the config file and make
        // this test flaky.
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
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
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/extensions/status",
            "params": {}
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    assert_eq!(
        resp.pointer("/result/enabled").and_then(|v| v.as_bool()),
        Some(false),
        "expected extensions.enabled=false initially, got: {resp:?}"
    );

    // Update config on disk and notify the server.
    std::fs::write(&config_path, "[extensions]\nenabled = true\n").expect("rewrite config");
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "workspace/didChangeConfiguration",
            "params": { "settings": {} }
        }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "nova/extensions/status",
            "params": {}
        }),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    assert_eq!(
        resp.pointer("/result/enabled").and_then(|v| v.as_bool()),
        Some(true),
        "expected didChangeConfiguration to reload config, got: {resp:?}"
    );

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
