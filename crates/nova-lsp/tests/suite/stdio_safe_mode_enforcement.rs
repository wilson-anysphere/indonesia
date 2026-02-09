use crate::support;
use lsp_types::Range;
use nova_lsp::text_pos::TextPos;
use serde_json::json;
use std::io::BufReader;
use std::process::{Command, Stdio};
use tempfile::TempDir;

#[test]
fn stdio_server_enforces_safe_mode_across_custom_endpoints() {
    const SAFE_MODE_MESSAGE: &str = "Nova is running in safe-mode (previous request crashed or timed out). Only `nova/bugReport`, `nova/metrics`, `nova/resetMetrics`, and `nova/safeModeStatus` are available for now.";

    let _guard = support::stdio_server_lock();

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        // Test hook (debug builds only): force safe-mode without relying on a real
        // watchdog timeout/panic.
        .env("NOVA_LSP_TEST_FORCE_SAFE_MODE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "capabilities": {} }
        }),
    );
    let _initialize_resp = support::read_response_with_id(&mut stdout, 1);
    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    // Allowlisted endpoints should still succeed.
    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "nova/safeModeStatus", "params": null }),
    );
    let safe_mode_resp = support::read_response_with_id(&mut stdout, 2);
    assert!(
        safe_mode_resp.get("error").is_none(),
        "expected success, got: {safe_mode_resp:?}"
    );
    assert_eq!(
        safe_mode_resp
            .get("result")
            .and_then(|v| v.get("enabled"))
            .and_then(|v| v.as_bool()),
        Some(true)
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "nova/metrics", "params": null }),
    );
    let metrics_resp = support::read_response_with_id(&mut stdout, 3);
    assert!(
        metrics_resp.get("error").is_none(),
        "expected success, got: {metrics_resp:?}"
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "nova/resetMetrics", "params": null }),
    );
    let reset_metrics_resp = support::read_response_with_id(&mut stdout, 4);
    assert!(
        reset_metrics_resp.get("error").is_none(),
        "expected success, got: {reset_metrics_resp:?}"
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 5, "method": "nova/bugReport", "params": null }),
    );
    let bug_report_resp = support::read_response_with_id(&mut stdout, 5);
    assert!(
        bug_report_resp.get("error").is_none(),
        "expected success, got: {bug_report_resp:?}"
    );
    assert!(
        bug_report_resp
            .get("result")
            .and_then(|v| v.get("path"))
            .and_then(|v| v.as_str())
            .is_some(),
        "expected bug report path, got: {bug_report_resp:?}"
    );

    // Non-allowlisted endpoints should be blocked, even if their params are invalid.
    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 6, "method": "nova/memoryStatus", "params": null }),
    );
    let memory_status_resp = support::read_response_with_id(&mut stdout, 6);
    assert_eq!(
        memory_status_resp
            .get("error")
            .and_then(|v| v.get("code"))
            .and_then(|v| v.as_i64()),
        Some(-32603),
        "expected safe-mode error, got: {memory_status_resp:?}"
    );
    assert_eq!(
        memory_status_resp
            .get("error")
            .and_then(|v| v.get("message"))
            .and_then(|v| v.as_str()),
        Some(SAFE_MODE_MESSAGE)
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 7, "method": nova_lsp::SEMANTIC_SEARCH_INDEX_STATUS_METHOD, "params": null }),
    );
    let index_status_resp = support::read_response_with_id(&mut stdout, 7);
    assert_eq!(
        index_status_resp
            .get("error")
            .and_then(|v| v.get("code"))
            .and_then(|v| v.as_i64()),
        Some(-32603),
        "expected safe-mode error, got: {index_status_resp:?}"
    );
    assert_eq!(
        index_status_resp
            .get("error")
            .and_then(|v| v.get("message"))
            .and_then(|v| v.as_str()),
        Some(SAFE_MODE_MESSAGE)
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 8, "method": nova_lsp::SEMANTIC_SEARCH_REINDEX_METHOD, "params": null }),
    );
    let reindex_resp = support::read_response_with_id(&mut stdout, 8);
    assert_eq!(
        reindex_resp
            .get("error")
            .and_then(|v| v.get("code"))
            .and_then(|v| v.as_i64()),
        Some(-32603),
        "expected safe-mode error, got: {reindex_resp:?}"
    );
    assert_eq!(
        reindex_resp
            .get("error")
            .and_then(|v| v.get("message"))
            .and_then(|v| v.as_str()),
        Some(SAFE_MODE_MESSAGE)
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 9, "method": "nova/extensions/status", "params": null }),
    );
    let extensions_status_resp = support::read_response_with_id(&mut stdout, 9);
    assert_eq!(
        extensions_status_resp
            .get("error")
            .and_then(|v| v.get("code"))
            .and_then(|v| v.as_i64()),
        Some(-32603),
        "expected safe-mode error, got: {extensions_status_resp:?}"
    );
    assert_eq!(
        extensions_status_resp
            .get("error")
            .and_then(|v| v.get("message"))
            .and_then(|v| v.as_str()),
        Some(SAFE_MODE_MESSAGE)
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 10, "method": "nova/extensions/navigation", "params": null }),
    );
    let extensions_navigation_resp = support::read_response_with_id(&mut stdout, 10);
    assert_eq!(
        extensions_navigation_resp
            .get("error")
            .and_then(|v| v.get("code"))
            .and_then(|v| v.as_i64()),
        Some(-32603),
        "expected safe-mode error, got: {extensions_navigation_resp:?}"
    );
    assert_eq!(
        extensions_navigation_resp
            .get("error")
            .and_then(|v| v.get("message"))
            .and_then(|v| v.as_str()),
        Some(SAFE_MODE_MESSAGE)
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 11, "method": "nova/java/organizeImports", "params": null }),
    );
    let organize_imports_resp = support::read_response_with_id(&mut stdout, 11);
    assert_eq!(
        organize_imports_resp
            .get("error")
            .and_then(|v| v.get("code"))
            .and_then(|v| v.as_i64()),
        Some(-32603),
        "expected safe-mode error, got: {organize_imports_resp:?}"
    );
    assert_eq!(
        organize_imports_resp
            .get("error")
            .and_then(|v| v.get("message"))
            .and_then(|v| v.as_str()),
        Some(SAFE_MODE_MESSAGE)
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 12, "method": "nova/ai/explainError", "params": null }),
    );
    let explain_error_resp = support::read_response_with_id(&mut stdout, 12);
    assert_eq!(
        explain_error_resp
            .get("error")
            .and_then(|v| v.get("code"))
            .and_then(|v| v.as_i64()),
        Some(-32603),
        "expected safe-mode error, got: {explain_error_resp:?}"
    );
    assert_eq!(
        explain_error_resp
            .get("error")
            .and_then(|v| v.get("message"))
            .and_then(|v| v.as_str()),
        Some(SAFE_MODE_MESSAGE)
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 13, "method": "nova/completion/more", "params": null }),
    );
    let completion_more_resp = support::read_response_with_id(&mut stdout, 13);
    assert_eq!(
        completion_more_resp
            .get("error")
            .and_then(|v| v.get("code"))
            .and_then(|v| v.as_i64()),
        Some(-32603),
        "expected safe-mode error, got: {completion_more_resp:?}"
    );
    assert_eq!(
        completion_more_resp
            .get("error")
            .and_then(|v| v.get("message"))
            .and_then(|v| v.as_str()),
        Some(SAFE_MODE_MESSAGE)
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 14, "method": "shutdown" }),
    );
    let _shutdown_resp = support::read_response_with_id(&mut stdout, 14);
    support::write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_hides_ai_code_actions_in_safe_mode() {
    let _guard = support::stdio_server_lock();

    let ai_server = support::TestAiServer::start(json!({ "completion": "mock" }));
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
url = "{}/complete"
model = "default"

[ai.privacy]
local_only = true
"#,
            ai_server.base_url()
        ),
    )
    .expect("write config");

    let file_path = temp.path().join("Main.java");
    let file_uri = support::file_uri_string(&file_path);
    let text = "class Test { Foo foo() { } }";
    std::fs::write(&file_path, text).expect("write Main.java");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        // Test hook (debug builds only): force safe-mode without relying on a real
        // watchdog timeout/panic.
        .env("NOVA_LSP_TEST_FORCE_SAFE_MODE", "1")
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

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "capabilities": {} }
        }),
    );
    let _initialize_resp = support::read_response_with_id(&mut stdout, 1);
    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": file_uri,
                    "languageId": "java",
                    "version": 1,
                    "text": text
                }
            }
        }),
    );

    // Request code actions on an empty method body selection (would normally offer AI code edits).
    let selection = "Foo foo() { }";
    let start_offset = text.find(selection).expect("selection start");
    let end_offset = start_offset + selection.len();
    let pos = nova_lsp::text_pos::TextPos::new(text);
    let range = Range {
        start: pos.lsp_position(start_offset).expect("start"),
        end: pos.lsp_position(end_offset).expect("end"),
    };

    let foo_end = start_offset + "Foo".len();
    let foo_range = Range {
        start: pos.lsp_position(start_offset).expect("foo start"),
        end: pos.lsp_position(foo_end).expect("foo end"),
    };

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/codeAction",
            "params": {
                "textDocument": { "uri": support::file_uri_string(&file_path) },
                "range": range,
                "context": {
                    "diagnostics": [{
                        "range": foo_range,
                        "code": "unresolved-type",
                        "message": "unresolved type `Foo`"
                    }]
                }
            }
        }),
    );

    let code_actions_resp = support::read_response_with_id(&mut stdout, 2);
    let actions = code_actions_resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");

    // Non-AI code actions should remain available in safe-mode.
    assert!(
        actions
            .iter()
            .any(|a| a.get("title").and_then(|t| t.as_str()) == Some("Create class 'Foo'")),
        "expected unresolved-type quick fix to remain available, got: {actions:?}"
    );

    // AI code actions should be hidden in safe-mode because `nova/ai/*` endpoints are disabled.
    for kind in [nova_ide::CODE_ACTION_KIND_EXPLAIN, nova_ide::CODE_ACTION_KIND_AI_GENERATE, nova_ide::CODE_ACTION_KIND_AI_TESTS] {
        assert!(
            !actions
                .iter()
                .any(|a| a.get("kind").and_then(|k| k.as_str()) == Some(kind)),
            "expected no AI code actions of kind {kind} in safe-mode, got: {actions:?}"
        );
    }

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = support::read_response_with_id(&mut stdout, 3);
    support::write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_blocks_ai_workspace_execute_command_in_safe_mode() {
    const SAFE_MODE_MESSAGE: &str = "Nova is running in safe-mode (previous request crashed or timed out). Only `nova/bugReport`, `nova/metrics`, `nova/resetMetrics`, and `nova/safeModeStatus` are available for now.";

    let _guard = support::stdio_server_lock();

    // Configure a test AI provider; safe-mode should prevent any hits.
    let completion = r#"{"edits":[{"file":"Main.java","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}},"text":"// AI INSERT\n"}]}"#;
    let ai_server = support::TestAiServer::start(json!({ "completion": completion }));
    let endpoint = format!("{}/complete", ai_server.base_url());

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = support::file_uri(root);

    let file_path = root.join("Main.java");
    let source = concat!(
        "class Main {\n",
        "    int add(int a, int b) {\n",
        "    }\n",
        "\n",
        "    // TESTS_PLACEHOLDER\n",
        "}\n",
    );
    std::fs::write(&file_path, source).expect("write Main.java");
    let file_uri = support::file_uri(&file_path);

    // Build a selection range that includes the empty `add` method snippet.
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
    let method_range = Range::new(selection_start, selection_end);

    let placeholder_line = "    // TESTS_PLACEHOLDER";
    let placeholder_start = source.find(placeholder_line).expect("placeholder start");
    let placeholder_end = placeholder_start + placeholder_line.len();
    let tests_range = Range::new(
        pos.lsp_position(placeholder_start).expect("placeholder start pos"),
        pos.lsp_position(placeholder_end).expect("placeholder end pos"),
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .current_dir(root)
        // Test hook (debug builds only): force safe-mode without relying on a real
        // watchdog timeout/panic.
        .env("NOVA_LSP_TEST_FORCE_SAFE_MODE", "1")
        // Ensure no ambient config affects this test; we want to exercise the env-var AI wiring.
        .env_remove("NOVA_CONFIG")
        .env_remove("NOVA_CONFIG_PATH")
        // Legacy env-var AI wiring (used here intentionally).
        .env("NOVA_AI_PROVIDER", "http")
        .env("NOVA_AI_ENDPOINT", &endpoint)
        .env("NOVA_AI_MODEL", "default")
        .env("NOVA_AI_LOCAL_ONLY", "1")
        // Keep prompts stable (avoid identifier anonymization in case the request slips through).
        .env("NOVA_AI_ANONYMIZE_IDENTIFIERS", "0")
        // Ensure a developer's environment doesn't disable AI unexpectedly.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "rootUri": root_uri, "capabilities": {} }
        }),
    );
    let _initialize_resp = support::read_response_with_id(&mut stdout, 1);
    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    // Open the document so AI commands can resolve `uri` + `range` (even though safe-mode should
    // block them before parsing args).
    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": { "textDocument": { "uri": file_uri.clone(), "languageId": "java", "version": 1, "text": source } }
        }),
    );

    // Force safe-mode on explicitly so the test remains meaningful even if the executeCommand
    // handler regresses and stops calling `guard_method`.
    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "nova/safeModeStatus", "params": null }),
    );
    let safe_mode_resp = support::read_response_with_id(&mut stdout, 2);
    assert_eq!(
        safe_mode_resp
            .get("result")
            .and_then(|v| v.get("enabled"))
            .and_then(|v| v.as_bool()),
        Some(true),
        "expected safe-mode enabled, got: {safe_mode_resp:?}"
    );

    let commands = [
        (
            nova_ide::COMMAND_EXPLAIN_ERROR,
            json!({
                "diagnosticMessage": "cannot find symbol",
                "code": "Main m = new Main(); m.add(1, 2);",
                "uri": file_uri.clone(),
                "range": method_range.clone(),
            }),
        ),
        (
            nova_ide::COMMAND_GENERATE_METHOD_BODY,
            json!({
                "methodSignature": "int add(int a, int b)",
                "context": null,
                "uri": file_uri.clone(),
                "range": method_range.clone(),
            }),
        ),
        (
            nova_ide::COMMAND_GENERATE_TESTS,
            json!({
                "target": "int add(int a, int b)",
                "context": null,
                "uri": file_uri.clone(),
                "range": tests_range,
            }),
        ),
    ];

    for (idx, (command, arg)) in commands.into_iter().enumerate() {
        let request_id = 3 + idx as i64;
        support::write_jsonrpc_message(
            &mut stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "workspace/executeCommand",
                "params": { "command": command, "arguments": [arg] }
            }),
        );

        let mut saw_apply_edit = false;
        let response = loop {
            let msg = support::read_jsonrpc_message(&mut stdout);
            if msg.get("method").and_then(|v| v.as_str()) == Some("workspace/applyEdit") {
                saw_apply_edit = true;
                let id = msg.get("id").cloned().expect("applyEdit id");
                support::write_jsonrpc_message(
                    &mut stdin,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "applied": true }
                    }),
                );
                continue;
            }
            if msg.get("method").is_none()
                && msg.get("id").and_then(|v| v.as_i64()) == Some(request_id)
            {
                break msg;
            }
        };

        assert!(
            !saw_apply_edit,
            "expected no workspace/applyEdit request in safe-mode, but saw one: {response:?}"
        );
        assert_eq!(
            response
                .get("error")
                .and_then(|v| v.get("code"))
                .and_then(|v| v.as_i64()),
            Some(-32603),
            "expected safe-mode error, got: {response:?}"
        );
        assert_eq!(
            response
                .get("error")
                .and_then(|v| v.get("message"))
                .and_then(|v| v.as_str()),
            Some(SAFE_MODE_MESSAGE),
            "expected safe-mode error message, got: {response:?}"
        );
    }

    ai_server.assert_hits(0);

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 100, "method": "shutdown" }),
    );
    let _shutdown_resp = support::read_response_with_id(&mut stdout, 100);
    support::write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
