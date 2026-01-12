use nova_index::Index;
use nova_testing::schema::TestDiscoverResponse;
use pretty_assertions::assert_eq;
use serde::Deserialize;
use serde_json::json;
use std::fs;
use std::io::{BufReader, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::thread;
use tempfile::TempDir;

use crate::support::{read_jsonrpc_message, read_response_with_id, write_jsonrpc_message};

#[derive(Debug, Clone, Deserialize)]
struct LspPosition {
    line: u32,
    character: u32,
}

#[derive(Debug, Clone, Deserialize)]
struct LspRange {
    start: LspPosition,
    end: LspPosition,
}

#[derive(Debug, Clone, Deserialize)]
struct LspTextEdit {
    range: LspRange,
    #[serde(rename = "newText")]
    new_text: String,
}

#[derive(Debug, Clone, Deserialize)]
struct LspWorkspaceEdit {
    #[serde(default)]
    changes: std::collections::HashMap<String, Vec<LspTextEdit>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SafeModeStatusResponse {
    schema_version: u32,
    enabled: bool,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum SafeModeStatusResult {
    Bool(bool),
    Object(SafeModeStatusResponse),
}

fn apply_lsp_text_edits(original: &str, edits: &[LspTextEdit]) -> String {
    if edits.is_empty() {
        return original.to_string();
    }

    let index = nova_core::LineIndex::new(original);
    let core_edits: Vec<nova_core::TextEdit> = edits
        .iter()
        .map(|edit| {
            let range = nova_core::Range::new(
                nova_core::Position::new(edit.range.start.line, edit.range.start.character),
                nova_core::Position::new(edit.range.end.line, edit.range.end.character),
            );
            let range = index.text_range(original, range).expect("valid range");
            nova_core::TextEdit::new(range, edit.new_text.clone())
        })
        .collect();

    nova_core::apply_text_edits(original, &core_edits).expect("apply edits")
}

fn utf16_position(text: &str, offset: usize) -> nova_core::Position {
    let index = nova_core::LineIndex::new(text);
    let offset = nova_core::TextSize::from(u32::try_from(offset).expect("offset fits in u32"));
    index.position(text, offset)
}

#[test]
fn stdio_server_handles_metrics_request() {
    let _lock = crate::support::stdio_server_lock();
    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // initialize
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "capabilities": {} }
        }),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    // metrics snapshot
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/metrics",
            "params": null
        }),
    );

    let resp = read_response_with_id(&mut stdout, 2);
    let totals = resp
        .get("result")
        .and_then(|v| v.get("totals"))
        .expect("result.totals");
    let request_count = totals
        .get("requestCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    assert!(
        request_count > 0,
        "expected requestCount > 0, got: {resp:#}"
    );

    // shutdown + exit
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_handles_safe_mode_status_request() {
    let _lock = crate::support::stdio_server_lock();
    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // initialize
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "capabilities": {} }
        }),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    // safeModeStatus
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/safeModeStatus",
            "params": null
        }),
    );

    let resp = read_response_with_id(&mut stdout, 2);
    let result = resp.get("result").cloned().expect("result");
    let status: SafeModeStatusResult = serde_json::from_value(result).expect("decode response");
    match status {
        SafeModeStatusResult::Object(status) => {
            assert_eq!(status.schema_version, 1);
            assert!(
                !status.enabled,
                "safe-mode should not be enabled at startup"
            );
            assert_eq!(status.reason, None);
        }
        SafeModeStatusResult::Bool(enabled) => {
            panic!("expected safeModeStatus response object, got bool {enabled}")
        }
    }

    // shutdown + exit
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_handles_extensions_status_request() {
    let _lock = crate::support::stdio_server_lock();
    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // initialize
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "capabilities": {} }
        }),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    // extensions/status
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": nova_lsp::EXTENSIONS_STATUS_METHOD,
            "params": { "schemaVersion": 1 }
        }),
    );

    let resp = read_response_with_id(&mut stdout, 2);
    let result = resp.get("result").cloned().expect("result");
    assert_eq!(
        result.get("schemaVersion").and_then(|v| v.as_u64()),
        Some(1)
    );
    assert!(
        result.get("enabled").and_then(|v| v.as_bool()).is_some(),
        "expected result.enabled boolean, got: {result:#}"
    );
    assert!(
        result
            .get("loadedExtensions")
            .and_then(|v| v.as_array())
            .is_some(),
        "expected result.loadedExtensions array, got: {result:#}"
    );
    assert!(
        result
            .get("stats")
            .and_then(|v| v.as_object())
            .and_then(|o| o.get("navigation"))
            .is_some(),
        "expected result.stats.navigation, got: {result:#}"
    );

    // shutdown + exit
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_handles_extensions_navigation_request() {
    let _lock = crate::support::stdio_server_lock();
    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // initialize
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "capabilities": {} }
        }),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    let uri = "file:///test/Foo.java";
    let text = "class Foo {}\n";
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
                    "text": text
                }
            }
        }),
    );

    // extensions/navigation
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": nova_lsp::EXTENSIONS_NAVIGATION_METHOD,
            "params": {
                "schemaVersion": 1,
                "textDocument": { "uri": uri }
            }
        }),
    );

    let resp = read_response_with_id(&mut stdout, 2);
    let result = resp.get("result").cloned().expect("result");
    assert_eq!(
        result.get("schemaVersion").and_then(|v| v.as_u64()),
        Some(1)
    );
    assert!(
        result.get("targets").and_then(|v| v.as_array()).is_some(),
        "expected result.targets array, got: {result:#}"
    );

    // shutdown + exit
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_handles_test_discover_request() {
    let _lock = crate::support::stdio_server_lock();
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../nova-testing/fixtures/maven-junit5");

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
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    // 2) discover tests
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/test/discover",
            "params": {
                "projectRoot": fixture.to_string_lossy(),
            }
        }),
    );

    let discover_resp = read_response_with_id(&mut stdout, 2);
    let result = discover_resp.get("result").cloned().expect("result");
    let resp: TestDiscoverResponse = serde_json::from_value(result).expect("decode response");
    assert_eq!(resp.schema_version, nova_testing::SCHEMA_VERSION);
    assert!(resp
        .tests
        .iter()
        .any(|t| t.id == "com.example.CalculatorTest"));

    // 3) shutdown + exit
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);

    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_handles_document_formatting_request() {
    let _lock = crate::support::stdio_server_lock();
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
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    let uri = "file:///test/Foo.java";
    let text = "class Foo{void m(){int x=1;}}\n";

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
                    "text": text
                }
            }
        }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/formatting",
            "params": {
                "textDocument": { "uri": uri },
                "options": { "tabSize": 4, "insertSpaces": true }
            }
        }),
    );

    let formatting_resp = read_response_with_id(&mut stdout, 2);
    let result = formatting_resp.get("result").cloned().expect("result");
    let edits: Vec<LspTextEdit> = serde_json::from_value(result).expect("decode text edits");
    let formatted = apply_lsp_text_edits(text, &edits);

    assert_eq!(
        formatted,
        "class Foo {\n    void m() {\n        int x = 1;\n    }\n}\n"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_handles_change_signature_request() {
    let _lock = crate::support::stdio_server_lock();
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
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    // 2) didOpen
    let uri = "file:///A.java";
    let source = concat!(
        "class A {\n",
        "    int sum(int a, int b) {\n",
        "        return a + b;\n",
        "    }\n",
        "\n",
        "    void test() {\n",
        "        int 𝒂 = sum(1, 2);\n",
        "    }\n",
        "}\n",
    );
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

    // Compute the target id the same way the server will (from an Index snapshot).
    let mut files = std::collections::BTreeMap::new();
    files.insert(uri.to_string(), source.to_string());
    let index = Index::new(files);
    let target = index.find_method("A", "sum").expect("method exists").id.0;

    // 3) changeSignature request
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/refactor/changeSignature",
            "params": {
                "target": target,
                "new_name": null,
                "parameters": [
                    { "Existing": { "old_index": 1, "new_name": null, "new_type": null } },
                    { "Existing": { "old_index": 0, "new_name": null, "new_type": null } }
                ],
                "new_return_type": null,
                "new_throws": null,
                "propagate_hierarchy": "None"
            }
        }),
    );

    let resp = read_response_with_id(&mut stdout, 2);
    let result = resp.get("result").cloned().expect("result");
    let edit: LspWorkspaceEdit = serde_json::from_value(result).expect("decode edit");
    let edits = edit.changes.get(uri).expect("edits for uri");

    let call_offset = source.find("sum(1, 2)").expect("call exists");
    let call_end = call_offset + "sum(1, 2)".len();
    let expected_start = utf16_position(source, call_offset);
    let expected_end = utf16_position(source, call_end);

    let call_edit = edits
        .iter()
        .find(|edit| edit.new_text == "sum(2, 1)")
        .expect("call edit");
    assert_eq!(call_edit.range.start.line, expected_start.line);
    assert_eq!(call_edit.range.start.character, expected_start.character);
    assert_eq!(call_edit.range.end.line, expected_end.line);
    assert_eq!(call_edit.range.end.character, expected_end.character);

    let updated = apply_lsp_text_edits(source, edits);
    assert!(updated.contains("int sum(int b, int a)"));
    assert!(updated.contains("sum(2, 1)"));

    // shutdown + exit
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_applies_incremental_did_change_utf16_correctly() {
    let _lock = crate::support::stdio_server_lock();
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
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    let uri = "file:///test/Foo.java";
    let text = "class Foo{void m(){String s=\"😀\";int x=1;}}\n";

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
                    "text": text
                }
            }
        }),
    );

    let start_offset = text.find("int x=1;").expect("int x=1 statement");
    let digit_offset = start_offset + "int x=".len();
    let digit_end = digit_offset + "1".len();

    let index = nova_core::LineIndex::new(text);
    let start_pos = index.position(text, nova_core::TextSize::from(digit_offset as u32));
    let end_pos = index.position(text, nova_core::TextSize::from(digit_end as u32));

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {
                "textDocument": { "uri": uri, "version": 2 },
                "contentChanges": [{
                    "range": {
                        "start": { "line": start_pos.line, "character": start_pos.character },
                        "end": { "line": end_pos.line, "character": end_pos.character }
                    },
                    "text": "2"
                }]
            }
        }),
    );

    let mut updated_text = text.to_string();
    updated_text.replace_range(digit_offset..digit_end, "2");

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/formatting",
            "params": {
                "textDocument": { "uri": uri },
                "options": { "tabSize": 4, "insertSpaces": true }
            }
        }),
    );

    let formatting_resp = read_response_with_id(&mut stdout, 2);
    let result = formatting_resp.get("result").cloned().expect("result");
    let edits: Vec<LspTextEdit> = serde_json::from_value(result).expect("decode text edits");
    let formatted = apply_lsp_text_edits(&updated_text, &edits);

    assert!(
        formatted.contains("int x = 2;"),
        "formatted output did not reflect incremental edit:\n{formatted}"
    );
    assert!(!formatted.contains("int x = 1;"));
    assert!(formatted.contains("😀"), "emoji should be preserved");

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_resolves_completion_item_imports() {
    let _lock = crate::support::stdio_server_lock();
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
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    let uri = "file:///test/Foo.java";
    let text = "package com.example;\n\nclass Foo {}\n";

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
                    "text": text
                }
            }
        }),
    );

    // Directly resolve an item with import requests stashed in `data.nova.imports`.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "completionItem/resolve",
            "params": {
                "label": "collect",
                "insertText": "collect(Collectors.toList())",
                "data": {
                    "nova": {
                        "imports": ["java.util.stream.Collectors"],
                        "uri": uri
                    }
                }
            }
        }),
    );

    let resp = read_response_with_id(&mut stdout, 2);
    let result = resp.get("result").cloned().expect("result");
    let edits_value = result
        .get("additionalTextEdits")
        .cloned()
        .expect("additionalTextEdits");
    let edits: Vec<LspTextEdit> = serde_json::from_value(edits_value).expect("decode text edits");

    assert!(!edits.is_empty(), "expected additional text edits");
    let updated = apply_lsp_text_edits(text, &edits);
    assert!(
        updated.contains("import java.util.stream.Collectors;\n"),
        "expected Collectors import to be inserted, got:\n{updated}"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_handles_completion_and_more_completions_request() {
    let _lock = crate::support::stdio_server_lock();
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
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    let uri = "file:///test/Completion.java";
    let text = "class A {\n  void m() {\n    String s = \"\";\n    s.\n  }\n}\n";

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
                    "text": text
                }
            }
        }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/completion",
            "params": {
                "textDocument": { "uri": uri },
                "position": { "line": 3, "character": 6 }
            }
        }),
    );

    let completion_resp = read_response_with_id(&mut stdout, 2);
    let result = completion_resp.get("result").cloned().expect("result");
    let items = result
        .get("items")
        .and_then(|v| v.as_array())
        .expect("completion items array");

    assert!(items
        .iter()
        .any(|item| item.get("label").and_then(|v| v.as_str()) == Some("length")));

    let context_id = items.iter().find_map(|item| {
        item.get("data")
            .and_then(|d| d.get("nova"))
            .and_then(|nova| nova.get("completion_context_id"))
            .and_then(|id| id.as_str())
    });

    // Only assert the "more completions" round-trip when AI completions are enabled.
    if let Some(context_id) = context_id {
        write_jsonrpc_message(
            &mut stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "nova/completion/more",
                "params": { "context_id": context_id }
            }),
        );

        let more_resp = read_response_with_id(&mut stdout, 3);
        let more_result = more_resp.get("result").cloned().expect("result");
        assert_eq!(
            more_result
                .get("items")
                .and_then(|v| v.as_array())
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            more_result.get("is_incomplete").and_then(|v| v.as_bool()),
            Some(false)
        );
    }

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
fn stdio_server_discovers_tests_in_simple_project_fixture() {
    let _lock = crate::support::stdio_server_lock();
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../nova-testing/fixtures/simple-junit5");

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
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/test/discover",
            "params": { "projectRoot": fixture.to_string_lossy() }
        }),
    );

    let discover_resp = read_response_with_id(&mut stdout, 2);
    let result = discover_resp.get("result").cloned().expect("result");
    let resp: TestDiscoverResponse = serde_json::from_value(result).expect("decode response");
    assert!(resp.tests.iter().any(|t| t.id == "com.example.SimpleTest"));

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);

    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_handles_debug_configurations_request() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().join("project");
    fs::create_dir_all(&root).expect("create project dir");

    let main_dir = root.join("src/main/java/com/example");
    let test_dir = root.join("src/test/java/com/example");
    fs::create_dir_all(&main_dir).expect("create main dir");
    fs::create_dir_all(&test_dir).expect("create test dir");

    fs::write(
        main_dir.join("Main.java"),
        r#"
            package com.example;

            public class Main {
                public static void main(String[] args) {}
            }
        "#,
    )
    .expect("write Main.java");

    fs::write(
        test_dir.join("MainTest.java"),
        r#"
            package com.example;

            import org.junit.jupiter.api.Test;

            public class MainTest {
                @Test void ok() {}
            }
        "#,
    )
    .expect("write MainTest.java");

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
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/debug/configurations",
            "params": { "projectRoot": root.to_string_lossy() }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let result = resp.get("result").cloned().expect("result");
    let configs = result.as_array().expect("configs array");

    let mut names: Vec<_> = configs
        .iter()
        .filter_map(|c| c.get("name").and_then(|n| n.as_str()))
        .collect();
    names.sort();
    assert_eq!(names, vec!["Debug Tests: MainTest", "Run Main"]);

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);

    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_provides_inline_method_code_action() {
    let _lock = crate::support::stdio_server_lock();
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
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    let uri = "file:///A.java";
    let source = r#"class A {
  private int addOne(int x) { return x + 1; }

  int test() {
    return addOne(41);
  }
}
"#;

    // Open the document so code actions can use in-memory contents.
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

    // Cursor on `addOne(41)` (line 4, character 11).
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/codeAction",
            "params": {
                "textDocument": { "uri": uri },
                "range": {
                    "start": { "line": 4, "character": 11 },
                    "end": { "line": 4, "character": 11 }
                },
                "context": { "diagnostics": [] }
            }
        }),
    );

    let resp = read_response_with_id(&mut stdout, 2);
    let result = resp.get("result").cloned().expect("result");
    let actions = result.as_array().expect("actions array");

    let inline_actions: Vec<_> = actions
        .iter()
        .filter(|action| action.get("kind").and_then(|v| v.as_str()) == Some("refactor.inline"))
        .collect();
    assert!(
        !inline_actions.is_empty(),
        "expected at least one inline-method code action"
    );

    let mut has_temp_arg = false;
    for action in inline_actions {
        let Some(edit) = action.get("edit") else {
            continue;
        };
        let Some(changes) = edit.get("changes").and_then(|v| v.as_object()) else {
            continue;
        };
        let Some(edits) = changes.get(uri).and_then(|v| v.as_array()) else {
            continue;
        };
        if edits.iter().any(|edit| {
            edit.get("newText")
                .and_then(|v| v.as_str())
                .is_some_and(|t| t.contains("int x_arg = 41;"))
        }) {
            has_temp_arg = true;
            break;
        }
    }
    assert!(has_temp_arg, "expected inline method to introduce arg temp");

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_handles_generated_sources_request() {
    let _lock = crate::support::stdio_server_lock();
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../nova-apt/testdata/maven_simple");

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
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/java/generatedSources",
            "params": { "projectRoot": fixture.to_string_lossy() }
        }),
    );

    let resp = read_response_with_id(&mut stdout, 2);
    let result = resp.get("result").cloned().expect("result");
    let modules = result
        .get("modules")
        .and_then(|v| v.as_array())
        .expect("modules array");
    assert!(!modules.is_empty());
    let roots = modules[0]
        .get("roots")
        .and_then(|v| v.as_array())
        .expect("roots array");
    assert!(roots.iter().any(|root| {
        root.get("path").and_then(|v| v.as_str()).is_some_and(|p| {
            p.replace('\\', "/")
                .contains("target/generated-sources/annotations")
        })
    }));

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_handles_run_annotation_processing_request() {
    let _lock = crate::support::stdio_server_lock();
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../nova-apt/testdata/maven_simple");

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
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/java/runAnnotationProcessing",
            "params": { "projectRoot": fixture.to_string_lossy() }
        }),
    );

    let resp = read_response_with_id(&mut stdout, 2);
    let result = resp.get("result").cloned().expect("result");
    let progress = result
        .get("progress")
        .and_then(|v| v.as_array())
        .expect("progress array");
    assert!(progress
        .iter()
        .any(|p| p.as_str() == Some("Running annotation processing")));

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn stdio_server_handles_java_classpath_request_with_fake_maven_and_cache() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().join("maven-project");
    fs::create_dir_all(&root).expect("create project dir");

    fs::write(
        root.join("pom.xml"),
        r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>demo</artifactId>
  <version>1.0-SNAPSHOT</version>
</project>
"#,
    )
    .expect("write pom");

    let dep_dir = root.join("deps");
    fs::create_dir_all(&dep_dir).expect("create deps");
    let dep1 = dep_dir.join("dep1.jar");
    let dep2 = dep_dir.join("dep2.jar");
    fs::write(&dep1, "").expect("write dep1");
    fs::write(&dep2, "").expect("write dep2");

    // Provide a fake `mvn` executable on PATH so the test doesn't depend on a
    // system Maven installation.
    let bin_dir = temp.path().join("bin");
    fs::create_dir_all(&bin_dir).expect("create bin dir");
    let mvn_path = bin_dir.join("mvn");
    fs::write(
        &mvn_path,
        format!(
            r#"#!/bin/sh
expr=""
for arg in "$@"; do
  case "$arg" in
    -Dexpression=*) expr="${{arg#-Dexpression=}}" ;;
  esac
done

case "$expr" in
  project.build.outputDirectory) printf '%s\n' 'target/classes' ;;
  project.build.testOutputDirectory) printf '%s\n' 'target/test-classes' ;;
  project.compileClasspathElements|project.testClasspathElements) printf '%s\n' '["{}","{}"]' ;;
  *) printf '%s\n' '[]' ;;
esac
"#,
            dep1.display(),
            dep2.display()
        ),
    )
    .expect("write fake mvn");
    let mut perms = fs::metadata(&mvn_path).expect("stat mvn").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&mvn_path, perms).expect("chmod mvn");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .env("PATH", bin_dir.to_string_lossy().to_string())
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
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    let expected = vec![
        root.join("target/classes").to_string_lossy().to_string(),
        dep1.to_string_lossy().to_string(),
        dep2.to_string_lossy().to_string(),
    ];

    // 1) initial request should invoke our fake Maven and populate the cache.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/java/classpath",
            "params": { "projectRoot": root.to_string_lossy() }
        }),
    );
    let classpath_resp = read_response_with_id(&mut stdout, 2);
    let result = classpath_resp.get("result").cloned().expect("result");
    let classpath = result
        .get("classpath")
        .and_then(|v| v.as_array())
        .expect("classpath array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(classpath, expected);

    // 2) remove the fake Maven binary; subsequent requests should still succeed
    //    thanks to the fingerprinted cache.
    fs::remove_file(&mvn_path).expect("remove fake mvn");

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "nova/java/classpath",
            "params": { "projectRoot": root.to_string_lossy() }
        }),
    );
    let cached_resp = read_response_with_id(&mut stdout, 3);
    let result = match cached_resp.get("result").cloned() {
        Some(result) => result,
        None => panic!("expected result, got: {cached_resp:?}"),
    };
    let classpath = result
        .get("classpath")
        .and_then(|v| v.as_array())
        .expect("classpath array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(classpath, expected);

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn stdio_server_handles_build_project_request_with_fake_maven_diagnostics() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().join("maven-project");
    fs::create_dir_all(&root).expect("create project dir");

    fs::write(
        root.join("pom.xml"),
        r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>demo</artifactId>
  <version>1.0-SNAPSHOT</version>
</project>
"#,
    )
    .expect("write pom");

    let java_dir = root.join("src/main/java/com/example");
    fs::create_dir_all(&java_dir).expect("create java dir");
    let java_file = java_dir.join("Foo.java");
    fs::write(&java_file, "package com.example; public class Foo {}").expect("write Foo.java");

    let bin_dir = temp.path().join("bin");
    fs::create_dir_all(&bin_dir).expect("create bin dir");
    let mvn_path = bin_dir.join("mvn");
    fs::write(
        &mvn_path,
        format!(
            r#"#!/bin/sh
printf '%s\n' '[ERROR] {}:[10,5] cannot find symbol'
printf '%s\n' '[ERROR]   symbol:   variable x'
printf '%s\n' '[ERROR]   location: class com.example.Foo'
exit 1
"#,
            java_file.display(),
        ),
    )
    .expect("write fake mvn");
    let mut perms = fs::metadata(&mvn_path).expect("stat mvn").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&mvn_path, perms).expect("chmod mvn");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .env("PATH", bin_dir.to_string_lossy().to_string())
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
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/buildProject",
            "params": { "projectRoot": root.to_string_lossy() }
        }),
    );

    let build_resp = read_response_with_id(&mut stdout, 2);
    let result = build_resp.get("result").cloned().expect("result");
    let build_id = result
        .get("buildId")
        .and_then(|v| v.as_u64())
        .expect("buildId");

    // Poll status until the background build completes.
    let mut next_id = 3;
    let mut final_status = None::<String>;
    for _ in 0..200 {
        write_jsonrpc_message(
            &mut stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": next_id,
                "method": "nova/build/status",
                "params": { "projectRoot": root.to_string_lossy() }
            }),
        );
        let status_resp = read_response_with_id(&mut stdout, next_id);
        next_id += 1;

        let status_obj = status_resp.get("result").cloned().expect("result");
        let status = status_obj
            .get("status")
            .and_then(|v| v.as_str())
            .expect("status")
            .to_string();

        match status.as_str() {
            "building" => thread::sleep(std::time::Duration::from_millis(10)),
            _ => {
                final_status = Some(status);
                break;
            }
        }
    }

    assert_eq!(
        final_status.as_deref(),
        Some("failed"),
        "expected gradle build to fail"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": next_id,
            "method": "nova/build/diagnostics",
            "params": { "projectRoot": root.to_string_lossy() }
        }),
    );
    let diagnostics_resp = read_response_with_id(&mut stdout, next_id);
    next_id += 1;

    let result = diagnostics_resp.get("result").cloned().expect("result");
    assert_eq!(
        result.get("buildId").and_then(|v| v.as_u64()),
        Some(build_id)
    );

    let diags = result
        .get("diagnostics")
        .and_then(|v| v.as_array())
        .expect("diagnostics array");
    assert_eq!(diags.len(), 1);
    let diag = &diags[0];
    assert_eq!(
        diag.get("file").and_then(|v| v.as_str()),
        Some(java_file.to_str().unwrap())
    );
    assert_eq!(diag.get("severity").and_then(|v| v.as_str()), Some("error"));
    assert_eq!(
        diag.pointer("/range/start/line").and_then(|v| v.as_u64()),
        Some(9)
    );
    assert_eq!(
        diag.pointer("/range/start/character")
            .and_then(|v| v.as_u64()),
        Some(4)
    );
    assert!(diag
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .contains("cannot find symbol"));

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": next_id, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, next_id);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn stdio_server_handles_java_classpath_request_with_fake_gradle_wrapper_and_cache() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().join("gradle-project");
    fs::create_dir_all(&root).expect("create project dir");

    fs::write(root.join("settings.gradle"), "rootProject.name = 'demo'\n").expect("write settings");
    fs::write(root.join("build.gradle"), "plugins { id 'java' }\n").expect("write build.gradle");

    let dep_dir = root.join("deps");
    fs::create_dir_all(&dep_dir).expect("create deps");
    let dep1 = dep_dir.join("dep1.jar");
    let dep2 = dep_dir.join("dep2.jar");
    fs::write(&dep1, "").expect("write dep1");
    fs::write(&dep2, "").expect("write dep2");

    let gradlew_path = root.join("gradlew");
    fs::write(
        &gradlew_path,
        format!(
            r#"#!/bin/sh
last=""
for arg in "$@"; do last="$arg"; done
case "$last" in
  *printNovaJavaCompileConfig)
    printf '%s\n' 'NOVA_JSON_BEGIN'
    printf '%s\n' '{{"compileClasspath":["{}","{}"]}}'
    printf '%s\n' 'NOVA_JSON_END'
    ;;
esac
"#,
            dep1.display(),
            dep2.display()
        ),
    )
    .expect("write fake gradlew");
    let mut perms = fs::metadata(&gradlew_path)
        .expect("stat gradlew")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&gradlew_path, perms).expect("chmod gradlew");

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
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    let expected = vec![
        root.join("build/classes/java/main")
            .to_string_lossy()
            .to_string(),
        dep1.to_string_lossy().to_string(),
        dep2.to_string_lossy().to_string(),
    ];

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/java/classpath",
            "params": { "projectRoot": root.to_string_lossy(), "buildTool": "gradle" }
        }),
    );
    let classpath_resp = read_response_with_id(&mut stdout, 2);
    let result = classpath_resp.get("result").cloned().expect("result");
    let classpath = result
        .get("classpath")
        .and_then(|v| v.as_array())
        .expect("classpath array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(classpath, expected);

    // Make the wrapper script non-executable; subsequent requests should still
    // succeed via the on-disk cache without invoking Gradle.
    let mut perms = fs::metadata(&gradlew_path)
        .expect("stat gradlew")
        .permissions();
    perms.set_mode(0o644);
    fs::set_permissions(&gradlew_path, perms).expect("chmod gradlew (disable exec)");

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "nova/java/classpath",
            "params": { "projectRoot": root.to_string_lossy(), "buildTool": "gradle" }
        }),
    );
    let cached_resp = read_response_with_id(&mut stdout, 3);
    let result = match cached_resp.get("result").cloned() {
        Some(result) => result,
        None => panic!("expected result, got: {cached_resp:?}"),
    };
    let classpath = result
        .get("classpath")
        .and_then(|v| v.as_array())
        .expect("classpath array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(classpath, expected);

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn stdio_server_handles_build_project_request_with_fake_gradle_diagnostics() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().join("gradle-project");
    fs::create_dir_all(&root).expect("create project dir");

    fs::write(root.join("settings.gradle"), "rootProject.name = 'demo'\n").expect("write settings");
    fs::write(root.join("build.gradle"), "plugins { id 'java' }\n").expect("write build.gradle");

    let java_dir = root.join("src/main/java/com/example");
    fs::create_dir_all(&java_dir).expect("create java dir");
    let java_file = java_dir.join("Foo.java");
    fs::write(&java_file, "package com.example; public class Foo {}").expect("write Foo.java");

    let gradlew_path = root.join("gradlew");
    fs::write(
        &gradlew_path,
        format!(
            r#"#!/bin/sh
found_compile_java=0
for arg in "$@"; do
  case "$arg" in
    *compileJava|*novaCompileAllJava) found_compile_java=1 ;;
  esac
done

if [ "$found_compile_java" -eq 1 ]; then
  printf '%s\n' '{}:10: error: cannot find symbol' >&2
  printf '%s\n' '        foo.bar();' >&2
  printf '%s\n' '            ^' >&2
  printf '%s\n' '  symbol:   method bar()' >&2
  printf '%s\n' '  location: variable foo of type Foo' >&2
  exit 1
fi

exit 0
"#,
            java_file.display()
        ),
    )
    .expect("write fake gradlew");
    let mut perms = fs::metadata(&gradlew_path)
        .expect("stat gradlew")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&gradlew_path, perms).expect("chmod gradlew");

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
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/buildProject",
            "params": { "projectRoot": root.to_string_lossy(), "buildTool": "gradle" }
        }),
    );

    let build_resp = read_response_with_id(&mut stdout, 2);
    let result = build_resp.get("result").cloned().expect("result");
    let build_id = result
        .get("buildId")
        .and_then(|v| v.as_u64())
        .expect("buildId");

    // Poll status until the background build completes.
    let mut next_id = 3;
    let mut final_status = None::<String>;
    for _ in 0..200 {
        write_jsonrpc_message(
            &mut stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": next_id,
                "method": "nova/build/status",
                "params": { "projectRoot": root.to_string_lossy() }
            }),
        );
        let status_resp = read_response_with_id(&mut stdout, next_id);
        next_id += 1;

        let status_obj = status_resp.get("result").cloned().expect("result");
        let status = status_obj
            .get("status")
            .and_then(|v| v.as_str())
            .expect("status")
            .to_string();

        match status.as_str() {
            "building" => thread::sleep(std::time::Duration::from_millis(10)),
            _ => {
                final_status = Some(status);
                break;
            }
        }
    }

    assert_eq!(
        final_status.as_deref(),
        Some("failed"),
        "expected gradle build to fail"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": next_id,
            "method": "nova/build/diagnostics",
            "params": { "projectRoot": root.to_string_lossy() }
        }),
    );
    let diagnostics_resp = read_response_with_id(&mut stdout, next_id);
    next_id += 1;

    let result = diagnostics_resp.get("result").cloned().expect("result");
    assert_eq!(
        result.get("buildId").and_then(|v| v.as_u64()),
        Some(build_id)
    );

    let diags = result
        .get("diagnostics")
        .and_then(|v| v.as_array())
        .expect("diagnostics array");
    assert_eq!(diags.len(), 1);
    let diag = &diags[0];
    assert_eq!(
        diag.get("file").and_then(|v| v.as_str()),
        Some(java_file.to_str().unwrap())
    );
    assert_eq!(diag.get("severity").and_then(|v| v.as_str()), Some("error"));
    assert_eq!(
        diag.pointer("/range/start/line").and_then(|v| v.as_u64()),
        Some(9)
    );
    // caret line is indented 12 characters before '^' (1-based column 13).
    assert_eq!(
        diag.pointer("/range/start/character")
            .and_then(|v| v.as_u64()),
        Some(12)
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": next_id, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, next_id);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn stdio_server_handles_debug_hot_swap_request_with_fake_maven_and_mock_jdwp() {
    let _lock = crate::support::stdio_server_lock();
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().join("maven-project");
    fs::create_dir_all(&root).expect("create project dir");

    fs::write(
        root.join("pom.xml"),
        r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>demo</artifactId>
  <version>1.0-SNAPSHOT</version>
</project>
"#,
    )
    .expect("write pom");

    let java_dir = root.join("src/main/java/com/example");
    fs::create_dir_all(&java_dir).expect("create java dir");
    let java_file = java_dir.join("Main.java");
    fs::write(
        &java_file,
        r#"
            package com.example;

            public class Main {
                public static void main(String[] args) {}
            }
        "#,
    )
    .expect("write Main.java");

    // Create a dummy class file to "hotswap".
    let class_dir = root.join("target/classes/com/example");
    fs::create_dir_all(&class_dir).expect("create class dir");
    let class_file = class_dir.join("Main.class");
    fs::write(&class_file, vec![0xCA, 0xFE, 0xBA, 0xBE]).expect("write class file");

    // Fake `mvn` so `nova-build` doesn't require a system Maven installation.
    let bin_dir = temp.path().join("bin");
    fs::create_dir_all(&bin_dir).expect("create bin dir");
    let mvn_path = bin_dir.join("mvn");
    fs::write(&mvn_path, "#!/bin/sh\nexit 0\n").expect("write fake mvn");
    let mut perms = fs::metadata(&mvn_path).expect("stat mvn").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&mvn_path, perms).expect("chmod mvn");

    // Minimal JDWP server that can satisfy `TcpJdwpClient` connect + redefine.
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind jdwp listener");
    let port = listener.local_addr().expect("local addr").port();
    let jdwp_thread = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept jdwp");
        let mut handshake = [0u8; 14];
        stream.read_exact(&mut handshake).expect("read handshake");
        assert_eq!(&handshake, b"JDWP-Handshake");
        stream.write_all(&handshake).expect("write handshake");
        stream.flush().ok();

        loop {
            let mut len_buf = [0u8; 4];
            if stream.read_exact(&mut len_buf).is_err() {
                break;
            }
            let length = u32::from_be_bytes(len_buf) as usize;
            assert!(
                (11..=16 * 1024 * 1024).contains(&length),
                "invalid packet length {length}"
            );

            let mut header = [0u8; 7];
            stream.read_exact(&mut header).expect("read header");

            let id = u32::from_be_bytes(header[0..4].try_into().unwrap());
            let flags = header[4];
            assert_eq!(flags & 0x80, 0, "client must send command packets");
            let command_set = header[5];
            let command = header[6];

            let data_len = length - 11;
            let mut data = Vec::new();
            data.try_reserve_exact(data_len)
                .expect("allocate packet payload");
            data.resize(data_len, 0);
            stream.read_exact(&mut data).expect("read packet payload");

            match (command_set, command) {
                (1, 7) => {
                    // VirtualMachine/IDSizes
                    assert!(data.is_empty());
                    let mut reply = Vec::new();
                    for _ in 0..5 {
                        reply.extend_from_slice(&(8u32).to_be_bytes());
                    }
                    write_reply(&mut stream, id, &reply);
                }
                (1, 2) => {
                    // VirtualMachine/ClassesBySignature
                    let _ = data;
                    let mut reply = Vec::new();
                    reply.extend_from_slice(&(1u32).to_be_bytes()); // count
                    reply.push(1); // tag = class
                    reply.extend_from_slice(&123u64.to_be_bytes()); // type id
                    reply.extend_from_slice(&(1u32).to_be_bytes()); // status
                    write_reply(&mut stream, id, &reply);
                }
                (1, 18) => {
                    // VirtualMachine/RedefineClasses
                    write_reply(&mut stream, id, &[]);
                    break;
                }
                other => panic!("unexpected JDWP command {other:?}"),
            }
        }
    });

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .env("PATH", bin_dir.to_string_lossy().to_string())
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
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/debug/hotSwap",
            "params": {
                "projectRoot": root.to_string_lossy(),
                "changedFiles": [java_file.to_string_lossy()],
                "port": port
            }
        }),
    );

    let resp = read_response_with_id(&mut stdout, 2);
    let result = resp.get("result").cloned().expect("result");
    let results = result
        .get("results")
        .and_then(|v| v.as_array())
        .expect("results array");
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].get("status").and_then(|v| v.as_str()),
        Some("success")
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
    jdwp_thread.join().expect("join jdwp thread");
}

#[cfg(unix)]
#[test]
fn stdio_server_reload_project_invalidates_maven_classpath_cache() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().join("maven-project");
    fs::create_dir_all(&root).expect("create project dir");

    fs::write(
        root.join("pom.xml"),
        r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>demo</artifactId>
  <version>1.0-SNAPSHOT</version>
</project>
"#,
    )
    .expect("write pom");

    let dep_dir = root.join("deps");
    fs::create_dir_all(&dep_dir).expect("create deps");
    let dep1 = dep_dir.join("dep1.jar");
    let dep2 = dep_dir.join("dep2.jar");
    fs::write(&dep1, "").expect("write dep1");
    fs::write(&dep2, "").expect("write dep2");

    // The fake Maven executable reads `.classpath-out` from the project root,
    // allowing the test to change the classpath output without touching build
    // files (so the fingerprint stays stable).
    fs::write(
        root.join(".classpath-out"),
        format!("[\"{}\"]\n", dep1.display()),
    )
    .expect("write classpath-out");

    let bin_dir = temp.path().join("bin");
    fs::create_dir_all(&bin_dir).expect("create bin dir");
    let mvn_path = bin_dir.join("mvn");
    fs::write(
        &mvn_path,
        r#"#!/bin/sh
expr=""
for arg in "$@"; do
  case "$arg" in
    -Dexpression=*) expr="${arg#-Dexpression=}" ;;
  esac
done

case "$expr" in
  project.build.outputDirectory) printf '%s\n' 'target/classes' ;;
  project.build.testOutputDirectory) printf '%s\n' 'target/test-classes' ;;
  project.compileClasspathElements|project.testClasspathElements) cat .classpath-out ;;
  *) printf '%s\n' '[]' ;;
esac
"#,
    )
    .expect("write fake mvn");
    let mut perms = fs::metadata(&mvn_path).expect("stat mvn").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&mvn_path, perms).expect("chmod mvn");

    let system_path = std::env::var("PATH").unwrap_or_default();
    let combined_path = if system_path.is_empty() {
        bin_dir.to_string_lossy().to_string()
    } else {
        format!("{}:{}", bin_dir.to_string_lossy(), system_path)
    };

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .env("PATH", combined_path)
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
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    // 1) Prime the cache.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/java/classpath",
            "params": { "projectRoot": root.to_string_lossy(), "buildTool": "maven" }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let result = resp.get("result").cloned().expect("result");
    let classpath = result
        .get("classpath")
        .and_then(|v| v.as_array())
        .expect("classpath array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        classpath,
        vec![
            root.join("target/classes").to_string_lossy().to_string(),
            dep1.to_string_lossy().to_string(),
        ]
    );

    // 2) Change Maven output without changing build files; cached response should
    // still return the old value.
    fs::write(
        root.join(".classpath-out"),
        format!("[\"{}\"]\n", dep2.display()),
    )
    .expect("rewrite classpath-out");

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "nova/java/classpath",
            "params": { "projectRoot": root.to_string_lossy(), "buildTool": "maven" }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    let result = resp.get("result").cloned().expect("result");
    let classpath = result
        .get("classpath")
        .and_then(|v| v.as_array())
        .expect("classpath array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        classpath,
        vec![
            root.join("target/classes").to_string_lossy().to_string(),
            dep1.to_string_lossy().to_string(),
        ]
    );

    // 3) reloadProject should clear the cache; the next request should see dep2.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "nova/reloadProject",
            "params": { "projectRoot": root.to_string_lossy(), "buildTool": "maven" }
        }),
    );
    let _reload_resp = read_response_with_id(&mut stdout, 4);

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "nova/java/classpath",
            "params": { "projectRoot": root.to_string_lossy(), "buildTool": "maven" }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 5);
    let result = resp.get("result").cloned().expect("result");
    let classpath = result
        .get("classpath")
        .and_then(|v| v.as_array())
        .expect("classpath array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        classpath,
        vec![
            root.join("target/classes").to_string_lossy().to_string(),
            dep2.to_string_lossy().to_string(),
        ]
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 6, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 6);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn stdio_server_reload_project_invalidates_gradle_classpath_cache() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().join("gradle-project");
    fs::create_dir_all(&root).expect("create project dir");

    fs::write(root.join("settings.gradle"), "rootProject.name = 'demo'\n").expect("write settings");
    fs::write(root.join("build.gradle"), "plugins { id 'java' }\n").expect("write build.gradle");

    let dep_dir = root.join("deps");
    fs::create_dir_all(&dep_dir).expect("create deps");
    let dep1 = dep_dir.join("dep1.jar");
    let dep2 = dep_dir.join("dep2.jar");
    fs::write(&dep1, "").expect("write dep1");
    fs::write(&dep2, "").expect("write dep2");

    fs::write(
        root.join(".classpath-out"),
        format!(
            "NOVA_JSON_BEGIN\n{{\"compileClasspath\":[\"{}\"]}}\nNOVA_JSON_END\n",
            dep1.display()
        ),
    )
    .expect("write classpath-out");

    let gradlew_path = root.join("gradlew");
    fs::write(
        &gradlew_path,
        r#"#!/bin/sh
last=""
for arg in "$@"; do last="$arg"; done
case "$last" in
  *printNovaJavaCompileConfig)
    cat .classpath-out
    ;;
  esac
"#,
    )
    .expect("write fake gradlew");
    let mut perms = fs::metadata(&gradlew_path)
        .expect("stat gradlew")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&gradlew_path, perms).expect("chmod gradlew");

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
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/java/classpath",
            "params": { "projectRoot": root.to_string_lossy(), "buildTool": "gradle" }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let result = resp.get("result").cloned().expect("result");
    let classpath = result
        .get("classpath")
        .and_then(|v| v.as_array())
        .expect("classpath array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        classpath,
        vec![
            root.join("build/classes/java/main")
                .to_string_lossy()
                .to_string(),
            dep1.to_string_lossy().to_string(),
        ]
    );

    fs::write(
        root.join(".classpath-out"),
        format!(
            "NOVA_JSON_BEGIN\n{{\"compileClasspath\":[\"{}\"]}}\nNOVA_JSON_END\n",
            dep2.display()
        ),
    )
    .expect("rewrite classpath-out");

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "nova/java/classpath",
            "params": { "projectRoot": root.to_string_lossy(), "buildTool": "gradle" }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    let result = resp.get("result").cloned().expect("result");
    let classpath = result
        .get("classpath")
        .and_then(|v| v.as_array())
        .expect("classpath array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        classpath,
        vec![
            root.join("build/classes/java/main")
                .to_string_lossy()
                .to_string(),
            dep1.to_string_lossy().to_string(),
        ]
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "nova/reloadProject",
            "params": { "projectRoot": root.to_string_lossy(), "buildTool": "gradle" }
        }),
    );
    let _reload_resp = read_response_with_id(&mut stdout, 4);

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "nova/java/classpath",
            "params": { "projectRoot": root.to_string_lossy(), "buildTool": "gradle" }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 5);
    let result = resp.get("result").cloned().expect("result");
    let classpath = result
        .get("classpath")
        .and_then(|v| v.as_array())
        .expect("classpath array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        classpath,
        vec![
            root.join("build/classes/java/main")
                .to_string_lossy()
                .to_string(),
            dep2.to_string_lossy().to_string(),
        ]
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 6, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 6);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[cfg(unix)]
fn write_reply(stream: &mut impl Write, id: u32, data: &[u8]) {
    let length = 11usize + data.len();
    stream
        .write_all(&(length as u32).to_be_bytes())
        .expect("write length");
    stream.write_all(&id.to_be_bytes()).expect("write id");
    stream.write_all(&[0x80]).expect("write flags");
    stream.write_all(&0u16.to_be_bytes()).expect("write error");
    stream.write_all(data).expect("write data");
    stream.flush().ok();
}
