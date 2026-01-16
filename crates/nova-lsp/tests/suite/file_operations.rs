use lsp_types::Uri;
use nova_core::{path_to_file_uri, AbsPathBuf};
use std::io::BufReader;
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support::{
    exit_notification, initialize_request_empty, initialized_notification, jsonrpc_notification,
    jsonrpc_request, read_response_with_id, shutdown_request, stdio_server_lock,
    write_jsonrpc_message,
};

fn uri_for_path(path: &std::path::Path) -> Uri {
    let abs = AbsPathBuf::try_from(path.to_path_buf()).expect("abs path");
    path_to_file_uri(&abs)
        .expect("file uri")
        .parse()
        .expect("lsp uri")
}

fn diagnostic_messages(resp: &serde_json::Value) -> Vec<String> {
    resp.pointer("/result/items")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|item| {
            item.get("message")
                .and_then(|m| m.as_str())
                .map(ToOwned::to_owned)
        })
        .collect()
}

#[test]
fn did_create_delete_files_updates_cached_analysis_state() {
    let _lock = stdio_server_lock();

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
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                identifier: None,
                previous_result_id: None,
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
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
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                identifier: None,
                previous_result_id: None,
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
            },
            3,
            "textDocument/diagnostic",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    assert!(
        diagnostic_messages(&resp).is_empty(),
        "expected cached missing state to stay in effect until didCreateFiles"
    );

    // 3) Notify about file creation; diagnostics should now see the unresolved reference.
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_notification(
            lsp_types::CreateFilesParams {
                files: vec![lsp_types::FileCreate {
                    uri: uri.to_string(),
                }],
            },
            "workspace/didCreateFiles",
        ),
    );

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            lsp_types::DocumentDiagnosticParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                identifier: None,
                previous_result_id: None,
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
            },
            4,
            "textDocument/diagnostic",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 4);
    assert!(
        diagnostic_messages(&resp)
            .iter()
            .any(|m| m.contains("Cannot resolve symbol 'bar'")),
        "expected diagnostics to refresh after didCreateFiles, got: {resp:?}"
    );

    // 4) Delete on disk without notifying; diagnostics should still use cached content.
    std::fs::remove_file(&file_path).expect("remove Main.java");
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            lsp_types::DocumentDiagnosticParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                identifier: None,
                previous_result_id: None,
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
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
        "expected cached contents to remain until didDeleteFiles"
    );

    // 5) Notify about deletion; diagnostics should now treat the file as missing again.
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_notification(
            lsp_types::DeleteFilesParams {
                files: vec![lsp_types::FileDelete {
                    uri: uri.to_string(),
                }],
            },
            "workspace/didDeleteFiles",
        ),
    );

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            lsp_types::DocumentDiagnosticParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                identifier: None,
                previous_result_id: None,
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
            },
            6,
            "textDocument/diagnostic",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 6);
    assert!(
        diagnostic_messages(&resp).is_empty(),
        "expected didDeleteFiles to mark the file missing, got: {resp:?}"
    );

    write_jsonrpc_message(&mut stdin, &shutdown_request(7));
    let _shutdown_resp = read_response_with_id(&mut stdout, 7);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
