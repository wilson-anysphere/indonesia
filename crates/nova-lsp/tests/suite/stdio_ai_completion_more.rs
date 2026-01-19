use lsp_types::{
    CompletionItem, CompletionList, CompletionParams, PartialResultParams, Position,
    TextDocumentIdentifier, TextDocumentPositionParams, WorkDoneProgressParams,
};
use nova_test_utils::apply_lsp_edits;
use serde_json::{Map, Value};
use std::fs;
use std::io::BufReader;
use std::process::{Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;

use crate::support::{
    did_open_notification, exit_notification, file_uri, initialize_request_empty,
    initialized_notification, jsonrpc_request, read_response_with_id, shutdown_request,
    write_jsonrpc_message,
};

fn start_test_ai_completion_server(completion_payload: &str) -> crate::support::TestAiServer {
    crate::support::TestAiServer::start(Value::Object({
        let mut resp = Map::new();
        resp.insert(
            "completion".to_string(),
            Value::String(completion_payload.to_string()),
        );
        resp
    }))
}

#[test]
fn stdio_server_supports_ai_multi_token_completion_polling() {
    let _lock = crate::support::stdio_server_lock();
    let completion_payload = r#"
    {
      "completions": [
        {
          "label": "stream chain",
          "insert_text": "filter(x -> true).map(x -> x).collect(Collectors.toList())",
          "format": "plain",
          "additional_edits": [{"add_import":"java.util.stream.Collectors"}],
          "confidence": 0.9
        }
      ]
    }
    "#;

    let ai_server = start_test_ai_completion_server(completion_payload);
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
multi_token_completion = true

[ai.timeouts]
multi_token_completion_ms = 5000

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
    let uri = file_uri(&file_path);

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        // Ensure a developer's environment doesn't disable AI for this test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        // Ensure completion-specific env overrides don't accidentally disable the AI completion
        // background tasks this test is asserting on.
        .env_remove("NOVA_AI_COMPLETIONS_MAX_ITEMS")
        // Ensure legacy AI env vars cannot override the config file.
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

    // 1) initialize
    write_jsonrpc_message(&mut stdin, &initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    // 2) open document
    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(uri.clone(), "java", 1, source),
    );

    // 3) request baseline completions.
    let cursor = Position::new(8, 15); // end of "stream."
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            CompletionParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position: cursor,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                context: None,
            },
            2,
            "textDocument/completion",
        ),
    );
    let completion_resp = read_response_with_id(&mut stdout, 2);
    let completion_result = completion_resp
        .get("result")
        .cloned()
        .expect("completion result");
    let list: CompletionList =
        serde_json::from_value(completion_result).expect("decode completion list");

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

    // 4) poll until AI items ready.
    let mut resolved: Option<(Vec<CompletionItem>, bool)> = None;
    for attempt in 0..50 {
        let request_id = 3 + attempt as i64;
        let mut params = Map::new();
        params.insert("context_id".to_string(), Value::String(context_id.clone()));
        write_jsonrpc_message(
            &mut stdin,
            &jsonrpc_request(Value::Object(params), request_id, "nova/completion/more"),
        );
        let resp = read_response_with_id(&mut stdout, request_id);
        let result = resp.get("result").cloned().expect("result");
        let is_incomplete = result
            .get("is_incomplete")
            .and_then(|v| v.as_bool())
            .expect("is_incomplete");
        let items: Vec<CompletionItem> =
            serde_json::from_value(result.get("items").cloned().expect("items"))
                .expect("decode items");
        if !is_incomplete {
            resolved = Some((items, is_incomplete));
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let (items, _) = resolved.expect("AI completions should resolve");
    assert!(
        !items.is_empty(),
        "expected at least one AI completion item"
    );
    assert!(
        ai_server.hits() > 0,
        "expected at least one AI provider request"
    );

    // 5) Resolve an AI completion item to compute its import additionalTextEdits.
    let unresolved_ai_item = items[0].clone();
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(unresolved_ai_item, 999, "completionItem/resolve"),
    );
    let resolved_resp = read_response_with_id(&mut stdout, 999);
    let resolved_item: CompletionItem = serde_json::from_value(
        resolved_resp
            .get("result")
            .cloned()
            .expect("resolved result"),
    )
    .expect("decode resolved CompletionItem");

    let edits = resolved_item
        .additional_text_edits
        .as_ref()
        .expect("AI completion should include additionalTextEdits after resolve");
    assert!(!edits.is_empty());

    // Apply edits to validate the resulting text.
    let updated = apply_lsp_edits(source, edits);
    let pkg = updated.find("package com.example;").expect("package");
    let list_import = updated
        .find("import java.util.List;")
        .expect("existing import");
    let stream_import = updated
        .find("import java.util.stream.Stream;")
        .expect("existing stream import");
    let collectors_import = updated
        .find("import java.util.stream.Collectors;")
        .expect("new import");
    let class_pos = updated.find("class Foo").expect("class");

    assert!(pkg < list_import);
    assert!(list_import < stream_import);
    assert!(stream_import < collectors_import);
    assert!(collectors_import < class_pos);

    // Ensure we didn't hardcode (0,0); insertion should be after the import block.
    let insert_pos = edits[0].range.start;
    assert_eq!(insert_pos.line, 4);
    assert_eq!(insert_pos.character, 0);

    // 6) shutdown + exit
    write_jsonrpc_message(&mut stdin, &shutdown_request(100));
    let _shutdown_resp = read_response_with_id(&mut stdout, 100);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_does_not_request_ai_completions_when_multi_token_feature_is_disabled() {
    let _lock = crate::support::stdio_server_lock();
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

    let ai_server = start_test_ai_completion_server(completion_payload);
    let endpoint = format!("{}/complete", ai_server.base_url());

    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nova.toml");
    fs::write(
        &config_path,
        format!(
            r#"
[ai]
enabled = true

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
    let uri = file_uri(&file_path);

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        .env_remove("NOVA_AI_COMPLETIONS_MAX_ITEMS")
        // Ensure legacy AI env vars cannot override the config file.
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

    write_jsonrpc_message(&mut stdin, &initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(uri.clone(), "java", 1, source),
    );

    let cursor = Position::new(8, 15); // end of "stream."
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            CompletionParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position: cursor,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                context: None,
            },
            2,
            "textDocument/completion",
        ),
    );
    let completion_resp = read_response_with_id(&mut stdout, 2);
    let completion_result = completion_resp
        .get("result")
        .cloned()
        .expect("completion result");
    let list: CompletionList =
        serde_json::from_value(completion_result).expect("decode completion list");
    assert_eq!(
        list.is_incomplete, false,
        "expected no AI completions when feature is disabled"
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

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            Value::Object({
                let mut params = Map::new();
                params.insert("context_id".to_string(), Value::String(context_id));
                params
            }),
            3,
            "nova/completion/more",
        ),
    );
    let more_resp = read_response_with_id(&mut stdout, 3);
    let more_result = more_resp.get("result").cloned().expect("result");
    assert_eq!(
        more_result.get("is_incomplete").and_then(|v| v.as_bool()),
        Some(false)
    );
    let items: Vec<CompletionItem> =
        serde_json::from_value(more_result.get("items").cloned().expect("items"))
            .expect("decode items");
    assert!(items.is_empty());

    write_jsonrpc_message(&mut stdin, &shutdown_request(4));
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());

    ai_server.assert_hits(0);
}

#[test]
fn stdio_server_does_not_request_ai_completions_when_disabled_by_env() {
    let _lock = crate::support::stdio_server_lock();
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
    let ai_server = start_test_ai_completion_server(completion_payload);
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
multi_token_completion = true

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
    let uri = file_uri(&file_path);

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        .env_remove("NOVA_AI_COMPLETIONS_MAX_ITEMS")
        .env("NOVA_DISABLE_AI_COMPLETIONS", "1")
        .env_remove("NOVA_DISABLE_AI")
        // Ensure legacy AI env vars cannot override the config file.
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

    write_jsonrpc_message(&mut stdin, &initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(uri.clone(), "java", 1, source),
    );

    let cursor = Position::new(8, 15); // end of "stream."
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            CompletionParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position: cursor,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                context: None,
            },
            2,
            "textDocument/completion",
        ),
    );
    let completion_resp = read_response_with_id(&mut stdout, 2);
    let completion_result = completion_resp
        .get("result")
        .cloned()
        .expect("completion result");
    let list: CompletionList =
        serde_json::from_value(completion_result).expect("decode completion list");
    assert_eq!(
        list.is_incomplete, false,
        "expected no AI completions when disabled via env"
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

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            Value::Object({
                let mut params = Map::new();
                params.insert("context_id".to_string(), Value::String(context_id));
                params
            }),
            3,
            "nova/completion/more",
        ),
    );
    let more_resp = read_response_with_id(&mut stdout, 3);
    let more_result = more_resp.get("result").cloned().expect("result");
    assert_eq!(
        more_result.get("is_incomplete").and_then(|v| v.as_bool()),
        Some(false)
    );
    let items: Vec<CompletionItem> =
        serde_json::from_value(more_result.get("items").cloned().expect("items"))
            .expect("decode items");
    assert!(items.is_empty());

    // Give the server a brief chance to issue any unexpected background requests.
    std::thread::sleep(Duration::from_millis(50));
    ai_server.assert_hits(0);

    write_jsonrpc_message(&mut stdin, &shutdown_request(4));
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_does_not_request_ai_completions_when_ai_is_disabled_by_env() {
    let _lock = crate::support::stdio_server_lock();
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
    let ai_server = start_test_ai_completion_server(completion_payload);
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
multi_token_completion = true

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
    let uri = file_uri(&file_path);

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        .env_remove("NOVA_AI_COMPLETIONS_MAX_ITEMS")
        .env("NOVA_DISABLE_AI", "1")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        // Ensure legacy AI env vars cannot override the config file.
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

    write_jsonrpc_message(&mut stdin, &initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(uri.clone(), "java", 1, source),
    );

    let cursor = Position::new(8, 15); // end of "stream."
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            CompletionParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position: cursor,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                context: None,
            },
            2,
            "textDocument/completion",
        ),
    );
    let completion_resp = read_response_with_id(&mut stdout, 2);
    let completion_result = completion_resp
        .get("result")
        .cloned()
        .expect("completion result");
    let list: CompletionList =
        serde_json::from_value(completion_result).expect("decode completion list");
    assert_eq!(
        list.is_incomplete, false,
        "expected no AI completions when AI is disabled via env"
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

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            Value::Object({
                let mut params = Map::new();
                params.insert("context_id".to_string(), Value::String(context_id));
                params
            }),
            3,
            "nova/completion/more",
        ),
    );
    let more_resp = read_response_with_id(&mut stdout, 3);
    let more_result = more_resp.get("result").cloned().expect("result");
    assert_eq!(
        more_result.get("is_incomplete").and_then(|v| v.as_bool()),
        Some(false)
    );
    let items: Vec<CompletionItem> =
        serde_json::from_value(more_result.get("items").cloned().expect("items"))
            .expect("decode items");
    assert!(items.is_empty());

    // Give the server a brief chance to issue any unexpected background requests.
    std::thread::sleep(Duration::from_millis(50));
    ai_server.assert_hits(0);

    write_jsonrpc_message(&mut stdin, &shutdown_request(4));
    let _shutdown_resp = read_response_with_id(&mut stdout, 4);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
