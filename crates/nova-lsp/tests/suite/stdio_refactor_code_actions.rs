use lsp_types::{
    CodeAction, CodeActionContext, CodeActionParams, PartialResultParams, Position, Range,
    TextDocumentIdentifier, Uri, WorkDoneProgressParams,
};
use nova_core::{
    LineIndex, Position as CorePosition, Range as CoreRange, TextEdit as CoreTextEdit, TextSize,
};
use pretty_assertions::assert_eq;
use std::fs;
use std::io::BufReader;
use std::process::{Command, Stdio};
use tempfile::TempDir;
use url::Url;

use crate::support::{
    did_open_notification, exit_notification, initialize_request_empty, initialized_notification,
    jsonrpc_request, read_response_with_id, shutdown_request, write_jsonrpc_message,
};

#[test]
fn stdio_server_resolves_extract_constant_code_action() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let file_path = temp.path().join("Test.java");

    let source = "class C {\n    void m() {\n        int x = /* 😀 */ 1 + 2;\n    }\n}\n";
    fs::write(&file_path, source).expect("write file");

    let uri: Uri = Url::from_file_path(&file_path)
        .expect("uri")
        .to_string()
        .parse()
        .expect("uri");

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
    write_jsonrpc_message(&mut stdin, &initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    // 2) didOpen (so resolution can read the in-memory snapshot)
    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(uri.clone(), "java", 1, source),
    );

    // 3) request code actions for the expression selection
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            CodeActionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                range,
                context: CodeActionContext {
                    diagnostics: Vec::new(),
                    only: None,
                    trigger_kind: None,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            2,
            "textDocument/codeAction",
        ),
    );

    let code_action_resp = read_response_with_id(&mut stdout, 2);
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
        &jsonrpc_request(extract_constant.clone(), 3, "codeAction/resolve"),
    );

    let resolve_resp = read_response_with_id(&mut stdout, 3);
    let resolved: CodeAction =
        serde_json::from_value(resolve_resp.get("result").cloned().expect("result"))
            .expect("decode resolved CodeAction");

    assert_eq!(
        resolved
            .data
            .as_ref()
            .and_then(|data| data.get("uri"))
            .and_then(|uri| uri.as_str()),
        Some(uri_string.as_str()),
        "expected resolved extract constant action to retain `data.uri`"
    );

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
        updated.contains(&format!("int x = /* 😀 */ {name};")),
        "expected initializer replaced with constant reference"
    );
    assert!(
        !updated.contains("int x = /* 😀 */ 1 + 2;"),
        "expected original expression to be replaced"
    );

    let expected = format!(
        "class C {{\n    private static final int {name} = 1 + 2;\n\n    void m() {{\n        int x = /* 😀 */ {name};\n    }}\n}}\n"
    );
    assert_eq!(updated, expected);

    // 5) shutdown + exit
    write_jsonrpc_message(&mut stdin, &shutdown_request(4));
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_resolves_extract_field_code_action() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let file_path = temp.path().join("Test.java");

    let source = "class C {\n    void m() {\n        int x = /* 😀 */ 1 + 2;\n    }\n}\n";
    fs::write(&file_path, source).expect("write file");

    let uri: Uri = Url::from_file_path(&file_path)
        .expect("uri")
        .to_string()
        .parse()
        .expect("uri");

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

    write_jsonrpc_message(&mut stdin, &initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(uri.clone(), "java", 1, source),
    );

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            CodeActionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                range,
                context: CodeActionContext {
                    diagnostics: Vec::new(),
                    only: None,
                    trigger_kind: None,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            2,
            "textDocument/codeAction",
        ),
    );

    let code_action_resp = read_response_with_id(&mut stdout, 2);
    let actions = code_action_resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");
    let extract_field = actions
        .iter()
        .find(|action| action.get("title").and_then(|v| v.as_str()) == Some("Extract field"))
        .expect("extract field action");

    assert!(
        extract_field.get("data").is_some(),
        "expected extract field to carry `data`"
    );
    let uri_string = uri.to_string();
    assert_eq!(
        extract_field
            .get("data")
            .and_then(|data| data.get("uri"))
            .and_then(|uri| uri.as_str()),
        Some(uri_string.as_str()),
        "expected extract field `data.uri` to round-trip for codeAction/resolve"
    );
    assert!(
        extract_field.get("edit").is_none(),
        "expected extract field to be unresolved (no `edit`)"
    );

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(extract_field.clone(), 3, "codeAction/resolve"),
    );

    let resolve_resp = read_response_with_id(&mut stdout, 3);
    let resolved: CodeAction =
        serde_json::from_value(resolve_resp.get("result").cloned().expect("result"))
            .expect("decode resolved CodeAction");

    assert_eq!(
        resolved
            .data
            .as_ref()
            .and_then(|data| data.get("uri"))
            .and_then(|uri| uri.as_str()),
        Some(uri_string.as_str()),
        "expected resolved extract field action to retain `data.uri`"
    );

    let edit = resolved.edit.expect("resolved edit");
    let changes = edit.changes.expect("changes");
    let edits = changes.get(&uri).expect("edits for file");
    let updated = apply_lsp_text_edits(source, edits);

    let field_edit = edits
        .iter()
        .find(|e| e.new_text.contains("private final"))
        .expect("field insertion edit");
    let name = field_edit
        .new_text
        .split("private final int ")
        .nth(1)
        .and_then(|rest| rest.split('=').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .expect("field name");

    assert!(
        updated.contains(&format!("private final int {name} = 1 + 2;")),
        "expected extracted field declaration"
    );
    assert!(
        updated.contains(&format!("int x = /* 😀 */ {name};")),
        "expected initializer replaced with field reference"
    );
    assert!(
        !updated.contains("int x = /* 😀 */ 1 + 2;"),
        "expected original expression to be replaced"
    );

    write_jsonrpc_message(&mut stdin, &shutdown_request(4));
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_offers_convert_to_record_code_action() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let file_path = temp.path().join("Point.java");

    let source = "\
public final /* 😀 */ class Point {\n\
    private final int x;\n\
\n\
    public Point(int x) {\n\
        this.x = x;\n\
    }\n\
}\n";
    fs::write(&file_path, source).expect("write file");

    let uri: Uri = Url::from_file_path(&file_path)
        .expect("uri")
        .to_string()
        .parse()
        .expect("uri");

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

    write_jsonrpc_message(&mut stdin, &initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(uri.clone(), "java", 1, source),
    );

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            CodeActionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                range,
                context: CodeActionContext {
                    diagnostics: Vec::new(),
                    only: None,
                    trigger_kind: None,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            2,
            "textDocument/codeAction",
        ),
    );

    let code_action_resp = read_response_with_id(&mut stdout, 2);
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

    write_jsonrpc_message(&mut stdin, &shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_resolves_extract_variable_code_action() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let file_path = temp.path().join("Test.java");

    let source = "class C {\n    void m() {\n        int x = /* 😀 */ 1 + 2;\n        System.out.println(x);\n    }\n}\n";
    fs::write(&file_path, source).expect("write file");

    let uri: Uri = Url::from_file_path(&file_path)
        .expect("uri")
        .to_string()
        .parse()
        .expect("uri");

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
    write_jsonrpc_message(&mut stdin, &initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    // 2) didOpen (so resolution can read the in-memory snapshot)
    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(uri.clone(), "java", 1, source),
    );

    // 3) request code actions for the expression selection
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            CodeActionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                range,
                context: CodeActionContext {
                    diagnostics: Vec::new(),
                    only: None,
                    trigger_kind: None,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            2,
            "textDocument/codeAction",
        ),
    );

    let code_action_resp = read_response_with_id(&mut stdout, 2);
    let actions = code_action_resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");
    let extract_variable = actions
        .iter()
        .find(|action| action.get("title").and_then(|v| v.as_str()) == Some("Extract variable…"))
        .expect("extract variable action");

    assert!(
        extract_variable.get("data").is_some(),
        "expected extract variable to carry `data`"
    );
    let uri_string = uri.to_string();
    assert_eq!(
        extract_variable
            .get("data")
            .and_then(|data| data.get("uri"))
            .and_then(|uri| uri.as_str()),
        Some(uri_string.as_str()),
        "expected extract variable `data.uri` to round-trip for codeAction/resolve"
    );
    assert!(
        extract_variable.get("edit").is_none(),
        "expected extract variable to be unresolved (no `edit`)"
    );

    // 4) resolve
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(extract_variable.clone(), 3, "codeAction/resolve"),
    );

    let resolve_resp = read_response_with_id(&mut stdout, 3);
    let resolved: CodeAction =
        serde_json::from_value(resolve_resp.get("result").cloned().expect("result"))
            .expect("decode resolved CodeAction");

    assert_eq!(
        resolved
            .data
            .as_ref()
            .and_then(|data| data.get("uri"))
            .and_then(|uri| uri.as_str()),
        Some(uri_string.as_str()),
        "expected resolved extract variable action to retain `data.uri`"
    );

    let edit = resolved.edit.expect("resolved edit");
    let changes = edit.changes.expect("changes");
    let edits = changes.get(&uri).expect("edits for file");
    let updated = apply_lsp_text_edits(source, edits);

    let replace_edit = edits
        .iter()
        .find(|e| e.range == range)
        .expect("expression replacement edit");
    let name = replace_edit
        .new_text
        .trim()
        .trim_end_matches(';')
        .trim()
        .to_string();
    assert!(
        !name.is_empty() && !name.chars().any(|ch| ch.is_whitespace()),
        "expected replacement to be a non-empty identifier, got {:?}",
        replace_edit.new_text
    );

    let decl_edit = edits
        .iter()
        .find(|e| {
            e.range.start == e.range.end
                && e.new_text.contains("1 + 2")
                && e.new_text.contains(&name)
                && e.new_text.contains('=')
        })
        .expect("variable declaration insertion edit");

    let (decl_before_eq, _) = decl_edit
        .new_text
        .split_once('=')
        .expect("declaration must include `=`");
    let decl_name = decl_before_eq
        .split_whitespace()
        .last()
        .expect("variable name in declaration");
    assert_eq!(
        decl_name, name,
        "expected declaration name to match replacement edit"
    );
    let decl_line = decl_edit.new_text.trim_end_matches(['\r', '\n']);

    assert!(
        updated.contains(decl_line),
        "expected extracted variable declaration"
    );
    assert!(
        updated.contains(&format!("int x = /* 😀 */ {name};")),
        "expected initializer replaced with extracted variable"
    );
    assert!(
        !updated.contains("int x = /* 😀 */ 1 + 2;"),
        "expected original expression to be replaced"
    );
    assert!(
        updated.find(decl_line).expect("declaration exists")
            < updated
                .find(&format!("int x = /* 😀 */ {name};"))
                .expect("replacement exists"),
        "expected variable declaration to appear before the rewritten statement"
    );

    // 5) shutdown + exit
    write_jsonrpc_message(&mut stdin, &shutdown_request(4));
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_does_not_offer_extract_variable_in_try_with_resources_header() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let file_path = temp.path().join("Test.java");

    // Selecting a pure subexpression inside a try-with-resources resource initializer should not
    // offer Extract Variable, since extracting within the `try (...)` header can change
    // AutoCloseable lifetime/closing behavior.
    let source = "class C {\n    void m() throws Exception {\n        try (java.io.ByteArrayInputStream r = new java.io.ByteArrayInputStream(new byte[1 + 2])) {\n            r.read();\n        }\n    }\n}\n";
    fs::write(&file_path, source).expect("write file");

    let uri: Uri = Url::from_file_path(&file_path)
        .expect("uri")
        .to_string()
        .parse()
        .expect("uri");

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
    write_jsonrpc_message(&mut stdin, &initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    // 2) didOpen
    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(uri.clone(), "java", 1, source),
    );

    // 3) request code actions for the expression selection
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            CodeActionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                range,
                context: CodeActionContext {
                    diagnostics: Vec::new(),
                    only: None,
                    trigger_kind: None,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            2,
            "textDocument/codeAction",
        ),
    );

    let code_action_resp = read_response_with_id(&mut stdout, 2);
    let actions = code_action_resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");
    for title in ["Extract variable…", "Extract variable… (explicit type)"] {
        assert!(
            actions
                .iter()
                .all(|action| action.get("title").and_then(|v| v.as_str()) != Some(title)),
            "expected {title:?} to not be offered inside try-with-resources header; got: {actions:?}"
        );
    }

    // 4) shutdown + exit
    write_jsonrpc_message(&mut stdin, &shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_offers_inline_variable_code_actions() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let file_path = temp.path().join("Test.java");

    let source = "class C {\n    void m() {\n        int a = 1 + 2;\n        System.out.println(a);\n        System.out.println(a);\n    }\n}\n";
    fs::write(&file_path, source).expect("write file");

    let uri: Uri = Url::from_file_path(&file_path)
        .expect("uri")
        .to_string()
        .parse()
        .expect("uri");

    let cursor_offset = source.find("println(a)").expect("println call") + "println(".len();
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

    // 1) initialize
    write_jsonrpc_message(&mut stdin, &initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    // 2) didOpen
    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(uri.clone(), "java", 1, source),
    );

    // 3) request code actions at cursor
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            CodeActionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                range,
                context: CodeActionContext {
                    diagnostics: Vec::new(),
                    only: None,
                    trigger_kind: None,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            2,
            "textDocument/codeAction",
        ),
    );

    let code_action_resp = read_response_with_id(&mut stdout, 2);
    let actions = code_action_resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");

    assert!(
        actions
            .iter()
            .any(|action| action.get("title").and_then(|v| v.as_str()) == Some("Inline variable")),
        "expected Inline variable action"
    );

    let inline_all = actions
        .iter()
        .find(|action| {
            action.get("title").and_then(|v| v.as_str()) == Some("Inline variable (all usages)")
        })
        .expect("inline all usages action");

    let inline_all: CodeAction =
        serde_json::from_value(inline_all.clone()).expect("decode CodeAction");
    let edit = inline_all.edit.expect("edit");
    let changes = edit.changes.expect("changes");
    let edits = changes.get(&uri).expect("edits for file");
    let updated = apply_lsp_text_edits(source, edits);

    let expected = "class C {\n    void m() {\n        System.out.println((1 + 2));\n        System.out.println((1 + 2));\n    }\n}\n";
    assert_eq!(updated, expected);

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
