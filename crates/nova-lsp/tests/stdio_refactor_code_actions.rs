use lsp_types::{CodeAction, Position, Range, Uri};
use nova_core::{
    LineIndex, Position as CorePosition, Range as CoreRange, TextEdit as CoreTextEdit, TextSize,
};
use pretty_assertions::assert_eq;
use serde_json::json;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::str::FromStr;
use tempfile::TempDir;

#[test]
fn stdio_server_resolves_extract_constant_code_action() {
    let temp = TempDir::new().expect("tempdir");
    let file_path = temp.path().join("Test.java");

    let source = "class C {\n    void m() {\n        int x = 1 + 2;\n    }\n}\n";
    fs::write(&file_path, source).expect("write file");

    let uri = Uri::from_str(&format!("file://{}", file_path.to_string_lossy())).expect("uri");

    let expr_start = source.find("1 + 2").expect("expression start");
    let expr_end = expr_start + "1 + 2".len();
    let index = LineIndex::new(source);
    let start = index.position(source, TextSize::from(expr_start as u32));
    let end = index.position(source, TextSize::from(expr_end as u32));
    let range = Range::new(
        Position::new(start.line, start.character),
        Position::new(end.line, end.character),
    );

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
    let _initialize_resp = read_jsonrpc_message(&mut stdout);

    // 2) didOpen (so resolution can read the in-memory snapshot)
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

    // 3) request code actions for the expression selection
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/codeAction",
            "params": {
                "textDocument": { "uri": uri },
                "range": range,
                "context": { "diagnostics": [] }
            }
        }),
    );

    let code_action_resp = read_jsonrpc_message(&mut stdout);
    let actions = code_action_resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");
    let extract_constant = actions
        .iter()
        .find(|action| {
            action
                .get("title")
                .and_then(|v| v.as_str())
                .is_some_and(|title| title == "Extract constant")
        })
        .expect("extract constant action");

    assert!(
        extract_constant.get("data").is_some(),
        "expected extract constant to carry `data`"
    );
    let uri_string = uri.to_string();
    assert_eq!(
        extract_constant
            .get("data")
            .and_then(|data| data.get("uri"))
            .and_then(|uri| uri.as_str()),
        Some(uri_string.as_str()),
        "expected extract constant `data.uri` to round-trip for codeAction/resolve"
    );
    assert!(
        extract_constant.get("edit").is_none(),
        "expected extract constant to be unresolved (no `edit`)"
    );

    // 4) resolve
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "codeAction/resolve",
            "params": extract_constant.clone()
        }),
    );

    let resolve_resp = read_jsonrpc_message(&mut stdout);
    let resolved: CodeAction =
        serde_json::from_value(resolve_resp.get("result").cloned().expect("result"))
            .expect("decode resolved CodeAction");

    let edit = resolved.edit.expect("resolved edit");
    let changes = edit.changes.expect("changes");
    let edits = changes.get(&uri).expect("edits for file");
    let updated = apply_lsp_text_edits(source, edits);

    let constant_edit = edits
        .iter()
        .find(|e| e.new_text.contains("private static final"))
        .expect("constant insertion edit");
    let name = constant_edit
        .new_text
        .split("private static final int ")
        .nth(1)
        .and_then(|rest| rest.split('=').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .expect("constant name");

    assert!(
        updated.contains(&format!("private static final int {name} = 1 + 2;")),
        "expected extracted constant declaration"
    );
    assert!(
        updated.contains(&format!("int x = {name};")),
        "expected initializer replaced with constant reference"
    );
    assert!(
        !updated.contains("int x = 1 + 2;"),
        "expected original expression to be replaced"
    );

    let expected = format!(
        "class C {{\n    private static final int {name} = 1 + 2;\n\n    void m() {{\n        int x = {name};\n    }}\n}}\n"
    );
    assert_eq!(updated, expected);

    // 5) shutdown + exit
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "shutdown" }),
    );
    let _shutdown_resp = read_jsonrpc_message(&mut stdout);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_offers_convert_to_record_code_action() {
    let temp = TempDir::new().expect("tempdir");
    let file_path = temp.path().join("Point.java");

    let source = "\
public final class Point {\n\
    private final int x;\n\
\n\
    public Point(int x) {\n\
        this.x = x;\n\
    }\n\
}\n";
    fs::write(&file_path, source).expect("write file");

    let uri = Uri::from_str(&format!("file://{}", file_path.to_string_lossy())).expect("uri");

    let cursor_offset = source.find("class Point").expect("class");
    let index = LineIndex::new(source);
    let cursor_pos = index.position(source, TextSize::from(cursor_offset as u32));
    let cursor = Position::new(cursor_pos.line, cursor_pos.character);
    let range = Range::new(cursor, cursor);

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
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
            "params": { "capabilities": {} }
        }),
    );
    let _initialize_resp = read_jsonrpc_message(&mut stdout);

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

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/codeAction",
            "params": {
                "textDocument": { "uri": uri },
                "range": range,
                "context": { "diagnostics": [] }
            }
        }),
    );

    let code_action_resp = read_jsonrpc_message(&mut stdout);
    let actions = code_action_resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");

    let convert = actions
        .iter()
        .find(|action| action.get("title").and_then(|v| v.as_str()) == Some("Convert to record"))
        .expect("convert to record action");
    assert!(
        convert.get("edit").is_some(),
        "expected convert-to-record to include `edit`"
    );

    let convert: CodeAction = serde_json::from_value(convert.clone()).expect("decode CodeAction");
    let edit = convert.edit.expect("edit");
    let changes = edit.changes.expect("changes");
    let edits = changes.get(&uri).expect("edits for file");
    let updated = apply_lsp_text_edits(source, edits);

    assert!(
        updated.contains("record Point"),
        "expected record declaration"
    );
    assert!(
        !updated.contains("class Point"),
        "expected class declaration to be rewritten"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = read_jsonrpc_message(&mut stdout);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

fn apply_lsp_text_edits(original: &str, edits: &[lsp_types::TextEdit]) -> String {
    if edits.is_empty() {
        return original.to_string();
    }

    let index = LineIndex::new(original);
    let core_edits: Vec<CoreTextEdit> = edits
        .iter()
        .map(|edit| {
            let range = CoreRange::new(
                CorePosition::new(edit.range.start.line, edit.range.start.character),
                CorePosition::new(edit.range.end.line, edit.range.end.character),
            );
            let range = index.text_range(original, range).expect("valid range");
            CoreTextEdit::new(range, edit.new_text.clone())
        })
        .collect();

    nova_core::apply_text_edits(original, &core_edits).expect("apply edits")
}

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
