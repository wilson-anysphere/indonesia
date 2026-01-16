use lsp_types::{
    DidChangeConfigurationParams, DidChangeWorkspaceFoldersParams, Location, OneOf,
    PartialResultParams, Uri, WorkDoneProgressParams, WorkspaceFolder, WorkspaceFoldersChangeEvent,
    WorkspaceLocation, WorkspaceSymbolParams, WorkspaceSymbolResponse,
};
use nova_core::{path_to_file_uri, AbsPathBuf};
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support::{
    decode_initialize_result, empty_object, exit_notification, initialize_request_empty,
    initialize_request_with_root_uri, initialized_notification, jsonrpc_notification,
    jsonrpc_request, read_response_with_id, shutdown_request, stdio_server_lock,
    write_jsonrpc_message,
};

fn uri_for_path(path: &Path) -> Uri {
    let abs = AbsPathBuf::canonicalize(path).expect("abs path");
    path_to_file_uri(&abs)
        .expect("file uri")
        .parse()
        .expect("lsp uri")
}

fn has_symbol_named_at_uri(
    result: WorkspaceSymbolResponse,
    name: &str,
    expected_uri: &Uri,
) -> bool {
    match result {
        WorkspaceSymbolResponse::Flat(items) => items
            .into_iter()
            .any(|sym| sym.name == name && sym.location.uri == *expected_uri),
        WorkspaceSymbolResponse::Nested(items) => items.into_iter().any(|sym| {
            if sym.name != name {
                return false;
            }
            match sym.location {
                OneOf::Left(Location { uri, .. }) => uri == *expected_uri,
                OneOf::Right(WorkspaceLocation { uri }) => uri == *expected_uri,
            }
        }),
    }
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
        &initialize_request_with_root_uri(1, root1_uri.as_str().to_string()),
    );
    let initialize_resp = read_response_with_id(&mut stdout, 1);
    let init = decode_initialize_result(&initialize_resp);
    let workspace_caps = init
        .capabilities
        .workspace
        .as_ref()
        .expect("workspace capability");
    assert!(
        workspace_caps
            .workspace_folders
            .as_ref()
            .and_then(|folders| folders.supported)
            .unwrap_or(false),
        "expected workspaceFolders capability to be advertised, got: {initialize_resp:?}"
    );
    assert!(
        workspace_caps
            .file_operations
            .as_ref()
            .and_then(|ops| ops.did_create.as_ref())
            .is_some(),
        "expected fileOperations capability to be advertised, got: {initialize_resp:?}"
    );

    write_jsonrpc_message(&mut stdin, &initialized_notification());

    // First request ensures the workspace is loaded for root1.
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            WorkspaceSymbolParams {
                partial_result_params: PartialResultParams::default(),
                work_done_progress_params: WorkDoneProgressParams::default(),
                query: "Foo".to_string(),
            },
            2,
            "workspace/symbol",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let result = resp
        .get("result")
        .cloned()
        .expect("workspace/symbol result");
    let results: WorkspaceSymbolResponse =
        serde_json::from_value(result).expect("decode workspace/symbol response");
    assert!(
        has_symbol_named_at_uri(results, "Foo", &foo_uri),
        "expected Foo symbol in initial workspace, got: {resp:?}"
    );

    // Switch to root2 and ensure subsequent requests use the new project root.
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_notification(
            DidChangeWorkspaceFoldersParams {
                event: WorkspaceFoldersChangeEvent {
                    added: vec![WorkspaceFolder {
                        uri: root2_uri.clone(),
                        name: "ws2".to_string(),
                    }],
                    removed: vec![WorkspaceFolder {
                        uri: root1_uri.clone(),
                        name: "ws1".to_string(),
                    }],
                },
            },
            "workspace/didChangeWorkspaceFolders",
        ),
    );

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            WorkspaceSymbolParams {
                partial_result_params: PartialResultParams::default(),
                work_done_progress_params: WorkDoneProgressParams::default(),
                query: "Bar".to_string(),
            },
            3,
            "workspace/symbol",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    let result = resp
        .get("result")
        .cloned()
        .expect("workspace/symbol result");
    let results: WorkspaceSymbolResponse =
        serde_json::from_value(result).expect("decode workspace/symbol response");
    assert!(
        has_symbol_named_at_uri(results, "Bar", &bar_uri),
        "expected Bar symbol after workspace folder change, got: {resp:?}"
    );

    write_jsonrpc_message(&mut stdin, &shutdown_request(4));
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &exit_notification());
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

    write_jsonrpc_message(&mut stdin, &initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(empty_object(), 2, "nova/extensions/status"),
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
        &jsonrpc_notification(
            DidChangeConfigurationParams {
                settings: empty_object(),
            },
            "workspace/didChangeConfiguration",
        ),
    );

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(empty_object(), 3, "nova/extensions/status"),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    assert_eq!(
        resp.pointer("/result/enabled").and_then(|v| v.as_bool()),
        Some(true),
        "expected didChangeConfiguration to reload config, got: {resp:?}"
    );

    write_jsonrpc_message(&mut stdin, &shutdown_request(4));
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
