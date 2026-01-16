use std::io::BufReader;
use std::process::{Command, Stdio};

use lsp_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DocumentDiagnosticParams,
    PartialResultParams, PublishDiagnosticsParams, TextDocumentContentChangeEvent,
    TextDocumentIdentifier, Uri, VersionedTextDocumentIdentifier, WorkDoneProgressParams,
};

use crate::support::{
    did_open_notification, drain_notifications_until_id, exit_notification,
    initialize_request_empty, initialized_notification, jsonrpc_notification, jsonrpc_request,
    read_response_with_id, shutdown_request, stdio_server_lock, write_jsonrpc_message,
};

fn find_publish_diagnostics(
    notifications: &[serde_json::Value],
    uri: &Uri,
) -> Option<PublishDiagnosticsParams> {
    notifications.iter().find_map(|msg| {
        if msg.get("method").and_then(|v| v.as_str()) != Some("textDocument/publishDiagnostics") {
            return None;
        }
        let params = msg.get("params")?;
        let params: PublishDiagnosticsParams = serde_json::from_value(params.clone()).ok()?;
        if &params.uri == uri {
            Some(params)
        } else {
            None
        }
    })
}

#[test]
fn stdio_publish_diagnostics_for_open_documents() {
    let _lock = stdio_server_lock();

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

    let uri: Uri = "file:///test/Main.java".parse().expect("uri");
    let broken = "class Main {\n    void test() {\n        bar();\n    }\n}\n";
    let fixed = "class Main {\n    void test() {\n    }\n}\n";

    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(uri.clone(), "java", 1, broken),
    );

    // Drain messages until we see a response, collecting notifications along the way. The
    // `publishDiagnostics` notification should have been emitted after didOpen.
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            DocumentDiagnosticParams {
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
    let (notifications, _resp) = drain_notifications_until_id(&mut stdout, 2);
    let publish = find_publish_diagnostics(&notifications, &uri).unwrap_or_else(|| {
        panic!(
            "expected publishDiagnostics for {} after didOpen; got notifications: {notifications:#?}",
            uri.as_str()
        )
    });
    assert!(
        !publish.diagnostics.is_empty(),
        "expected diagnostics to be non-empty for the broken file; got: {publish:#?}"
    );

    let messages = publish
        .diagnostics
        .iter()
        .map(|d| d.message.as_str())
        .collect::<Vec<_>>();
    assert!(
        messages
            .iter()
            .any(|m| m.contains("Cannot resolve symbol") && m.contains("bar")),
        "expected a 'Cannot resolve symbol' diagnostic mentioning `bar`, got: {messages:?}"
    );

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_notification(
            DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri: uri.clone(),
                    version: 2,
                },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: fixed.to_string(),
                }],
            },
            "textDocument/didChange",
        ),
    );

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            DocumentDiagnosticParams {
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
    let (notifications, _resp) = drain_notifications_until_id(&mut stdout, 3);
    let publish = find_publish_diagnostics(&notifications, &uri).unwrap_or_else(|| {
        panic!(
            "expected publishDiagnostics for {} after didChange; got notifications: {notifications:#?}",
            uri.as_str()
        )
    });
    assert!(
        publish.diagnostics.is_empty(),
        "expected diagnostics to be cleared after fixing the file; got: {publish:#?}"
    );

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_notification(
            DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
            },
            "textDocument/didClose",
        ),
    );

    write_jsonrpc_message(&mut stdin, &shutdown_request(4));
    let (notifications, _shutdown_resp) = drain_notifications_until_id(&mut stdout, 4);
    let publish = find_publish_diagnostics(&notifications, &uri).unwrap_or_else(|| {
        panic!(
            "expected publishDiagnostics for {} after didClose; got notifications: {notifications:#?}",
            uri.as_str()
        )
    });
    assert!(
        publish.diagnostics.is_empty(),
        "expected diagnostics to be cleared on didClose; got: {publish:#?}"
    );

    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
