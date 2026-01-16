use lsp_types::{
    HoverParams, PartialResultParams, Position, ReferenceContext, ReferenceParams, SignatureHelp,
    SignatureHelpParams, TextDocumentIdentifier, TextDocumentPositionParams, Uri,
    WorkDoneProgressParams,
};
use nova_core::{path_to_file_uri, AbsPathBuf, LineIndex, TextSize};
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support::{
    did_open_notification, exit_notification, initialize_request_with_root_uri,
    initialized_notification, jsonrpc_request, read_response_with_id, shutdown_request,
    stdio_server_lock, write_jsonrpc_message,
};

fn uri_for_path(path: &Path) -> Uri {
    let abs = AbsPathBuf::try_from(path.to_path_buf()).expect("abs path");
    path_to_file_uri(&abs)
        .expect("file uri")
        .parse()
        .expect("lsp uri")
}

fn utf16_position(text: &str, offset: usize) -> nova_core::Position {
    let index = LineIndex::new(text);
    let offset = TextSize::from(u32::try_from(offset).expect("offset fits in u32"));
    index.position(text, offset)
}

#[test]
fn stdio_server_supports_hover_signature_help_and_references() {
    let _lock = stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let cache_dir = TempDir::new().expect("cache dir");

    let file_path = root.join("Main.java");
    let file_uri = uri_for_path(&file_path);
    let disk_file_path = root.join("OnDisk.java");
    let disk_file_uri = uri_for_path(&disk_file_path);
    let root_uri = uri_for_path(root).as_str().to_string();

    let source = concat!(
        "class Main {\n",
        "    void foo(int x, String y) {}\n",
        "\n",
        "    void test() {\n",
        "        foo(1, \"a\");\n",
        "        foo(2, \"b\");\n",
        "    }\n",
        "}\n",
    );
    let disk_source = concat!(
        "class OnDisk {\n",
        "    void foo(int x, String y) {}\n",
        "\n",
        "    void test() {\n",
        "        foo(1, \"a\");\n",
        "        foo(2, \"b\");\n",
        "    }\n",
        "}\n",
    );

    std::fs::write(&disk_file_path, disk_source).expect("write on-disk java file");

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

    // initialize
    write_jsonrpc_message(&mut stdin, &initialize_request_with_root_uri(1, root_uri));
    let initialize_resp = read_response_with_id(&mut stdout, 1);
    let caps = initialize_resp
        .get("result")
        .and_then(|v| v.get("capabilities"))
        .expect("initialize capabilities");
    assert_eq!(
        caps.get("referencesProvider").and_then(|v| v.as_bool()),
        Some(true)
    );
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    // open document
    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(file_uri.clone(), "java", 1, source),
    );

    // hover on method identifier in declaration
    let hover_offset = source
        .find("foo(int")
        .expect("foo(int in method declaration");
    let hover_pos = utf16_position(source, hover_offset);
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            HoverParams {
                text_document_position_params: TextDocumentPositionParams::new(
                    TextDocumentIdentifier {
                        uri: file_uri.clone(),
                    },
                    Position::new(hover_pos.line, hover_pos.character),
                ),
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
            2,
            "textDocument/hover",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let hover_contents = resp
        .pointer("/result/contents/value")
        .and_then(|v| v.as_str())
        .expect("hover contents");
    assert!(
        hover_contents.contains("```java"),
        "expected java code block"
    );
    assert!(
        hover_contents.contains("foo"),
        "expected method name in hover"
    );

    // signature help inside argument list
    let call_offset = source.find("foo(1").expect("foo(1 call") + "foo(".len();
    let call_pos = utf16_position(source, call_offset);
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            SignatureHelpParams {
                context: None,
                text_document_position_params: TextDocumentPositionParams::new(
                    TextDocumentIdentifier {
                        uri: file_uri.clone(),
                    },
                    Position::new(call_pos.line, call_pos.character),
                ),
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
            3,
            "textDocument/signatureHelp",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    let help: SignatureHelp =
        serde_json::from_value(resp.get("result").cloned().unwrap_or_default())
            .expect("signatureHelp result");
    assert!(!help.signatures.is_empty(), "expected non-empty signatures");
    assert!(
        help.signatures[0].label.contains("foo"),
        "expected foo in signature label"
    );

    // references on call-site identifier
    let ref_offset = source.find("foo(1").expect("foo(1 call");
    let ref_pos = utf16_position(source, ref_offset);
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            ReferenceParams {
                text_document_position: TextDocumentPositionParams::new(
                    TextDocumentIdentifier {
                        uri: file_uri.clone(),
                    },
                    Position::new(ref_pos.line, ref_pos.character),
                ),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                context: ReferenceContext {
                    include_declaration: true,
                },
            },
            4,
            "textDocument/references",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 4);
    let locations: Vec<lsp_types::Location> =
        serde_json::from_value(resp.get("result").cloned().unwrap_or_default())
            .expect("references result array");
    assert!(
        locations.len() >= 2,
        "expected at least 2 reference locations (including declaration)"
    );

    // ---------------------------------------------------------------------
    // Disk-backed (non-overlay) requests
    // ---------------------------------------------------------------------

    // hover on method identifier in declaration
    let hover_offset = disk_source
        .find("foo(int")
        .expect("foo(int in method declaration");
    let hover_pos = utf16_position(disk_source, hover_offset);
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            HoverParams {
                text_document_position_params: TextDocumentPositionParams::new(
                    TextDocumentIdentifier {
                        uri: disk_file_uri.clone(),
                    },
                    Position::new(hover_pos.line, hover_pos.character),
                ),
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
            5,
            "textDocument/hover",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 5);
    let hover_contents = resp
        .pointer("/result/contents/value")
        .and_then(|v| v.as_str())
        .expect("hover contents");
    assert!(
        hover_contents.contains("```java"),
        "expected java code block"
    );
    assert!(
        hover_contents.contains("foo"),
        "expected method name in hover"
    );

    // signature help inside argument list
    let call_offset = disk_source.find("foo(1").expect("foo(1 call") + "foo(".len();
    let call_pos = utf16_position(disk_source, call_offset);
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            SignatureHelpParams {
                context: None,
                text_document_position_params: TextDocumentPositionParams::new(
                    TextDocumentIdentifier {
                        uri: disk_file_uri.clone(),
                    },
                    Position::new(call_pos.line, call_pos.character),
                ),
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
            6,
            "textDocument/signatureHelp",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 6);
    let help: SignatureHelp =
        serde_json::from_value(resp.get("result").cloned().unwrap_or_default())
            .expect("signatureHelp result");
    assert!(!help.signatures.is_empty(), "expected non-empty signatures");
    assert!(
        help.signatures[0].label.contains("foo"),
        "expected foo in signature label"
    );

    // references on call-site identifier
    let ref_offset = disk_source.find("foo(1").expect("foo(1 call");
    let ref_pos = utf16_position(disk_source, ref_offset);
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            ReferenceParams {
                text_document_position: TextDocumentPositionParams::new(
                    TextDocumentIdentifier {
                        uri: disk_file_uri.clone(),
                    },
                    Position::new(ref_pos.line, ref_pos.character),
                ),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                context: ReferenceContext {
                    include_declaration: true,
                },
            },
            7,
            "textDocument/references",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 7);
    let locations: Vec<lsp_types::Location> =
        serde_json::from_value(resp.get("result").cloned().unwrap_or_default())
            .expect("references result array");
    assert!(
        locations.len() >= 2,
        "expected at least 2 reference locations (including declaration)"
    );

    // shutdown + exit
    write_jsonrpc_message(&mut stdin, &shutdown_request(8));
    let _shutdown_resp = read_response_with_id(&mut stdout, 8);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
