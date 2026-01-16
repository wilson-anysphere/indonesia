use lsp_types::{
    CodeActionContext, CodeActionOrCommand, CodeActionParams, Diagnostic, NumberOrString,
    PartialResultParams, Position, Range, TextDocumentIdentifier, Uri, WorkDoneProgressParams,
};
use nova_core::{
    LineIndex, Position as CorePosition, Range as CoreRange, TextEdit as CoreTextEdit, TextSize,
};
use std::fs;
use std::io::BufReader;
use std::process::{Command, Stdio};
use tempfile::TempDir;
use url::Url;

use crate::support::{
    did_open_notification, exit_notification, initialize_request_empty, initialized_notification,
    jsonrpc_request, read_response_with_id, shutdown_request, stdio_server_lock,
    write_jsonrpc_message,
};

#[test]
fn stdio_server_offers_unresolved_type_import_and_fqn_quick_fixes_for_cursor_at_end() {
    let _lock = stdio_server_lock();
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
    write_jsonrpc_message(&mut stdin, &initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    // 2) didOpen
    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(uri.clone(), "java", 1, source),
    );

    // 3) request code actions with diagnostics context
    let diagnostic = Diagnostic {
        range: diag_range,
        severity: None,
        code: Some(NumberOrString::String("unresolved-type".to_string())),
        code_description: None,
        source: None,
        message: "unresolved type `List`".to_string(),
        related_information: None,
        tags: None,
        data: None,
    };
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            CodeActionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                range: selection_range,
                context: CodeActionContext {
                    diagnostics: vec![diagnostic],
                    ..CodeActionContext::default()
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            2,
            "textDocument/codeAction",
        ),
    );

    let code_action_resp = read_response_with_id(&mut stdout, 2);
    let actions: Vec<CodeActionOrCommand> = serde_json::from_value(
        code_action_resp
            .get("result")
            .cloned()
            .expect("code actions result"),
    )
    .expect("decode CodeActionResponse");

    let import_action = actions
        .iter()
        .find_map(|action| match action {
            CodeActionOrCommand::CodeAction(action) if action.title == "Import java.util.List" => {
                Some(action.clone())
            }
            _ => None,
        })
        .expect("expected import quick fix");
    let fqn_action = actions
        .iter()
        .find_map(|action| match action {
            CodeActionOrCommand::CodeAction(action)
                if action.title == "Use fully qualified name 'java.util.List'" =>
            {
                Some(action.clone())
            }
            _ => None,
        })
        .expect("expected FQN quick fix");

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
    write_jsonrpc_message(&mut stdin, &shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &exit_notification());
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
