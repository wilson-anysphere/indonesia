use nova_db::{FileId, NovaSemantic, NovaSyntax, SalsaDatabase};
use nova_memory::{MemoryBudget, MemoryManager};
use nova_metrics::MetricsSnapshot;
use serde_json::Value;
use std::io::BufReader;
use std::process::{Command, Stdio};

use crate::support::{
    did_open_notification, exit_notification, initialize_request_empty, initialized_notification,
    jsonrpc_request, read_response_with_id, shutdown_request, stdio_server_lock,
    write_jsonrpc_message,
};

#[test]
fn stdio_server_exposes_metrics_snapshot() {
    let _lock = stdio_server_lock();
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

    // Trigger a couple more methods so the snapshot isn't empty.
    let uri = "file:///test/Foo.java";
    write_jsonrpc_message(
        &mut stdin,
        &did_open_notification(uri, "java", 1, "class Foo{}\n"),
    );

    write_jsonrpc_message(&mut stdin, &jsonrpc_request(Value::Null, 2, "nova/metrics"));
    let metrics_resp = read_response_with_id(&mut stdout, 2);
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

    write_jsonrpc_message(&mut stdin, &shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &exit_notification());
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
