use serde_json::json;
use std::io::BufReader;
use std::process::{Command, Stdio};

use tempfile::TempDir;

use crate::support::{file_uri_string, read_response_with_id, stdio_server_lock, write_jsonrpc_message};

#[test]
fn semantic_search_search_returns_workspace_relative_results_with_snippets() {
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

    std::fs::write(&zebra_path, zebra_text).expect("write UsesZebra.java");
    std::fs::write(&other_path, other_text).expect("write Other.java");

    // Enable semantic search via config. Keep the provider section omitted; semantic search uses an
    // in-process index and does not require a live LLM provider.
    let config_path = root.join("nova.config.toml");
    std::fs::write(
        &config_path,
        r#"
[ai]
enabled = true

[ai.features]
semantic_search = true
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

    // 1) Initialize the server with a workspace root so it can return workspace-relative paths.
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

    // 2) Open both documents so the semantic-search index is populated even if workspace indexing
    // is not available.
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

    // 3) Query semantic search for a token that only appears in UsesZebra.java.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": nova_lsp::SEMANTIC_SEARCH_SEARCH_METHOD,
            "params": { "query": "zebraToken", "limit": 10 }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 2);
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
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn semantic_search_search_invalid_params_does_not_echo_secret_string_values() {
    let _lock = stdio_server_lock();

    let secret = "NOVA_SEMANTIC_SEARCH_SECRET_DO_NOT_LEAK";

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = file_uri_string(root);

    // Enable semantic search via config. Keep the provider section omitted; semantic search uses an
    // in-process index and does not require a live LLM provider.
    let config_path = root.join("nova.config.toml");
    std::fs::write(
        &config_path,
        r#"
[ai]
enabled = true

[ai.features]
semantic_search = true
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

    // Pass a scalar instead of the expected params object. `serde_json::Error` would normally echo
    // string values (including secrets) in its display output; ensure the server sanitizes that
    // error before returning it to clients.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": nova_lsp::SEMANTIC_SEARCH_SEARCH_METHOD,
            "params": secret,
        }),
    );

    let resp = read_response_with_id(&mut stdout, 2);
    let error = resp.get("error").cloned().expect("expected error response");
    assert_eq!(error.get("code").and_then(|v| v.as_i64()), Some(-32602));
    assert!(
        !resp.to_string().contains(secret),
        "expected JSON-RPC error to omit secret string values; got: {resp:?}"
    );

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
