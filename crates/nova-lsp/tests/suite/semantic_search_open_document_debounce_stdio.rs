use serde_json::json;
use std::io::BufReader;
use std::process::{Command, Stdio};
use std::time::Duration;

use nova_metrics::MetricsSnapshot;
use tempfile::TempDir;

use crate::support::{
    file_uri_string, read_response_with_id, stdio_server_lock, write_jsonrpc_message,
};

fn open_doc_index_count(snapshot: &MetricsSnapshot) -> u64 {
    let legacy = snapshot
        .methods
        .get("semantic_search/open_document_index")
        .map(|m| m.request_count)
        .unwrap_or(0);
    let v2 = snapshot
        .methods
        .get("lsp/semantic_search/open_document_index")
        .map(|m| m.request_count)
        .unwrap_or(0);

    if v2 != 0 {
        v2
    } else {
        legacy
    }
}

#[test]
fn semantic_search_open_document_indexing_is_debounced_for_expensive_embeddings() {
    let _lock = stdio_server_lock();

    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let root_uri = file_uri_string(root);

    let config_path = root.join("nova.config.toml");
    std::fs::write(
        &config_path,
        r#"
[ai]
enabled = true

[ai.features]
semantic_search = true

[ai.embeddings]
enabled = true
backend = "hash"
"#,
    )
    .expect("write config");

    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");
    let file_path = src_dir.join("Main.java");
    let file_uri = file_uri_string(&file_path);

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
        // Test-only hook: force the open-document debouncer even with hash embeddings.
        .env("NOVA_LSP_FORCE_SEMANTIC_SEARCH_OPEN_DOCUMENT_DEBOUNCE", "1")
        // Keep the test fast/deterministic.
        .env("NOVA_SEMANTIC_SEARCH_OPEN_DOCUMENT_DEBOUNCE_MS", "50")
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

    let initial_text = "class Main { String value = \"baseline\"; }\n";
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": file_uri,
                    "languageId": "java",
                    "version": 1,
                    "text": initial_text
                }
            }
        }),
    );

    // Snapshot the baseline count after didOpen (which indexes once).
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/metrics",
            "params": null
        }),
    );
    let metrics_resp = read_response_with_id(&mut stdout, 2);
    let snapshot: MetricsSnapshot = serde_json::from_value(
        metrics_resp.get("result").cloned().expect("metrics result"),
    )
    .expect("decode metrics snapshot");
    let baseline = open_doc_index_count(&snapshot);
    let legacy = snapshot
        .methods
        .get("semantic_search/open_document_index")
        .map(|m| m.request_count)
        .unwrap_or(0);
    let v2 = snapshot
        .methods
        .get("lsp/semantic_search/open_document_index")
        .map(|m| m.request_count)
        .unwrap_or(0);
    assert!(
        v2 > 0,
        "expected lsp/semantic_search/open_document_index to be recorded"
    );
    assert_eq!(
        legacy, v2,
        "expected legacy and v2 open-document indexing metrics to match"
    );

    // Send multiple rapid edits; the server should coalesce these and only index once.
    let mut version = 2;
    let mut final_marker = String::new();
    for idx in 0..5 {
        final_marker = format!("NOVA_DEBOUNCE_MARKER_{idx}");
        let text = format!("class Main {{ String value = \"{final_marker}\"; }}\n");
        write_jsonrpc_message(
            &mut stdin,
            &json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didChange",
                "params": {
                    "textDocument": { "uri": file_uri, "version": version },
                    "contentChanges": [{ "text": text }]
                }
            }),
        );
        version += 1;
    }

    let expected = baseline + 1;

    // Poll metrics until the debounced indexing run is observed.
    for attempt in 0..200u64 {
        write_jsonrpc_message(
            &mut stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": 1000 + attempt as i64,
                "method": "nova/metrics",
                "params": null
            }),
        );
        let metrics_resp = read_response_with_id(&mut stdout, 1000 + attempt as i64);
        let snapshot: MetricsSnapshot = serde_json::from_value(
            metrics_resp.get("result").cloned().expect("metrics result"),
        )
        .expect("decode metrics snapshot");

        let count = open_doc_index_count(&snapshot);
        assert!(
            count <= expected,
            "expected at most {expected} open-document semantic search index runs; got {count}"
        );

        if count == expected {
            break;
        }

        std::thread::sleep(Duration::from_millis(10));
    }

    // Ensure we didn't get additional late indexing work.
    std::thread::sleep(Duration::from_millis(150));
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 5000,
            "method": "nova/metrics",
            "params": null
        }),
    );
    let metrics_resp = read_response_with_id(&mut stdout, 5000);
    let snapshot: MetricsSnapshot = serde_json::from_value(
        metrics_resp.get("result").cloned().expect("metrics result"),
    )
    .expect("decode metrics snapshot");
    assert_eq!(
        open_doc_index_count(&snapshot),
        expected,
        "expected exactly one additional open-document indexing run after debounce"
    );

    // Validate we indexed the final edit, not an intermediate version.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 6000,
            "method": nova_lsp::SEMANTIC_SEARCH_SEARCH_METHOD,
            "params": { "query": final_marker, "limit": 10 }
        }),
    );
    let resp = read_response_with_id(&mut stdout, 6000);
    let results = resp
        .pointer("/result/results")
        .and_then(|v| v.as_array())
        .expect("result.results array");
    assert!(
        results.iter().any(|value| value.get("path").and_then(|v| v.as_str()) == Some("src/Main.java")),
        "expected semantic search results to include src/Main.java; got {results:#?}"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 7000, "method": "shutdown" }),
    );
    let _shutdown_resp = read_response_with_id(&mut stdout, 7000);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
