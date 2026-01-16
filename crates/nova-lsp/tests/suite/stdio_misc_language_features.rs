use nova_core::{path_to_file_uri, AbsPathBuf, LineIndex, TextSize};
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;

use lsp_types::{
    DocumentHighlight, DocumentHighlightParams, FoldingRange, FoldingRangeParams,
    PartialResultParams, Position, SelectionRange, SelectionRangeParams, TextDocumentIdentifier,
    TextDocumentPositionParams, Uri, WorkDoneProgressParams,
};

use crate::support::{
    decode_initialize_result, did_open_notification, exit_notification,
    initialize_request_with_root_uri, initialized_notification, jsonrpc_request,
    read_response_with_id, shutdown_request, stdio_server_lock, write_jsonrpc_message,
};

fn uri_for_path(path: &Path) -> Uri {
    let abs = AbsPathBuf::try_from(path.to_path_buf()).expect("abs path");
    path_to_file_uri(&abs)
        .expect("file uri")
        .parse()
        .expect("uri")
}

fn utf16_position(text: &str, offset: usize) -> nova_core::Position {
    let index = LineIndex::new(text);
    let offset = TextSize::from(u32::try_from(offset).expect("offset fits in u32"));
    index.position(text, offset)
}

#[test]
fn stdio_server_supports_document_highlight_folding_range_and_selection_range() {
    let _lock = stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let cache_dir = TempDir::new().expect("cache dir");

    let file_path = root.join("Foo.java");
    let file_uri = uri_for_path(&file_path);
    let root_uri = uri_for_path(root);

    let text = concat!(
        "class Foo {\n",
        "    int foo;\n",
        "    void bar() {\n",
        "        foo = foo + 1;\n",
        "        /* multi\n",
        "           line\n",
        "           comment */\n",
        "        if (foo > 0) {\n",
        "            foo++;\n",
        "        }\n",
        "    }\n",
        "}\n",
    );

    let foo_offset = text.find("foo =").expect("foo in assignment");
    let foo_pos = utf16_position(text, foo_offset);

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
        &initialize_request_with_root_uri(1, root_uri.as_str().to_string()),
    );
    let initialize_resp = read_response_with_id(&mut stdout, 1);
    let init = decode_initialize_result(&initialize_resp);
    assert!(
        init.capabilities.document_highlight_provider.is_some(),
        "expected documentHighlightProvider capability: {initialize_resp:#}",
    );
    assert!(
        init.capabilities.selection_range_provider.is_some(),
        "expected selectionRangeProvider capability: {initialize_resp:#}",
    );
    assert!(
        init.capabilities.folding_range_provider.is_some(),
        "expected foldingRangeProvider capability: {initialize_resp:#}"
    );

    write_jsonrpc_message(&mut stdin, &initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(file_uri.clone(), "java", 1, text),
    );

    // 1) documentHighlight
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            DocumentHighlightParams {
                text_document_position_params: TextDocumentPositionParams::new(
                    TextDocumentIdentifier {
                        uri: file_uri.clone(),
                    },
                    Position::new(foo_pos.line, foo_pos.character),
                ),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            2,
            "textDocument/documentHighlight",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let highlights: Vec<DocumentHighlight> =
        serde_json::from_value(resp.get("result").cloned().unwrap_or_default())
            .expect("documentHighlight result array");
    assert!(
        highlights.len() >= 2,
        "expected >= 2 document highlights for `foo`"
    );

    // 2) foldingRange
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            FoldingRangeParams {
                text_document: TextDocumentIdentifier {
                    uri: file_uri.clone(),
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            3,
            "textDocument/foldingRange",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    let ranges: Vec<FoldingRange> =
        serde_json::from_value(resp.get("result").cloned().unwrap_or_default())
            .expect("foldingRange result array");
    assert!(
        ranges.iter().any(|range| range.start_line < range.end_line),
        "expected at least one folding range with startLine < endLine",
    );

    // 3) selectionRange
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            SelectionRangeParams {
                text_document: TextDocumentIdentifier {
                    uri: file_uri.clone(),
                },
                positions: vec![Position::new(foo_pos.line, foo_pos.character)],
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            4,
            "textDocument/selectionRange",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 4);
    let selections: Vec<SelectionRange> =
        serde_json::from_value(resp.get("result").cloned().unwrap_or_default())
            .expect("selectionRange result array");
    assert_eq!(selections.len(), 1);
    let mut depth = 0usize;
    let mut current = selections[0].clone();
    loop {
        depth += 1;
        match current.parent {
            Some(parent) => current = *parent,
            None => break,
        }
    }
    assert!(depth > 1, "expected a nested SelectionRange chain");

    write_jsonrpc_message(&mut stdin, &shutdown_request(5));
    let _shutdown_resp = read_response_with_id(&mut stdout, 5);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
