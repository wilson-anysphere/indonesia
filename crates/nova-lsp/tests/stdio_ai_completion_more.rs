use httpmock::prelude::*;
use lsp_types::{CompletionItem, CompletionList, Position, TextEdit, Uri};
use nova_lsp::MoreCompletionsResult;
use serde_json::json;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::time::Duration;
use tempfile::TempDir;

#[test]
fn stdio_server_supports_ai_multi_token_completion_polling() {
    let mock_server = MockServer::start();
    let completion_payload = r#"
    {
      "completions": [
        {
          "label": "stream chain",
          "insert_text": "filter(x -> true).map(x -> x).collect(Collectors.toList())",
          "format": "plain",
          "additional_edits": [{"add_import":"java.util.stream.Collectors"}],
          "confidence": 0.9
        }
      ]
    }
    "#;

    let endpoint = format!("{}/complete", mock_server.base_url());
    let mock = mock_server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200)
            .json_body(json!({ "completion": completion_payload }));
    });

    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.features]
multi_token_completion = true

[ai.timeouts]
multi_token_completion_ms = 5000

[ai.provider]
kind = "http"
url = "{endpoint}"
model = "default"
"#
        ),
    )
    .expect("write config");

    let file_path = temp.path().join("Foo.java");
    let source = concat!(
        "package com.example;\n",
        "\n",
        "import java.util.List;\n",
        "import java.util.stream.Stream;\n",
        "\n",
        "class Foo {\n",
        "    void test() {\n",
        "        Stream stream = List.of(1).stream();\n",
        "        stream.\n",
        "    }\n",
        "}\n"
    );
    fs::write(&file_path, source).expect("write Foo.java");
    let uri = Uri::from_str(&format!("file://{}", file_path.to_string_lossy())).expect("uri");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
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
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    // 2) open document
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": { "textDocument": { "uri": uri, "languageId": "java", "version": 1, "text": source } }
        }),
    );

    // 3) request baseline completions.
    let cursor = Position::new(8, 15); // end of "stream."
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/completion",
            "params": {
                "textDocument": { "uri": uri },
                "position": cursor
            }
        }),
    );
    let completion_resp = read_jsonrpc_response_with_id(&mut stdout, 2);
    let completion_result = completion_resp
        .get("result")
        .cloned()
        .expect("completion result");
    let list: CompletionList =
        serde_json::from_value(completion_result).expect("decode completion list");

    let context_id = list
        .items
        .iter()
        .find_map(|item| {
            item.data
                .as_ref()
                .and_then(|data| data.get("nova"))
                .and_then(|nova| nova.get("completion_context_id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .expect("completion_context_id present on at least one item");

    // 4) poll until AI items ready.
    let mut resolved: Option<MoreCompletionsResult> = None;
    for attempt in 0..50 {
        let request_id = 3 + attempt as i64;
        write_jsonrpc_message(
            &mut stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "nova/completion/more",
                "params": { "context_id": context_id.clone() }
            }),
        );
        let resp = read_jsonrpc_response_with_id(&mut stdout, request_id);
        let result = resp.get("result").cloned().expect("result");
        let more: MoreCompletionsResult =
            serde_json::from_value(result).expect("decode more completions");
        if !more.is_incomplete {
            resolved = Some(more);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let more = resolved.expect("AI completions should resolve");
    assert!(
        !more.items.is_empty(),
        "expected at least one AI completion item"
    );
    mock.assert();

    // 5) Resolve an AI completion item to compute its import additionalTextEdits.
    let unresolved_ai_item = more.items[0].clone();
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 999,
            "method": "completionItem/resolve",
            "params": unresolved_ai_item
        }),
    );
    let resolved_resp = read_jsonrpc_response_with_id(&mut stdout, 999);
    let resolved_item: CompletionItem = serde_json::from_value(
        resolved_resp
            .get("result")
            .cloned()
            .expect("resolved result"),
    )
    .expect("decode resolved CompletionItem");

    let edits = resolved_item
        .additional_text_edits
        .as_ref()
        .expect("AI completion should include additionalTextEdits after resolve");
    assert!(!edits.is_empty());

    // Apply edits to validate the resulting text.
    let updated = apply_lsp_edits(source, edits);
    let pkg = updated.find("package com.example;").expect("package");
    let list_import = updated
        .find("import java.util.List;")
        .expect("existing import");
    let stream_import = updated
        .find("import java.util.stream.Stream;")
        .expect("existing stream import");
    let collectors_import = updated
        .find("import java.util.stream.Collectors;")
        .expect("new import");
    let class_pos = updated.find("class Foo").expect("class");

    assert!(pkg < list_import);
    assert!(list_import < stream_import);
    assert!(stream_import < collectors_import);
    assert!(collectors_import < class_pos);

    // Ensure we didn't hardcode (0,0); insertion should be after the import block.
    let insert_pos = edits[0].range.start;
    assert_eq!(insert_pos.line, 4);
    assert_eq!(insert_pos.character, 0);

    // 6) shutdown + exit
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 100, "method": "shutdown" }),
    );
    let _shutdown_resp = read_jsonrpc_response_with_id(&mut stdout, 100);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_does_not_request_ai_completions_when_multi_token_feature_is_disabled() {
    let mock_server = MockServer::start();
    let completion_payload = r#"
    {
      "completions": [
        {
          "label": "should not be requested",
          "insert_text": "unused()",
          "format": "plain",
          "additional_edits": [],
          "confidence": 0.9
        }
      ]
    }
    "#;

    let endpoint = format!("{}/complete", mock_server.base_url());
    let mock = mock_server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200)
            .json_body(json!({ "completion": completion_payload }));
    });

    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.provider]
kind = "http"
url = "{endpoint}"
model = "default"
"#
        ),
    )
    .expect("write config");

    let file_path = temp.path().join("Foo.java");
    let source = concat!(
        "package com.example;\n",
        "\n",
        "import java.util.List;\n",
        "import java.util.stream.Stream;\n",
        "\n",
        "class Foo {\n",
        "    void test() {\n",
        "        Stream stream = List.of(1).stream();\n",
        "        stream.\n",
        "    }\n",
        "}\n"
    );
    fs::write(&file_path, source).expect("write Foo.java");
    let uri = Uri::from_str(&format!("file://{}", file_path.to_string_lossy())).expect("uri");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
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
    let _initialize_resp = read_jsonrpc_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": { "textDocument": { "uri": uri, "languageId": "java", "version": 1, "text": source } }
        }),
    );

    let cursor = Position::new(8, 15); // end of "stream."
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/completion",
            "params": {
                "textDocument": { "uri": uri },
                "position": cursor
            }
        }),
    );
    let completion_resp = read_jsonrpc_response_with_id(&mut stdout, 2);
    let completion_result = completion_resp
        .get("result")
        .cloned()
        .expect("completion result");
    let list: CompletionList =
        serde_json::from_value(completion_result).expect("decode completion list");
    assert_eq!(
        list.is_incomplete, false,
        "expected no AI completions when feature is disabled"
    );

    let context_id = list
        .items
        .iter()
        .find_map(|item| {
            item.data
                .as_ref()
                .and_then(|data| data.get("nova"))
                .and_then(|nova| nova.get("completion_context_id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .expect("completion_context_id present on at least one item");

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "nova/completion/more",
            "params": { "context_id": context_id }
        }),
    );
    let more_resp = read_jsonrpc_response_with_id(&mut stdout, 3);
    let more_result = more_resp.get("result").cloned().expect("result");
    let more: MoreCompletionsResult =
        serde_json::from_value(more_result).expect("decode more completions");
    assert!(!more.is_incomplete);
    assert!(more.items.is_empty());

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "shutdown" }),
    );
    let _shutdown_resp = read_jsonrpc_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());

    mock.assert_hits(0);
}

fn apply_lsp_edits(source: &str, edits: &[TextEdit]) -> String {
    let mut edits: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|e| {
            let start = position_to_offset_utf16(source, e.range.start).expect("start offset");
            let end = position_to_offset_utf16(source, e.range.end).expect("end offset");
            (start, end, e.new_text.as_str())
        })
        .collect();

    // Apply from the end so offsets remain stable.
    edits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));

    let mut out = source.to_string();
    for (start, end, text) in edits {
        out.replace_range(start..end, text);
    }
    out
}

fn position_to_offset_utf16(text: &str, pos: Position) -> Option<usize> {
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    let mut idx = 0usize;

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
