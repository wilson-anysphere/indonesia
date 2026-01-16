use std::io::BufReader;
use std::process::{Command, Stdio};

use lsp_types::{
    DocumentSymbolParams, DocumentSymbolResponse, PartialResultParams, TextDocumentIdentifier,
    TextDocumentSyncCapability, TextDocumentSyncSaveOptions, Uri, WorkDoneProgressParams,
};

use crate::support::{
    decode_initialize_result, did_open_notification, exit_notification, initialize_request_empty,
    initialized_notification, jsonrpc_notification, jsonrpc_request, read_response_with_id,
    shutdown_request, stdio_server_lock, write_jsonrpc_message,
};

#[test]
fn stdio_server_supports_did_save_and_updates_open_document_contents() {
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
    let include_text = match init.capabilities.text_document_sync {
        Some(TextDocumentSyncCapability::Options(opts)) => match opts.save {
            Some(TextDocumentSyncSaveOptions::SaveOptions(save)) => save.include_text,
            _ => None,
        },
        _ => None,
    };
    assert_eq!(
        include_text,
        Some(false),
        "expected textDocumentSync.save support with includeText=false, got: {initialize_resp:#}"
    );

    write_jsonrpc_message(&mut stdin, &initialized_notification());

    let uri: Uri = "file:///test/Foo.java".parse().expect("uri");
    let opened_text = "public class Foo {\n}\n";
    let saved_text = "public class Foo {\n    void bar() {}\n}\n";

    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(uri.clone(), "java", 1, opened_text),
    );

    // Use didSave to update the file contents without sending didChange. This exercises the
    // server's ability to accept save notifications with included text.
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_notification(
            lsp_types::DidSaveTextDocumentParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                text: Some(saved_text.to_string()),
            },
            "textDocument/didSave",
        ),
    );

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            DocumentSymbolParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            2,
            "textDocument/documentSymbol",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let result = resp.get("result").cloned().expect("documentSymbol result");
    let symbols: DocumentSymbolResponse =
        serde_json::from_value(result).expect("decode documentSymbol response");
    let DocumentSymbolResponse::Nested(results) = symbols else {
        panic!("expected hierarchical DocumentSymbol response: {resp:#}");
    };

    let foo = results
        .iter()
        .find(|sym| sym.name == "Foo")
        .expect("expected Foo symbol");
    let children = foo.children.as_ref().expect("Foo should have children");
    assert!(
        children.iter().any(|sym| sym.name == "bar"),
        "expected Foo to contain bar() method after didSave update, got: {resp:#}"
    );

    write_jsonrpc_message(&mut stdin, &shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
