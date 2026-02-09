use lsp_types::{CompletionList, Position, Range};
use nova_lsp::MoreCompletionsResult;
use nova_lsp::text_pos::TextPos;
use serde_json::json;
use std::fs;
use std::io::BufReader;
use std::process::{Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;

use crate::support;

fn run_completion_request_with_env(env_key: &str, env_value: &str, completion_ranking: bool) {
    let _lock = support::stdio_server_lock();
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

    let ai_server = support::TestAiServer::start(json!({ "completion": completion_payload }));
    let endpoint = format!("{}/complete", ai_server.base_url());

    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    let completion_ranking_feature = completion_ranking
        .then_some("completion_ranking = true\n")
        .unwrap_or_default();
    fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.features]
multi_token_completion = true
{completion_ranking_feature}

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
    let uri = support::file_uri(&file_path);

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
        // Ensure the only overrides in effect are the ones explicitly under test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env_remove("NOVA_DISABLE_AI_CODE_ACTIONS")
        .env_remove("NOVA_DISABLE_AI_CODE_REVIEW")
        .env_remove("NOVA_AI_COMPLETIONS_MAX_ITEMS")
        .env(env_key, env_value)
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
            "params": { "textDocument": { "uri": uri, "languageId": "java", "version": 1, "text": source } }
        }),
    );

    let cursor = Position::new(8, 15); // end of "stream."
    support::write_jsonrpc_message(
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
    let completion_resp = support::read_response_with_id(&mut stdout, 2);
    let completion_result = completion_resp
        .get("result")
        .cloned()
        .expect("completion result");
    let list: CompletionList =
        serde_json::from_value(completion_result).expect("decode completion list");
    assert_eq!(
        list.is_incomplete, false,
        "expected no AI completions when {env_key}={env_value}"
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

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "nova/completion/more",
            "params": { "context_id": context_id }
        }),
    );
    let more_resp = support::read_response_with_id(&mut stdout, 3);
    let more_result = more_resp.get("result").cloned().expect("result");
    let more: MoreCompletionsResult =
        serde_json::from_value(more_result).expect("decode more completions");
    assert!(!more.is_incomplete);
    assert!(more.items.is_empty());

    // Best-effort: give any background tasks a chance to misbehave and hit the provider.
    std::thread::sleep(Duration::from_millis(100));

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "shutdown" }),
    );
    let _shutdown_resp = support::read_response_with_id(&mut stdout, 4);
    support::write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());

    ai_server.assert_hits(0);
}

fn run_completion_ranking_request_with_optional_env(env: Option<(&str, &str)>) -> usize {
    let _lock = support::stdio_server_lock();
    // Respond with a valid JSON array so completion ranking can parse it.
    let ranking_payload = "[0]";
    let ai_server = support::TestAiServer::start(json!({ "completion": ranking_payload }));
    let endpoint = format!("{}/complete", ai_server.base_url());

    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.features]
completion_ranking = true
multi_token_completion = false

[ai.timeouts]
completion_ranking_ms = 1000

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
    let uri = support::file_uri(&file_path);

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nova-lsp"));
    cmd.arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        // The test config file should be authoritative; clear any legacy env-var AI wiring that
        // could override `--config` (common in developer shells).
        .env_remove("NOVA_AI_PROVIDER")
        .env_remove("NOVA_AI_ENDPOINT")
        .env_remove("NOVA_AI_MODEL")
        .env_remove("NOVA_AI_API_KEY")
        .env_remove("NOVA_AI_AUDIT_LOGGING")
        // Ensure the only overrides in effect are the ones explicitly under test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env_remove("NOVA_AI_COMPLETIONS_MAX_ITEMS");

    if let Some((key, value)) = env {
        cmd.env(key, value);
    }

    let mut child = cmd
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
            "params": { "textDocument": { "uri": uri, "languageId": "java", "version": 1, "text": source } }
        }),
    );

    let cursor = Position::new(8, 15); // end of "stream."
    support::write_jsonrpc_message(
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
    let completion_resp = support::read_response_with_id(&mut stdout, 2);
    let completion_result = completion_resp
        .get("result")
        .cloned()
        .expect("completion result");
    let list: CompletionList =
        serde_json::from_value(completion_result).expect("decode completion list");
    assert!(
        list.items.len() > 1,
        "expected multiple completion items so ranking can run"
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = support::read_response_with_id(&mut stdout, 3);
    support::write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());

    ai_server.hits()
}

fn run_completion_request_with_audit_logging_and_env_override(
    env_key: &str,
    env_value: &str,
    completion_ranking: bool,
) {
    let _lock = support::stdio_server_lock();
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
    let ai_server = support::TestAiServer::start(json!({ "completion": completion_payload }));
    let endpoint = format!("{}/complete", ai_server.base_url());

    // Start with AI disabled in the config file so that `NOVA_AI_AUDIT_LOGGING=1` is the only
    // reason AI would be enabled.
    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    let completion_ranking_feature = completion_ranking
        .then_some("completion_ranking = true\n")
        .unwrap_or_default();
    fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = false

[ai.features]
multi_token_completion = true
{completion_ranking_feature}

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
    let uri = support::file_uri(&file_path);

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
        // Ensure the only overrides in effect are the ones explicitly under test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env_remove("NOVA_DISABLE_AI_CODE_ACTIONS")
        .env_remove("NOVA_DISABLE_AI_CODE_REVIEW")
        .env_remove("NOVA_AI_COMPLETIONS_MAX_ITEMS")
        // Audit logging forces `ai.enabled=true` in the legacy env-var AI mode; the disable env vars
        // must always win over this special case.
        .env("NOVA_AI_AUDIT_LOGGING", "1")
        .env(env_key, env_value)
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
            "params": { "textDocument": { "uri": uri, "languageId": "java", "version": 1, "text": source } }
        }),
    );

    let cursor = Position::new(8, 15); // end of "stream."
    support::write_jsonrpc_message(
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
    let completion_resp = support::read_response_with_id(&mut stdout, 2);
    let completion_result = completion_resp
        .get("result")
        .cloned()
        .expect("completion result");
    let list: CompletionList =
        serde_json::from_value(completion_result).expect("decode completion list");
    assert_eq!(
        list.is_incomplete, false,
        "expected no AI completions when {env_key}={env_value} (even with NOVA_AI_AUDIT_LOGGING=1)"
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

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "nova/completion/more",
            "params": { "context_id": context_id }
        }),
    );
    let more_resp = support::read_response_with_id(&mut stdout, 3);
    let more_result = more_resp.get("result").cloned().expect("result");
    let more: MoreCompletionsResult =
        serde_json::from_value(more_result).expect("decode more completions");
    assert!(!more.is_incomplete);
    assert!(more.items.is_empty());

    // Best-effort: give any background tasks a chance to misbehave and hit the provider.
    std::thread::sleep(Duration::from_millis(100));

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "shutdown" }),
    );
    let _shutdown_resp = support::read_response_with_id(&mut stdout, 4);
    support::write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());

    ai_server.assert_hits(0);
}

fn run_ai_explain_error_request_with_env(env_key: &str, env_value: &str) {
    let _lock = support::stdio_server_lock();
    let ai_server = support::TestAiServer::start(json!({ "completion": "mock explanation" }));
    let endpoint = format!("{}/complete", ai_server.base_url());

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        // Configure AI via the legacy env-var provider wiring, but also set the override env var
        // under test. The server should never hit the provider endpoint when AI is force-disabled.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env_remove("NOVA_DISABLE_AI_CODE_ACTIONS")
        .env_remove("NOVA_DISABLE_AI_CODE_REVIEW")
        .env_remove("NOVA_AI_COMPLETIONS_MAX_ITEMS")
        .env("NOVA_AI_PROVIDER", "http")
        .env("NOVA_AI_ENDPOINT", &endpoint)
        .env("NOVA_AI_MODEL", "default")
        .env(env_key, env_value)
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
            "params": { "textDocument": { "uri": "file:///Test.java", "languageId": "java", "version": 1, "text": "class Test { void run() { unknown(); } }" } }
        }),
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/ai/explainError",
            "params": {
                "diagnosticMessage": "cannot find symbol",
                "code": "unknown()"
            }
        }),
    );
    let explain_resp = support::read_response_with_id(&mut stdout, 2);
    let error = explain_resp
        .get("error")
        .and_then(|v| v.as_object())
        .expect("expected error response when AI is disabled");
    assert_eq!(
        error
            .get("code")
            .and_then(|v| v.as_i64())
            .expect("error.code"),
        -32600,
        "expected AI request to fail with 'AI is not configured' when {env_key}={env_value}"
    );
    assert_eq!(
        error
            .get("data")
            .and_then(|v| v.get("kind"))
            .and_then(|v| v.as_str()),
        Some("notConfigured"),
        "expected error.data.kind == \"notConfigured\" when AI is disabled, got: {explain_resp:#?}"
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = support::read_response_with_id(&mut stdout, 3);
    support::write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());

    ai_server.assert_hits(0);
}

fn run_ai_code_action_request_with_env_override(env_key: &str, env_value: &str) {
    let _lock = support::stdio_server_lock();
    let ai_server = support::TestAiServer::start(json!({ "completion": "mock" }));
    let endpoint = format!("{}/complete", ai_server.base_url());

    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.features]
code_actions = true
explain_errors = true

[ai.provider]
kind = "http"
url = "{endpoint}"
model = "default"
"#
        ),
    )
    .expect("write config");

    let file_path = temp.path().join("Main.java");
    let file_uri = support::file_uri_string(&file_path);
    let text = "class Test { void foo() { } }";
    fs::write(&file_path, text).expect("write Main.java");

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
        // Ensure the only overrides in effect are the ones explicitly under test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env_remove("NOVA_DISABLE_AI_CODE_ACTIONS")
        .env_remove("NOVA_DISABLE_AI_CODE_REVIEW")
        .env_remove("NOVA_AI_COMPLETIONS_MAX_ITEMS")
        .env(env_key, env_value)
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

    let selection = "void foo() { }";
    let start_offset = text.find(selection).expect("selection start");
    let end_offset = start_offset + selection.len();
    let pos = TextPos::new(text);
    let range = Range {
        start: pos.lsp_position(start_offset).expect("start"),
        end: pos.lsp_position(end_offset).expect("end"),
    };

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/codeAction",
            "params": {
                "textDocument": { "uri": file_uri },
                "range": range,
                "context": { "diagnostics": [] }
            }
        }),
    );
    let code_actions_resp = support::read_response_with_id(&mut stdout, 2);
    let actions = code_actions_resp
        .get("result")
        .and_then(|v| v.as_array())
        .expect("code actions array");

    for forbidden in ["Explain this error", "Generate method body with AI", "Generate tests with AI"]
    {
        assert!(
            !actions
                .iter()
                .any(|a| a.get("title").and_then(|t| t.as_str()) == Some(forbidden)),
            "expected `{forbidden}` to be hidden when {env_key}={env_value}"
        );
    }

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "nova/ai/explainError",
            "params": {
                "diagnosticMessage": "cannot find symbol",
                "uri": file_uri,
                "range": { "start": { "line": range.start.line, "character": range.start.character }, "end": { "line": range.end.line, "character": range.end.character } }
            }
        }),
    );
    let resp = support::read_response_with_id(&mut stdout, 3);
    let error = resp
        .get("error")
        .and_then(|v| v.as_object())
        .expect("expected error response");
    assert_eq!(error.get("code").and_then(|v| v.as_i64()), Some(-32600));
    assert_eq!(
        error
            .get("data")
            .and_then(|v| v.get("kind"))
            .and_then(|v| v.as_str()),
        Some("disabled"),
        "expected disabled error kind, got: {resp:#?}"
    );
    assert_eq!(
        error
            .get("data")
            .and_then(|v| v.get("feature"))
            .and_then(|v| v.as_str()),
        Some("ai.features.explain_errors"),
        "expected disabled error feature, got: {resp:#?}"
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "nova/ai/generateMethodBody",
            "params": {
                "methodSignature": "void foo()",
                "uri": file_uri,
                "range": { "start": { "line": range.start.line, "character": range.start.character }, "end": { "line": range.end.line, "character": range.end.character } }
            }
        }),
    );
    let resp = support::read_response_with_id(&mut stdout, 4);
    let error = resp
        .get("error")
        .and_then(|v| v.as_object())
        .expect("expected error response");
    assert_eq!(error.get("code").and_then(|v| v.as_i64()), Some(-32600));
    assert_eq!(
        error
            .get("data")
            .and_then(|v| v.get("kind"))
            .and_then(|v| v.as_str()),
        Some("disabled"),
        "expected disabled error kind, got: {resp:#?}"
    );
    assert_eq!(
        error
            .get("data")
            .and_then(|v| v.get("feature"))
            .and_then(|v| v.as_str()),
        Some("ai.features.code_actions"),
        "expected disabled error feature, got: {resp:#?}"
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 5, "method": "shutdown" }),
    );
    let _shutdown_resp = support::read_response_with_id(&mut stdout, 5);
    support::write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
    ai_server.assert_hits(0);
}

fn run_ai_code_review_request_with_env_override(env_key: &str, env_value: &str) {
    let _lock = support::stdio_server_lock();
    let ai_server = support::TestAiServer::start(json!({ "completion": "mock review" }));
    let endpoint = format!("{}/complete", ai_server.base_url());

    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

[ai.features]
code_review = true

[ai.provider]
kind = "http"
url = "{endpoint}"
model = "default"
"#
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
        // Ensure the only overrides in effect are the ones explicitly under test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env_remove("NOVA_DISABLE_AI_CODE_ACTIONS")
        .env_remove("NOVA_DISABLE_AI_CODE_REVIEW")
        .env_remove("NOVA_AI_COMPLETIONS_MAX_ITEMS")
        .env(env_key, env_value)
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
            "id": 2,
            "method": "nova/ai/codeReview",
            "params": { "diff": "diff --git a/Main.java b/Main.java\n--- a/Main.java\n+++ b/Main.java\n@@\n-class Main {}\n+class Main { int x; }\n" }
        }),
    );
    let resp = support::read_response_with_id(&mut stdout, 2);
    let error = resp
        .get("error")
        .and_then(|v| v.as_object())
        .expect("expected error response");
    assert_eq!(error.get("code").and_then(|v| v.as_i64()), Some(-32600));
    assert_eq!(
        error
            .get("data")
            .and_then(|v| v.get("kind"))
            .and_then(|v| v.as_str()),
        Some("disabled"),
        "expected disabled error kind, got: {resp:#?}"
    );
    assert_eq!(
        error
            .get("data")
            .and_then(|v| v.get("feature"))
            .and_then(|v| v.as_str()),
        Some("ai.features.code_review"),
        "expected disabled error feature, got: {resp:#?}"
    );

    support::write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = support::read_response_with_id(&mut stdout, 3);
    support::write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
    ai_server.assert_hits(0);
}

#[test]
fn stdio_server_honors_nova_disable_ai_env_var() {
    run_completion_request_with_env("NOVA_DISABLE_AI", "1", true);
}

#[test]
fn stdio_server_honors_nova_disable_ai_completions_env_var() {
    run_completion_request_with_env("NOVA_DISABLE_AI_COMPLETIONS", "1", true);
}

#[test]
fn stdio_server_completion_ranking_hits_provider_when_enabled() {
    let hits = run_completion_ranking_request_with_optional_env(None);
    assert!(hits > 0, "expected completion ranking to hit AI provider");
}

#[test]
fn stdio_server_honors_nova_disable_ai_completions_env_var_for_completion_ranking() {
    let hits = run_completion_ranking_request_with_optional_env(Some(("NOVA_DISABLE_AI_COMPLETIONS", "1")));
    assert_eq!(
        hits, 0,
        "expected no AI provider hits when NOVA_DISABLE_AI_COMPLETIONS=1 disables ranking"
    );
}

#[test]
fn stdio_server_honors_nova_ai_completions_max_items_env_var() {
    run_completion_request_with_env("NOVA_AI_COMPLETIONS_MAX_ITEMS", "0", false);
}

#[test]
fn stdio_server_honors_nova_disable_ai_env_var_for_ai_requests() {
    run_ai_explain_error_request_with_env("NOVA_DISABLE_AI", "1");
}

#[test]
fn stdio_server_honors_nova_disable_ai_code_actions_env_var() {
    run_ai_code_action_request_with_env_override("NOVA_DISABLE_AI_CODE_ACTIONS", "1");
}

#[test]
fn stdio_server_honors_nova_disable_ai_code_review_env_var() {
    run_ai_code_review_request_with_env_override("NOVA_DISABLE_AI_CODE_REVIEW", "1");
}

#[test]
fn stdio_server_nova_disable_ai_env_var_wins_over_audit_logging() {
    run_completion_request_with_audit_logging_and_env_override("NOVA_DISABLE_AI", "1", true);
}

#[test]
fn stdio_server_nova_disable_ai_completions_env_var_wins_over_audit_logging() {
    run_completion_request_with_audit_logging_and_env_override(
        "NOVA_DISABLE_AI_COMPLETIONS",
        "1",
        true,
    );
}
