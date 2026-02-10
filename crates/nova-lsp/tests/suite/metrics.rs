use nova_db::{FileId, NovaSemantic, NovaSyntax, SalsaDatabase};
use nova_memory::{MemoryBudget, MemoryManager};
use nova_metrics::MetricsSnapshot;
use serde_json::json;
use std::io::BufReader;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::support::{read_response_with_id, write_jsonrpc_message};

#[test]
fn stdio_server_exposes_metrics_snapshot() {
    let _lock = crate::support::stdio_server_lock();

    let workspace = tempfile::tempdir().expect("temp workspace");
    let root = workspace.path();

    let file_path = root.join("Foo.java");
    std::fs::write(&file_path, "class Foo { void bar() {} }\n").expect("write Foo.java");

    let config_path = root.join("nova.toml");
    std::fs::write(
        &config_path,
        r#"
[ai]
enabled = true

[ai.features]
semantic_search = true
"#,
    )
    .expect("write nova.toml");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        // Ensure a developer's legacy AI env-var wiring can't override the config file and make
        // this test flaky.
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
            "params": {
                "capabilities": {},
                "rootUri": crate::support::file_uri_string(root),
            }
        }),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    // Wait for background semantic-search workspace indexing to finish so the indexing metrics are
    // recorded before we snapshot `nova/metrics`.
    let mut indexing_done = false;
    for attempt in 0..200 {
        let id = 10 + attempt;
        write_jsonrpc_message(
            &mut stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "nova/semanticSearch/indexStatus",
                "params": null,
            }),
        );
        let resp = read_response_with_id(&mut stdout, id);
        let result = resp.get("result").cloned().expect("indexStatus result");
        let done = result
            .get("done")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if done {
            indexing_done = true;
            break;
        }

        // If indexing never started, fail early with the server's reason to avoid long timeouts.
        let current = result
            .get("currentRunId")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if current == 0 {
            let reason = result
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("<missing>");
            panic!("expected semantic-search indexing to start, got reason: {reason}");
        }

        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(indexing_done, "timed out waiting for workspace indexing");

    // Trigger semantic search itself so the AI search metric is recorded.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/semanticSearch/search",
            "params": { "query": "Foo", "limit": 5 },
        }),
    );
    let _search_resp = read_response_with_id(&mut stdout, 2);

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "nova/metrics",
            "params": null
        }),
    );
    let metrics_resp = read_response_with_id(&mut stdout, 3);
    let result = metrics_resp.get("result").cloned().expect("result");
    let snapshot: MetricsSnapshot = serde_json::from_value(result).expect("decode snapshot");

    assert!(snapshot.totals.request_count > 0);
    assert!(
        snapshot
            .methods
            .get("initialize")
            .is_some_and(|m| m.request_count > 0),
        "expected initialize to be recorded"
    );
    assert!(
        snapshot
            .methods
            .get("ai/semantic_search/index_file")
            .is_some_and(|m| m.request_count > 0),
        "expected semantic-search index_file to be recorded"
    );
    assert!(
        snapshot
            .methods
            .get("ai/semantic_search/search")
            .is_some_and(|m| m.request_count > 0),
        "expected semantic-search search to be recorded"
    );
    assert!(
        snapshot
            .methods
            .get("lsp/semantic_search/workspace_index")
            .is_some_and(|m| m.request_count > 0),
        "expected workspace-indexing duration metric to be recorded"
    );
    assert!(
        snapshot
            .methods
            .get("lsp/semantic_search/workspace_index/file")
            .is_some_and(|m| m.request_count > 0),
        "expected workspace-indexing file count metric to be recorded"
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

#[test]
fn stdio_server_records_semantic_search_workspace_index_skip_metric_missing_root() {
    let _lock = crate::support::stdio_server_lock();

    let workspace = tempfile::tempdir().expect("temp workspace");
    let root = workspace.path();

    let config_path = root.join("nova.toml");
    std::fs::write(
        &config_path,
        r#"
[ai]
enabled = true

[ai.features]
semantic_search = true
"#,
    )
    .expect("write nova.toml");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        // Ensure a developer's legacy AI env-var wiring can't override the config file and make
        // this test flaky.
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

    // No rootUri/rootPath: semantic-search workspace indexing should be skipped.
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
    let result = metrics_resp.get("result").cloned().expect("result");
    let snapshot: MetricsSnapshot = serde_json::from_value(result).expect("decode snapshot");

    assert!(
        snapshot
            .methods
            .get("lsp/semantic_search/workspace_index/skipped_missing_workspace_root")
            .is_some_and(|m| m.request_count > 0),
        "expected skip metric to be recorded"
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
fn stdio_server_records_semantic_search_workspace_index_skip_metric_safe_mode() {
    let _lock = crate::support::stdio_server_lock();

    let workspace = tempfile::tempdir().expect("temp workspace");
    let root = workspace.path();

    let config_path = root.join("nova.toml");
    std::fs::write(
        &config_path,
        r#"
[ai]
enabled = true

[ai.features]
semantic_search = true
"#,
    )
    .expect("write nova.toml");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        .env("NOVA_LSP_TEST_FORCE_SAFE_MODE", "1")
        // Ensure a developer's legacy AI env-var wiring can't override the config file and make
        // this test flaky.
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
            "params": {
                "capabilities": {},
                "rootUri": crate::support::file_uri_string(root),
            }
        }),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

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
    let result = metrics_resp.get("result").cloned().expect("result");
    let snapshot: MetricsSnapshot = serde_json::from_value(result).expect("decode snapshot");

    assert!(
        snapshot
            .methods
            .get("lsp/semantic_search/workspace_index/skipped_safe_mode")
            .is_some_and(|m| m.request_count > 0),
        "expected safe-mode skip metric to be recorded"
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
fn stdio_server_records_semantic_search_workspace_index_skip_metric_runtime_unavailable() {
    let _lock = crate::support::stdio_server_lock();

    let workspace = tempfile::tempdir().expect("temp workspace");
    let root = workspace.path();

    let config_path = root.join("nova.toml");
    std::fs::write(
        &config_path,
        r#"
[ai]
enabled = true

[ai.features]
semantic_search = true

[ai.provider]
kind = "open_ai"
"#,
    )
    .expect("write nova.toml");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .arg("--config")
        .arg(&config_path)
        // Ensure a developer's legacy AI env-var wiring can't override the config file and make
        // this test flaky.
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
            "params": {
                "capabilities": {},
                "rootUri": crate::support::file_uri_string(root),
            }
        }),
    );
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

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
    let result = metrics_resp.get("result").cloned().expect("result");
    let snapshot: MetricsSnapshot = serde_json::from_value(result).expect("decode snapshot");

    assert!(
        snapshot
            .methods
            .get("lsp/semantic_search/workspace_index/skipped_runtime_unavailable")
            .is_some_and(|m| m.request_count > 0),
        "expected runtime-unavailable skip metric to be recorded"
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
fn salsa_memos_evict_under_memory_enforcement() {
    // Use a tiny budget so we reliably exceed the query-cache allocation even in small tests.
    let memory = MemoryManager::new(MemoryBudget::from_total(1_000));
    let salsa = SalsaDatabase::new();
    salsa.register_salsa_memo_evictor(&memory);

    let files: Vec<FileId> = (0..128).map(FileId::from_raw).collect();
    for (idx, file) in files.iter().copied().enumerate() {
        salsa.set_file_text(
            file,
            format!("class C{idx} {{ int x = {idx}; int y = {idx}; }}"),
        );
    }

    salsa.with_snapshot(|snap| {
        for file in &files {
            let _ = snap.parse(*file);
            let _ = snap.item_tree(*file);
        }
    });

    let bytes_before = salsa.salsa_memo_bytes();
    assert!(
        bytes_before > 0,
        "expected memo tracker to grow after queries"
    );
    assert_eq!(
        memory.report().usage.query_cache,
        bytes_before,
        "memory manager should see tracked salsa memo usage"
    );

    let parse_exec_before = query_executions(&salsa, "parse");
    let item_tree_exec_before = query_executions(&salsa, "item_tree");

    // Validate that memoization is working prior to eviction.
    salsa.with_snapshot(|snap| {
        for file in &files {
            let _ = snap.parse(*file);
            let _ = snap.item_tree(*file);
        }
    });
    assert_eq!(
        query_executions(&salsa, "parse"),
        parse_exec_before,
        "expected cached parse results prior to eviction"
    );
    assert_eq!(
        query_executions(&salsa, "item_tree"),
        item_tree_exec_before,
        "expected cached item_tree results prior to eviction"
    );

    // Trigger an enforcement pass; the evictor should rebuild the database and drop memoized
    // results.
    memory.enforce();

    assert_eq!(
        salsa.salsa_memo_bytes(),
        0,
        "expected memo tracker to clear after eviction"
    );

    // Subsequent queries should recompute after eviction.
    let parse_exec_after_evict = query_executions(&salsa, "parse");
    salsa.with_snapshot(|snap| {
        let _ = snap.parse(files[0]);
        let _ = snap.item_tree(files[0]);
    });
    assert!(
        query_executions(&salsa, "parse") > parse_exec_after_evict,
        "expected parse to re-execute after memo eviction"
    );
}

fn query_executions(db: &SalsaDatabase, name: &str) -> u64 {
    db.query_stats()
        .by_query
        .get(name)
        .map(|s| s.executions)
        .unwrap_or(0)
}
