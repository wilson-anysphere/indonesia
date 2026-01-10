use lsp_types::{Position, Range, Uri, WorkspaceEdit};
use nova_test_utils::extract_range;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::str::FromStr;
use tempfile::TempDir;

#[test]
fn stdio_server_supports_extract_method_code_action_and_execute_command() {
    let temp = TempDir::new().expect("tempdir");
    let file_path = temp.path().join("Main.java");

    let fixture = r#"
class C {
    void m(int a) {
        int b = 1;
        /*start*/System.out.println(a + b);/*end*/
        System.out.println("done");
    }
}
"#;

    let (source, selection) = extract_range(fixture);
    fs::write(&file_path, &source).expect("write file");

    let uri = Uri::from_str(&format!("file://{}", file_path.to_string_lossy())).expect("uri");
    let range = Range {
        start: offset_to_position(&source, selection.start),
        end: offset_to_position(&source, selection.end),
    };

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
            "params": {
                "capabilities": {}
            }
        }),
    );
    let _initialize_resp = read_jsonrpc_message(&mut stdout);

    // 2) request code actions
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
    let extract = actions
        .iter()
        .find(|action| {
            action
                .pointer("/command/command")
                .and_then(|v| v.as_str())
                == Some("nova.extractMethod")
        })
        .expect("extract method action");

    let args = extract
        .pointer("/command/arguments/0")
        .cloned()
        .expect("command args");

    // 3) execute the command
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "workspace/executeCommand",
            "params": {
                "command": "nova.extractMethod",
                "arguments": [args]
            }
        }),
    );

    let exec_resp = read_jsonrpc_message(&mut stdout);
    let result = exec_resp.get("result").cloned().expect("workspace edit");
    let edit: WorkspaceEdit = serde_json::from_value(result).expect("decode workspace edit");
    let changes = edit.changes.expect("changes map");
    let edits = changes.get(&uri).expect("edits for uri");

    let actual = apply_lsp_edits(&source, edits);
    let expected = r#"
class C {
    void m(int a) {
        int b = 1;
        extracted(a, b);
        System.out.println("done");
    }

    private void extracted(int a, int b) {
        System.out.println(a + b);
    }
}
"#;
    assert_eq!(actual, expected);

    // 4) shutdown + exit
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

fn offset_to_position(text: &str, offset: usize) -> Position {
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    let mut idx = 0;

    for ch in text.chars() {
        if idx >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col_utf16 = 0;
        } else {
            col_utf16 += ch.len_utf16() as u32;
        }
        idx += ch.len_utf8();
    }

    Position {
        line,
        character: col_utf16,
    }
}

fn position_to_offset(text: &str, pos: Position) -> Option<usize> {
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    let mut idx = 0;

    for ch in text.chars() {
        if line == pos.line && col_utf16 == pos.character {
            return Some(idx);
        }

        if ch == '\n' {
            if line == pos.line {
                if col_utf16 == pos.character {
                    return Some(idx);
                }
                return None;
            }
            line += 1;
            col_utf16 = 0;
            idx += 1;
            continue;
        }

        if line == pos.line {
            col_utf16 += ch.len_utf16() as u32;
            if col_utf16 > pos.character {
                return None;
            }
        }
        idx += ch.len_utf8();
    }

    if line == pos.line && col_utf16 == pos.character {
        Some(idx)
    } else {
        None
    }
}

fn apply_lsp_edits(source: &str, edits: &[lsp_types::TextEdit]) -> String {
    let mut edits: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|e| {
            let start = position_to_offset(source, e.range.start).expect("start offset");
            let end = position_to_offset(source, e.range.end).expect("end offset");
            (start, end, e.new_text.as_str())
        })
        .collect();

    // Apply from the end so offsets remain stable.
    edits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));

    // Overlap check.
    let mut last_start = source.len();
    for (start, end, _) in &edits {
        assert!(*start <= *end);
        assert!(*end <= source.len());
        assert!(*end <= last_start, "overlapping edits");
        last_start = *start;
    }

    let mut out = source.to_string();
    for (start, end, text) in edits {
        out.replace_range(start..end, text);
    }

    out
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

