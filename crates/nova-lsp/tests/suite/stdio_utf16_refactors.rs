use lsp_types::{
    DocumentChangeOperation, DocumentChanges, OneOf, Range, ResourceOp, Uri, WorkspaceEdit,
};
use nova_test_utils::extract_range;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::io::BufReader;
use std::process::{Command, Stdio};
use std::str::FromStr;
use tempfile::tempdir;

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
fn stdio_server_rename_type_emits_file_rename_and_updates_references() {
    let _lock = crate::support::stdio_server_lock();

    let foo_uri = Uri::from_str("file:///workspace/src/main/java/p/Foo.java").unwrap();
    let use_uri = Uri::from_str("file:///workspace/src/main/java/p/Use.java").unwrap();
    let bar_uri = Uri::from_str("file:///workspace/src/main/java/p/Bar.java").unwrap();

    let foo_source = "package p; public class Foo { Foo() {} }";
    let use_source = "package p; class Use { Foo f; void m(){ new Foo(); } }";

    let foo_start = foo_source.find("class Foo").expect("class declaration") + "class ".len();
    let foo_end = foo_start + "Foo".len();
    let foo_position = lsp_position_utf16(foo_source, foo_start + 1);
    let foo_range = lsp_range_utf16(foo_source, foo_start, foo_end);

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

    // 2) open documents (overlays only; no real files)
    for (uri, source) in [(foo_uri.clone(), foo_source), (use_uri.clone(), use_source)] {
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
    }

    // 3) prepareRename on the type name => identifier range only.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/prepareRename",
            "params": {
                "textDocument": { "uri": foo_uri },
                "position": foo_position,
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let result = resp.get("result").cloned().expect("prepareRename result");
    let range: Range = serde_json::from_value(result).expect("decode prepareRename range");
    assert_eq!(range, foo_range);

    // 4) rename Foo -> Bar => should include a file rename op + text edits for Bar.java and Use.java.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "textDocument/rename",
            "params": {
                "textDocument": { "uri": foo_uri },
                "position": foo_position,
                "newName": "Bar"
            }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    let result = resp.get("result").cloned().expect("rename result");
    let edit: WorkspaceEdit = serde_json::from_value(result).expect("decode workspace edit");

    assert!(
        edit.changes.is_none(),
        "expected rename edits with file ops to use documentChanges only, got: {edit:?}"
    );

    let Some(document_changes) = edit.document_changes else {
        panic!("expected documentChanges for type rename with file rename");
    };
    let DocumentChanges::Operations(ops) = document_changes else {
        panic!("expected documentChanges as Operations");
    };

    let rename_op = ops.iter().find_map(|op| match op {
        DocumentChangeOperation::Op(ResourceOp::Rename(op)) => Some(op),
        _ => None,
    });
    let rename_op = rename_op.expect("expected ResourceOp::Rename");
    assert_eq!(rename_op.old_uri.as_str(), foo_uri.as_str());
    assert_eq!(rename_op.new_uri.as_str(), bar_uri.as_str());

    fn flatten_text_edits(
        edits: &[OneOf<lsp_types::TextEdit, lsp_types::AnnotatedTextEdit>],
    ) -> Vec<lsp_types::TextEdit> {
        edits
            .iter()
            .map(|e| match e {
                OneOf::Left(e) => e.clone(),
                OneOf::Right(e) => e.text_edit.clone(),
            })
            .collect()
    }

    let mut bar_edits: Vec<lsp_types::TextEdit> = Vec::new();
    let mut use_edits: Vec<lsp_types::TextEdit> = Vec::new();
    for op in ops {
        let DocumentChangeOperation::Edit(edit) = op else {
            continue;
        };
        if edit.text_document.uri.as_str() == bar_uri.as_str() {
            bar_edits.extend(flatten_text_edits(&edit.edits));
        } else if edit.text_document.uri.as_str() == use_uri.as_str() {
            use_edits.extend(flatten_text_edits(&edit.edits));
        }
    }

    assert!(
        !bar_edits.is_empty(),
        "expected TextDocumentEdit operations for {}",
        bar_uri.as_str()
    );
    assert!(
        !use_edits.is_empty(),
        "expected TextDocumentEdit operations for {}",
        use_uri.as_str()
    );

    let bar_actual = apply_lsp_text_edits(foo_source, &bar_edits);
    let bar_expected = "package p; public class Bar { Bar() {} }";
    assert_eq!(bar_actual, bar_expected);

    let use_actual = apply_lsp_text_edits(use_source, &use_edits);
    let use_expected = "package p; class Use { Bar f; void m(){ new Bar(); } }";
    assert_eq!(use_actual, use_expected);

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

    // 3) prepareRename on field => range
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

    // 4) rename on field updates declaration and usages
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
fn stdio_server_rename_type_updates_multiple_files() {
    let _lock = crate::support::stdio_server_lock();
    let tmp = tempdir().expect("tempdir");
    let root = tmp.path();

    std::fs::create_dir_all(root.join("src")).expect("mkdir src");
    let foo_path = root.join("src/Foo.java");
    let use_path = root.join("src/Use.java");

    let foo_src = r#"class Foo { Foo(){} }"#;
    let use_src = r#"class Use { void m(){ new Foo(); } }"#;

    std::fs::write(&foo_path, foo_src).expect("write Foo.java");
    std::fs::write(&use_path, use_src).expect("write Use.java");

    fn uri_for_path(path: &std::path::Path) -> Uri {
        let abs = nova_core::AbsPathBuf::new(path.to_path_buf()).expect("absolute path");
        let uri = nova_core::path_to_file_uri(&abs).expect("path to uri");
        Uri::from_str(&uri).expect("parse uri")
    }

    let foo_uri = uri_for_path(&foo_path);
    let use_uri = uri_for_path(&use_path);

    let foo_offset = foo_src.find("class Foo").expect("class Foo") + "class ".len() + 1;
    let foo_position = lsp_position_utf16(foo_src, foo_offset);

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

    // 2) open Foo.java (Use.java stays on disk only)
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": foo_uri,
                    "languageId": "java",
                    "version": 1,
                    "text": foo_src,
                }
            }
        }),
    );

    // 3) rename type Foo -> Bar (should update Foo.java and Use.java)
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/rename",
            "params": {
                "textDocument": { "uri": foo_uri },
                "position": foo_position,
                "newName": "Bar"
            }
        }),
    );

    let rename_resp = read_response_with_id(&mut stdout, 2);
    let result = rename_resp.get("result").cloned().expect("workspace edit");
    let edit: WorkspaceEdit = serde_json::from_value(result).expect("decode workspace edit");
    let changes = edit.changes.expect("changes map");
    assert!(
        changes.contains_key(&foo_uri),
        "expected edits for Foo.java"
    );
    assert!(
        changes.contains_key(&use_uri),
        "expected edits for Use.java"
    );

    let foo_actual = apply_lsp_text_edits(foo_src, changes.get(&foo_uri).expect("Foo edits"));
    let use_actual = apply_lsp_text_edits(use_src, changes.get(&use_uri).expect("Use edits"));

    assert_eq!(foo_actual, r#"class Bar { Bar(){} }"#);
    assert_eq!(use_actual, r#"class Use { void m(){ new Bar(); } }"#);

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
