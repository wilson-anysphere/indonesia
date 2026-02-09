use serde_json::json;
use std::io::BufReader;
use std::process::{Command, Stdio};

use tempfile::TempDir;

use crate::support::{file_uri_string, read_response_with_id, stdio_server_lock, write_jsonrpc_message};

#[test]
fn semantic_search_config_reload_indexes_open_docs_when_workspace_indexing_unavailable() {
    let _lock = stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = file_uri_string(root);

    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");

    let zebra_path = src_dir.join("UsesZebra.java");
    let other_path = src_dir.join("Other.java");

    let zebra_text = r#"class UsesZebra { String token = "zebraToken"; }"#;
    let other_text = r#"class Other { String token = "otherToken"; }"#;

    // Create on-disk files for completeness (open docs are what populate the index in this test).
    std::fs::write(&zebra_path, zebra_text).expect("write UsesZebra.java");
    std::fs::write(&other_path, other_text).expect("write Other.java");

    // Start with semantic search disabled. Configure a cloud provider without an API key so
    // `NovaAi::new` fails and workspace indexing cannot run (no Tokio runtime in the server).
    let config_path = root.join("nova.config.toml");
    std::fs::write(
        &config_path,
        r#"
[ai]
enabled = true

[ai.privacy]
local_only = false

[ai.features]
semantic_search = false

[ai.provider]
kind = "open_ai"
url = "https://api.openai.com/v1"
model = "gpt-4"
timeout_ms = 2000
max_tokens = 64
"#,
    )
    .expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        // Ensure developer environments don't interfere with AI/semantic search settings.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
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
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "rootUri": root_uri, "capabilities": {} }
        }),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    // Open both documents while semantic search is disabled. This ensures we have overlay
    // documents that need to be reindexed on config reload.
    let zebra_uri = file_uri_string(&zebra_path);
    let other_uri = file_uri_string(&other_path);
    for (uri, text) in [(&zebra_uri, zebra_text), (&other_uri, other_text)] {
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
    }

    // Enable semantic search via config reload.
    std::fs::write(
        &config_path,
        r#"
[ai]
enabled = true

[ai.privacy]
local_only = false

[ai.features]
semantic_search = true

[ai.provider]
kind = "open_ai"
url = "https://api.openai.com/v1"
model = "gpt-4"
timeout_ms = 2000
max_tokens = 64
"#,
    )
    .expect("rewrite config");

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "workspace/didChangeConfiguration",
            "params": { "settings": {} }
        }),
    );

    // Confirm the index status reports a missing runtime (workspace indexing is unavailable).
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": nova_lsp::SEMANTIC_SEARCH_INDEX_STATUS_METHOD,
            "params": {}
        }),
    );
    let index_status = read_response_with_id(&mut stdout, 2);
    let status = index_status.get("result").cloned().expect("result");
    assert_eq!(
        status.get("enabled").and_then(|v| v.as_bool()),
        Some(true),
        "expected semantic search to be enabled after config reload; got {status:#}"
    );
    assert_eq!(
        status.get("reason").and_then(|v| v.as_str()),
        Some("runtime_unavailable"),
        "expected semantic search indexStatus to report runtime_unavailable; got {status:#}"
    );

    // Query semantic search. Even without workspace indexing, the server should have reindexed open
    // documents after the config reload.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": nova_lsp::SEMANTIC_SEARCH_SEARCH_METHOD,
            "params": { "query": "zebraToken", "limit": 10 }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    let results = resp
        .pointer("/result/results")
        .and_then(|v| v.as_array())
        .expect("result.results array");

    assert!(
        results.iter().any(|result| {
            let path = result.get("path").and_then(|v| v.as_str());
            let snippet = result.get("snippet").and_then(|v| v.as_str());
            path == Some("src/UsesZebra.java") && snippet.is_some_and(|s| s.contains("zebraToken"))
        }),
        "expected semantic search results to include UsesZebra.java with a zebraToken snippet; got {results:#?}"
    );

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

