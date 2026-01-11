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

mod support;
use support::{read_jsonrpc_message, read_response_with_id, write_jsonrpc_message};

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

#[test]
fn stdio_server_handles_metrics_request() {
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
    assert!(request_count > 0, "expected requestCount > 0, got: {resp:#}");

    // shutdown + exit
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_handles_test_discover_request() {
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
fn stdio_server_applies_incremental_did_change_utf16_correctly() {
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

    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].range.start.line, 1);
    assert_eq!(edits[0].range.start.character, 0);
    assert_eq!(edits[0].new_text, "import java.util.stream.Collectors;\n");

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
fn stdio_server_discovers_tests_in_simple_project_fixture() {
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
            "#!/bin/sh\nprintf '%s\\n' '[\"{}\",\"{}\"]'\n",
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
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/buildProject",
            "params": { "projectRoot": root.to_string_lossy() }
        }),
    );

    let build_resp = read_response_with_id(&mut stdout, 2);
    let result = build_resp.get("result").cloned().expect("result");
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
fn stdio_server_handles_java_classpath_request_with_fake_gradle_wrapper_and_cache() {
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
last=""
for arg in "$@"; do last="$arg"; done
case "$last" in
  *compileJava|*novaCompileAllJava)
    printf '%s\n' '{}:10: error: cannot find symbol'
    printf '%s\n' '        foo.bar();'
    printf '%s\n' '            ^'
    printf '%s\n' '  symbol:   method bar()'
    printf '%s\n' '  location: variable foo of type Foo'
    exit 1
    ;;
esac
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
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/buildProject",
            "params": { "projectRoot": root.to_string_lossy(), "buildTool": "gradle" }
        }),
    );

    let build_resp = read_response_with_id(&mut stdout, 2);
    let result = build_resp.get("result").cloned().expect("result");
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
fn stdio_server_handles_debug_hot_swap_request_with_fake_maven_and_mock_jdwp() {
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
            assert!(length >= 11, "invalid packet length {length}");
            let mut rest = vec![0u8; length - 4];
            stream.read_exact(&mut rest).expect("read packet");

            let id = u32::from_be_bytes(rest[0..4].try_into().unwrap());
            let flags = rest[4];
            assert_eq!(flags & 0x80, 0, "client must send command packets");
            let command_set = rest[5];
            let command = rest[6];
            let data = &rest[7..];

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
    fs::write(&mvn_path, "#!/bin/sh\ncat .classpath-out\n").expect("write fake mvn");
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
