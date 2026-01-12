use lsp_types::{
    DocumentChangeOperation, DocumentChanges, OneOf, Range, ResourceOp, Uri, WorkspaceEdit,
};
use nova_test_utils::extract_range;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::io::BufReader;
use std::process::{Command, Stdio};
use std::str::FromStr;

use crate::support::{read_response_with_id, write_jsonrpc_message};

fn lsp_position_utf16(text: &str, offset: usize) -> lsp_types::Position {
    let index = nova_core::LineIndex::new(text);
    let pos = index.position(text, nova_core::TextSize::from(offset as u32));
    lsp_types::Position::new(pos.line, pos.character)
}

fn lsp_range_utf16(text: &str, start: usize, end: usize) -> Range {
    Range {
        start: lsp_position_utf16(text, start),
        end: lsp_position_utf16(text, end),
    }
}

fn apply_lsp_text_edits(original: &str, edits: &[lsp_types::TextEdit]) -> String {
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
            let range = index.text_range(original, range).expect("valid lsp range");
            nova_core::TextEdit::new(range, edit.new_text.clone())
        })
        .collect();

    nova_core::apply_text_edits(original, &core_edits).expect("apply edits")
}

#[test]
fn stdio_server_rename_package_declaration_dispatches_to_move_package() {
    let uri = Uri::from_str("file:///workspace/src/main/java/com/example/C.java").unwrap();
    let source = "package com.example;\n\npublic class C {}\n";

    let pkg_start = source.find("com.example").expect("package name");
    let pkg_end = pkg_start + "com.example".len();
    let pkg_pos = lsp_position_utf16(source, pkg_start + 1);

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

    // 2) open document
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
                    "text": source,
                }
            }
        }),
    );

    // 3) prepareRename on package => full dotted range
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/prepareRename",
            "params": {
                "textDocument": { "uri": uri },
                "position": pkg_pos,
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let result = resp.get("result").cloned().expect("prepareRename result");
    let range: Range = serde_json::from_value(result).expect("decode prepareRename range");
    assert_eq!(range, lsp_range_utf16(source, pkg_start, pkg_end));

    // 4) rename package => move_package refactor (file rename + text updates)
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "textDocument/rename",
            "params": {
                "textDocument": { "uri": uri },
                "position": pkg_pos,
                "newName": "com.foo"
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    let result = resp.get("result").cloned().expect("rename result");
    let edit: WorkspaceEdit = serde_json::from_value(result).expect("decode workspace edit");

    let Some(document_changes) = edit.document_changes else {
        panic!("expected documentChanges for package rename");
    };
    let DocumentChanges::Operations(ops) = document_changes else {
        panic!("expected documentChanges as Operations");
    };
    assert!(ops
        .iter()
        .any(|op| matches!(op, DocumentChangeOperation::Op(ResourceOp::Rename(_)))));

    let new_uri = Uri::from_str("file:///workspace/src/main/java/com/foo/C.java").unwrap();
    let mut saw_updated_package = false;
    for op in ops {
        let DocumentChangeOperation::Edit(edit) = op else {
            continue;
        };
        if edit.text_document.uri != new_uri {
            continue;
        }
        if edit.edits.iter().any(|e| match e {
            OneOf::Left(edit) => edit.new_text.contains("package com.foo;"),
            OneOf::Right(edit) => edit.text_edit.new_text.contains("package com.foo;"),
        }) {
            saw_updated_package = true;
        }
    }
    assert!(saw_updated_package, "expected package declaration rewrite");

    // 5) shutdown + exit
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

#[test]
fn stdio_server_rename_is_utf16_correct_with_crlf() {
    let _lock = crate::support::stdio_server_lock();
    let uri = Uri::from_str("file:///Test.java").unwrap();
    let source = r#"
 class C {
     void m() {
        int x = 0;
        int /*😀*/foo = x;
        foo = foo + 1;
    }
}
"#
    .replace("\n", "\r\n");

    let foo_offset = source.find("foo").expect("foo identifier");
    let foo_position = lsp_position_utf16(&source, foo_offset);

    // Positions that point inside a surrogate pair should be rejected.
    let emoji_offset = source.find('😀').expect("emoji in source");
    let emoji_pos = lsp_position_utf16(&source, emoji_offset);
    let inside_surrogate = lsp_types::Position::new(emoji_pos.line, emoji_pos.character + 1);

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

    // 2) open document (CRLF + surrogate pair)
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
                    "text": source,
                }
            }
        }),
    );

    // 3) prepareRename inside surrogate pair => null
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/prepareRename",
            "params": {
                "textDocument": { "uri": uri },
                "position": inside_surrogate,
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    assert_eq!(resp.get("result"), Some(&serde_json::Value::Null));

    // 4) rename on identifier after emoji
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "textDocument/rename",
            "params": {
                "textDocument": { "uri": uri },
                "position": foo_position,
                "newName": "bar"
            }
        }),
    );

    let rename_resp = read_response_with_id(&mut stdout, 3);
    let result = rename_resp.get("result").cloned().expect("workspace edit");
    let edit: WorkspaceEdit = serde_json::from_value(result).expect("decode workspace edit");
    let changes = edit.changes.expect("changes map");
    let edits = changes.get(&uri).expect("edits for uri");

    let actual = apply_lsp_text_edits(&source, edits);
    let expected = source.replace("foo", "bar");
    // The refactor engine may normalize newlines in inserted text. The purpose of this test is to
    // ensure UTF-16 ranges are handled correctly when the document uses CRLF line endings, not to
    // enforce a specific newline style in the resulting edits.
    assert_eq!(actual.replace("\r\n", "\n"), expected.replace("\r\n", "\n"));

    // 5) shutdown + exit
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

#[test]
fn stdio_server_rename_does_not_touch_type_arguments_or_annotations() {
    let _lock = crate::support::stdio_server_lock();
    let uri = Uri::from_str("file:///Test.java").unwrap();
    let source = r#"class Test {
  @interface Foo {}

  void m() {
    int Foo = 1;
    java.util.List<Foo> xs = null;
    @Foo int y = Foo;
  }
}
"#;

    let foo_offset = source.find("int Foo = 1").expect("local Foo declaration") + "int ".len() + 1;
    let foo_position = lsp_position_utf16(source, foo_offset);

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

    // 2) open document
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
                    "text": source,
                }
            }
        }),
    );

    // 3) rename local Foo -> Bar
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/rename",
            "params": {
                "textDocument": { "uri": uri },
                "position": foo_position,
                "newName": "Bar"
            }
        }),
    );

    let rename_resp = read_response_with_id(&mut stdout, 2);
    let result = rename_resp.get("result").cloned().expect("workspace edit");
    let edit: WorkspaceEdit = serde_json::from_value(result).expect("decode workspace edit");
    let changes = edit.changes.expect("changes map");
    let edits = changes.get(&uri).expect("edits for uri");

    let actual = apply_lsp_text_edits(source, edits);
    let expected = r#"class Test {
  @interface Foo {}

  void m() {
    int Bar = 1;
    java.util.List<Foo> xs = null;
    @Foo int y = Bar;
  }
}
"#;
    assert_eq!(actual, expected);

    // 4) shutdown + exit
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
fn stdio_server_supports_field_rename() {
    let _lock = crate::support::stdio_server_lock();
    let uri = Uri::from_str("file:///Test.java").unwrap();
    let source = r#"class Test {
  int foo = 0;

  void m() {
    foo = 1;
  }
}
"#;

    let foo_offset = source.find("int foo = 0").expect("field foo declaration") + "int ".len() + 1;
    let foo_position = lsp_position_utf16(source, foo_offset);
    let foo_name_offset = source.find("foo").expect("field foo identifier");
    let foo_range = lsp_range_utf16(source, foo_name_offset, foo_name_offset + "foo".len());

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

    // 2) open document
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
                    "text": source,
                }
            }
        }),
    );

    // 3) prepareRename on field => null
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/prepareRename",
            "params": {
                "textDocument": { "uri": uri },
                "position": foo_position,
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let result = resp.get("result").cloned().expect("prepareRename result");
    let range: Range = serde_json::from_value(result).expect("decode prepareRename range");
    assert_eq!(range, foo_range);

    // 4) rename on field
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "textDocument/rename",
            "params": {
                "textDocument": { "uri": uri },
                "position": foo_position,
                "newName": "bar"
            }
        }),
    );
    let rename_resp = read_response_with_id(&mut stdout, 3);
    let result = rename_resp.get("result").cloned().expect("workspace edit");
    let edit: WorkspaceEdit = serde_json::from_value(result).expect("decode workspace edit");
    let changes = edit.changes.expect("changes map");
    let edits = changes.get(&uri).expect("edits for uri");
    let actual = apply_lsp_text_edits(source, edits);
    let expected = r#"class Test {
  int bar = 0;

  void m() {
    bar = 1;
  }
}
"#;
    assert_eq!(actual, expected);

    // 5) shutdown + exit
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

#[test]
fn stdio_server_extract_method_is_utf16_correct_with_crlf() {
    let _lock = crate::support::stdio_server_lock();
    let uri = Uri::from_str("file:///Test.java").unwrap();
    let fixture = r#"
class C {
    void m(int a) {
        int b = 1;
        /*start*/System.out.println("😀" + a + b);/*end*/
        System.out.println("done");
    }
}
"#
    .replace("\n", "\r\n");

    let (source, selection) = extract_range(&fixture);
    let range = lsp_range_utf16(&source, selection.start, selection.end);

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

    // 2) open document (CRLF + surrogate pair)
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
                    "text": source,
                }
            }
        }),
    );

    // 3) request code actions for selection (selection includes emoji)
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

    let code_action_resp = read_response_with_id(&mut stdout, 2);
    let actions = code_action_resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");
    let extract = actions
        .iter()
        .find(|action| {
            action.pointer("/command/command").and_then(|v| v.as_str())
                == Some("nova.extractMethod")
        })
        .expect("extract method action");

    let args = extract
        .pointer("/command/arguments/0")
        .cloned()
        .expect("command args");

    // 4) execute the command
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

    let exec_resp = read_response_with_id(&mut stdout, 3);
    let result = exec_resp.get("result").cloned().expect("workspace edit");
    let edit: WorkspaceEdit = serde_json::from_value(result).expect("decode workspace edit");
    let changes = edit.changes.expect("changes map");
    let edits = changes.get(&uri).expect("edits for uri");

    let actual = apply_lsp_text_edits(&source, edits);

    // Build the expected text using the same high-level transformation:
    // - Replace selection with `extracted(a, b);`
    // - Insert the extracted method after the enclosing method.
    let insertion_offset = source
        .rfind("\r\n}")
        .expect("newline before class closing brace");
    let inserted_method = "\r\n\r\n    private void extracted(int a, int b) {\r\n        System.out.println(\"😀\" + a + b);\r\n    }";
    let mut expected = source.clone();
    expected.insert_str(insertion_offset, &inserted_method);
    expected.replace_range(selection.start..selection.end, "extracted(a, b);");

    assert_eq!(actual, expected);

    // 5) shutdown + exit
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
