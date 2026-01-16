use serde_json::Value;
use std::io::BufReader;
use std::process::{Command, Stdio};

use lsp_types::{TextDocumentIdentifier, TextDocumentSaveReason, Uri, WillSaveTextDocumentParams};

use crate::support::{
    decode_initialize_result, did_open_notification, exit_notification, initialize_request_empty,
    initialized_notification, jsonrpc_notification, jsonrpc_request, read_response_with_id,
    shutdown_request, stdio_server_lock, write_jsonrpc_message,
};

#[test]
fn stdio_server_handles_will_save_notification() {
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
    let initialize_resp = read_response_with_id(&mut stdout, 1);
    let init = decode_initialize_result(&initialize_resp);
    let will_save = match init.capabilities.text_document_sync {
        Some(lsp_types::TextDocumentSyncCapability::Options(opts)) => {
            opts.will_save.unwrap_or(false)
        }
        _ => false,
    };
    assert!(
        will_save,
        "expected initializeResult.capabilities.textDocumentSync.willSave=true, got: {initialize_resp:?}"
    );

    write_jsonrpc_message(&mut stdin, &initialized_notification());

    let uri: Uri = "file:///test/Foo.java".parse().expect("uri");
    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(uri.clone(), "java", 1, "class Foo{}\n"),
    );

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_notification(
            WillSaveTextDocumentParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                reason: TextDocumentSaveReason::MANUAL,
            },
            "textDocument/willSave",
        ),
    );

    // Verify the server remains responsive after the notification by issuing a lightweight request.
    write_jsonrpc_message(&mut stdin, &jsonrpc_request(Value::Null, 2, "nova/metrics"));
    let metrics_resp = read_response_with_id(&mut stdout, 2);
    assert!(
        metrics_resp.get("result").is_some(),
        "expected nova/metrics to return a result, got: {metrics_resp:?}"
    );

    write_jsonrpc_message(&mut stdin, &shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
