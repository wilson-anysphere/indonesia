use lsp_types::{CodeAction, Position, Range, Uri};
use nova_core::{
    LineIndex, Position as CorePosition, Range as CoreRange, TextEdit as CoreTextEdit, TextSize,
};
use serde_json::json;
use std::fs;
use std::io::BufReader;
use std::process::{Command, Stdio};
use tempfile::TempDir;
use url::Url;

use crate::support::{read_response_with_id, write_jsonrpc_message};

#[test]
fn stdio_server_offers_unresolved_type_import_and_fqn_quick_fixes_for_cursor_at_end() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let file_path = temp.path().join("Test.java");

    let source = "class A {\n  List<String> xs;\n}\n";
    fs::write(&file_path, source).expect("write file");

    let uri: Uri = Url::from_file_path(&file_path)
        .expect("uri")
        .to_string()
        .parse()
        .expect("uri");

    let list_start = source.find("List<String>").expect("List occurrence");
    let list_end = list_start + "List".len();

    let index = LineIndex::new(source);
    let diag_start = index.position(source, TextSize::from(list_start as u32));
    let diag_end = index.position(source, TextSize::from(list_end as u32));

    let diag_range = Range::new(
        Position::new(diag_start.line, diag_start.character),
        Position::new(diag_end.line, diag_end.character),
    );
    // Cursor selection at the end of `List` (common when the cursor is after the token).
    let selection_range = Range::new(
        Position::new(diag_end.line, diag_end.character),
        Position::new(diag_end.line, diag_end.character),
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
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    // 2) didOpen
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

    // 3) request code actions with diagnostics context
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/codeAction",
            "params": {
                "textDocument": { "uri": uri },
                "range": selection_range,
                "context": {
                    "diagnostics": [{
                        "range": diag_range,
                        "code": "unresolved-type",
                        "message": "unresolved type `List`"
                    }]
                }
            }
        }),
    );

    let code_action_resp = read_response_with_id(&mut stdout, 2);
    let actions = code_action_resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");

    let import_action = actions
        .iter()
        .find(|action| {
            action
                .get("title")
                .and_then(|v| v.as_str())
                .is_some_and(|title| title == "Import java.util.List")
        })
        .expect("expected import quick fix");
    let fqn_action = actions
        .iter()
        .find(|action| {
            action
                .get("title")
                .and_then(|v| v.as_str())
                .is_some_and(|title| title == "Use fully qualified name 'java.util.List'")
        })
        .expect("expected FQN quick fix");

    let import_action: CodeAction =
        serde_json::from_value(import_action.clone()).expect("decode import CodeAction");
    let fqn_action: CodeAction =
        serde_json::from_value(fqn_action.clone()).expect("decode fqn CodeAction");

    let import_edit = import_action.edit.expect("expected import edit");
    let import_changes = import_edit.changes.expect("expected changes");
    let import_edits = import_changes.get(&uri).expect("edits for file");
    let updated = apply_lsp_text_edits(source, import_edits);
    assert!(
        updated.contains("import java.util.List;"),
        "expected import insertion; got:\n{updated}"
    );

    let fqn_edit = fqn_action.edit.expect("expected fqn edit");
    let fqn_changes = fqn_edit.changes.expect("expected changes");
    let fqn_edits = fqn_changes.get(&uri).expect("edits for file");
    let updated = apply_lsp_text_edits(source, fqn_edits);
    assert!(
        updated.contains("java.util.List<String> xs;"),
        "expected type reference to be qualified; got:\n{updated}"
    );

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

