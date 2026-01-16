use lsp_types::{
    DocumentSymbolParams, PartialResultParams, TextDocumentIdentifier, Uri, WorkDoneProgressParams,
};
use nova_core::{path_to_file_uri, AbsPathBuf};
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support::{
    decode_initialize_result, did_open_notification, exit_notification,
    initialize_request_with_root_uri, initialized_notification, jsonrpc_request,
    read_response_with_id, shutdown_request, stdio_server_lock, write_jsonrpc_message,
};

fn uri_for_path(path: &Path) -> String {
    let abs = AbsPathBuf::try_from(path.to_path_buf()).expect("abs path");
    path_to_file_uri(&abs).expect("file uri")
}

#[test]
fn stdio_server_supports_document_symbol_requests() {
    let _lock = stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let cache_dir = TempDir::new().expect("cache dir");

    let file_path = root.join("Foo.java");
    let file_uri: Uri = uri_for_path(&file_path).parse().expect("file uri");
    let root_uri = uri_for_path(root);

    let text = r#"
        public class Foo {
            int field;
            void bar() {}
        }
    "#;

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

    write_jsonrpc_message(&mut stdin, &initialize_request_with_root_uri(1, root_uri));
    let initialize_resp = read_response_with_id(&mut stdout, 1);
    let init = decode_initialize_result(&initialize_resp);
    assert!(
        init.capabilities.document_symbol_provider.is_some(),
        "expected documentSymbolProvider capability: {initialize_resp:#}"
    );

    write_jsonrpc_message(&mut stdin, &initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(file_uri.clone(), "java", 1, text),
    );

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            DocumentSymbolParams {
                text_document: TextDocumentIdentifier {
                    uri: file_uri.clone(),
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            2,
            "textDocument/documentSymbol",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let results = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("documentSymbol result array");

    let foo = results
        .iter()
        .find(|value| value.get("name").and_then(|v| v.as_str()) == Some("Foo"))
        .expect("expected Foo symbol");
    let children = foo
        .get("children")
        .and_then(|v| v.as_array())
        .expect("Foo should have children");
    assert!(
        children
            .iter()
            .any(|value| value.get("name").and_then(|v| v.as_str()) == Some("bar")),
        "expected Foo to contain bar() method"
    );

    write_jsonrpc_message(&mut stdin, &shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
