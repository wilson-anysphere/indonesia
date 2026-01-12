use nova_core::{path_to_file_uri, AbsPathBuf, LineIndex, TextSize};
use serde_json::json;
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support::{read_response_with_id, stdio_server_lock, write_jsonrpc_message};

fn uri_for_path(path: &Path) -> String {
    let abs = AbsPathBuf::try_from(path.to_path_buf()).expect("abs path");
    path_to_file_uri(&abs).expect("file uri")
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
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "rootUri": root_uri, "capabilities": {} }
        }),
    );
    let initialize_resp = read_response_with_id(&mut stdout, 1);
    assert!(
        initialize_resp
            .pointer("/result/capabilities/documentHighlightProvider")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        "expected documentHighlightProvider capability",
    );
    assert!(
        initialize_resp
            .pointer("/result/capabilities/selectionRangeProvider")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        "expected selectionRangeProvider capability",
    );
    let folding_cap = initialize_resp.pointer("/result/capabilities/foldingRangeProvider");
    let folding_supported = folding_cap.is_some_and(|cap| {
        cap.as_bool().unwrap_or(false)
            || cap
                .get("lineFoldingOnly")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
    });
    assert!(
        folding_supported,
        "expected foldingRangeProvider capability"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": file_uri,
                    "languageId": "java",
                    "version": 1,
                    "text": text,
                }
            }
        }),
    );

    // 1) documentHighlight
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/documentHighlight",
            "params": {
                "textDocument": { "uri": file_uri },
                "position": { "line": foo_pos.line, "character": foo_pos.character },
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let highlights = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("documentHighlight result array");
    assert!(
        highlights.len() >= 2,
        "expected >= 2 document highlights for `foo`"
    );

    // 2) foldingRange
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "textDocument/foldingRange",
            "params": { "textDocument": { "uri": file_uri } }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    let ranges = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("foldingRange result array");
    assert!(
        ranges.iter().any(
            |range| range.get("startLine").and_then(|v| v.as_i64()).unwrap_or(0)
                < range.get("endLine").and_then(|v| v.as_i64()).unwrap_or(0)
        ),
        "expected at least one folding range with startLine < endLine",
    );

    // 3) selectionRange
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "textDocument/selectionRange",
            "params": {
                "textDocument": { "uri": file_uri },
                "positions": [{ "line": foo_pos.line, "character": foo_pos.character }],
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 4);
    let selections = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("selectionRange result array");
    assert_eq!(selections.len(), 1);
    let mut depth = 0usize;
    let mut current = selections[0].clone();
    loop {
        depth += 1;
        match current.get("parent") {
            Some(parent) if parent.is_object() => current = parent.clone(),
            _ => break,
        }
    }
    assert!(depth > 1, "expected a nested SelectionRange chain");

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 5, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 5);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
