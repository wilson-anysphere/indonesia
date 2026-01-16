use lsp_types::{
    CompletionItem, CompletionParams, CompletionResponse, CompletionTextEdit, PartialResultParams,
    Position, TextDocumentIdentifier, TextDocumentPositionParams, Uri, WorkDoneProgressParams,
};
use pretty_assertions::assert_eq;
use std::io::BufReader;
use std::process::{Command, Stdio};

use crate::support::{
    did_open_notification, exit_notification, initialize_request_empty, initialized_notification,
    jsonrpc_request, read_response_with_id, shutdown_request, stdio_server_lock,
    write_jsonrpc_message,
};

#[test]
fn stdio_server_completion_replaces_prefix_and_supports_resolve() {
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

    // 1) initialize
    write_jsonrpc_message(&mut stdin, &initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    // 2) open document
    let uri: Uri = "file:///test/Main.java".parse().expect("uri");
    let source = "class Main { void foo() { String s = \"\"; s.len } }\n";
    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(uri.clone(), "java", 1, source),
    );

    // Cursor after `len`.
    let cursor_offset = source.find("s.len").expect("contains s.len") + "s.len".len();
    let prefix_start_offset = source.find("s.len").expect("contains s.len") + "s.".len();

    let index = nova_core::LineIndex::new(source);
    let cursor_pos = index.position(source, nova_core::TextSize::from(cursor_offset as u32));
    let cursor_pos = Position::new(cursor_pos.line, cursor_pos.character);

    // 3) request completion
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            CompletionParams {
                text_document_position: TextDocumentPositionParams::new(
                    TextDocumentIdentifier { uri: uri.clone() },
                    cursor_pos,
                ),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                context: None,
            },
            2,
            "textDocument/completion",
        ),
    );

    let completion_resp = read_response_with_id(&mut stdout, 2);
    let result = completion_resp.get("result").cloned().expect("result");

    let items =
        match serde_json::from_value::<CompletionResponse>(result).expect("completion result") {
            CompletionResponse::Array(items) => items,
            CompletionResponse::List(list) => list.items,
        };

    let item = items
        .iter()
        .find(|item| item.label == "length" && item.text_edit.is_some())
        .or_else(|| items.iter().find(|item| item.text_edit.is_some()))
        .expect("expected at least one completion with textEdit")
        .clone();

    let (range, new_text) = match item.text_edit.clone().expect("textEdit") {
        CompletionTextEdit::Edit(edit) => (edit.range, edit.new_text),
        CompletionTextEdit::InsertAndReplace(edit) => (edit.replace, edit.new_text),
    };

    let expected_start = index.position(
        source,
        nova_core::TextSize::from(prefix_start_offset as u32),
    );
    let expected_start = Position::new(expected_start.line, expected_start.character);

    assert_eq!(range.start, expected_start);
    assert_eq!(range.end, cursor_pos);
    assert!(!new_text.is_empty(), "expected non-empty completion text");

    // 4) resolve the completion item
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(item.clone(), 3, "completionItem/resolve"),
    );

    let resolved_resp = read_response_with_id(&mut stdout, 3);
    let resolved = resolved_resp.get("result").cloned().expect("result");
    let resolved: CompletionItem =
        serde_json::from_value(resolved).expect("resolved completion item");

    assert!(
        resolved.documentation.is_some() || resolved.detail.is_some(),
        "expected resolve to add documentation or detail"
    );

    // 5) shutdown + exit
    write_jsonrpc_message(&mut stdin, &shutdown_request(4));
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
