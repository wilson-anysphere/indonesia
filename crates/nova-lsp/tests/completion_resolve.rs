use lsp_types::{CompletionItem, CompletionList, CompletionTextEdit};
use pretty_assertions::assert_eq;
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn write_jsonrpc_message(writer: &mut impl Write, message: &serde_json::Value) {
    let bytes = serde_json::to_vec(message).expect("serialize");
    write!(writer, "Content-Length: {}\r\n\r\n", bytes.len()).expect("write header");
    writer.write_all(&bytes).expect("write body");
    writer.flush().expect("flush");
}

fn read_jsonrpc_message(reader: &mut impl BufRead) -> serde_json::Value {
    let mut content_length: Option<usize> = None;

    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).expect("read header line");
        assert!(bytes_read > 0, "unexpected EOF while reading headers");

        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }

        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("Content-Length") {
                content_length = value.trim().parse::<usize>().ok();
            }
        }
    }

    let len = content_length.expect("Content-Length header");
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).expect("read body");
    serde_json::from_slice(&buf).expect("parse json")
}

fn read_jsonrpc_response_with_id(reader: &mut impl BufRead, id: i64) -> serde_json::Value {
    loop {
        let msg = read_jsonrpc_message(reader);
        if msg.get("id").and_then(|v| v.as_i64()) == Some(id) {
            return msg;
        }
    }
}

#[test]
fn stdio_server_completion_replaces_prefix_and_supports_resolve() {
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
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "capabilities": {} }
        }),
    );
    let _initialize_resp = read_jsonrpc_response_with_id(&mut stdout, 1);

    // 2) open document
    let uri = "file:///test/Main.java";
    let source = "class Main { void foo() { String s = \"\"; s.len } }\n";
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": "java",
                    "version": 1,
                    "text": source
                }
            }
        }),
    );

    // Cursor after `len`.
    let cursor_offset = source.find("s.len").expect("contains s.len") + "s.len".len();
    let prefix_start_offset = source.find("s.len").expect("contains s.len") + "s.".len();

    let index = nova_core::LineIndex::new(source);
    let cursor_pos = index.position(source, nova_core::TextSize::from(cursor_offset as u32));
    let cursor_pos = lsp_types::Position::new(cursor_pos.line, cursor_pos.character);

    // 3) request completion
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/completion",
            "params": {
                "textDocument": { "uri": uri },
                "position": { "line": cursor_pos.line, "character": cursor_pos.character }
            }
        }),
    );

    let completion_resp = read_jsonrpc_response_with_id(&mut stdout, 2);
    let result = completion_resp.get("result").cloned().expect("result");

    let items: Vec<CompletionItem> = if result.is_array() {
        serde_json::from_value(result).expect("completion items")
    } else {
        let list: CompletionList = serde_json::from_value(result).expect("completion list");
        list.items
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
    let expected_start = lsp_types::Position::new(expected_start.line, expected_start.character);

    assert_eq!(range.start, expected_start);
    assert_eq!(range.end, cursor_pos);
    assert!(!new_text.is_empty(), "expected non-empty completion text");

    // 4) resolve the completion item
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "completionItem/resolve",
            "params": serde_json::to_value(&item).expect("serialize item")
        }),
    );

    let resolved_resp = read_jsonrpc_response_with_id(&mut stdout, 3);
    let resolved = resolved_resp.get("result").cloned().expect("result");
    let resolved: CompletionItem =
        serde_json::from_value(resolved).expect("resolved completion item");

    assert!(
        resolved.documentation.is_some() || resolved.detail.is_some(),
        "expected resolve to add documentation or detail"
    );

    // 5) shutdown + exit
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "shutdown" }),
    );
    let _shutdown_resp = read_jsonrpc_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
