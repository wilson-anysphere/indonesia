use lsp_types::{
    FileChangeType, FileEvent, GotoDefinitionParams, PartialResultParams, Range,
    TextDocumentIdentifier, TextDocumentPositionParams, Uri, WorkDoneProgressParams,
};
use nova_core::{path_to_file_uri, AbsPathBuf};
use nova_test_utils::{offset_to_position, position_to_offset};
use serde_json::Value;
use std::io::BufReader;
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support::{
    exit_notification, initialize_request_empty, initialized_notification, jsonrpc_notification,
    jsonrpc_request, read_response_with_id, shutdown_request, write_jsonrpc_message,
};

fn uri_for_path(path: &std::path::Path) -> Uri {
    let abs = AbsPathBuf::try_from(path.to_path_buf()).expect("abs path");
    path_to_file_uri(&abs)
        .expect("file uri")
        .parse()
        .expect("lsp uri")
}

fn range_text<'a>(text: &'a str, range: Range) -> &'a str {
    let start = position_to_offset(text, range.start).unwrap();
    let end = position_to_offset(text, range.end).unwrap();
    &text[start..end]
}

fn diagnostic_messages(resp: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(items) = resp.pointer("/result/items").and_then(|v| v.as_array()) else {
        return out;
    };
    for item in items {
        if let Some(msg) = item.get("message").and_then(|m| m.as_str()) {
            out.push(msg.to_string());
        }
    }
    out
}

#[test]
fn did_change_watched_files_updates_cached_analysis_state() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let file_path = temp.path().join("Main.java");
    let uri = uri_for_path(&file_path);

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
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    // 1) Request diagnostics for a file that doesn't exist. The server should cache "missing".
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            lsp_types::DocumentDiagnosticParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                identifier: None,
                previous_result_id: None,
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            2,
            "textDocument/diagnostic",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    assert!(diagnostic_messages(&resp).is_empty());

    // 2) Create the file on disk, but *don't* notify yet. Diagnostics should remain stale.
    let created = r#"class Main {
    void test() {
        bar();
    }
}
"#;
    std::fs::write(&file_path, created).expect("write Main.java");

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            lsp_types::DocumentDiagnosticParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                identifier: None,
                previous_result_id: None,
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            3,
            "textDocument/diagnostic",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    assert!(
        diagnostic_messages(&resp).is_empty(),
        "expected cached missing state to stay in effect until didChangeWatchedFiles"
    );

    // 3) Notify about file creation; diagnostics should now see the unresolved reference.
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_notification(
            lsp_types::DidChangeWatchedFilesParams {
                changes: vec![FileEvent::new(uri.clone(), FileChangeType::CREATED)],
            },
            "workspace/didChangeWatchedFiles",
        ),
    );

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            lsp_types::DocumentDiagnosticParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                identifier: None,
                previous_result_id: None,
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            4,
            "textDocument/diagnostic",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 4);
    assert!(diagnostic_messages(&resp)
        .iter()
        .any(|m| m.contains("Cannot resolve symbol 'bar'")));

    // 4) Fix the file on disk but don't notify; diagnostics should stay stale.
    let fixed = r#"class Main {
    void bar() {}
    void test() {
        bar();
    }
}
"#;
    std::fs::write(&file_path, fixed).expect("rewrite Main.java");

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            lsp_types::DocumentDiagnosticParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                identifier: None,
                previous_result_id: None,
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            5,
            "textDocument/diagnostic",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 5);
    assert!(
        diagnostic_messages(&resp)
            .iter()
            .any(|m| m.contains("Cannot resolve symbol 'bar'")),
        "expected cached contents to remain until didChangeWatchedFiles"
    );

    // 5) Notify about the change; diagnostics should clear.
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_notification(
            lsp_types::DidChangeWatchedFilesParams {
                changes: vec![FileEvent::new(uri.clone(), FileChangeType::CHANGED)],
            },
            "workspace/didChangeWatchedFiles",
        ),
    );

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            lsp_types::DocumentDiagnosticParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                identifier: None,
                previous_result_id: None,
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            6,
            "textDocument/diagnostic",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 6);
    let messages = diagnostic_messages(&resp);
    assert!(
        messages
            .iter()
            .all(|m| !m.contains("Cannot resolve symbol 'bar'")),
        "expected didChangeWatchedFiles to refresh cached contents, but still saw unresolved `bar`: {messages:?}"
    );

    // Confirm that go-to-definition sees the updated on-disk file.
    let offset = fixed.find("bar();").unwrap() + 1;
    let position = offset_to_position(fixed, offset);

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            7,
            "textDocument/definition",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 7);
    let location = resp.get("result").cloned().expect("definition result");
    let range: Range =
        serde_json::from_value(location.get("range").cloned().expect("range")).expect("range");
    assert_eq!(range_text(fixed, range), "bar");

    // 6) Delete on disk without notifying; definition should still use cached content.
    std::fs::remove_file(&file_path).expect("remove Main.java");
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            8,
            "textDocument/definition",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 8);
    assert!(resp.get("result").is_some_and(|v| !v.is_null()));

    // 7) Notify about deletion; definition should now treat the file as missing.
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_notification(
            lsp_types::DidChangeWatchedFilesParams {
                changes: vec![FileEvent::new(uri.clone(), FileChangeType::DELETED)],
            },
            "workspace/didChangeWatchedFiles",
        ),
    );

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            9,
            "textDocument/definition",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 9);
    assert!(resp.get("result").is_some_and(|v| v.is_null()));

    write_jsonrpc_message(&mut stdin, &shutdown_request(10));
    let _shutdown_resp = read_response_with_id(&mut stdout, 10);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn did_change_watched_files_reloads_nova_config() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    let config_uri = uri_for_path(&config_path);

    std::fs::write(&config_path, "[extensions]\nenabled = false\n").expect("write nova.toml");

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
        &jsonrpc_request(Value::Null, 2, "nova/extensions/status"),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    assert_eq!(
        resp.pointer("/result/enabled").and_then(|v| v.as_bool()),
        Some(false),
        "expected initial config to disable extensions"
    );

    // Update config on disk but don't notify; the server should keep the cached config.
    std::fs::write(&config_path, "[extensions]\nenabled = true\n").expect("rewrite nova.toml");

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(Value::Null, 3, "nova/extensions/status"),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    assert_eq!(
        resp.pointer("/result/enabled").and_then(|v| v.as_bool()),
        Some(false),
        "expected cached config to remain in effect until didChangeWatchedFiles"
    );

    // Notify about the file change; the server should reload `nova_config` from disk.
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_notification(
            lsp_types::DidChangeWatchedFilesParams {
                changes: vec![FileEvent::new(config_uri, FileChangeType::CHANGED)],
            },
            "workspace/didChangeWatchedFiles",
        ),
    );

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(Value::Null, 4, "nova/extensions/status"),
    );
    let resp = read_response_with_id(&mut stdout, 4);
    assert_eq!(
        resp.pointer("/result/enabled").and_then(|v| v.as_bool()),
        Some(true),
        "expected didChangeWatchedFiles to reload nova.toml"
    );

    write_jsonrpc_message(&mut stdin, &shutdown_request(5));
    let _shutdown_resp = read_response_with_id(&mut stdout, 5);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn did_change_watched_files_falls_back_to_default_config_when_config_is_deleted() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    let config_uri = uri_for_path(&config_path);

    std::fs::write(&config_path, "[extensions]\nenabled = false\n").expect("write nova.toml");

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
        &jsonrpc_request(Value::Null, 2, "nova/extensions/status"),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    assert_eq!(
        resp.pointer("/result/enabled").and_then(|v| v.as_bool()),
        Some(false),
        "expected initial config to disable extensions"
    );

    // Delete the config on disk but don't notify; the server should keep the cached config.
    std::fs::remove_file(&config_path).expect("remove nova.toml");
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(Value::Null, 3, "nova/extensions/status"),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    assert_eq!(
        resp.pointer("/result/enabled").and_then(|v| v.as_bool()),
        Some(false),
        "expected cached config to remain in effect until didChangeWatchedFiles"
    );

    // Notify about the deletion; the server should fall back to defaults.
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_notification(
            lsp_types::DidChangeWatchedFilesParams {
                changes: vec![FileEvent::new(config_uri, FileChangeType::DELETED)],
            },
            "workspace/didChangeWatchedFiles",
        ),
    );

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(Value::Null, 4, "nova/extensions/status"),
    );
    let resp = read_response_with_id(&mut stdout, 4);
    assert_eq!(
        resp.pointer("/result/enabled").and_then(|v| v.as_bool()),
        Some(true),
        "expected didChangeWatchedFiles to fall back to defaults when nova.toml is deleted"
    );

    write_jsonrpc_message(&mut stdin, &shutdown_request(5));
    let _shutdown_resp = read_response_with_id(&mut stdout, 5);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
