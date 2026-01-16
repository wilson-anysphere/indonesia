use httpmock::prelude::*;
use lsp_types::{
    ApplyWorkspaceEditResponse, CodeActionContext, CodeActionParams, CompletionParams, Diagnostic,
    ExecuteCommandParams, NumberOrString, PartialResultParams, Position, Range,
    TextDocumentIdentifier, TextDocumentPositionParams, TextEdit, Uri, WorkDoneProgressParams,
    WorkspaceEdit,
};
use pretty_assertions::assert_eq;
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};
use std::str::FromStr;

use nova_core::{path_to_file_uri, AbsPathBuf, LineIndex, TextSize};
use nova_lsp::text_pos::TextPos;
use tempfile::TempDir;

use crate::support::{
    drain_notifications_until_id, read_jsonrpc_message, read_response_with_id,
    write_jsonrpc_message,
};

fn find_apply_edit_request(messages: &[serde_json::Value]) -> serde_json::Value {
    messages
        .iter()
        .find(|msg| msg.get("method").and_then(|m| m.as_str()) == Some("workspace/applyEdit"))
        .cloned()
        .expect("expected workspace/applyEdit request")
}

fn uri_for_path(path: &Path) -> String {
    let abs = AbsPathBuf::try_from(path.to_path_buf()).expect("abs path");
    path_to_file_uri(&abs).expect("file uri")
}

fn apply_lsp_edits(original: &str, edits: &[TextEdit]) -> String {
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
fn stdio_server_hides_cloud_ai_code_edit_actions_when_anonymization_is_enabled() {
    let _lock = crate::support::stdio_server_lock();
    let ai_server = crate::support::TestAiServer::start(serde_json::Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert(
            "completion".to_string(),
            serde_json::Value::String("mock".to_string()),
        );
        resp
    }));

    let temp = TempDir::new().expect("tempdir");

    let config_path = temp.path().join("nova.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.provider]
kind = "http"
url = "{endpoint}"
model = "default"

[ai.privacy]
local_only = false
"#,
            endpoint = format!("{}/complete", ai_server.base_url())
        ),
    )
    .expect("write config");

    let file_path = temp.path().join("Main.java");
    let file_uri = uri_for_path(&file_path);
    let text = "class Test { void foo() { } }";
    std::fs::write(&file_path, text).expect("write Main.java");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        // The test config file should be authoritative; clear any legacy env-var AI wiring that
        // could override `--config` (common in developer shells).
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        // Ensure a developer's environment doesn't disable AI for this test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // initialize
    write_jsonrpc_message(&mut stdin, &crate::support::initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &crate::support::initialized_notification());

    // open a document
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::did_open_notification(file_uri.clone(), "java", 1, text),
    );

    // request code actions on an empty method body selection (would normally offer AI code edits).
    let selection = "void foo() { }";
    let start_offset = text.find(selection).expect("selection start");
    let end_offset = start_offset + selection.len();
    let pos = TextPos::new(text);
    let range = Range {
        start: pos.lsp_position(start_offset).expect("start"),
        end: pos.lsp_position(end_offset).expect("end"),
    };
    let uri: Uri = file_uri.parse().expect("lsp uri");

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            CodeActionParams {
                text_document: TextDocumentIdentifier { uri },
                range,
                context: CodeActionContext {
                    diagnostics: vec![Diagnostic::new_simple(
                        range,
                        "cannot find symbol".to_string(),
                    )],
                    ..CodeActionContext::default()
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            2,
            "textDocument/codeAction",
        ),
    );

    let code_actions_resp = read_response_with_id(&mut stdout, 2);
    let actions = code_actions_resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");

    assert!(
        actions
            .iter()
            .any(|a| a.get("title").and_then(|t| t.as_str()) == Some("Explain this error")),
        "expected explain-error action to remain available"
    );

    assert!(
        !actions.iter().any(|a| {
            a.get("title").and_then(|t| t.as_str()) == Some("Generate method body with AI")
        }),
        "generate-method-body action should be hidden when code edits are disallowed"
    );
    assert!(
        !actions
            .iter()
            .any(|a| a.get("title").and_then(|t| t.as_str()) == Some("Generate tests with AI")),
        "generate-tests action should be hidden when code edits are disallowed"
    );

    // shutdown + exit
    write_jsonrpc_message(&mut stdin, &crate::support::shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &crate::support::exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_generate_method_body_with_ai_applies_workspace_edit() {
    let _lock = crate::support::stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = uri_for_path(root);

    let file_path = root.join("Test.java");
    let file_rel = "Test.java";
    let file_uri = uri_for_path(&file_path);
    let source = "class Test {\n    int add(int a, int b) {\n    }\n}\n";
    std::fs::write(&file_path, source).expect("write Test.java");

    // The patch inserts a return statement inside the empty method body.
    let method_line = "    int add(int a, int b) {";
    let open_brace_offset = source
        .find(method_line)
        .expect("method line")
        .saturating_add(method_line.len().saturating_sub(1));
    let close_brace_offset = source
        .find("\n    }\n")
        .expect("method close")
        .saturating_add("\n    ".len());

    let pos = TextPos::new(source);
    let insert_start = pos
        .lsp_position(open_brace_offset + 1)
        .expect("insert start pos");
    let insert_end = pos
        .lsp_position(close_brace_offset)
        .expect("insert end pos");
    let completion = serde_json::to_string(&serde_json::Value::Object({
        let mut patch = serde_json::Map::new();
        patch.insert(
            "edits".to_string(),
            serde_json::Value::Array(vec![serde_json::Value::Object({
                let mut edit = serde_json::Map::new();
                edit.insert(
                    "file".to_string(),
                    serde_json::Value::String(file_rel.to_string()),
                );
                edit.insert(
                    "range".to_string(),
                    serde_json::to_value(Range::new(insert_start, insert_end))
                        .expect("serialize range"),
                );
                edit.insert(
                    "text".to_string(),
                    serde_json::Value::String("\n        return a + b;\n    ".to_string()),
                );
                edit
            })]),
        );
        patch
    }))
    .expect("patch json");

    let ai_server = crate::support::TestAiServer::start(serde_json::Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert(
            "completion".to_string(),
            serde_json::Value::String(completion),
        );
        resp
    }));

    let config_path = root.join("nova.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.provider]
kind = "http"
url = "{}/complete"
model = "default"

[ai.privacy]
local_only = true
"#,
            ai_server.base_url()
        ),
    )
    .expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        // Ensure env vars don't override the config file.
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::initialize_request_with_root_uri(1, root_uri),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &crate::support::initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::did_open_notification(file_uri.clone(), "java", 1, source),
    );

    // Request code actions over the empty method (must include `{}` so `detect_empty_method_signature` triggers).
    let selection_start = pos
        .lsp_position(source.find(method_line).expect("selection start"))
        .unwrap();
    let selection_end = pos
        .lsp_position(close_brace_offset + 1)
        .expect("selection end pos");
    let text_document_uri: Uri = uri_for_path(&file_path).parse().expect("lsp uri");

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            CodeActionParams {
                text_document: TextDocumentIdentifier {
                    uri: text_document_uri,
                },
                range: Range::new(selection_start, selection_end),
                context: CodeActionContext::default(),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            2,
            "textDocument/codeAction",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let actions = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");
    let action = actions
        .iter()
        .find(|a| a.get("title").and_then(|t| t.as_str()) == Some("Generate method body with AI"))
        .expect("generate method body action");
    let cmd = action
        .pointer("/command/command")
        .and_then(|v| v.as_str())
        .expect("command string");
    assert_eq!(cmd, nova_ide::COMMAND_GENERATE_METHOD_BODY);
    let args = action
        .pointer("/command/arguments")
        .cloned()
        .expect("command arguments");

    // Execute the code action (triggers patch-based codegen).
    let arguments = args
        .as_array()
        .cloned()
        .expect("command.arguments must be an array");

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            ExecuteCommandParams {
                command: cmd.to_string(),
                arguments,
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
            3,
            "workspace/executeCommand",
        ),
    );

    let mut apply_edit = None;
    let exec_resp = loop {
        let msg = read_jsonrpc_message(&mut stdout);
        if msg.get("method").and_then(|v| v.as_str()) == Some("workspace/applyEdit") {
            let id = msg.get("id").cloned().expect("applyEdit id");
            apply_edit = Some(msg.clone());
            write_jsonrpc_message(
                &mut stdin,
                &crate::support::jsonrpc_response_ok(
                    id,
                    ApplyWorkspaceEditResponse {
                        applied: true,
                        failure_reason: None,
                        failed_change: None,
                    },
                ),
            );
            continue;
        }
        if msg.get("id").and_then(|v| v.as_i64()) == Some(3) {
            break msg;
        }
    };

    assert!(
        exec_resp.get("error").is_none(),
        "expected executeCommand success, got: {exec_resp:#?}"
    );

    let apply_edit = apply_edit.expect("server emitted workspace/applyEdit request");
    let edit_value = apply_edit.pointer("/params/edit").cloned().expect("edit");
    let edit: WorkspaceEdit = serde_json::from_value(edit_value).expect("workspace edit");
    let uri: Uri = file_uri.parse().expect("file uri");
    let changes = edit.changes.expect("changes map");
    let edits = changes.get(&uri).expect("edits for uri");
    let updated = apply_lsp_edits(source, edits);
    assert!(
        updated.contains("return a + b;"),
        "expected generated return statement, got:\n{updated}"
    );

    ai_server.assert_hits(1);

    write_jsonrpc_message(&mut stdin, &crate::support::shutdown_request(4));
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &crate::support::exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_ai_generate_method_body_custom_request_rejects_non_empty_method_body() {
    let _lock = crate::support::stdio_server_lock();
    let ai_server = crate::support::TestAiServer::start(serde_json::Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert(
            "completion".to_string(),
            serde_json::Value::String("unused".to_string()),
        );
        resp
    }));

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = uri_for_path(root);

    let file_path = root.join("Test.java");
    let file_uri = uri_for_path(&file_path);
    let source = concat!(
        "class Test {\n",
        "    int add(int a, int b) {\n",
        "        return a + b;\n",
        "    }\n",
        "}\n",
    );
    std::fs::write(&file_path, source).expect("write Test.java");

    let config_path = root.join("nova.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.provider]
kind = "http"
url = "{}/complete"
model = "default"

[ai.privacy]
local_only = true
"#,
            ai_server.base_url()
        ),
    )
    .expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        // Ensure env vars don't override the config file.
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::initialize_request_with_root_uri(1, root_uri),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &crate::support::initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::did_open_notification(file_uri.clone(), "java", 1, source),
    );

    // Select the method snippet including `{}`. The method body is *not* empty, so the request
    // must fail with invalid params and should not call the AI provider.
    let pos = TextPos::new(source);
    let method_start = source
        .find("    int add(int a, int b) {")
        .expect("method start");
    let method_close = source.find("\n    }\n").expect("method close");
    let close_brace_offset = method_close + "\n    ".len();

    let selection_start = pos.lsp_position(method_start).expect("selection start");
    let selection_end = pos
        .lsp_position(close_brace_offset + 1)
        .expect("selection end");
    let range = Range::new(selection_start, selection_end);

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            serde_json::Value::Object({
                let mut params = serde_json::Map::new();
                params.insert(
                    "method_signature".to_string(),
                    serde_json::Value::String("int add(int a, int b)".to_string()),
                );
                params.insert("context".to_string(), serde_json::Value::Null);
                params.insert("uri".to_string(), serde_json::Value::String(file_uri));
                params.insert(
                    "range".to_string(),
                    serde_json::to_value(range).expect("serialize range"),
                );
                params
            }),
            2,
            "nova/ai/generateMethodBody",
        ),
    );

    let mut saw_apply_edit = false;
    let response = loop {
        let msg = read_jsonrpc_message(&mut stdout);
        if msg.get("method").and_then(|v| v.as_str()) == Some("workspace/applyEdit") {
            saw_apply_edit = true;
            let id = msg.get("id").cloned().expect("applyEdit id");
            write_jsonrpc_message(
                &mut stdin,
                &crate::support::jsonrpc_response_ok(
                    id,
                    ApplyWorkspaceEditResponse {
                        applied: true,
                        failure_reason: None,
                        failed_change: None,
                    },
                ),
            );
            continue;
        }

        if msg.get("method").is_none() && msg.get("id").and_then(|v| v.as_i64()) == Some(2) {
            break msg;
        }
    };

    assert!(
        response.get("error").is_some(),
        "expected error response, got: {response:#?}"
    );
    assert!(
        response.get("result").is_none(),
        "expected result to be absent on error, got: {response:#?}"
    );

    let error = response.get("error").expect("error object");
    assert_eq!(
        error.get("code").and_then(|v| v.as_i64()),
        Some(-32602),
        "expected invalid params error, got: {response:#?}"
    );
    assert!(
        error
            .get("message")
            .and_then(|v| v.as_str())
            .is_some_and(|m| m.contains("not empty")),
        "expected error message to mention non-empty method body, got: {response:#?}"
    );
    assert!(
        !saw_apply_edit,
        "expected no workspace/applyEdit request, but saw one"
    );

    ai_server.assert_hits(0);

    write_jsonrpc_message(&mut stdin, &crate::support::shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &crate::support::exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_generate_tests_with_ai_applies_workspace_edit() {
    let _lock = crate::support::stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = uri_for_path(root);

    let file_path = root.join("src/main/java/Test.java");
    let test_rel = "src/test/java/TestTest.java";
    let file_uri = uri_for_path(&file_path);
    let source = concat!(
        "class Test {\n",
        "    int add(int a, int b) {\n",
        "        return a + b;\n",
        "    }\n",
        "\n",
        "    // TESTS_PLACEHOLDER\n",
        "}\n",
    );
    std::fs::create_dir_all(file_path.parent().expect("parent dir")).expect("create src dirs");
    std::fs::write(&file_path, source).expect("write Test.java");

    let placeholder_line = "    // TESTS_PLACEHOLDER";
    let placeholder_start_offset = source.find(placeholder_line).expect("placeholder start");
    let placeholder_end_offset = placeholder_start_offset + placeholder_line.len();

    let pos = TextPos::new(source);
    let selection_start = pos
        .lsp_position(placeholder_start_offset)
        .expect("selection start pos");
    let selection_end = pos
        .lsp_position(placeholder_end_offset)
        .expect("selection end pos");

    let completion = serde_json::to_string(&serde_json::Value::Object({
        let mut patch = serde_json::Map::new();
        patch.insert(
            "edits".to_string(),
            serde_json::Value::Array(vec![serde_json::Value::Object({
                let mut edit = serde_json::Map::new();
                edit.insert(
                    "file".to_string(),
                    serde_json::Value::String(test_rel.to_string()),
                );
                edit.insert(
                    "range".to_string(),
                    serde_json::to_value(Range::new(Position::new(0, 0), Position::new(0, 0)))
                        .expect("serialize range"),
                );
                edit.insert(
                    "text".to_string(),
                    serde_json::Value::String(
                        concat!(
                            "class TestTest {\n",
                            "    void testAdd() {\n",
                            "        // TODO: add assertions\n",
                            "    }\n",
                            "}\n"
                        )
                        .to_string(),
                    ),
                );
                edit
            })]),
        );
        patch
    }))
    .expect("patch json");

    let ai_server = crate::support::TestAiServer::start(serde_json::Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert(
            "completion".to_string(),
            serde_json::Value::String(completion),
        );
        resp
    }));

    let config_path = root.join("nova.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.provider]
kind = "http"
url = "{}/complete"
model = "default"

[ai.privacy]
local_only = true
"#,
            ai_server.base_url()
        ),
    )
    .expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        // Ensure env vars don't override the config file.
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::initialize_request_with_root_uri(1, root_uri),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &crate::support::initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::did_open_notification(file_uri.clone(), "java", 1, source),
    );

    let text_document_uri: Uri = file_uri.parse().expect("lsp uri");

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            CodeActionParams {
                text_document: TextDocumentIdentifier {
                    uri: text_document_uri,
                },
                range: Range::new(selection_start, selection_end),
                context: CodeActionContext::default(),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            2,
            "textDocument/codeAction",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let actions = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");
    let action = actions
        .iter()
        .find(|a| a.get("title").and_then(|t| t.as_str()) == Some("Generate tests with AI"))
        .expect("generate tests action");
    let cmd = action
        .pointer("/command/command")
        .and_then(|v| v.as_str())
        .expect("command string");
    assert_eq!(cmd, nova_ide::COMMAND_GENERATE_TESTS);
    let args = action
        .pointer("/command/arguments")
        .cloned()
        .expect("command arguments");
    let arguments = args
        .as_array()
        .cloned()
        .expect("command.arguments must be an array");

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            ExecuteCommandParams {
                command: cmd.to_string(),
                arguments,
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
            3,
            "workspace/executeCommand",
        ),
    );

    let mut apply_edit = None;
    let exec_resp = loop {
        let msg = read_jsonrpc_message(&mut stdout);
        if msg.get("method").and_then(|v| v.as_str()) == Some("workspace/applyEdit") {
            let id = msg.get("id").cloned().expect("applyEdit id");
            apply_edit = Some(msg.clone());
            write_jsonrpc_message(
                &mut stdin,
                &crate::support::jsonrpc_response_ok(
                    id,
                    ApplyWorkspaceEditResponse {
                        applied: true,
                        failure_reason: None,
                        failed_change: None,
                    },
                ),
            );
            continue;
        }
        if msg.get("id").and_then(|v| v.as_i64()) == Some(3) {
            break msg;
        }
    };

    assert!(
        exec_resp.get("error").is_none(),
        "expected executeCommand success, got: {exec_resp:#?}"
    );

    let apply_edit = apply_edit.expect("server emitted workspace/applyEdit request");
    let edit_value = apply_edit.pointer("/params/edit").cloned().expect("edit");
    let edit: WorkspaceEdit = serde_json::from_value(edit_value).expect("workspace edit");
    let document_changes = edit.document_changes.expect("documentChanges");
    let ops = match document_changes {
        lsp_types::DocumentChanges::Operations(ops) => ops,
        other => panic!("expected documentChanges operations, got {other:?}"),
    };
    let expected_test_uri = uri_for_path(&root.join(test_rel))
        .parse::<Uri>()
        .expect("test uri");
    assert!(
        ops.iter().any(|op| matches!(op, lsp_types::DocumentChangeOperation::Op(lsp_types::ResourceOp::Create(create)) if create.uri == expected_test_uri)),
        "expected CreateFile for test uri, got {ops:?}"
    );
    assert!(
        ops.iter().any(|op| {
            let lsp_types::DocumentChangeOperation::Edit(edit) = op else {
                return false;
            };
            if edit.text_document.uri != expected_test_uri {
                return false;
            }
            edit.edits.iter().any(|edit| match edit {
                lsp_types::OneOf::Left(edit) => edit.new_text.contains("void testAdd()"),
                lsp_types::OneOf::Right(edit) => edit.text_edit.new_text.contains("void testAdd()"),
            })
        }),
        "expected TextDocumentEdit containing testAdd, got {ops:?}"
    );

    ai_server.assert_hits(1);

    write_jsonrpc_message(&mut stdin, &crate::support::shutdown_request(4));
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &crate::support::exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_nova_ai_generate_method_body_request_returns_null_and_applies_workspace_edit() {
    let _lock = crate::support::stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = uri_for_path(root);

    let file_rel = "src/main/java/com/example/Calculator.java";
    let file_path = root.join(file_rel);
    std::fs::create_dir_all(file_path.parent().expect("parent dir"))
        .expect("create src/main/java dirs");
    let file_uri = uri_for_path(&file_path);
    let source = concat!(
        "package com.example;\n",
        "\n",
        "public class Calculator {\n",
        "    public int add(int a, int b) {\n",
        "    }\n",
        "}\n",
    );
    std::fs::write(&file_path, source).expect("write Calculator.java");

    // The patch inserts a return statement inside the empty method body.
    let method_line = "    public int add(int a, int b) {";
    let open_brace_offset = source
        .find(method_line)
        .expect("method line")
        .saturating_add(method_line.len().saturating_sub(1));
    let close_brace_offset = source
        .find("\n    }\n")
        .expect("method close")
        .saturating_add("\n    ".len());

    let pos = TextPos::new(source);
    let insert_start = pos
        .lsp_position(open_brace_offset + 1)
        .expect("insert start pos");
    let insert_end = pos
        .lsp_position(close_brace_offset)
        .expect("insert end pos");
    let completion = serde_json::to_string(&serde_json::Value::Object({
        let mut patch = serde_json::Map::new();
        patch.insert(
            "edits".to_string(),
            serde_json::Value::Array(vec![serde_json::Value::Object({
                let mut edit = serde_json::Map::new();
                edit.insert(
                    "file".to_string(),
                    serde_json::Value::String(file_rel.to_string()),
                );
                edit.insert(
                    "range".to_string(),
                    serde_json::to_value(Range::new(insert_start, insert_end))
                        .expect("serialize range"),
                );
                edit.insert(
                    "text".to_string(),
                    serde_json::Value::String("\n        return a + b;\n    ".to_string()),
                );
                edit
            })]),
        );
        patch
    }))
    .expect("patch json");
    let ai_server = crate::support::TestAiServer::start(serde_json::Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert(
            "completion".to_string(),
            serde_json::Value::String(completion),
        );
        resp
    }));
    let endpoint = format!("{}/complete", ai_server.base_url());

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .current_dir(root)
        // Ensure no ambient config affects this test; we want to exercise the env-var AI provider wiring.
        .env_remove("NOVA_CONFIG")
        .env_remove("NOVA_CONFIG_PATH")
        .env("NOVA_AI_PROVIDER", "http")
        .env("NOVA_AI_ENDPOINT", &endpoint)
        .env("NOVA_AI_MODEL", "default")
        // Patch-based code edits are gated on `local_only` in env-var mode.
        .env("NOVA_AI_LOCAL_ONLY", "1")
        // Keep prompts stable: don't anonymize identifiers in the prompt we send to the mock server.
        .env("NOVA_AI_ANONYMIZE_IDENTIFIERS", "0")
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::initialize_request_with_root_uri(1, root_uri),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &crate::support::initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::did_open_notification(file_uri.clone(), "java", 1, source),
    );

    // Request over the empty method snippet (must include `{}` so the server can compute the insert range).
    let selection_start = pos
        .lsp_position(source.find(method_line).expect("selection start"))
        .expect("selection start pos");
    let selection_end = pos
        .lsp_position(close_brace_offset + 1)
        .expect("selection end pos");

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            serde_json::Value::Object({
                let mut params = serde_json::Map::new();
                params.insert(
                    "workDoneToken".to_string(),
                    serde_json::Value::String("t1".to_string()),
                );
                params.insert(
                    "method_signature".to_string(),
                    serde_json::Value::String("public int add(int a, int b)".to_string()),
                );
                params.insert("context".to_string(), serde_json::Value::Null);
                params.insert("uri".to_string(), serde_json::Value::String(file_uri));
                params.insert(
                    "range".to_string(),
                    serde_json::to_value(Range::new(selection_start, selection_end))
                        .expect("serialize range"),
                );
                params
            }),
            2,
            "nova/ai/generateMethodBody",
        ),
    );

    let mut apply_edit = None;
    let resp = loop {
        let msg = read_jsonrpc_message(&mut stdout);
        if msg.get("method").and_then(|v| v.as_str()) == Some("workspace/applyEdit") {
            let id = msg.get("id").cloned().expect("applyEdit id");
            apply_edit = Some(msg.clone());
            write_jsonrpc_message(
                &mut stdin,
                &crate::support::jsonrpc_response_ok(
                    id,
                    ApplyWorkspaceEditResponse {
                        applied: true,
                        failure_reason: None,
                        failed_change: None,
                    },
                ),
            );
            continue;
        }
        if msg.get("id").and_then(|v| v.as_i64()) == Some(2) {
            break msg;
        }
    };

    assert!(
        resp.get("error").is_none(),
        "expected nova/ai/generateMethodBody success, got: {resp:#?}"
    );
    assert_eq!(
        resp.get("result"),
        Some(&serde_json::Value::Null),
        "expected nova/ai/generateMethodBody result to be JSON null"
    );

    let apply_edit = apply_edit.expect("server emitted workspace/applyEdit request");
    assert_eq!(
        apply_edit.pointer("/params/label").and_then(|v| v.as_str()),
        Some("AI: Generate method body"),
        "unexpected workspace/applyEdit label: {apply_edit:#?}"
    );

    let edit_value = apply_edit.pointer("/params/edit").cloned().expect("edit");
    let edit: WorkspaceEdit = serde_json::from_value(edit_value).expect("workspace edit");
    let uri = Uri::from_str(&uri_for_path(&file_path)).expect("uri");
    let changes = edit.changes.expect("changes map");
    let edits = changes.get(&uri).expect("edits for uri");
    let updated = apply_lsp_edits(source, edits);
    assert!(
        updated.contains("return a + b;"),
        "expected generated return statement, got:\n{updated}"
    );

    ai_server.assert_hits(1);

    write_jsonrpc_message(&mut stdin, &crate::support::shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &crate::support::exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_nova_ai_generate_tests_request_returns_null_and_applies_workspace_edit() {
    let _lock = crate::support::stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = uri_for_path(root);

    let file_rel = "src/main/java/com/example/Calculator.java";
    let file_path = root.join(file_rel);
    std::fs::create_dir_all(file_path.parent().expect("parent dir"))
        .expect("create src/main/java dirs");
    let file_uri = uri_for_path(&file_path);
    let source = concat!(
        "package com.example;\n",
        "\n",
        "public class Calculator {\n",
        "    public int add(int a, int b) {\n",
        "        return a + b;\n",
        "    }\n",
        "}\n",
    );
    std::fs::write(&file_path, source).expect("write Calculator.java");

    let test_rel = "src/test/java/com/example/CalculatorTest.java";
    let completion = serde_json::to_string(&serde_json::Value::Object({
        let mut patch = serde_json::Map::new();
        patch.insert(
            "edits".to_string(),
            serde_json::Value::Array(vec![serde_json::Value::Object({
                let mut edit = serde_json::Map::new();
                edit.insert(
                    "file".to_string(),
                    serde_json::Value::String(test_rel.to_string()),
                );
                edit.insert(
                    "range".to_string(),
                    serde_json::to_value(Range::new(Position::new(0, 0), Position::new(0, 0)))
                        .expect("serialize range"),
                );
                edit.insert(
                    "text".to_string(),
                    serde_json::Value::String(
                        concat!(
                            "package com.example;\n",
                            "\n",
                            "class CalculatorTest {\n",
                            "    void testAdd() {\n",
                            "        // TODO: add assertions\n",
                            "    }\n",
                            "}\n"
                        )
                        .to_string(),
                    ),
                );
                edit
            })]),
        );
        patch
    }))
    .expect("patch json");

    let ai_server = crate::support::TestAiServer::start(serde_json::Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert(
            "completion".to_string(),
            serde_json::Value::String(completion),
        );
        resp
    }));
    let endpoint = format!("{}/complete", ai_server.base_url());

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .current_dir(root)
        .env_remove("NOVA_CONFIG")
        .env_remove("NOVA_CONFIG_PATH")
        .env("NOVA_AI_PROVIDER", "http")
        .env("NOVA_AI_ENDPOINT", &endpoint)
        .env("NOVA_AI_MODEL", "default")
        .env("NOVA_AI_LOCAL_ONLY", "1")
        .env("NOVA_AI_ANONYMIZE_IDENTIFIERS", "0")
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::initialize_request_with_root_uri(1, root_uri),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &crate::support::initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::did_open_notification(&file_uri, "java", 1, source),
    );

    // Select the method snippet so GenerateTestsArgs.target is meaningful.
    let method_line = "    public int add(int a, int b) {";
    let method_start_offset = source.find(method_line).expect("method start");
    let method_end_offset = source
        .find("    }\n")
        .expect("method end")
        .saturating_add("    }\n".len());
    let pos = TextPos::new(source);
    let selection_start = pos
        .lsp_position(method_start_offset)
        .expect("selection start");
    let selection_end = pos.lsp_position(method_end_offset).expect("selection end");

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            serde_json::Value::Object({
                let mut params = serde_json::Map::new();
                params.insert(
                    "workDoneToken".to_string(),
                    serde_json::Value::String("t1".to_string()),
                );
                params.insert(
                    "target".to_string(),
                    serde_json::Value::String("public int add(int a, int b)".to_string()),
                );
                params.insert("context".to_string(), serde_json::Value::Null);
                params.insert("uri".to_string(), serde_json::Value::String(file_uri));
                params.insert(
                    "range".to_string(),
                    serde_json::to_value(Range::new(selection_start, selection_end))
                        .expect("serialize range"),
                );
                params
            }),
            2,
            "nova/ai/generateTests",
        ),
    );

    let mut apply_edit = None;
    let resp = loop {
        let msg = read_jsonrpc_message(&mut stdout);
        if msg.get("method").and_then(|v| v.as_str()) == Some("workspace/applyEdit") {
            let id = msg.get("id").cloned().expect("applyEdit id");
            apply_edit = Some(msg.clone());
            write_jsonrpc_message(
                &mut stdin,
                &crate::support::jsonrpc_response_ok(
                    id,
                    ApplyWorkspaceEditResponse {
                        applied: true,
                        failure_reason: None,
                        failed_change: None,
                    },
                ),
            );
            continue;
        }
        if msg.get("id").and_then(|v| v.as_i64()) == Some(2) {
            break msg;
        }
    };

    assert!(
        resp.get("error").is_none(),
        "expected nova/ai/generateTests success, got: {resp:#?}"
    );
    assert_eq!(
        resp.get("result"),
        Some(&serde_json::Value::Null),
        "expected nova/ai/generateTests result to be JSON null"
    );

    let apply_edit = apply_edit.expect("server emitted workspace/applyEdit request");
    assert_eq!(
        apply_edit.pointer("/params/label").and_then(|v| v.as_str()),
        Some("AI: Generate tests"),
        "unexpected workspace/applyEdit label: {apply_edit:#?}"
    );

    let edit_value = apply_edit.pointer("/params/edit").cloned().expect("edit");
    let edit: WorkspaceEdit = serde_json::from_value(edit_value).expect("workspace edit");
    let document_changes = edit.document_changes.expect("documentChanges");
    let ops = match document_changes {
        lsp_types::DocumentChanges::Operations(ops) => ops,
        other => panic!("expected documentChanges operations, got {other:?}"),
    };
    let expected_test_uri = uri_for_path(&root.join(test_rel))
        .parse::<Uri>()
        .expect("test uri");
    assert!(
        ops.iter().any(|op| matches!(op, lsp_types::DocumentChangeOperation::Op(lsp_types::ResourceOp::Create(create)) if create.uri == expected_test_uri)),
        "expected CreateFile for test uri, got {ops:?}"
    );
    assert!(
        ops.iter().any(|op| {
            let lsp_types::DocumentChangeOperation::Edit(edit) = op else {
                return false;
            };
            if edit.text_document.uri != expected_test_uri {
                return false;
            }
            edit.edits.iter().any(|edit| match edit {
                lsp_types::OneOf::Left(edit) => edit.new_text.contains("class CalculatorTest"),
                lsp_types::OneOf::Right(edit) => {
                    edit.text_edit.new_text.contains("class CalculatorTest")
                }
            })
        }),
        "expected TextDocumentEdit containing generated test class, got {ops:?}"
    );

    ai_server.assert_hits(1);

    write_jsonrpc_message(&mut stdin, &crate::support::shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &crate::support::exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_generate_tests_prompt_includes_target_and_source_when_editing_derived_test_file() {
    let _lock = crate::support::stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = uri_for_path(root);

    let src_main_dir = root.join("src/main/java/com/example");
    std::fs::create_dir_all(&src_main_dir).expect("create src/main/java");
    let src_test_dir = root.join("src/test/java/com/example");
    std::fs::create_dir_all(&src_test_dir).expect("create src/test/java");

    let source_path = src_main_dir.join("Calculator.java");
    let source_uri = uri_for_path(&source_path);
    let source = concat!(
        "package com.example;\n",
        "\n",
        "class Calculator {\n",
        "    int add(int a, int b) {\n",
        "        return a + b;\n",
        "    }\n",
        "}\n",
    );
    std::fs::write(&source_path, source).expect("write Calculator.java");

    // Ensure the derived test file exists but is empty so the test-generation prompt would be
    // otherwise unhelpful without the source-target context.
    let test_path = src_test_dir.join("CalculatorTest.java");
    std::fs::write(&test_path, "").expect("write empty CalculatorTest.java");

    let expected_target = "int add(int a, int b) {";
    let completion = serde_json::to_string(&serde_json::Value::Object({
        let mut patch = serde_json::Map::new();
        patch.insert(
            "edits".to_string(),
            serde_json::Value::Array(vec![serde_json::Value::Object({
                let mut edit = serde_json::Map::new();
                edit.insert(
                    "file".to_string(),
                    serde_json::Value::String(
                        "src/test/java/com/example/CalculatorTest.java".to_string(),
                    ),
                );
                edit.insert(
                    "range".to_string(),
                    serde_json::to_value(Range::new(Position::new(0, 0), Position::new(0, 0)))
                        .expect("serialize range"),
                );
                edit.insert(
                    "text".to_string(),
                    serde_json::Value::String(
                        "package com.example;\n\nclass CalculatorTest {}\n".to_string(),
                    ),
                );
                edit
            })]),
        );
        patch
    }))
    .expect("patch json");
    let completion_response = serde_json::Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert(
            "completion".to_string(),
            serde_json::Value::String(completion),
        );
        resp
    });

    let mock_server = MockServer::start();
    let mock = mock_server.mock(|when, then| {
        when.method(POST)
            .path("/complete")
            .body_contains("File: src/test/java/com/example/CalculatorTest.java")
            .body_contains("Test target:")
            .body_contains(expected_target)
            .body_contains("Selected source snippet:")
            .body_contains("return a + b;");
        then.status(200).json_body(completion_response.clone());
    });

    let config_path = root.join("nova.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.provider]
kind = "http"
url = "{}/complete"
model = "default"

[ai.privacy]
local_only = true
"#,
            mock_server.base_url()
        ),
    )
    .expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::initialize_request_with_root_uri(1, root_uri),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &crate::support::initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::did_open_notification(source_uri.clone(), "java", 1, source),
    );

    // Select the method body so GenerateTestsArgs.target and the source snippet are meaningful.
    let method_start_offset = source
        .find("    int add(int a, int b) {")
        .expect("method start");
    let method_end_offset = source
        .find("    }\n")
        .expect("method end")
        .saturating_add("    }\n".len());
    let pos = TextPos::new(source);
    let selection_start = pos
        .lsp_position(method_start_offset)
        .expect("selection start");
    let selection_end = pos.lsp_position(method_end_offset).expect("selection end");
    let source_uri_lsp: Uri = source_uri.parse().expect("lsp uri");

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            CodeActionParams {
                text_document: TextDocumentIdentifier {
                    uri: source_uri_lsp,
                },
                range: Range::new(selection_start, selection_end),
                context: CodeActionContext::default(),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            2,
            "textDocument/codeAction",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let actions = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");
    let action = actions
        .iter()
        .find(|a| a.get("title").and_then(|t| t.as_str()) == Some("Generate tests with AI"))
        .expect("generate tests action");
    let cmd = action
        .pointer("/command/command")
        .and_then(|v| v.as_str())
        .expect("command string");
    assert_eq!(cmd, nova_ide::COMMAND_GENERATE_TESTS);
    let args = action
        .pointer("/command/arguments")
        .cloned()
        .expect("command arguments");
    let arguments = args
        .as_array()
        .cloned()
        .expect("command.arguments must be an array");

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            ExecuteCommandParams {
                command: cmd.to_string(),
                arguments,
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
            3,
            "workspace/executeCommand",
        ),
    );

    let mut apply_edit = None;
    let exec_resp = loop {
        let msg = read_jsonrpc_message(&mut stdout);
        if msg.get("method").and_then(|v| v.as_str()) == Some("workspace/applyEdit") {
            let id = msg.get("id").cloned().expect("applyEdit id");
            apply_edit = Some(msg.clone());
            write_jsonrpc_message(
                &mut stdin,
                &crate::support::jsonrpc_response_ok(
                    id,
                    ApplyWorkspaceEditResponse {
                        applied: true,
                        failure_reason: None,
                        failed_change: None,
                    },
                ),
            );
            continue;
        }
        if msg.get("id").and_then(|v| v.as_i64()) == Some(3) {
            break msg;
        }
    };

    assert!(
        exec_resp.get("error").is_none(),
        "expected executeCommand success, got: {exec_resp:#?}"
    );

    let apply_edit = apply_edit.expect("server emitted workspace/applyEdit request");
    let edit_value = apply_edit.pointer("/params/edit").cloned().expect("edit");
    let edit: WorkspaceEdit = serde_json::from_value(edit_value).expect("workspace edit");

    let test_uri = Uri::from_str(&uri_for_path(&test_path)).expect("test uri");
    let mut touched = false;
    if let Some(changes) = &edit.changes {
        touched |= changes.contains_key(&test_uri);
    }
    if let Some(doc_changes) = &edit.document_changes {
        match doc_changes {
            lsp_types::DocumentChanges::Edits(edits) => {
                touched |= edits.iter().any(|edit| edit.text_document.uri == test_uri);
            }
            lsp_types::DocumentChanges::Operations(ops) => {
                let has_create = ops.iter().any(|op| matches!(
                    op,
                    lsp_types::DocumentChangeOperation::Op(lsp_types::ResourceOp::Create(create))
                        if create.uri == test_uri
                ));
                let has_edit = ops.iter().any(|op| matches!(
                    op,
                    lsp_types::DocumentChangeOperation::Edit(edit) if edit.text_document.uri == test_uri
                ));
                touched |= has_create || has_edit;
                assert!(
                    has_create,
                    "expected CreateFile for derived test uri, got {ops:?}"
                );
                assert!(
                    has_edit,
                    "expected TextDocumentEdit for derived test uri, got {ops:?}"
                );
            }
        }
    }
    assert!(
        touched,
        "expected edit to touch derived test file, got: {edit:#?}"
    );

    mock.assert_hits(1);

    write_jsonrpc_message(&mut stdin, &crate::support::shutdown_request(4));
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &crate::support::exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_ai_excluded_paths_blocks_patch_based_code_edits_without_model_call() {
    let _lock = crate::support::stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = uri_for_path(root);

    let secret_dir = root.join("secret");
    std::fs::create_dir_all(&secret_dir).expect("create secret dir");
    let file_path = secret_dir.join("Example.java");
    let file_uri = uri_for_path(&file_path);
    let source = "class Example {\n    int add(int a, int b) {\n    }\n}\n";
    std::fs::write(&file_path, source).expect("write Example.java");

    let ai_server = crate::support::TestAiServer::start(serde_json::Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert(
            "completion".to_string(),
            serde_json::Value::String("// should not be used\n".to_string()),
        );
        resp
    }));

    let config_path = root.join("nova.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.provider]
kind = "http"
url = "{}/complete"
model = "default"

[ai.privacy]
local_only = true
excluded_paths = ["secret/**"]
"#,
            ai_server.base_url()
        ),
    )
    .expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::initialize_request_with_root_uri(1, root_uri),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &crate::support::initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::did_open_notification(file_uri.clone(), "java", 1, source),
    );

    // Execute the command directly (AI code actions may be suppressed, but executeCommand should still fail closed).
    let method_line = "    int add(int a, int b) {";
    let start_offset = source.find(method_line).expect("method line");
    let end_offset = source
        .find("\n    }\n")
        .expect("method close")
        .saturating_add("\n    ".len().saturating_add(1));
    let pos = TextPos::new(source);
    let range_start = pos.lsp_position(start_offset).expect("range start");
    let range_end = pos.lsp_position(end_offset).expect("range end");

    let arguments = vec![serde_json::Value::Object({
        let mut args = serde_json::Map::new();
        args.insert(
            "method_signature".to_string(),
            serde_json::Value::String("int add(int a, int b)".to_string()),
        );
        args.insert("context".to_string(), serde_json::Value::Null);
        args.insert("uri".to_string(), serde_json::Value::String(file_uri));
        args.insert(
            "range".to_string(),
            serde_json::to_value(Range::new(range_start, range_end)).expect("serialize range"),
        );
        args
    })];

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            ExecuteCommandParams {
                command: nova_ide::COMMAND_GENERATE_METHOD_BODY.to_string(),
                arguments,
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
            2,
            "workspace/executeCommand",
        ),
    );

    let resp = loop {
        let msg = read_jsonrpc_message(&mut stdout);
        if msg.get("method").and_then(|v| v.as_str()) == Some("workspace/applyEdit") {
            let id = msg.get("id").cloned().expect("applyEdit id");
            write_jsonrpc_message(
                &mut stdin,
                &crate::support::jsonrpc_response_ok(
                    id,
                    ApplyWorkspaceEditResponse {
                        applied: false,
                        failure_reason: None,
                        failed_change: None,
                    },
                ),
            );
            continue;
        }
        if msg.get("id").and_then(|v| v.as_i64()) == Some(2) {
            break msg;
        }
    };

    assert!(
        resp.get("error").is_some(),
        "expected executeCommand error, got: {resp:#?}"
    );
    assert_eq!(
        ai_server.hits(),
        0,
        "expected excluded_paths to prevent model calls"
    );

    write_jsonrpc_message(&mut stdin, &crate::support::shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &crate::support::exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_ai_excluded_paths_blocks_patch_based_generate_tests_without_model_call() {
    let _lock = crate::support::stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = uri_for_path(root);

    let secret_dir = root.join("secret");
    std::fs::create_dir_all(&secret_dir).expect("create secret dir");
    let file_path = secret_dir.join("Example.java");
    let file_uri = uri_for_path(&file_path);
    let source = "class Example {\n    int add(int a, int b) {\n        return a + b;\n    }\n}\n";
    std::fs::write(&file_path, source).expect("write Example.java");

    let ai_server = crate::support::TestAiServer::start(serde_json::Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert(
            "completion".to_string(),
            serde_json::Value::String("// should not be used\n".to_string()),
        );
        resp
    }));

    let config_path = root.join("nova.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.provider]
kind = "http"
url = "{}/complete"
model = "default"

[ai.privacy]
local_only = true
excluded_paths = ["secret/**"]
"#,
            ai_server.base_url()
        ),
    )
    .expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::initialize_request_with_root_uri(1, root_uri),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &crate::support::initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::did_open_notification(file_uri.clone(), "java", 1, source),
    );

    // Execute the command directly (AI code actions are suppressed, but executeCommand should still
    // fail closed and must not call the model).
    let selection = "add";
    let start_offset = source.find(selection).expect("selection start");
    let end_offset = start_offset + selection.len();
    let pos = TextPos::new(source);
    let range_start = pos.lsp_position(start_offset).expect("range start");
    let range_end = pos.lsp_position(end_offset).expect("range end");

    let arguments = vec![serde_json::Value::Object({
        let mut args = serde_json::Map::new();
        args.insert(
            "target".to_string(),
            serde_json::Value::String("add".to_string()),
        );
        args.insert("context".to_string(), serde_json::Value::Null);
        args.insert("uri".to_string(), serde_json::Value::String(file_uri));
        args.insert(
            "range".to_string(),
            serde_json::to_value(Range::new(range_start, range_end)).expect("serialize range"),
        );
        args
    })];

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            ExecuteCommandParams {
                command: nova_ide::COMMAND_GENERATE_TESTS.to_string(),
                arguments,
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
            2,
            "workspace/executeCommand",
        ),
    );

    let resp = loop {
        let msg = read_jsonrpc_message(&mut stdout);
        if msg.get("method").and_then(|v| v.as_str()) == Some("workspace/applyEdit") {
            let id = msg.get("id").cloned().expect("applyEdit id");
            write_jsonrpc_message(
                &mut stdin,
                &crate::support::jsonrpc_response_ok(
                    id,
                    ApplyWorkspaceEditResponse {
                        applied: false,
                        failure_reason: None,
                        failed_change: None,
                    },
                ),
            );
            continue;
        }
        if msg.get("id").and_then(|v| v.as_i64()) == Some(2) {
            break msg;
        }
    };

    assert!(
        resp.get("error").is_some(),
        "expected executeCommand error, got: {resp:#?}"
    );
    assert_eq!(
        ai_server.hits(),
        0,
        "expected excluded_paths to prevent model calls"
    );

    write_jsonrpc_message(&mut stdin, &crate::support::shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &crate::support::exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_hides_ai_code_actions_for_excluded_paths() {
    let _lock = crate::support::stdio_server_lock();
    let ai_server = crate::support::TestAiServer::start(serde_json::Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert(
            "completion".to_string(),
            serde_json::Value::String("mock".to_string()),
        );
        resp
    }));

    let temp = TempDir::new().expect("tempdir");

    let config_path = temp.path().join("nova.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.provider]
kind = "http"
url = "{endpoint}"
model = "default"

[ai.privacy]
local_only = true
excluded_paths = ["secret/**"]
"#,
            endpoint = format!("{}/complete", ai_server.base_url())
        ),
    )
    .expect("write config");

    let secret_dir = temp.path().join("secret");
    std::fs::create_dir_all(&secret_dir).expect("create secret dir");
    let file_path = secret_dir.join("Main.java");
    let file_uri = uri_for_path(&file_path);
    let text = "class Test { void foo() { } }";
    std::fs::write(&file_path, text).expect("write secret/Main.java");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        // The test config file should be authoritative; clear any legacy env-var AI wiring that
        // could override `--config` (common in developer shells).
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        // Ensure a developer's environment doesn't disable AI for this test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // initialize
    write_jsonrpc_message(&mut stdin, &crate::support::initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &crate::support::initialized_notification());

    // open a document
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::did_open_notification(file_uri.clone(), "java", 1, text),
    );

    // request code actions on an empty method body selection (would normally offer AI code edits).
    let selection = "void foo() { }";
    let start_offset = text.find(selection).expect("selection start");
    let end_offset = start_offset + selection.len();
    let pos = TextPos::new(text);
    let range = Range {
        start: pos.lsp_position(start_offset).expect("start"),
        end: pos.lsp_position(end_offset).expect("end"),
    };
    let uri: Uri = file_uri.parse().expect("lsp uri");

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            CodeActionParams {
                text_document: TextDocumentIdentifier { uri },
                range,
                context: CodeActionContext {
                    diagnostics: vec![Diagnostic::new_simple(
                        range,
                        "cannot find symbol".to_string(),
                    )],
                    ..CodeActionContext::default()
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            2,
            "textDocument/codeAction",
        ),
    );

    let code_actions_resp = read_response_with_id(&mut stdout, 2);
    let actions = code_actions_resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");

    // Explain-only actions remain available for excluded paths, but must not include any
    // file-backed context (code snippet).
    let explain = actions
        .iter()
        .find(|a| {
            a.get("command")
                .and_then(|c| c.get("command"))
                .and_then(|v| v.as_str())
                == Some(nova_ide::COMMAND_EXPLAIN_ERROR)
        })
        .expect("expected explain-error action to remain available");
    // Ensure we don't include a code snippet for excluded files.
    let explain_args = explain
        .get("command")
        .and_then(|c| c.get("arguments"))
        .and_then(|v| v.as_array())
        .and_then(|v| v.first())
        .and_then(|v| v.as_object())
        .expect("ExplainErrorArgs payload");
    assert!(
        explain_args.get("code").is_none() || explain_args.get("code").is_some_and(|v| v.is_null()),
        "expected explainError args.code to be omitted/null for excluded paths, got: {explain_args:?}"
    );

    let code_edit_commands = [
        nova_ide::COMMAND_GENERATE_METHOD_BODY,
        nova_ide::COMMAND_GENERATE_TESTS,
    ];
    for cmd in code_edit_commands {
        assert!(
            actions.iter().all(|a| {
                a.get("command")
                    .and_then(|c| c.get("command"))
                    .and_then(|v| v.as_str())
                    != Some(cmd)
            }),
            "expected AI code edit action {cmd:?} to be suppressed, got: {actions:?}"
        );
    }

    // shutdown + exit
    write_jsonrpc_message(&mut stdin, &crate::support::shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &crate::support::exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_handles_ai_explain_error_code_action() {
    let _lock = crate::support::stdio_server_lock();
    let ai_server = crate::support::TestAiServer::start(serde_json::Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert(
            "completion".to_string(),
            serde_json::Value::String("mock explanation".to_string()),
        );
        resp
    }));

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        // Ensure a developer's environment doesn't disable AI for this test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        // Enable code-edit actions for this test (cloud-mode policy would otherwise hide them for
        // the `http` provider).
        .env("NOVA_AI_LOCAL_ONLY", "1")
        .env("NOVA_AI_PROVIDER", "http")
        .env(
            "NOVA_AI_ENDPOINT",
            format!("{}/complete", ai_server.base_url()),
        )
        .env("NOVA_AI_MODEL", "default")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // 1) initialize
    write_jsonrpc_message(&mut stdin, &crate::support::initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &crate::support::initialized_notification());

    // 2) open a document so the server can attach code snippets.
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::did_open_notification(
            "file:///Test.java",
            "java",
            1,
            "class Test { void run() { unknown(); } }",
        ),
    );

    // 3) request code actions with a diagnostic.
    let range = Range::new(
        lsp_types::Position::new(0, 0),
        lsp_types::Position::new(0, 10),
    );
    let uri: Uri = "file:///Test.java".parse().expect("lsp uri");

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            CodeActionParams {
                text_document: TextDocumentIdentifier { uri },
                range,
                context: CodeActionContext {
                    diagnostics: vec![Diagnostic::new_simple(
                        range,
                        "cannot find symbol".to_string(),
                    )],
                    ..CodeActionContext::default()
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            2,
            "textDocument/codeAction",
        ),
    );

    let code_actions_resp = read_response_with_id(&mut stdout, 2);
    let actions = code_actions_resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");

    let explain = actions
        .iter()
        .find(|a| a.get("title").and_then(|t| t.as_str()) == Some("Explain this error"))
        .expect("explain code action");

    let cmd = explain
        .get("command")
        .expect("command")
        .get("command")
        .and_then(|v| v.as_str())
        .expect("command string");

    let args = explain
        .get("command")
        .expect("command")
        .get("arguments")
        .cloned()
        .expect("arguments");

    assert_eq!(cmd, nova_ide::COMMAND_EXPLAIN_ERROR);

    // 4) Execute the command (this triggers the mock LLM call).
    let progress_token = serde_json::Value::String("progress-token".to_string());
    let arguments = args
        .as_array()
        .cloned()
        .expect("command.arguments must be an array");
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            ExecuteCommandParams {
                command: cmd.to_string(),
                arguments,
                work_done_progress_params: WorkDoneProgressParams {
                    work_done_token: Some(lsp_types::NumberOrString::String(
                        "progress-token".to_string(),
                    )),
                },
            },
            3,
            "workspace/executeCommand",
        ),
    );

    // Collect work-done progress notifications emitted during the AI request.
    let mut progress_kinds = Vec::new();
    let exec_resp = loop {
        let msg = read_jsonrpc_message(&mut stdout);
        if msg.get("method").and_then(|v| v.as_str()) == Some("$/progress") {
            if msg.get("params").and_then(|p| p.get("token")) == Some(&progress_token) {
                if let Some(kind) = msg
                    .get("params")
                    .and_then(|p| p.get("value"))
                    .and_then(|v| v.get("kind"))
                    .and_then(|v| v.as_str())
                {
                    progress_kinds.push(kind.to_string());
                }
            }
            continue;
        }

        if msg.get("id").and_then(|v| v.as_i64()) == Some(3) {
            break msg;
        }
        // Notification or unrelated response; ignore.
    };
    assert_eq!(
        exec_resp.get("result"),
        Some(&serde_json::Value::String("mock explanation".to_string())),
    );
    assert!(progress_kinds.contains(&"begin".to_string()));
    assert!(progress_kinds.contains(&"end".to_string()));

    ai_server.assert_hits(1);

    // 5) shutdown + exit
    write_jsonrpc_message(&mut stdin, &crate::support::shutdown_request(4));
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);

    write_jsonrpc_message(&mut stdin, &crate::support::exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_ai_prompt_includes_project_and_semantic_context_when_root_is_available() {
    let _lock = crate::support::stdio_server_lock();
    let mock_server = MockServer::start();
    let mock = mock_server.mock(|when, then| {
        when.method(POST)
            .path("/complete")
            .body_contains("## Project context")
            .body_contains("## Symbol/type info");
        then.status(200).json_body(serde_json::Value::Object({
            let mut resp = serde_json::Map::new();
            resp.insert(
                "completion".to_string(),
                serde_json::Value::String("mock explanation".to_string()),
            );
            resp
        }));
    });

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = uri_for_path(root);

    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");

    let file_path = src_dir.join("Main.java");
    let file_uri = uri_for_path(&file_path);
    let text = r#"class Main { void run() { String s = "hi"; } }"#;
    std::fs::write(&file_path, text).expect("write Main.java");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        // Ensure a developer's environment doesn't disable AI for this test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env("NOVA_AI_PROVIDER", "http")
        .env(
            "NOVA_AI_ENDPOINT",
            format!("{}/complete", mock_server.base_url()),
        )
        .env("NOVA_AI_MODEL", "default")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // 1) initialize with a workspace root so project context can be loaded.
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::initialize_request_with_root_uri(1, root_uri),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &crate::support::initialized_notification());

    // 2) open an on-disk document so hover/type info has a stable path.
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::did_open_notification(file_uri.clone(), "java", 1, text),
    );

    // 3) request code actions with a diagnostic range over an identifier so hover returns info.
    let offset = text.find("s =").expect("variable occurrence");
    let index = LineIndex::new(text);
    let start = index.position(
        text,
        TextSize::from(u32::try_from(offset).expect("offset fits in u32")),
    );
    let end = index.position(
        text,
        TextSize::from(u32::try_from(offset + 1).expect("offset fits in u32")),
    );
    let range = Range {
        start: Position::new(start.line, start.character),
        end: Position::new(end.line, end.character),
    };
    let text_document_uri: Uri = file_uri.parse().expect("lsp uri");

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            CodeActionParams {
                text_document: TextDocumentIdentifier {
                    uri: text_document_uri,
                },
                range,
                context: CodeActionContext {
                    diagnostics: vec![Diagnostic::new_simple(
                        range,
                        "cannot find symbol".to_string(),
                    )],
                    ..CodeActionContext::default()
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            2,
            "textDocument/codeAction",
        ),
    );

    let code_actions_resp = read_response_with_id(&mut stdout, 2);
    let actions = code_actions_resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");

    let explain = actions
        .iter()
        .find(|a| a.get("title").and_then(|t| t.as_str()) == Some("Explain this error"))
        .expect("explain code action");

    let cmd = explain
        .get("command")
        .expect("command")
        .get("command")
        .and_then(|v| v.as_str())
        .expect("command string");

    let args = explain
        .get("command")
        .expect("command")
        .get("arguments")
        .cloned()
        .expect("arguments");

    assert_eq!(cmd, nova_ide::COMMAND_EXPLAIN_ERROR);

    // 4) Execute the command (this triggers the mock LLM call, which asserts on prompt contents).
    let arguments = args
        .as_array()
        .cloned()
        .expect("command.arguments must be an array");
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            ExecuteCommandParams {
                command: cmd.to_string(),
                arguments,
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
            3,
            "workspace/executeCommand",
        ),
    );
    let exec_resp = read_response_with_id(&mut stdout, 3);
    assert_eq!(
        exec_resp.get("result"),
        Some(&serde_json::Value::String("mock explanation".to_string())),
    );
    mock.assert_hits(1);

    // 5) shutdown + exit
    write_jsonrpc_message(&mut stdin, &crate::support::shutdown_request(4));
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);

    write_jsonrpc_message(&mut stdin, &crate::support::exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_ai_generate_method_body_sends_apply_edit() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let src_dir = root.join("src/main/java/com/example");
    std::fs::create_dir_all(&src_dir).expect("create src dir");
    let file_path = src_dir.join("Example.java");
    let file_uri = uri_for_path(&file_path);

    let text = "package com.example;\n\npublic class Example {\n    public int answer() { }\n}\n";
    std::fs::write(&file_path, text).expect("write file");

    let selection = "public int answer() { }";
    let selection_start = text.find(selection).expect("selection present");
    let selection_end = selection_start + selection.len();
    let pos = TextPos::new(text);
    let range = Range::new(
        pos.lsp_position(selection_start).expect("start pos"),
        pos.lsp_position(selection_end).expect("end pos"),
    );

    // Build a deterministic patch that inserts `return 42;` inside the braces.
    let selected = &text[selection_start..selection_end];
    let open = selected.find('{').expect("open brace");
    let close = selected.rfind('}').expect("close brace");
    let insert_start = selection_start + open + 1;
    let insert_end = selection_start + close;
    let insert_range = Range::new(
        pos.lsp_position(insert_start).expect("insert start"),
        pos.lsp_position(insert_end).expect("insert end"),
    );
    let patch = serde_json::to_string(&serde_json::Value::Object({
        let mut patch = serde_json::Map::new();
        patch.insert(
            "edits".to_string(),
            serde_json::Value::Array(vec![serde_json::Value::Object({
                let mut edit = serde_json::Map::new();
                edit.insert(
                    "file".to_string(),
                    serde_json::Value::String("src/main/java/com/example/Example.java".to_string()),
                );
                edit.insert(
                    "range".to_string(),
                    serde_json::to_value(insert_range).expect("serialize range"),
                );
                edit.insert(
                    "text".to_string(),
                    serde_json::Value::String("\n        return 42;\n    ".to_string()),
                );
                edit
            })]),
        );
        patch
    }))
    .expect("patch json");

    let ai_server = crate::support::TestAiServer::start(serde_json::Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert("completion".to_string(), serde_json::Value::String(patch));
        resp
    }));

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env("NOVA_AI_PROVIDER", "http")
        .env(
            "NOVA_AI_ENDPOINT",
            format!("{}/complete", ai_server.base_url()),
        )
        .env("NOVA_AI_MODEL", "default")
        .env("NOVA_AI_ANONYMIZE_IDENTIFIERS", "0")
        .env("NOVA_AI_ALLOW_CLOUD_CODE_EDITS", "1")
        .env("NOVA_AI_ALLOW_CODE_EDITS_WITHOUT_ANONYMIZATION", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // initialize with a workspace root so file paths are relative.
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::initialize_request_with_root_uri(1, uri_for_path(root)),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &crate::support::initialized_notification());

    // open document
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::did_open_notification(file_uri.clone(), "java", 1, text),
    );
    let text_document_uri: Uri = file_uri.parse().expect("lsp uri");

    // request code actions over the empty method range.
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            CodeActionParams {
                text_document: TextDocumentIdentifier {
                    uri: text_document_uri,
                },
                range,
                context: CodeActionContext::default(),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            2,
            "textDocument/codeAction",
        ),
    );

    let code_actions_resp = read_response_with_id(&mut stdout, 2);
    let actions = code_actions_resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");

    let gen = actions
        .iter()
        .find(|a| a.get("title").and_then(|t| t.as_str()) == Some("Generate method body with AI"))
        .expect("generate method body action");

    let cmd = gen
        .get("command")
        .and_then(|c| c.get("command"))
        .and_then(|v| v.as_str())
        .expect("command string");
    assert_eq!(cmd, nova_ide::COMMAND_GENERATE_METHOD_BODY);
    let args = gen
        .get("command")
        .and_then(|c| c.get("arguments"))
        .cloned()
        .expect("command args");
    let arguments = args
        .as_array()
        .cloned()
        .expect("command.arguments must be an array");

    // execute the command
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            ExecuteCommandParams {
                command: cmd.to_string(),
                arguments,
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
            3,
            "workspace/executeCommand",
        ),
    );

    let (messages, resp) = drain_notifications_until_id(&mut stdout, 3);
    assert!(
        resp.get("error").is_none(),
        "expected executeCommand success, got: {resp:#?}"
    );
    let apply_edit = find_apply_edit_request(&messages);
    let result = resp.get("result").expect("executeCommand result");
    if !result.is_null() {
        assert_eq!(
            result.get("applied").and_then(|v| v.as_bool()),
            Some(true),
            "expected executeCommand result.applied true, got: {resp:#?}"
        );
        if let Some(result_edit) = result.get("edit") {
            let apply_edit_value = apply_edit
                .get("params")
                .and_then(|p| p.get("edit"))
                .expect("applyEdit params.edit");
            assert_eq!(
                result_edit, apply_edit_value,
                "expected executeCommand result.edit to match applyEdit edit"
            );
        }
    }

    assert_eq!(
        apply_edit
            .get("params")
            .and_then(|p| p.get("label"))
            .and_then(|v| v.as_str()),
        Some("AI: Generate method body")
    );

    let edit = apply_edit
        .get("params")
        .and_then(|p| p.get("edit"))
        .expect("applyEdit params.edit");
    let edit: WorkspaceEdit = serde_json::from_value(edit.clone()).expect("workspace edit");
    let changes = edit.changes.expect("changes map");
    let uri: Uri = file_uri.parse().expect("uri");
    let edits = changes.get(&uri).expect("edits for file uri");
    assert!(
        edits
            .iter()
            .any(|edit| edit.new_text.contains("return 42;")),
        "expected edit to contain return statement, got: {edits:?}"
    );

    let apply_edit_id = apply_edit.get("id").cloned().expect("applyEdit id");
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_response_ok(
            apply_edit_id,
            ApplyWorkspaceEditResponse {
                applied: true,
                failure_reason: None,
                failed_change: None,
            },
        ),
    );

    ai_server.assert_hits(1);

    // shutdown + exit
    write_jsonrpc_message(&mut stdin, &crate::support::shutdown_request(4));
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &crate::support::exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_ai_generate_method_body_custom_request_sends_apply_edit() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let src_dir = root.join("src/main/java/com/example");
    std::fs::create_dir_all(&src_dir).expect("create src dir");
    let file_path = src_dir.join("Example.java");
    let file_uri = uri_for_path(&file_path);

    let text = "package com.example;\n\npublic class Example {\n    public int answer() { }\n}\n";
    std::fs::write(&file_path, text).expect("write file");

    let selection = "public int answer() { }";
    let selection_start = text.find(selection).expect("selection present");
    let selection_end = selection_start + selection.len();
    let pos = TextPos::new(text);
    let range = Range::new(
        pos.lsp_position(selection_start).expect("start pos"),
        pos.lsp_position(selection_end).expect("end pos"),
    );

    // Build a deterministic patch that inserts `return 42;` inside the braces.
    let selected = &text[selection_start..selection_end];
    let open = selected.find('{').expect("open brace");
    let close = selected.rfind('}').expect("close brace");
    let insert_start = selection_start + open + 1;
    let insert_end = selection_start + close;
    let insert_range = Range::new(
        pos.lsp_position(insert_start).expect("insert start"),
        pos.lsp_position(insert_end).expect("insert end"),
    );
    let patch = serde_json::to_string(&serde_json::Value::Object({
        let mut patch = serde_json::Map::new();
        patch.insert(
            "edits".to_string(),
            serde_json::Value::Array(vec![serde_json::Value::Object({
                let mut edit = serde_json::Map::new();
                edit.insert(
                    "file".to_string(),
                    serde_json::Value::String("src/main/java/com/example/Example.java".to_string()),
                );
                edit.insert(
                    "range".to_string(),
                    serde_json::to_value(insert_range).expect("serialize range"),
                );
                edit.insert(
                    "text".to_string(),
                    serde_json::Value::String("\n        return 42;\n    ".to_string()),
                );
                edit
            })]),
        );
        patch
    }))
    .expect("patch json");

    let ai_server = crate::support::TestAiServer::start(serde_json::Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert("completion".to_string(), serde_json::Value::String(patch));
        resp
    }));

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env("NOVA_AI_PROVIDER", "http")
        .env(
            "NOVA_AI_ENDPOINT",
            format!("{}/complete", ai_server.base_url()),
        )
        .env("NOVA_AI_MODEL", "default")
        .env("NOVA_AI_ANONYMIZE_IDENTIFIERS", "0")
        .env("NOVA_AI_ALLOW_CLOUD_CODE_EDITS", "1")
        .env("NOVA_AI_ALLOW_CODE_EDITS_WITHOUT_ANONYMIZATION", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // initialize with a workspace root so file paths are relative.
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::initialize_request_with_root_uri(1, uri_for_path(root)),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &crate::support::initialized_notification());

    // open document
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::did_open_notification(file_uri.clone(), "java", 1, text),
    );

    // Call the custom request endpoint.
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            serde_json::Value::Object({
                let mut params = serde_json::Map::new();
                params.insert(
                    "method_signature".to_string(),
                    serde_json::Value::String("public int answer()".to_string()),
                );
                params.insert("context".to_string(), serde_json::Value::Null);
                params.insert(
                    "uri".to_string(),
                    serde_json::Value::String(file_uri.clone()),
                );
                params.insert(
                    "range".to_string(),
                    serde_json::to_value(range).expect("serialize range"),
                );
                params
            }),
            2,
            "nova/ai/generateMethodBody",
        ),
    );

    let (messages, resp) = drain_notifications_until_id(&mut stdout, 2);
    assert!(
        resp.get("error").is_none(),
        "expected nova/ai/generateMethodBody success, got: {resp:#?}"
    );
    assert_eq!(resp.get("result"), Some(&serde_json::Value::Null));

    let apply_edit = find_apply_edit_request(&messages);
    assert_eq!(
        apply_edit
            .get("params")
            .and_then(|p| p.get("label"))
            .and_then(|v| v.as_str()),
        Some("AI: Generate method body")
    );

    let edit = apply_edit
        .get("params")
        .and_then(|p| p.get("edit"))
        .expect("applyEdit params.edit");
    let edit: WorkspaceEdit = serde_json::from_value(edit.clone()).expect("workspace edit");
    let changes = edit.changes.expect("changes map");
    let uri: Uri = file_uri.parse().expect("uri");
    let edits = changes.get(&uri).expect("edits for file uri");
    assert!(
        edits
            .iter()
            .any(|edit| edit.new_text.contains("return 42;")),
        "expected edit to contain return statement, got: {edits:?}"
    );

    let apply_edit_id = apply_edit.get("id").cloned().expect("applyEdit id");
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_response_ok(
            apply_edit_id,
            ApplyWorkspaceEditResponse {
                applied: true,
                failure_reason: None,
                failed_change: None,
            },
        ),
    );

    ai_server.assert_hits(1);

    // shutdown + exit
    write_jsonrpc_message(&mut stdin, &crate::support::shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &crate::support::exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_ai_generate_method_body_custom_request_rejects_non_empty_method() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let src_dir = root.join("src/main/java/com/example");
    std::fs::create_dir_all(&src_dir).expect("create src dir");
    let file_path = src_dir.join("Example.java");
    let file_uri = uri_for_path(&file_path);

    let text =
        "package com.example;\n\npublic class Example {\n    public int answer() { return 1; }\n}\n";
    std::fs::write(&file_path, text).expect("write file");

    let selection = "public int answer() { return 1; }";
    let selection_start = text.find(selection).expect("selection present");
    let selection_end = selection_start + selection.len();
    let pos = TextPos::new(text);
    let range = Range::new(
        pos.lsp_position(selection_start).expect("start pos"),
        pos.lsp_position(selection_end).expect("end pos"),
    );

    let ai_server = crate::support::TestAiServer::start(serde_json::Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert(
            "completion".to_string(),
            serde_json::Value::String("unused".to_string()),
        );
        resp
    }));

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env("NOVA_AI_PROVIDER", "http")
        .env(
            "NOVA_AI_ENDPOINT",
            format!("{}/complete", ai_server.base_url()),
        )
        .env("NOVA_AI_MODEL", "default")
        .env("NOVA_AI_ANONYMIZE_IDENTIFIERS", "0")
        .env("NOVA_AI_ALLOW_CLOUD_CODE_EDITS", "1")
        .env("NOVA_AI_ALLOW_CODE_EDITS_WITHOUT_ANONYMIZATION", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::initialize_request_with_root_uri(1, uri_for_path(root)),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &crate::support::initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::did_open_notification(file_uri.clone(), "java", 1, text),
    );

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            serde_json::Value::Object({
                let mut params = serde_json::Map::new();
                params.insert(
                    "method_signature".to_string(),
                    serde_json::Value::String("public int answer()".to_string()),
                );
                params.insert("context".to_string(), serde_json::Value::Null);
                params.insert("uri".to_string(), serde_json::Value::String(file_uri));
                params.insert(
                    "range".to_string(),
                    serde_json::to_value(range).expect("serialize range"),
                );
                params
            }),
            2,
            "nova/ai/generateMethodBody",
        ),
    );

    let resp = read_response_with_id(&mut stdout, 2);
    let error = resp
        .get("error")
        .and_then(|v| v.as_object())
        .expect("expected error response");
    assert_eq!(error.get("code").and_then(|v| v.as_i64()), Some(-32602));
    assert!(
        error
            .get("message")
            .and_then(|v| v.as_str())
            .is_some_and(|m| m.contains("method body")),
        "expected error message mentioning method body, got: {resp:#?}"
    );

    write_jsonrpc_message(&mut stdin, &crate::support::shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &crate::support::exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
    ai_server.assert_hits(0);
}

#[test]
fn stdio_server_ai_generate_tests_sends_apply_edit() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    // Configure AI via `nova.toml` so we can set `ai.privacy.excluded_paths`.
    //
    // With `excluded_paths = ["src/test/java/**"]`, `run_ai_generate_tests` must *not* derive a
    // `src/test/java/...` destination (even though the source file lives under `src/main/java/`).
    // Instead, it should fall back to inserting tests into the source file.
    let config_path = root.join("nova.toml");

    let src_dir = root.join("src/main/java/com/example");
    std::fs::create_dir_all(&src_dir).expect("create src dir");
    let file_path = src_dir.join("Example.java");
    let file_uri = uri_for_path(&file_path);

    // `run_ai_generate_tests` prefers generating a new test file under `src/test/java/...` when the
    // source file is in `src/main/java/...`.
    let test_dir = root.join("src/test/java/com/example");
    std::fs::create_dir_all(&test_dir).expect("create test dir");
    let test_file_path = test_dir.join("ExampleTest.java");

    let text =
        "package com.example;\n\npublic class Example {\n    public int answer() { return 1; }\n}\n";
    std::fs::write(&file_path, text).expect("write file");

    // Selection range over the method name is enough to trigger "Generate tests with AI".
    let method_offset = text.find("answer").expect("method present");
    let pos = TextPos::new(text);
    let start = pos.lsp_position(method_offset).expect("start pos");
    let end = pos
        .lsp_position(method_offset + "answer".len())
        .expect("end pos");
    let range = Range::new(start, end);

    // `ai.privacy.excluded_paths` prevents generating tests under `src/test/java`, so this should
    // fall back to inserting tests into the source file.
    let patch = serde_json::to_string(&serde_json::Value::Object({
        let mut patch = serde_json::Map::new();
        patch.insert(
            "edits".to_string(),
            serde_json::Value::Array(vec![serde_json::Value::Object({
                let mut edit = serde_json::Map::new();
                edit.insert(
                    "file".to_string(),
                    serde_json::Value::String("src/main/java/com/example/Example.java".to_string()),
                );
                edit.insert(
                    "range".to_string(),
                    serde_json::to_value(Range::new(Position::new(4, 0), Position::new(4, 0)))
                        .expect("serialize range"),
                );
                edit.insert(
                    "text".to_string(),
                    serde_json::Value::String(
                        "    // AI-generated tests would go here\n".to_string(),
                    ),
                );
                edit
            })]),
        );
        patch
    }))
    .expect("patch json");

    let ai_server = crate::support::TestAiServer::start(serde_json::Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert("completion".to_string(), serde_json::Value::String(patch));
        resp
    }));

    // Write `nova.toml` with the actual AI server endpoint.
    std::fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.provider]
kind = "http"
url = "{}/complete"
model = "default"

[ai.privacy]
local_only = true
excluded_paths = ["src/test/java/**"]
"#,
            ai_server.base_url()
        ),
    )
    .expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        // Ensure a developer's environment doesn't override `--config`.
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::initialize_request_with_root_uri(1, uri_for_path(root)),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &crate::support::initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::did_open_notification(file_uri.clone(), "java", 1, text),
    );

    let text_document_uri: Uri = file_uri.parse().expect("lsp uri");

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            CodeActionParams {
                text_document: TextDocumentIdentifier {
                    uri: text_document_uri,
                },
                range,
                context: CodeActionContext::default(),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            2,
            "textDocument/codeAction",
        ),
    );

    let code_actions_resp = read_response_with_id(&mut stdout, 2);
    let actions = code_actions_resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");

    let gen = actions
        .iter()
        .find(|a| a.get("title").and_then(|t| t.as_str()) == Some("Generate tests with AI"))
        .expect("generate tests action");

    let cmd = gen
        .get("command")
        .and_then(|c| c.get("command"))
        .and_then(|v| v.as_str())
        .expect("command string");
    assert_eq!(cmd, nova_ide::COMMAND_GENERATE_TESTS);
    let args = gen
        .get("command")
        .and_then(|c| c.get("arguments"))
        .cloned()
        .expect("command args");
    let arguments = args
        .as_array()
        .cloned()
        .expect("command.arguments must be an array");

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            ExecuteCommandParams {
                command: cmd.to_string(),
                arguments,
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
            3,
            "workspace/executeCommand",
        ),
    );

    let (messages, resp) = drain_notifications_until_id(&mut stdout, 3);
    assert!(
        resp.get("error").is_none(),
        "expected executeCommand success, got: {resp:#?}"
    );
    let apply_edit = find_apply_edit_request(&messages);
    let result = resp.get("result").expect("executeCommand result");
    if !result.is_null() {
        assert_eq!(
            result.get("applied").and_then(|v| v.as_bool()),
            Some(true),
            "expected executeCommand result.applied true, got: {resp:#?}"
        );
        if let Some(result_edit) = result.get("edit") {
            let apply_edit_value = apply_edit
                .get("params")
                .and_then(|p| p.get("edit"))
                .expect("applyEdit params.edit");
            assert_eq!(
                result_edit, apply_edit_value,
                "expected executeCommand result.edit to match applyEdit edit"
            );
        }
    }

    assert_eq!(
        apply_edit
            .get("params")
            .and_then(|p| p.get("label"))
            .and_then(|v| v.as_str()),
        Some("AI: Generate tests")
    );
    let edit = apply_edit
        .get("params")
        .and_then(|p| p.get("edit"))
        .expect("applyEdit params.edit");
    let edit: WorkspaceEdit = serde_json::from_value(edit.clone()).expect("workspace edit");

    let expected_source_uri: Uri = file_uri.parse().expect("file uri");
    let expected_test_uri: Uri = uri_for_path(&test_file_path).parse().expect("test uri");

    let mut all_new_texts = Vec::new();
    let mut saw_excluded_test_uri = false;

    if let Some(changes) = &edit.changes {
        if let Some(edits) = changes.get(&expected_source_uri) {
            all_new_texts.extend(edits.iter().map(|e| e.new_text.clone()));
        }
        if changes.contains_key(&expected_test_uri) {
            saw_excluded_test_uri = true;
        }
    }
    if let Some(doc_changes) = &edit.document_changes {
        match doc_changes {
            lsp_types::DocumentChanges::Edits(edits) => {
                for doc_edit in edits {
                    if doc_edit.text_document.uri == expected_test_uri {
                        saw_excluded_test_uri = true;
                    }
                    if doc_edit.text_document.uri != expected_source_uri {
                        continue;
                    }
                    for edit in &doc_edit.edits {
                        match edit {
                            lsp_types::OneOf::Left(edit) => {
                                all_new_texts.push(edit.new_text.clone())
                            }
                            lsp_types::OneOf::Right(edit) => {
                                all_new_texts.push(edit.text_edit.new_text.clone())
                            }
                        }
                    }
                }
            }
            lsp_types::DocumentChanges::Operations(ops) => {
                for op in ops {
                    match op {
                        lsp_types::DocumentChangeOperation::Op(lsp_types::ResourceOp::Create(
                            create,
                        )) if create.uri == expected_test_uri => {
                            saw_excluded_test_uri = true;
                        }
                        lsp_types::DocumentChangeOperation::Op(lsp_types::ResourceOp::Rename(
                            rename,
                        )) if rename.old_uri == expected_test_uri
                            || rename.new_uri == expected_test_uri =>
                        {
                            saw_excluded_test_uri = true;
                        }
                        lsp_types::DocumentChangeOperation::Op(lsp_types::ResourceOp::Delete(
                            delete,
                        )) if delete.uri == expected_test_uri => {
                            saw_excluded_test_uri = true;
                        }
                        lsp_types::DocumentChangeOperation::Edit(doc_edit) => {
                            if doc_edit.text_document.uri == expected_test_uri {
                                saw_excluded_test_uri = true;
                            }
                            if doc_edit.text_document.uri != expected_source_uri {
                                continue;
                            }
                            for edit in &doc_edit.edits {
                                match edit {
                                    lsp_types::OneOf::Left(edit) => {
                                        all_new_texts.push(edit.new_text.clone())
                                    }
                                    lsp_types::OneOf::Right(edit) => {
                                        all_new_texts.push(edit.text_edit.new_text.clone())
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    assert!(
        !saw_excluded_test_uri,
        "expected no edits for excluded test uri, got: {edit:#?}"
    );
    assert!(
        all_new_texts
            .iter()
            .any(|text| text.contains("AI-generated tests would go here")),
        "expected inserted comment, got: {edit:#?}"
    );

    let apply_edit_id = apply_edit.get("id").cloned().expect("applyEdit id");
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_response_ok(
            apply_edit_id,
            ApplyWorkspaceEditResponse {
                applied: true,
                failure_reason: None,
                failed_change: None,
            },
        ),
    );

    ai_server.assert_hits(1);

    write_jsonrpc_message(&mut stdin, &crate::support::shutdown_request(4));
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &crate::support::exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_ai_generate_tests_custom_request_sends_apply_edit() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let src_dir = root.join("src/main/java/com/example");
    std::fs::create_dir_all(&src_dir).expect("create src dir");
    let file_path = src_dir.join("Example.java");
    let file_uri = uri_for_path(&file_path);

    // `run_ai_generate_tests_apply` prefers generating a new test file under `src/test/java/...`
    // when the source file is in `src/main/java/...`.
    let test_dir = root.join("src/test/java/com/example");
    std::fs::create_dir_all(&test_dir).expect("create test dir");
    let test_file_path = test_dir.join("ExampleTest.java");

    let text =
        "package com.example;\n\npublic class Example {\n    public int answer() { return 1; }\n}\n";
    std::fs::write(&file_path, text).expect("write file");

    // Select the full method line so the prompt contains a meaningful source snippet (including
    // `return 1;`). The patch can still create a separate test file.
    let method_offset = text
        .find("public int answer() { return 1; }")
        .expect("method present");
    let pos = TextPos::new(text);
    let start = pos.lsp_position(method_offset).expect("start pos");
    let end = pos
        .lsp_position(method_offset + "public int answer() { return 1; }".len())
        .expect("end pos");
    let range = Range::new(start, end);

    let patch = serde_json::to_string(&serde_json::Value::Object({
        let mut patch = serde_json::Map::new();
        patch.insert(
            "edits".to_string(),
            serde_json::Value::Array(vec![serde_json::Value::Object({
                let mut edit = serde_json::Map::new();
                edit.insert(
                    "file".to_string(),
                    serde_json::Value::String(
                        "src/test/java/com/example/ExampleTest.java".to_string(),
                    ),
                );
                edit.insert(
                    "range".to_string(),
                    serde_json::to_value(Range::new(Position::new(0, 0), Position::new(0, 0)))
                        .expect("serialize range"),
                );
                edit.insert(
                    "text".to_string(),
                    serde_json::Value::String(
                        "package com.example;\n\npublic class ExampleTest {}\n".to_string(),
                    ),
                );
                edit
            })]),
        );
        patch
    }))
    .expect("patch json");
    let mock_server = MockServer::start();
    let mock = mock_server.mock(|when, then| {
        when.method(POST)
            .path("/complete")
            .body_contains("File: src/test/java/com/example/ExampleTest.java")
            .body_contains("Test target:")
            .body_contains("public int answer()")
            .body_contains("Source file under test: src/main/java/com/example/Example.java")
            .body_contains("Selected source snippet:")
            .body_contains("return 1;");
        then.status(200).json_body(serde_json::Value::Object({
            let mut resp = serde_json::Map::new();
            resp.insert(
                "completion".to_string(),
                serde_json::Value::String(patch.clone()),
            );
            resp
        }));
    });

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env("NOVA_AI_PROVIDER", "http")
        .env(
            "NOVA_AI_ENDPOINT",
            format!("{}/complete", mock_server.base_url()),
        )
        .env("NOVA_AI_MODEL", "default")
        .env("NOVA_AI_ANONYMIZE_IDENTIFIERS", "0")
        .env("NOVA_AI_ALLOW_CLOUD_CODE_EDITS", "1")
        .env("NOVA_AI_ALLOW_CODE_EDITS_WITHOUT_ANONYMIZATION", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::initialize_request_with_root_uri(1, uri_for_path(root)),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &crate::support::initialized_notification());

    // Open the source document so the server can load document text from the overlay.
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::did_open_notification(file_uri.clone(), "java", 1, text),
    );

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            serde_json::Value::Object({
                let mut params = serde_json::Map::new();
                params.insert(
                    "target".to_string(),
                    serde_json::Value::String("public int answer()".to_string()),
                );
                params.insert("context".to_string(), serde_json::Value::Null);
                params.insert(
                    "uri".to_string(),
                    serde_json::Value::String(file_uri.clone()),
                );
                params.insert(
                    "range".to_string(),
                    serde_json::to_value(range).expect("serialize range"),
                );
                params
            }),
            2,
            "nova/ai/generateTests",
        ),
    );

    let (messages, resp) = drain_notifications_until_id(&mut stdout, 2);
    assert!(
        resp.get("error").is_none(),
        "expected nova/ai/generateTests success, got: {resp:#?}"
    );
    assert_eq!(resp.get("result"), Some(&serde_json::Value::Null));

    let apply_edit = find_apply_edit_request(&messages);
    assert_eq!(
        apply_edit
            .get("params")
            .and_then(|p| p.get("label"))
            .and_then(|v| v.as_str()),
        Some("AI: Generate tests")
    );
    let edit = apply_edit
        .get("params")
        .and_then(|p| p.get("edit"))
        .expect("applyEdit params.edit");
    let edit: WorkspaceEdit = serde_json::from_value(edit.clone()).expect("workspace edit");

    let expected_test_uri: Uri = uri_for_path(&test_file_path).parse().expect("test uri");
    let mut all_new_texts = Vec::new();
    let mut saw_create = false;

    if let Some(changes) = &edit.changes {
        if let Some(edits) = changes.get(&expected_test_uri) {
            all_new_texts.extend(edits.iter().map(|e| e.new_text.clone()));
        }
    }
    if let Some(doc_changes) = &edit.document_changes {
        match doc_changes {
            lsp_types::DocumentChanges::Edits(edits) => {
                for doc_edit in edits {
                    if doc_edit.text_document.uri != expected_test_uri {
                        continue;
                    }
                    for edit in &doc_edit.edits {
                        match edit {
                            lsp_types::OneOf::Left(edit) => {
                                all_new_texts.push(edit.new_text.clone())
                            }
                            lsp_types::OneOf::Right(edit) => {
                                all_new_texts.push(edit.text_edit.new_text.clone())
                            }
                        }
                    }
                }
            }
            lsp_types::DocumentChanges::Operations(ops) => {
                for op in ops {
                    match op {
                        lsp_types::DocumentChangeOperation::Op(lsp_types::ResourceOp::Create(
                            create,
                        )) if create.uri == expected_test_uri => {
                            saw_create = true;
                        }
                        lsp_types::DocumentChangeOperation::Edit(doc_edit) => {
                            if doc_edit.text_document.uri != expected_test_uri {
                                continue;
                            }
                            for edit in &doc_edit.edits {
                                match edit {
                                    lsp_types::OneOf::Left(edit) => {
                                        all_new_texts.push(edit.new_text.clone())
                                    }
                                    lsp_types::OneOf::Right(edit) => {
                                        all_new_texts.push(edit.text_edit.new_text.clone())
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    if matches!(
        &edit.document_changes,
        Some(lsp_types::DocumentChanges::Operations(_))
    ) {
        assert!(
            saw_create,
            "expected CreateFile for test file, got: {edit:#?}"
        );
    }

    assert!(
        all_new_texts
            .iter()
            .any(|text| text.contains("class ExampleTest")),
        "expected edits to contain `class ExampleTest`, got: {edit:#?}"
    );

    let apply_edit_id = apply_edit.get("id").cloned().expect("applyEdit id");
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_response_ok(
            apply_edit_id,
            ApplyWorkspaceEditResponse {
                applied: true,
                failure_reason: None,
                failed_change: None,
            },
        ),
    );

    mock.assert_hits(1);

    write_jsonrpc_message(&mut stdin, &crate::support::shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &crate::support::exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_ai_generate_tests_custom_request_respects_excluded_paths() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = uri_for_path(root);

    // Configure AI via `nova.toml` so we can set `ai.privacy.excluded_paths`.
    //
    // With `excluded_paths = ["src/test/java/**"]`, the `nova/ai/generateTests` endpoint must *not*
    // derive a `src/test/java/...` destination (even though the source file lives under
    // `src/main/java/`). Instead, it should fall back to inserting tests into the source file.
    let config_path = root.join("nova.toml");

    let src_dir = root.join("src/main/java/com/example");
    std::fs::create_dir_all(&src_dir).expect("create src dir");
    let file_path = src_dir.join("Example.java");
    let file_uri = uri_for_path(&file_path);

    // `run_ai_generate_tests_apply` prefers generating a new test file under `src/test/java/...`
    // when the source file is in `src/main/java/...`.
    let test_dir = root.join("src/test/java/com/example");
    std::fs::create_dir_all(&test_dir).expect("create test dir");
    let test_file_path = test_dir.join("ExampleTest.java");

    let text =
        "package com.example;\n\npublic class Example {\n    public int answer() { return 1; }\n}\n";
    std::fs::write(&file_path, text).expect("write file");

    // Selection range over the method name is enough for the AI prompt.
    let method_offset = text.find("answer").expect("method present");
    let pos = TextPos::new(text);
    let start = pos.lsp_position(method_offset).expect("start pos");
    let end = pos
        .lsp_position(method_offset + "answer".len())
        .expect("end pos");
    let range = Range::new(start, end);

    // `ai.privacy.excluded_paths` prevents generating tests under `src/test/java`, so this should
    // fall back to inserting tests into the source file.
    let patch = serde_json::to_string(&serde_json::Value::Object({
        let mut patch = serde_json::Map::new();
        patch.insert(
            "edits".to_string(),
            serde_json::Value::Array(vec![serde_json::Value::Object({
                let mut edit = serde_json::Map::new();
                edit.insert(
                    "file".to_string(),
                    serde_json::Value::String("src/main/java/com/example/Example.java".to_string()),
                );
                edit.insert(
                    "range".to_string(),
                    serde_json::to_value(Range::new(Position::new(4, 0), Position::new(4, 0)))
                        .expect("serialize range"),
                );
                edit.insert(
                    "text".to_string(),
                    serde_json::Value::String(
                        "    // AI-generated tests would go here\n".to_string(),
                    ),
                );
                edit
            })]),
        );
        patch
    }))
    .expect("patch json");

    let ai_server = crate::support::TestAiServer::start(serde_json::Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert("completion".to_string(), serde_json::Value::String(patch));
        resp
    }));

    // Write `nova.toml` with the actual AI server endpoint.
    std::fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.provider]
kind = "http"
url = "{}/complete"
model = "default"

[ai.privacy]
local_only = true
excluded_paths = ["src/test/java/**"]
"#,
            ai_server.base_url()
        ),
    )
    .expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        // The test config file should be authoritative; clear any legacy env-var AI wiring that
        // could override `--config` (common in developer shells).
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        // Ensure a developer's environment doesn't disable AI for this test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::initialize_request_with_root_uri(1, root_uri),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &crate::support::initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::did_open_notification(file_uri.clone(), "java", 1, text),
    );

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            serde_json::Value::Object({
                let mut params = serde_json::Map::new();
                params.insert(
                    "target".to_string(),
                    serde_json::Value::String("public int answer()".to_string()),
                );
                params.insert("context".to_string(), serde_json::Value::Null);
                params.insert(
                    "uri".to_string(),
                    serde_json::Value::String(file_uri.clone()),
                );
                params.insert(
                    "range".to_string(),
                    serde_json::to_value(range).expect("serialize range"),
                );
                params
            }),
            2,
            "nova/ai/generateTests",
        ),
    );

    let (messages, resp) = drain_notifications_until_id(&mut stdout, 2);
    assert!(
        resp.get("error").is_none(),
        "expected nova/ai/generateTests success, got: {resp:#?}"
    );
    assert_eq!(resp.get("result"), Some(&serde_json::Value::Null));

    let apply_edit = find_apply_edit_request(&messages);
    assert_eq!(
        apply_edit
            .get("params")
            .and_then(|p| p.get("label"))
            .and_then(|v| v.as_str()),
        Some("AI: Generate tests")
    );
    let edit = apply_edit
        .get("params")
        .and_then(|p| p.get("edit"))
        .expect("applyEdit params.edit");
    let edit: WorkspaceEdit = serde_json::from_value(edit.clone()).expect("workspace edit");

    let expected_source_uri: Uri = file_uri.parse().expect("file uri");
    let expected_test_uri: Uri = uri_for_path(&test_file_path).parse().expect("test uri");

    let mut all_new_texts = Vec::new();
    let mut saw_excluded_test_uri = false;

    if let Some(changes) = &edit.changes {
        if let Some(edits) = changes.get(&expected_source_uri) {
            all_new_texts.extend(edits.iter().map(|e| e.new_text.clone()));
        }
        if changes.contains_key(&expected_test_uri) {
            saw_excluded_test_uri = true;
        }
    }
    if let Some(doc_changes) = &edit.document_changes {
        match doc_changes {
            lsp_types::DocumentChanges::Edits(edits) => {
                for doc_edit in edits {
                    if doc_edit.text_document.uri == expected_test_uri {
                        saw_excluded_test_uri = true;
                    }
                    if doc_edit.text_document.uri != expected_source_uri {
                        continue;
                    }
                    for edit in &doc_edit.edits {
                        match edit {
                            lsp_types::OneOf::Left(edit) => {
                                all_new_texts.push(edit.new_text.clone())
                            }
                            lsp_types::OneOf::Right(edit) => {
                                all_new_texts.push(edit.text_edit.new_text.clone())
                            }
                        }
                    }
                }
            }
            lsp_types::DocumentChanges::Operations(ops) => {
                for op in ops {
                    match op {
                        lsp_types::DocumentChangeOperation::Op(lsp_types::ResourceOp::Create(
                            create,
                        )) if create.uri == expected_test_uri => {
                            saw_excluded_test_uri = true;
                        }
                        lsp_types::DocumentChangeOperation::Op(lsp_types::ResourceOp::Rename(
                            rename,
                        )) if rename.old_uri == expected_test_uri
                            || rename.new_uri == expected_test_uri =>
                        {
                            saw_excluded_test_uri = true;
                        }
                        lsp_types::DocumentChangeOperation::Op(lsp_types::ResourceOp::Delete(
                            delete,
                        )) if delete.uri == expected_test_uri => {
                            saw_excluded_test_uri = true;
                        }
                        lsp_types::DocumentChangeOperation::Edit(doc_edit) => {
                            if doc_edit.text_document.uri == expected_test_uri {
                                saw_excluded_test_uri = true;
                            }
                            if doc_edit.text_document.uri != expected_source_uri {
                                continue;
                            }
                            for edit in &doc_edit.edits {
                                match edit {
                                    lsp_types::OneOf::Left(edit) => {
                                        all_new_texts.push(edit.new_text.clone())
                                    }
                                    lsp_types::OneOf::Right(edit) => {
                                        all_new_texts.push(edit.text_edit.new_text.clone())
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    assert!(
        !saw_excluded_test_uri,
        "expected no edits for excluded test uri, got: {edit:#?}"
    );
    assert!(
        all_new_texts
            .iter()
            .any(|text| text.contains("AI-generated tests would go here")),
        "expected inserted comment, got: {edit:#?}"
    );

    let apply_edit_id = apply_edit.get("id").cloned().expect("applyEdit id");
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_response_ok(
            apply_edit_id,
            ApplyWorkspaceEditResponse {
                applied: true,
                failure_reason: None,
                failed_change: None,
            },
        ),
    );

    ai_server.assert_hits(1);

    write_jsonrpc_message(&mut stdin, &crate::support::shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &crate::support::exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_ai_custom_requests_reject_excluded_paths() {
    let _lock = crate::support::stdio_server_lock();
    let ai_server = crate::support::TestAiServer::start(serde_json::Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert(
            "completion".to_string(),
            serde_json::Value::String("unused".to_string()),
        );
        resp
    }));
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = uri_for_path(root);

    let secrets_dir = root.join("src/secrets");
    std::fs::create_dir_all(&secrets_dir).expect("create secrets dir");
    let file_path = secrets_dir.join("Secret.java");
    let file_uri = uri_for_path(&file_path);
    let text = "class Secret { int answer() { } }\n";
    std::fs::write(&file_path, text).expect("write Secret.java");

    let config_path = root.join("nova.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.provider]
kind = "http"
url = "{}/complete"
model = "default"

[ai.privacy]
local_only = true
excluded_paths = ["src/secrets/**"]
"#,
            ai_server.base_url()
        ),
    )
    .expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        // The test config file should be authoritative; clear any legacy env-var AI wiring that
        // could override `--config` (common in developer shells).
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        // Ensure a developer's environment doesn't disable AI for this test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::initialize_request_with_root_uri(1, root_uri),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &crate::support::initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::did_open_notification(file_uri.clone(), "java", 1, text),
    );

    let range = Range::new(Position::new(0, 0), Position::new(0, 0));

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            serde_json::Value::Object({
                let mut params = serde_json::Map::new();
                params.insert(
                    "method_signature".to_string(),
                    serde_json::Value::String("int answer()".to_string()),
                );
                params.insert("context".to_string(), serde_json::Value::Null);
                params.insert(
                    "uri".to_string(),
                    serde_json::Value::String(file_uri.clone()),
                );
                params.insert(
                    "range".to_string(),
                    serde_json::to_value(range).expect("serialize range"),
                );
                params
            }),
            2,
            "nova/ai/generateMethodBody",
        ),
    );

    let resp = read_response_with_id(&mut stdout, 2);
    let error = resp
        .get("error")
        .and_then(|v| v.as_object())
        .expect("expected error response");
    assert_eq!(error.get("code").and_then(|v| v.as_i64()), Some(-32600));
    assert!(
        error
            .get("message")
            .and_then(|v| v.as_str())
            .is_some_and(|m| m.contains("excluded_paths")),
        "expected error message to mention excluded_paths, got: {resp:#?}"
    );

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            serde_json::Value::Object({
                let mut params = serde_json::Map::new();
                params.insert(
                    "target".to_string(),
                    serde_json::Value::String("int answer()".to_string()),
                );
                params.insert("context".to_string(), serde_json::Value::Null);
                params.insert("uri".to_string(), serde_json::Value::String(file_uri));
                params.insert(
                    "range".to_string(),
                    serde_json::to_value(range).expect("serialize range"),
                );
                params
            }),
            3,
            "nova/ai/generateTests",
        ),
    );

    let resp = read_response_with_id(&mut stdout, 3);
    let error = resp
        .get("error")
        .and_then(|v| v.as_object())
        .expect("expected error response");
    assert_eq!(error.get("code").and_then(|v| v.as_i64()), Some(-32600));
    assert!(
        error
            .get("message")
            .and_then(|v| v.as_str())
            .is_some_and(|m| m.contains("excluded_paths")),
        "expected error message to mention excluded_paths, got: {resp:#?}"
    );

    write_jsonrpc_message(&mut stdin, &crate::support::shutdown_request(4));
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &crate::support::exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
    ai_server.assert_hits(0);
}

#[test]
fn stdio_server_ai_custom_requests_require_document_text() {
    let _lock = crate::support::stdio_server_lock();
    let ai_server = crate::support::TestAiServer::start(serde_json::Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert(
            "completion".to_string(),
            serde_json::Value::String("unused".to_string()),
        );
        resp
    }));
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = uri_for_path(root);

    let missing_path = root.join("src/main/java/com/example/Missing.java");
    let missing_uri = uri_for_path(&missing_path);
    let range = Range::new(Position::new(0, 0), Position::new(0, 0));

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        // Ensure a developer's environment doesn't disable AI for this test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env("NOVA_AI_PROVIDER", "http")
        .env(
            "NOVA_AI_ENDPOINT",
            format!("{}/complete", ai_server.base_url()),
        )
        .env("NOVA_AI_MODEL", "default")
        .env("NOVA_AI_ANONYMIZE_IDENTIFIERS", "0")
        .env("NOVA_AI_ALLOW_CLOUD_CODE_EDITS", "1")
        .env("NOVA_AI_ALLOW_CODE_EDITS_WITHOUT_ANONYMIZATION", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::initialize_request_with_root_uri(1, root_uri),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &crate::support::initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            serde_json::Value::Object({
                let mut params = serde_json::Map::new();
                params.insert(
                    "method_signature".to_string(),
                    serde_json::Value::String("int answer()".to_string()),
                );
                params.insert("context".to_string(), serde_json::Value::Null);
                params.insert(
                    "uri".to_string(),
                    serde_json::Value::String(missing_uri.clone()),
                );
                params.insert("range".to_string(), crate::support::json_value(range));
                params
            }),
            2,
            "nova/ai/generateMethodBody",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let error = resp
        .get("error")
        .and_then(|v| v.as_object())
        .expect("expected error response");
    assert_eq!(error.get("code").and_then(|v| v.as_i64()), Some(-32602));
    assert!(
        error
            .get("message")
            .and_then(|v| v.as_str())
            .is_some_and(|m| m.contains("missing document text")),
        "expected missing document text error, got: {resp:#?}"
    );

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            serde_json::Value::Object({
                let mut params = serde_json::Map::new();
                params.insert(
                    "target".to_string(),
                    serde_json::Value::String("int answer()".to_string()),
                );
                params.insert("context".to_string(), serde_json::Value::Null);
                params.insert("uri".to_string(), serde_json::Value::String(missing_uri));
                params.insert("range".to_string(), crate::support::json_value(range));
                params
            }),
            3,
            "nova/ai/generateTests",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    let error = resp
        .get("error")
        .and_then(|v| v.as_object())
        .expect("expected error response");
    assert_eq!(error.get("code").and_then(|v| v.as_i64()), Some(-32602));
    assert!(
        error
            .get("message")
            .and_then(|v| v.as_str())
            .is_some_and(|m| m.contains("missing document text")),
        "expected missing document text error, got: {resp:#?}"
    );

    write_jsonrpc_message(&mut stdin, &crate::support::shutdown_request(4));
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &crate::support::exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
    ai_server.assert_hits(0);
}

#[test]
fn stdio_server_chunks_long_ai_explain_error_log_messages() {
    let _lock = crate::support::stdio_server_lock();
    let mock_server = MockServer::start();

    // Large enough that `nova-lsp` must split it across multiple `window/logMessage`
    // notifications (see `AI_LOG_MESSAGE_CHUNK_BYTES`).
    let long = "Nova AI output ".repeat(4_000);
    let mock = mock_server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200).json_body(serde_json::Value::Object({
            let mut resp = serde_json::Map::new();
            resp.insert(
                "completion".to_string(),
                serde_json::Value::String(long.clone()),
            );
            resp
        }));
    });

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        // Ensure a developer's environment doesn't disable AI for this test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env("NOVA_AI_PROVIDER", "http")
        .env(
            "NOVA_AI_ENDPOINT",
            format!("{}/complete", mock_server.base_url()),
        )
        .env("NOVA_AI_MODEL", "default")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // initialize
    write_jsonrpc_message(&mut stdin, &crate::support::initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &crate::support::initialized_notification());

    // open document so the server can attach snippets.
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::did_open_notification(
            "file:///Test.java",
            "java",
            1,
            "class Test { void run() { unknown(); } }",
        ),
    );

    // request code actions with a diagnostic.
    let range = Range::new(
        lsp_types::Position::new(0, 0),
        lsp_types::Position::new(0, 10),
    );
    let uri: Uri = "file:///Test.java".parse().expect("lsp uri");

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            CodeActionParams {
                text_document: TextDocumentIdentifier { uri },
                range,
                context: CodeActionContext {
                    diagnostics: vec![Diagnostic::new_simple(
                        range,
                        "cannot find symbol".to_string(),
                    )],
                    ..CodeActionContext::default()
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            2,
            "textDocument/codeAction",
        ),
    );
    let code_actions_resp = read_response_with_id(&mut stdout, 2);
    let actions = code_actions_resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");
    let explain = actions
        .iter()
        .find(|a| a.get("title").and_then(|t| t.as_str()) == Some("Explain this error"))
        .expect("explain code action");
    let cmd = explain
        .get("command")
        .expect("command")
        .get("command")
        .and_then(|v| v.as_str())
        .expect("command string");
    let args = explain
        .get("command")
        .expect("command")
        .get("arguments")
        .cloned()
        .expect("arguments");

    // execute command (triggers the mock AI call).
    let progress_token = NumberOrString::String("progress-token".to_string());
    let progress_token_value = crate::support::json_value(progress_token.clone());
    let arguments = args
        .as_array()
        .cloned()
        .expect("command.arguments must be an array");
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            ExecuteCommandParams {
                command: cmd.to_string(),
                arguments,
                work_done_progress_params: WorkDoneProgressParams {
                    work_done_token: Some(progress_token.clone()),
                },
            },
            3,
            "workspace/executeCommand",
        ),
    );

    let mut progress_kinds = Vec::new();
    let mut output_chunks = Vec::new();
    let exec_resp = loop {
        let msg = read_jsonrpc_message(&mut stdout);

        if msg.get("method").and_then(|v| v.as_str()) == Some("$/progress") {
            if msg.get("params").and_then(|p| p.get("token")) == Some(&progress_token_value) {
                if let Some(kind) = msg
                    .get("params")
                    .and_then(|p| p.get("value"))
                    .and_then(|v| v.get("kind"))
                    .and_then(|v| v.as_str())
                {
                    progress_kinds.push(kind.to_string());
                }
            }
            continue;
        }

        if msg.get("method").and_then(|v| v.as_str()) == Some("window/logMessage") {
            if let Some(text) = msg
                .get("params")
                .and_then(|p| p.get("message"))
                .and_then(|m| m.as_str())
            {
                if text.starts_with("AI explainError") {
                    let (_, chunk) = text
                        .split_once(": ")
                        .expect("AI chunk messages should contain ': ' delimiter");
                    output_chunks.push(chunk.to_string());
                }
            }
            continue;
        }

        if msg.get("id").and_then(|v| v.as_i64()) == Some(3) {
            break msg;
        }
        // Other notification/response; ignore.
    };

    let result = exec_resp
        .get("result")
        .and_then(|v| v.as_str())
        .expect("executeCommand result string");
    assert_eq!(result, long.as_str());
    assert!(progress_kinds.contains(&"begin".to_string()));
    assert!(progress_kinds.contains(&"end".to_string()));
    assert!(
        output_chunks.len() > 1,
        "expected long AI output to be chunked, got {output_chunks:?}"
    );
    assert_eq!(output_chunks.join(""), long);

    mock.assert_hits(1);

    // shutdown + exit
    write_jsonrpc_message(&mut stdin, &crate::support::shutdown_request(4));
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &crate::support::exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_completion_ranking_misconfiguration_falls_back_to_baseline_completions() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    std::fs::write(
        &config_path,
        r#"
[ai]
enabled = true

[ai.privacy]
local_only = false

[ai.provider]
kind = "open_ai"

[ai.features]
completion_ranking = true
"#,
    )
    .expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(&mut stdin, &crate::support::initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &crate::support::initialized_notification());

    let uri_str = "file:///Test.java";
    let text = "class Test { void run() { String s = \"hi\"; s. } }";
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::did_open_notification(uri_str, "java", 1, text),
    );

    let offset = text.find("s.").expect("cursor");
    let index = LineIndex::new(text);
    let pos = index.position(
        text,
        TextSize::from(u32::try_from(offset + 2).expect("offset fits in u32")),
    );
    let pos = Position::new(pos.line, pos.character);
    let uri: Uri = uri_str.parse().expect("lsp uri");

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            CompletionParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: pos,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                context: None,
            },
            2,
            "textDocument/completion",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 2);

    assert!(
        resp.get("error").is_none(),
        "expected completion response to succeed, got: {resp:#?}"
    );

    let items = resp
        .get("result")
        .and_then(|v| v.get("items"))
        .and_then(|v| v.as_array())
        .expect("completion list");
    assert!(!items.is_empty(), "expected completion items");

    write_jsonrpc_message(&mut stdin, &crate::support::shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);

    write_jsonrpc_message(&mut stdin, &crate::support::exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_extracts_utf16_ranges_for_ai_code_actions() {
    let _lock = crate::support::stdio_server_lock();
    // The code action request itself should not invoke the provider, but we need
    // a valid endpoint so the server considers AI configured.
    let ai_server = crate::support::TestAiServer::start(serde_json::Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert(
            "completion".to_string(),
            serde_json::Value::String("unused in this test".to_string()),
        );
        resp
    }));

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        // Ensure a developer's environment doesn't disable AI for this test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        // Enable code-edit actions for this test (cloud-mode policy would otherwise hide them for
        // the `http` provider).
        .env("NOVA_AI_LOCAL_ONLY", "1")
        .env("NOVA_AI_PROVIDER", "http")
        // Force local-only mode so patch-based AI code actions are offered; this
        // test focuses on UTF-16 range extraction rather than privacy gating.
        .env("NOVA_AI_LOCAL_ONLY", "true")
        .env(
            "NOVA_AI_ENDPOINT",
            format!("{}/complete", ai_server.base_url()),
        )
        .env("NOVA_AI_MODEL", "default")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // initialize
    write_jsonrpc_message(&mut stdin, &crate::support::initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &crate::support::initialized_notification());

    let uri_str = "file:///Test.java";
    let text = "class Test { void run() { String s = \"😀\"; } }\n";
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::did_open_notification(uri_str, "java", 1, text),
    );

    let emoji_offset = text.find('😀').expect("emoji present");
    let index = LineIndex::new(text);
    let start = index.position(
        text,
        TextSize::from(u32::try_from(emoji_offset).expect("offset fits in u32")),
    );
    let end = index.position(
        text,
        TextSize::from(u32::try_from(emoji_offset + '😀'.len_utf8()).expect("offset fits in u32")),
    );
    let start = Position::new(start.line, start.character);
    let end = Position::new(end.line, end.character);
    let uri: Uri = uri_str.parse().expect("lsp uri");

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            CodeActionParams {
                text_document: TextDocumentIdentifier { uri },
                range: Range::new(start, end),
                context: CodeActionContext {
                    diagnostics: vec![],
                    ..CodeActionContext::default()
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            2,
            "textDocument/codeAction",
        ),
    );

    let resp = read_response_with_id(&mut stdout, 2);
    let actions = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");

    let gen_tests = actions
        .iter()
        .find(|a| a.get("title").and_then(|t| t.as_str()) == Some("Generate tests with AI"))
        .unwrap_or_else(|| panic!("missing generate tests action in {actions:#?}"));
    let cmd = gen_tests
        .get("command")
        .and_then(|c| c.get("command"))
        .and_then(|v| v.as_str())
        .expect("command string");
    assert_eq!(cmd, nova_ide::COMMAND_GENERATE_TESTS);

    let args = gen_tests
        .get("command")
        .and_then(|c| c.get("arguments"))
        .and_then(|v| v.as_array())
        .expect("arguments");
    let target = args[0]
        .get("target")
        .and_then(|v| v.as_str())
        .expect("target");
    assert_eq!(target, "😀");
    assert_eq!(
        ai_server.hits(),
        0,
        "codeAction should not call the AI provider"
    );

    // shutdown + exit
    write_jsonrpc_message(&mut stdin, &crate::support::shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &crate::support::exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_rejects_surrogate_pair_interior_ranges_for_ai_code_actions() {
    let _lock = crate::support::stdio_server_lock();
    let ai_server = crate::support::TestAiServer::start(serde_json::Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert(
            "completion".to_string(),
            serde_json::Value::String("unused in this test".to_string()),
        );
        resp
    }));

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        // Ensure a developer's environment doesn't disable AI for this test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env("NOVA_AI_PROVIDER", "http")
        // Force local-only so absence of code actions is due to invalid UTF-16,
        // not privacy policy gating.
        .env("NOVA_AI_LOCAL_ONLY", "true")
        .env(
            "NOVA_AI_ENDPOINT",
            format!("{}/complete", ai_server.base_url()),
        )
        .env("NOVA_AI_MODEL", "default")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(&mut stdin, &crate::support::initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &crate::support::initialized_notification());

    let uri_str = "file:///Test.java";
    let text = "class Test { String s = \"😀\"; }\n";
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::did_open_notification(uri_str, "java", 1, text),
    );

    let emoji_offset = text.find('😀').expect("emoji present");
    let index = LineIndex::new(text);
    let start = index.position(
        text,
        TextSize::from(u32::try_from(emoji_offset).expect("offset fits in u32")),
    );
    let start = Position::new(start.line, start.character);
    let inside = Position::new(start.line, start.character + 1);
    let end = Position::new(start.line, start.character + 2);
    let uri: Uri = uri_str.parse().expect("lsp uri");

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            CodeActionParams {
                text_document: TextDocumentIdentifier { uri },
                range: Range::new(inside, end),
                context: CodeActionContext {
                    diagnostics: vec![],
                    ..CodeActionContext::default()
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            2,
            "textDocument/codeAction",
        ),
    );

    let resp = read_response_with_id(&mut stdout, 2);
    let actions = resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");

    assert!(
        actions
            .iter()
            .all(|a| a.get("title").and_then(|t| t.as_str()) != Some("Generate tests with AI")),
        "expected no generate tests action for invalid UTF-16 range, got: {actions:#?}"
    );
    assert_eq!(
        ai_server.hits(),
        0,
        "codeAction should not call the AI provider"
    );

    write_jsonrpc_message(&mut stdin, &crate::support::shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &crate::support::exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_rejects_cloud_ai_code_edits_when_anonymization_is_enabled() {
    let _lock = crate::support::stdio_server_lock();
    let ai_server = crate::support::TestAiServer::start(serde_json::Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert(
            "completion".to_string(),
            serde_json::Value::String("unused".to_string()),
        );
        resp
    }));

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = uri_for_path(root);

    let config_path = root.join("nova.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.provider]
kind = "http"
url = "{}/complete"
model = "default"

[ai.privacy]
local_only = false
"#,
            ai_server.base_url()
        ),
    )
    .expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        // Ensure a developer's environment doesn't disable AI for this test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // initialize
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::initialize_request_with_root_uri(1, root_uri),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &crate::support::initialized_notification());

    let file_path = temp.path().join("Main.java");
    let file_uri = uri_for_path(&file_path);
    let text = "class Test { void run() { } }";
    std::fs::write(&file_path, text).expect("write Main.java");

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::did_open_notification(file_uri.clone(), "java", 1, text),
    );

    let selection = "void run() { }";
    let start_offset = text.find(selection).expect("selection start");
    let end_offset = start_offset + selection.len();
    let pos = TextPos::new(text);
    let range = Range {
        start: pos.lsp_position(start_offset).expect("start"),
        end: pos.lsp_position(end_offset).expect("end"),
    };

    // Even when privacy policy disallows edits, `workspace/executeCommand` must still enforce the
    // policy for clients that attempt to invoke the command directly.
    let arguments = vec![serde_json::Value::Object({
        let mut arg = serde_json::Map::new();
        arg.insert(
            "method_signature".to_string(),
            serde_json::Value::String("void run()".to_string()),
        );
        arg.insert("context".to_string(), serde_json::Value::Null);
        arg.insert(
            "uri".to_string(),
            serde_json::Value::String(file_uri.clone()),
        );
        arg.insert("range".to_string(), crate::support::json_value(range));
        arg
    })];

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            ExecuteCommandParams {
                command: nova_ide::COMMAND_GENERATE_METHOD_BODY.to_string(),
                arguments,
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
            2,
            "workspace/executeCommand",
        ),
    );

    let exec_resp = read_response_with_id(&mut stdout, 2);
    let err_msg = exec_resp
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .expect("expected executeCommand to return an error");
    assert!(
        err_msg.contains(
            "AI code edits are disabled when identifier anonymization is enabled in cloud mode"
        ),
        "expected CodeEditPolicyError in error message, got: {err_msg}"
    );
    assert_eq!(
        ai_server.hits(),
        0,
        "expected no AI provider calls when code edits are blocked by policy"
    );

    // shutdown + exit
    write_jsonrpc_message(&mut stdin, &crate::support::shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &crate::support::exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_execute_command_generate_method_body_applies_workspace_edit() {
    let _lock = crate::support::stdio_server_lock();

    let mock_server = MockServer::start();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = uri_for_path(root);

    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");
    let file_path = src_dir.join("Main.java");

    // Keep the file on a single line so we can assert marker placement precisely.
    let text = "class Main { int add(int a, int b) { } }\n";
    std::fs::write(&file_path, text).expect("write Main.java");
    let file_uri = uri_for_path(&file_path);

    // Compute selection range for the method snippet.
    let method_start = text.find("int add").expect("method start");
    let open = text[method_start..].find('{').expect("method brace") + method_start;
    let close = text[open..].find('}').expect("method close") + open;
    let selection_start = TextPos::new(text)
        .lsp_position(method_start)
        .expect("selection start");
    let selection_end = TextPos::new(text)
        .lsp_position(close + 1)
        .expect("selection end");

    // Compute insert range between braces for the mock patch response.
    let insert_start = TextPos::new(text)
        .lsp_position(open + 1)
        .expect("insert start");
    let insert_end = TextPos::new(text).lsp_position(close).expect("insert end");
    let patch = serde_json::to_string(&serde_json::Value::Object({
        let mut patch = serde_json::Map::new();
        patch.insert(
            "edits".to_string(),
            serde_json::Value::Array(vec![serde_json::Value::Object({
                let mut edit = serde_json::Map::new();
                edit.insert(
                    "file".to_string(),
                    serde_json::Value::String("src/Main.java".to_string()),
                );
                edit.insert(
                    "range".to_string(),
                    crate::support::json_value(Range::new(insert_start, insert_end)),
                );
                edit.insert(
                    "text".to_string(),
                    serde_json::Value::String(" return a + b; ".to_string()),
                );
                edit
            })]),
        );
        patch
    }))
    .expect("patch json");
    let completion_response = serde_json::Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert(
            "completion".to_string(),
            serde_json::Value::String(patch.clone()),
        );
        resp
    });

    // Assert the server sends range markers *inside* the method body braces.
    let expected_marker = "int add(int a, int b) {/*__NOVA_AI_RANGE_START__*/";
    let mock = mock_server.mock(|when, then| {
        when.method(POST)
            .path("/complete")
            .body_contains(expected_marker)
            .body_contains("/*__NOVA_AI_RANGE_END__*/");
        then.status(200).json_body(completion_response.clone());
    });

    let config_path = root.join("nova.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.provider]
kind = "http"
url = "{}/complete"
model = "default"

[ai.privacy]
local_only = true
"#,
            mock_server.base_url()
        ),
    )
    .expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        // Ensure a developer's environment doesn't disable AI for this test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // Initialize with rootUri so file paths are workspace-relative.
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::initialize_request_with_root_uri(1, root_uri),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &crate::support::initialized_notification());

    // Open document (in-memory overlay should be used for prompts + patch validation).
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::did_open_notification(file_uri.clone(), "java", 1, text),
    );

    let progress_token = NumberOrString::String("progress-token".to_string());
    let progress_token_value = crate::support::json_value(progress_token.clone());
    let arguments = vec![serde_json::Value::Object({
        let mut arg = serde_json::Map::new();
        arg.insert(
            "method_signature".to_string(),
            serde_json::Value::String("int add(int a, int b)".to_string()),
        );
        arg.insert("context".to_string(), serde_json::Value::Null);
        arg.insert(
            "uri".to_string(),
            serde_json::Value::String(file_uri.clone()),
        );
        arg.insert(
            "range".to_string(),
            crate::support::json_value(Range::new(selection_start, selection_end)),
        );
        arg
    })];
    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            ExecuteCommandParams {
                command: nova_ide::COMMAND_GENERATE_METHOD_BODY.to_string(),
                arguments,
                work_done_progress_params: WorkDoneProgressParams {
                    work_done_token: Some(progress_token.clone()),
                },
            },
            2,
            "workspace/executeCommand",
        ),
    );

    // Collect server->client messages (progress + workspace/applyEdit) until the command response.
    let (notifications, exec_resp) = crate::support::drain_notifications_until_id(&mut stdout, 2);

    // Ensure we requested workspace edits.
    let apply_edit = notifications
        .iter()
        .find(|msg| msg.get("method").and_then(|m| m.as_str()) == Some("workspace/applyEdit"))
        .expect("expected workspace/applyEdit request");
    let applied_edit = apply_edit
        .get("params")
        .and_then(|p| p.get("edit"))
        .expect("applyEdit params.edit");
    let new_text = applied_edit
        .get("changes")
        .and_then(|c| c.get(&file_uri))
        .and_then(|v| v.as_array())
        .and_then(|edits| edits.first())
        .and_then(|e| e.get("newText"))
        .and_then(|t| t.as_str())
        .expect("workspace edit newText");
    assert!(
        new_text.contains("return a + b;"),
        "expected generated method body in new text: {new_text}"
    );

    assert!(
        exec_resp.get("error").is_none(),
        "expected executeCommand success, got: {exec_resp:#?}"
    );
    assert!(
        exec_resp.get("error").is_none(),
        "expected executeCommand success, got: {exec_resp:#?}"
    );
    assert!(
        exec_resp.get("result").map_or(false, |v| v.is_null()),
        "expected executeCommand result null, got: {exec_resp:#?}"
    );

    // Work-done progress should begin and end.
    let progress_kinds: Vec<String> = notifications
        .iter()
        .filter(|msg| msg.get("method").and_then(|m| m.as_str()) == Some("$/progress"))
        .filter(|msg| msg.get("params").and_then(|p| p.get("token")) == Some(&progress_token_value))
        .filter_map(|msg| {
            msg.get("params")
                .and_then(|p| p.get("value"))
                .and_then(|v| v.get("kind"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect();
    assert!(progress_kinds.contains(&"begin".to_string()));
    assert!(progress_kinds.contains(&"end".to_string()));

    mock.assert_hits(1);

    // shutdown + exit
    write_jsonrpc_message(&mut stdin, &crate::support::shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &crate::support::exit_notification());
    drop(stdin);
    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_execute_command_generate_method_body_without_root_uri_applies_workspace_edit() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");

    // Use spaces in the directory + filename to ensure percent-encoded URIs round-trip through the
    // patch path normalization logic when `initialize.rootUri` is missing.
    let workspace_dir = temp.path().join("My Workspace");
    std::fs::create_dir_all(&workspace_dir).expect("create workspace dir");
    let file_path = workspace_dir.join("My File.java");
    let file_uri = uri_for_path(&file_path);

    // Keep the file on a single line so we can compute offsets precisely.
    let text = "class Main { int add(int a, int b) { } }\n";
    std::fs::write(&file_path, text).expect("write file");

    // Compute selection range for the method snippet.
    let method_start = text.find("int add").expect("method start");
    let open = text[method_start..].find('{').expect("method brace") + method_start;
    let close = text[open..].find('}').expect("method close") + open;
    let selection_start = TextPos::new(text)
        .lsp_position(method_start)
        .expect("selection start");
    let selection_end = TextPos::new(text)
        .lsp_position(close + 1)
        .expect("selection end");

    // Insert range between braces for the mock patch response.
    let insert_start = TextPos::new(text)
        .lsp_position(open + 1)
        .expect("insert start");
    let insert_end = TextPos::new(text).lsp_position(close).expect("insert end");

    // With no rootUri, patch paths fall back to the basename (`My File.java`).
    let patch = serde_json::to_string(&serde_json::Value::Object({
        let mut patch = serde_json::Map::new();
        patch.insert(
            "edits".to_string(),
            serde_json::Value::Array(vec![serde_json::Value::Object({
                let mut edit = serde_json::Map::new();
                edit.insert(
                    "file".to_string(),
                    serde_json::Value::String("My File.java".to_string()),
                );
                edit.insert(
                    "range".to_string(),
                    crate::support::json_value(Range::new(insert_start, insert_end)),
                );
                edit.insert(
                    "text".to_string(),
                    serde_json::Value::String(" return a + b; ".to_string()),
                );
                edit
            })]),
        );
        patch
    }))
    .expect("patch json");
    let ai_server = crate::support::TestAiServer::start(serde_json::Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert("completion".to_string(), serde_json::Value::String(patch));
        resp
    }));

    let config_path = temp.path().join("nova.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.provider]
kind = "http"
url = "{}/complete"
model = "default"

[ai.privacy]
local_only = true
"#,
            ai_server.base_url()
        ),
    )
    .expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        // Ensure env vars don't override the config file.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // Intentionally omit rootUri so `patch_root_uri_and_file_rel` uses basename mode.
    write_jsonrpc_message(&mut stdin, &crate::support::initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &crate::support::initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::did_open_notification(file_uri.clone(), "java", 1, text),
    );

    let arguments = vec![serde_json::Value::Object({
        let mut arg = serde_json::Map::new();
        arg.insert(
            "method_signature".to_string(),
            serde_json::Value::String("int add(int a, int b)".to_string()),
        );
        arg.insert("context".to_string(), serde_json::Value::Null);
        arg.insert(
            "uri".to_string(),
            serde_json::Value::String(file_uri.clone()),
        );
        arg.insert(
            "range".to_string(),
            crate::support::json_value(Range::new(selection_start, selection_end)),
        );
        arg
    })];

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            ExecuteCommandParams {
                command: nova_ide::COMMAND_GENERATE_METHOD_BODY.to_string(),
                arguments,
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
            2,
            "workspace/executeCommand",
        ),
    );

    let (notifications, exec_resp) = drain_notifications_until_id(&mut stdout, 2);

    let apply_edit = find_apply_edit_request(&notifications);
    let applied_edit = apply_edit
        .get("params")
        .and_then(|p| p.get("edit"))
        .expect("applyEdit params.edit");
    let new_text = applied_edit
        .get("changes")
        .and_then(|c| c.get(&file_uri))
        .and_then(|v| v.as_array())
        .and_then(|edits| edits.first())
        .and_then(|e| e.get("newText"))
        .and_then(|t| t.as_str())
        .expect("workspace edit newText");
    assert!(
        new_text.contains("return a + b;"),
        "expected generated method body in new text: {new_text}"
    );

    assert!(
        exec_resp.get("error").is_none(),
        "expected executeCommand success, got: {exec_resp:#?}"
    );
    assert!(
        exec_resp.get("result").map_or(false, |v| v.is_null()),
        "expected executeCommand result null, got: {exec_resp:#?}"
    );

    ai_server.assert_hits(1);

    // shutdown + exit
    write_jsonrpc_message(&mut stdin, &crate::support::shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &crate::support::exit_notification());
    drop(stdin);
    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_generate_method_body_refuses_excluded_paths_without_model_call() {
    let _lock = crate::support::stdio_server_lock();

    let ai_server = crate::support::TestAiServer::start(serde_json::Value::Object({
        let mut resp = serde_json::Map::new();
        resp.insert(
            "completion".to_string(),
            serde_json::Value::String("unused in this test".to_string()),
        );
        resp
    }));

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = uri_for_path(root);

    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");
    let file_path = src_dir.join("Main.java");
    let text = "class Main { int add(int a, int b) { } }\n";
    std::fs::write(&file_path, text).expect("write Main.java");
    let file_uri = uri_for_path(&file_path);

    let config_path = root.join("nova.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.provider]
kind = "http"
url = "{}/complete"
model = "default"

[ai.privacy]
local_only = true
excluded_paths = ["src/Main.java"]
"#,
            ai_server.base_url()
        ),
    )
    .expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        // Ensure a developer's environment doesn't disable AI for this test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::initialize_request_with_root_uri(1, root_uri),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &crate::support::initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::did_open_notification(file_uri.clone(), "java", 1, text),
    );

    let selection_start = TextPos::new(text)
        .lsp_position(text.find("int add").unwrap())
        .unwrap();
    let selection_end = TextPos::new(text)
        .lsp_position(text.find('}').unwrap() + 1)
        .unwrap();

    let arguments = vec![serde_json::Value::Object({
        let mut arg = serde_json::Map::new();
        arg.insert(
            "method_signature".to_string(),
            serde_json::Value::String("int add(int a, int b)".to_string()),
        );
        arg.insert("context".to_string(), serde_json::Value::Null);
        arg.insert(
            "uri".to_string(),
            serde_json::Value::String(file_uri.clone()),
        );
        arg.insert(
            "range".to_string(),
            crate::support::json_value(Range::new(selection_start, selection_end)),
        );
        arg
    })];

    write_jsonrpc_message(
        &mut stdin,
        &crate::support::jsonrpc_request(
            ExecuteCommandParams {
                command: nova_ide::COMMAND_GENERATE_METHOD_BODY.to_string(),
                arguments,
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
            2,
            "workspace/executeCommand",
        ),
    );

    let resp = read_response_with_id(&mut stdout, 2);
    assert!(
        resp.get("error").is_some(),
        "expected executeCommand to fail for excluded paths, got: {resp:#?}"
    );
    assert_eq!(
        ai_server.hits(),
        0,
        "excluded_paths should prevent any model call"
    );

    write_jsonrpc_message(&mut stdin, &crate::support::shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &crate::support::exit_notification());
    drop(stdin);
    let status = child.wait().expect("wait");
    assert!(status.success());
}
