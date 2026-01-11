use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use nova_fuzzy::FuzzyMatcher;
use nova_remote_proto::v3::{Notification, Request, Response, WireFrame};
use nova_remote_proto::{FileText, ShardId, ShardIndex, Symbol, WorkerStats};
use nova_remote_rpc::{RouterConfig, RpcConnection};
use tempfile::TempDir;
use tokio::sync::{watch, Mutex};

#[cfg(unix)]
use tokio::io::AsyncWriteExt;
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
#[cfg(unix)]
use tokio::process::Command;

const WORKSPACE_SYMBOL_LIMIT: usize = 200;

#[cfg(unix)]
#[tokio::test]
async fn distributed_indexing_updates_only_one_shard() -> Result<()> {
    let tmp = TempDir::new()?;
    let workspace_root = tmp.path();

    let module_a = workspace_root.join("module_a").join("src");
    let module_b = workspace_root.join("module_b").join("src");
    tokio::fs::create_dir_all(&module_a).await?;
    tokio::fs::create_dir_all(&module_b).await?;

    let file_a = module_a.join("A.java");
    let file_b = module_b.join("B.java");
    tokio::fs::write(&file_a, "package a; public class Alpha {}").await?;
    tokio::fs::write(&file_b, "package b; public class Beta {}").await?;

    let listen_path = workspace_root.join("router.sock");
    let cache_dir = workspace_root.join("cache");
    let worker_bin = PathBuf::from(env!("CARGO_BIN_EXE_nova-worker"));

    let router = TestRouter::new(listen_path.clone()).await?;

    let mut workers = vec![
        spawn_worker(&worker_bin, &listen_path, &cache_dir, 0).await?,
        spawn_worker(&worker_bin, &listen_path, &cache_dir, 1).await?,
    ];

    router.wait_for_workers(&[0, 1]).await?;

    router.index_shard(0, &module_a).await?;
    router.index_shard(1, &module_b).await?;

    let symbols = router.workspace_symbols("").await;
    let names: Vec<_> = symbols.into_iter().map(|s| s.name).collect();
    assert!(names.contains(&"Alpha".to_string()));
    assert!(names.contains(&"Beta".to_string()));

    let before = router.worker_stats(&[0, 1]).await?;
    assert_eq!(before.len(), 2);

    let updated = "package a; public class Alpha {} class Gamma {}";
    tokio::fs::write(&file_a, updated).await?;
    router
        .update_file(
            0,
            FileText {
                path: file_a.to_string_lossy().to_string(),
                text: updated.to_string(),
            },
        )
        .await?;

    let after = router.worker_stats(&[0, 1]).await?;
    assert_worker_generations(&before, &after, 0, true);
    assert_worker_generations(&before, &after, 1, false);

    let symbols = router.workspace_symbols("").await;
    let names: Vec<_> = symbols.into_iter().map(|s| s.name).collect();
    assert!(names.contains(&"Alpha".to_string()));
    assert!(names.contains(&"Beta".to_string()));
    assert!(names.contains(&"Gamma".to_string()));

    router.shutdown_workers(&[0, 1]).await?;
    router.shutdown().await?;

    for worker in &mut workers {
        let status = tokio::time::timeout(Duration::from_secs(10), worker.wait()).await??;
        assert!(status.success());
    }

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn invalid_hello_doesnt_kill_accept_loop() -> Result<()> {
    let tmp = TempDir::new()?;
    let workspace_root = tmp.path();

    let module = workspace_root.join("module").join("src");
    tokio::fs::create_dir_all(&module).await?;

    let file = module.join("A.java");
    tokio::fs::write(&file, "package a; public class Alpha {}").await?;

    let listen_path = workspace_root.join("router.sock");
    let cache_dir = workspace_root.join("cache");
    let worker_bin = PathBuf::from(env!("CARGO_BIN_EXE_nova-worker"));

    let router = TestRouter::new(listen_path.clone()).await?;

    // Send a valid v3 frame but with the wrong handshake type. The router should reject it and
    // continue accepting subsequent worker connections.
    let mut stream = connect_unix_with_retry(&listen_path).await?;
    let invalid = WireFrame::Packet {
        id: 2,
        compression: nova_remote_proto::v3::CompressionAlgo::None,
        data: Vec::new(),
    };
    let payload = nova_remote_proto::v3::encode_wire_frame(&invalid)?;
    write_len_prefixed(&mut stream, &payload).await?;
    drop(stream);

    let mut worker = spawn_worker(&worker_bin, &listen_path, &cache_dir, 0).await?;
    router.wait_for_workers(&[0]).await?;
    router.index_shard(0, &module).await?;

    let symbols = router.workspace_symbols("Alpha").await;
    assert!(symbols.iter().any(|s| s.name == "Alpha"));

    router.shutdown_workers(&[0]).await?;
    router.shutdown().await?;

    let status = tokio::time::timeout(Duration::from_secs(10), worker.wait()).await??;
    assert!(status.success());

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn oversized_frame_rejected_safely() -> Result<()> {
    let tmp = TempDir::new()?;
    let workspace_root = tmp.path();

    let module = workspace_root.join("module").join("src");
    tokio::fs::create_dir_all(&module).await?;

    let file = module.join("A.java");
    tokio::fs::write(&file, "package a; public class Alpha {}").await?;

    let listen_path = workspace_root.join("router.sock");
    let cache_dir = workspace_root.join("cache");
    let worker_bin = PathBuf::from(env!("CARGO_BIN_EXE_nova-worker"));

    let router = TestRouter::new(listen_path.clone()).await?;

    // Declare an absurdly large frame size; the router should reject it without allocating and
    // keep the accept loop alive.
    let mut stream = connect_unix_with_retry(&listen_path).await?;
    stream.write_u32_le(u32::MAX).await?;
    stream.flush().await?;
    drop(stream);

    let mut worker = spawn_worker(&worker_bin, &listen_path, &cache_dir, 0).await?;
    router.wait_for_workers(&[0]).await?;
    router.index_shard(0, &module).await?;

    let symbols = router.workspace_symbols("Alpha").await;
    assert!(symbols.iter().any(|s| s.name == "Alpha"));

    router.shutdown_workers(&[0]).await?;
    router.shutdown().await?;

    let status = tokio::time::timeout(Duration::from_secs(10), worker.wait()).await??;
    assert!(status.success());

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn worker_restart_rehydrates_shard_files_from_cache() -> Result<()> {
    let tmp = TempDir::new()?;
    let workspace_root = tmp.path();

    let module_a = workspace_root.join("module_a").join("src");
    let module_b = workspace_root.join("module_b").join("src");
    tokio::fs::create_dir_all(&module_a).await?;
    tokio::fs::create_dir_all(&module_b).await?;

    let file_a1 = module_a.join("A1.java");
    let file_a2 = module_a.join("A2.java");
    let file_b = module_b.join("B.java");
    tokio::fs::write(&file_a1, "package a; public class Alpha {}").await?;
    tokio::fs::write(&file_a2, "package a; public class Delta {}").await?;
    tokio::fs::write(&file_b, "package b; public class Beta {}").await?;

    let listen_path_1 = workspace_root.join("router-1.sock");
    let listen_path_2 = workspace_root.join("router-2.sock");
    let cache_dir = workspace_root.join("cache");
    let worker_bin = PathBuf::from(env!("CARGO_BIN_EXE_nova-worker"));

    // First run: index and populate cache.
    let router = TestRouter::new(listen_path_1.clone()).await?;
    let mut workers = vec![
        spawn_worker(&worker_bin, &listen_path_1, &cache_dir, 0).await?,
        spawn_worker(&worker_bin, &listen_path_1, &cache_dir, 1).await?,
    ];
    router.wait_for_workers(&[0, 1]).await?;
    router.index_shard(0, &module_a).await?;
    router.index_shard(1, &module_b).await?;
    router.shutdown_workers(&[0, 1]).await?;
    router.shutdown().await?;
    for worker in &mut workers {
        let status = tokio::time::timeout(Duration::from_secs(10), worker.wait()).await??;
        assert!(status.success());
    }

    // Second run: new router, workers reuse cached index and get a fresh file snapshot via LoadFiles.
    let router = TestRouter::new(listen_path_2.clone()).await?;
    let mut workers = vec![
        spawn_worker(&worker_bin, &listen_path_2, &cache_dir, 0).await?,
        spawn_worker(&worker_bin, &listen_path_2, &cache_dir, 1).await?,
    ];
    router.wait_for_workers(&[0, 1]).await?;

    // Wait for CachedIndex notifications so workspaceSymbols can be served immediately.
    router.wait_for_indexes(&[0, 1]).await?;

    let symbols = router.workspace_symbols("Delta").await;
    assert!(symbols.iter().any(|s| s.name == "Delta"));

    router.load_shard_files(0, &module_a).await?;
    router.load_shard_files(1, &module_b).await?;
    router.wait_for_file_counts(&[(0, 2), (1, 1)]).await?;

    // This update should reindex shard 0 but still retain symbols from the other file in the shard.
    let updated = "package a; public class Alpha {} class Gamma {}";
    tokio::fs::write(&file_a1, updated).await?;
    router
        .update_file(
            0,
            FileText {
                path: file_a1.to_string_lossy().to_string(),
                text: updated.to_string(),
            },
        )
        .await?;

    let symbols = router.workspace_symbols("Delta").await;
    assert!(symbols.iter().any(|s| s.name == "Delta"));
    let symbols = router.workspace_symbols("Gamma").await;
    assert!(symbols.iter().any(|s| s.name == "Gamma"));

    router.shutdown_workers(&[0, 1]).await?;
    router.shutdown().await?;

    for worker in &mut workers {
        let status = tokio::time::timeout(Duration::from_secs(10), worker.wait()).await??;
        assert!(status.success());
    }

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn distributed_workspace_symbols_merges_across_shards_deterministically() -> Result<()> {
    let tmp = TempDir::new()?;
    let workspace_root = tmp.path();

    let module_a = workspace_root.join("module_a").join("src");
    let module_b = workspace_root.join("module_b").join("src");
    tokio::fs::create_dir_all(&module_a).await?;
    tokio::fs::create_dir_all(&module_b).await?;

    let file_a = module_a.join("FooBar.java");
    let file_b = module_b.join("FooBar.java");
    tokio::fs::write(&file_a, "package a; public class FooBar {}").await?;
    tokio::fs::write(&file_b, "package b; public class FooBar {}").await?;

    let listen_path = workspace_root.join("router.sock");
    let cache_dir = workspace_root.join("cache");
    let worker_bin = PathBuf::from(env!("CARGO_BIN_EXE_nova-worker"));

    let router = TestRouter::new(listen_path.clone()).await?;
    let mut workers = vec![
        spawn_worker(&worker_bin, &listen_path, &cache_dir, 0).await?,
        spawn_worker(&worker_bin, &listen_path, &cache_dir, 1).await?,
    ];
    router.wait_for_workers(&[0, 1]).await?;
    router.index_shard(0, &module_a).await?;
    router.index_shard(1, &module_b).await?;

    let symbols = router.workspace_symbols("FooBar").await;
    let foobars: Vec<_> = symbols.iter().filter(|s| s.name == "FooBar").collect();
    assert_eq!(foobars.len(), 2, "expected FooBar from both shards");
    assert!(
        foobars[0].path.contains("module_a"),
        "expected module_a FooBar to sort before module_b: {foobars:?}"
    );
    assert!(
        foobars[1].path.contains("module_b"),
        "expected module_b FooBar to sort after module_a: {foobars:?}"
    );

    router.shutdown_workers(&[0, 1]).await?;
    router.shutdown().await?;
    for worker in &mut workers {
        let status = tokio::time::timeout(Duration::from_secs(10), worker.wait()).await??;
        assert!(status.success());
    }

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn distributed_workspace_symbols_prefers_prefix_matches_across_shards() -> Result<()> {
    let tmp = TempDir::new()?;
    let workspace_root = tmp.path();

    let module_a = workspace_root.join("module_a").join("src");
    let module_b = workspace_root.join("module_b").join("src");
    tokio::fs::create_dir_all(&module_a).await?;
    tokio::fs::create_dir_all(&module_b).await?;

    let file_a = module_a.join("A.java");
    let file_b = module_b.join("B.java");
    tokio::fs::write(&file_a, "package a; class foobar {}").await?;
    tokio::fs::write(&file_b, "package b; class barfoo {}").await?;

    let listen_path = workspace_root.join("router.sock");
    let cache_dir = workspace_root.join("cache");
    let worker_bin = PathBuf::from(env!("CARGO_BIN_EXE_nova-worker"));

    let router = TestRouter::new(listen_path.clone()).await?;
    let mut workers = vec![
        spawn_worker(&worker_bin, &listen_path, &cache_dir, 0).await?,
        spawn_worker(&worker_bin, &listen_path, &cache_dir, 1).await?,
    ];
    router.wait_for_workers(&[0, 1]).await?;
    router.index_shard(0, &module_a).await?;
    router.index_shard(1, &module_b).await?;

    let symbols = router.workspace_symbols("foo").await;
    assert_eq!(symbols[0].name, "foobar");

    router.shutdown_workers(&[0, 1]).await?;
    router.shutdown().await?;
    for worker in &mut workers {
        let status = tokio::time::timeout(Duration::from_secs(10), worker.wait()).await??;
        assert!(status.success());
    }

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn distributed_workspace_symbols_supports_acronym_queries_across_shards() -> Result<()> {
    let tmp = TempDir::new()?;
    let workspace_root = tmp.path();

    let module_a = workspace_root.join("module_a").join("src");
    let module_b = workspace_root.join("module_b").join("src");
    tokio::fs::create_dir_all(&module_a).await?;
    tokio::fs::create_dir_all(&module_b).await?;

    let file_a = module_a.join("A.java");
    let file_b = module_b.join("B.java");
    tokio::fs::write(&file_a, "package a; public class FooBar {}").await?;
    tokio::fs::write(&file_b, "package b; class foobar {}").await?;

    let listen_path = workspace_root.join("router.sock");
    let cache_dir = workspace_root.join("cache");
    let worker_bin = PathBuf::from(env!("CARGO_BIN_EXE_nova-worker"));

    let router = TestRouter::new(listen_path.clone()).await?;
    let mut workers = vec![
        spawn_worker(&worker_bin, &listen_path, &cache_dir, 0).await?,
        spawn_worker(&worker_bin, &listen_path, &cache_dir, 1).await?,
    ];
    router.wait_for_workers(&[0, 1]).await?;
    router.index_shard(0, &module_a).await?;
    router.index_shard(1, &module_b).await?;

    let symbols = router.workspace_symbols("fb").await;
    assert_eq!(symbols[0].name, "FooBar");

    router.shutdown_workers(&[0, 1]).await?;
    router.shutdown().await?;
    for worker in &mut workers {
        let status = tokio::time::timeout(Duration::from_secs(10), worker.wait()).await??;
        assert!(status.success());
    }

    Ok(())
}

#[cfg(unix)]
fn assert_worker_generations(
    before: &HashMap<u32, WorkerStats>,
    after: &HashMap<u32, WorkerStats>,
    shard: u32,
    should_change: bool,
) {
    let before_gen = before.get(&shard).unwrap().index_generation;
    let after_gen = after.get(&shard).unwrap().index_generation;
    if should_change {
        assert!(after_gen > before_gen, "expected shard {shard} to reindex");
    } else {
        assert_eq!(
            after_gen, before_gen,
            "expected shard {shard} to stay unchanged"
        );
    }
}

#[cfg(unix)]
struct TestRouter {
    socket_path: PathBuf,
    state: Arc<TestRouterState>,
    shutdown_tx: watch::Sender<bool>,
    accept_task: tokio::task::JoinHandle<()>,
}

#[cfg(unix)]
struct TestRouterState {
    workers: Mutex<HashMap<ShardId, RpcConnection>>,
    indexes: Mutex<HashMap<ShardId, ShardIndex>>,
    next_worker_id: AtomicU32,
    revision: AtomicU64,
}

#[cfg(unix)]
impl TestRouter {
    async fn new(socket_path: PathBuf) -> Result<Self> {
        let _ = tokio::fs::remove_file(&socket_path).await;
        let listener = UnixListener::bind(&socket_path).context("bind unix socket")?;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let state = Arc::new(TestRouterState {
            workers: Mutex::new(HashMap::new()),
            indexes: Mutex::new(HashMap::new()),
            next_worker_id: AtomicU32::new(1),
            revision: AtomicU64::new(0),
        });

        let accept_task = tokio::spawn(accept_loop(listener, shutdown_rx, state.clone()));

        Ok(Self {
            socket_path,
            state,
            shutdown_tx,
            accept_task,
        })
    }

    async fn shutdown(self) -> Result<()> {
        let Self {
            socket_path,
            shutdown_tx,
            mut accept_task,
            ..
        } = self;

        let _ = shutdown_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(2), &mut accept_task).await;
        let _ = tokio::fs::remove_file(&socket_path).await;
        Ok(())
    }

    async fn wait_for_workers(&self, shards: &[ShardId]) -> Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            {
                let guard = self.state.workers.lock().await;
                if shards.iter().all(|id| guard.contains_key(id)) {
                    return Ok(());
                }
            }

            if tokio::time::Instant::now() >= deadline {
                let guard = self.state.workers.lock().await;
                return Err(anyhow!(
                    "timed out waiting for workers; expected {shards:?}, connected: {:?}",
                    guard.keys().collect::<Vec<_>>()
                ));
            }

            tokio::task::yield_now().await;
        }
    }

    async fn wait_for_indexes(&self, shards: &[ShardId]) -> Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            {
                let guard = self.state.indexes.lock().await;
                if shards.iter().all(|id| guard.contains_key(id)) {
                    return Ok(());
                }
            }

            if tokio::time::Instant::now() >= deadline {
                let guard = self.state.indexes.lock().await;
                return Err(anyhow!(
                    "timed out waiting for cached indexes; expected {shards:?}, present: {:?}",
                    guard.keys().collect::<Vec<_>>()
                ));
            }

            tokio::task::yield_now().await;
        }
    }

    async fn index_shard(&self, shard_id: ShardId, root: &Path) -> Result<()> {
        let files = collect_java_files(root).await?;
        let revision = self.next_revision();
        let resp = self
            .call(shard_id, Request::IndexShard { revision, files })
            .await?;
        match resp {
            Response::ShardIndex(index) => {
                self.state.indexes.lock().await.insert(shard_id, index);
                Ok(())
            }
            other => Err(anyhow!("unexpected IndexShard response: {other:?}")),
        }
    }

    async fn load_shard_files(&self, shard_id: ShardId, root: &Path) -> Result<()> {
        let files = collect_java_files(root).await?;
        let revision = self.current_revision();
        let resp = self
            .call(shard_id, Request::LoadFiles { revision, files })
            .await?;
        match resp {
            Response::Ack => Ok(()),
            other => Err(anyhow!("unexpected LoadFiles response: {other:?}")),
        }
    }

    async fn update_file(&self, shard_id: ShardId, file: FileText) -> Result<()> {
        let revision = self.next_revision();
        let resp = self
            .call(shard_id, Request::UpdateFile { revision, file })
            .await?;
        match resp {
            Response::ShardIndex(index) => {
                self.state.indexes.lock().await.insert(shard_id, index);
                Ok(())
            }
            other => Err(anyhow!("unexpected UpdateFile response: {other:?}")),
        }
    }

    async fn worker_stats(&self, shards: &[ShardId]) -> Result<HashMap<ShardId, WorkerStats>> {
        let mut out = HashMap::new();
        for &shard_id in shards {
            let resp = self.call(shard_id, Request::GetWorkerStats).await?;
            match resp {
                Response::WorkerStats(stats) => {
                    out.insert(shard_id, stats);
                }
                other => return Err(anyhow!("unexpected GetWorkerStats response: {other:?}")),
            }
        }
        Ok(out)
    }

    async fn wait_for_file_counts(&self, expected: &[(ShardId, u32)]) -> Result<()> {
        let shards: Vec<ShardId> = expected.iter().map(|(shard_id, _)| *shard_id).collect();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let stats = self.worker_stats(&shards).await?;
            if expected.iter().all(|(shard_id, count)| {
                stats.get(shard_id).is_some_and(|s| s.file_count == *count)
            }) {
                return Ok(());
            }

            if tokio::time::Instant::now() >= deadline {
                return Err(anyhow!(
                    "timed out waiting for worker file counts; last stats: {stats:?}"
                ));
            }

            tokio::task::yield_now().await;
        }
    }

    async fn shutdown_workers(&self, shards: &[ShardId]) -> Result<()> {
        for &shard_id in shards {
            let _ = self.call(shard_id, Request::Shutdown).await;
        }
        Ok(())
    }

    async fn workspace_symbols(&self, query: &str) -> Vec<Symbol> {
        let mut all_symbols: Vec<Symbol> = {
            let guard = self.state.indexes.lock().await;
            guard
                .values()
                .flat_map(|index| index.symbols.iter().cloned())
                .collect()
        };

        if all_symbols.is_empty() {
            return Vec::new();
        }

        if query.is_empty() {
            all_symbols.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
            all_symbols.dedup_by(|a, b| a.name == b.name && a.path == b.path);
            return all_symbols
                .into_iter()
                .take(WORKSPACE_SYMBOL_LIMIT)
                .collect();
        }

        let mut matcher = FuzzyMatcher::new(query);
        let mut scored: Vec<(nova_fuzzy::MatchScore, Symbol)> = Vec::new();
        for sym in all_symbols {
            if let Some(score) = matcher.score(&sym.name) {
                scored.push((score, sym));
            }
        }

        scored.sort_by(|(a_score, a_sym), (b_score, b_sym)| {
            b_score
                .rank_key()
                .cmp(&a_score.rank_key())
                .then_with(|| a_sym.name.len().cmp(&b_sym.name.len()))
                .then_with(|| a_sym.name.cmp(&b_sym.name))
                .then_with(|| a_sym.path.cmp(&b_sym.path))
        });

        let mut out = Vec::new();
        for (_score, sym) in scored {
            if out
                .last()
                .is_some_and(|prev: &Symbol| prev.name == sym.name && prev.path == sym.path)
            {
                continue;
            }
            out.push(sym);
            if out.len() == WORKSPACE_SYMBOL_LIMIT {
                break;
            }
        }
        out
    }

    async fn call(&self, shard_id: ShardId, request: Request) -> Result<Response> {
        let conn = {
            let guard = self.state.workers.lock().await;
            guard
                .get(&shard_id)
                .cloned()
                .ok_or_else(|| anyhow!("no worker connected for shard {shard_id}"))?
        };
        conn.call(request)
            .await
            .map_err(|err| anyhow!("rpc call failed: {err:?}"))
    }

    fn next_revision(&self) -> u64 {
        self.state.revision.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn current_revision(&self) -> u64 {
        self.state.revision.load(Ordering::SeqCst)
    }
}

#[cfg(unix)]
async fn accept_loop(
    listener: UnixListener,
    mut shutdown_rx: watch::Receiver<bool>,
    state: Arc<TestRouterState>,
) {
    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            res = listener.accept() => {
                let (stream, _) = match res {
                    Ok(pair) => pair,
                    Err(_) => continue,
                };
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle_connection(stream, state).await {
                        tracing::warn!(error = ?err, "router handshake failed");
                    }
                });
            }
        }
    }
}

#[cfg(unix)]
async fn handle_connection(stream: UnixStream, state: Arc<TestRouterState>) -> Result<()> {
    let worker_id = state.next_worker_id.fetch_add(1, Ordering::SeqCst);
    let cfg = RouterConfig {
        worker_id,
        revision: state.revision.load(Ordering::SeqCst),
        ..RouterConfig::default()
    };

    let (conn, welcome) = RpcConnection::handshake_as_router_with_config(stream, cfg)
        .await
        .map_err(|err| anyhow!("handshake failed: {err}"))?;

    conn.set_notification_handler({
        let state = state.clone();
        move |notification| {
            let state = state.clone();
            async move {
                match notification {
                    Notification::CachedIndex(index) => {
                        state.indexes.lock().await.insert(index.shard_id, index);
                    }
                    Notification::Unknown => {}
                }
            }
        }
    });

    state.workers.lock().await.insert(welcome.shard_id, conn);
    Ok(())
}

#[cfg(unix)]
async fn collect_java_files(root: &Path) -> Result<Vec<FileText>> {
    let mut paths = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut rd = tokio::fs::read_dir(&dir)
            .await
            .with_context(|| format!("read_dir {}", dir.display()))?;
        while let Some(entry) = rd.next_entry().await? {
            let path = entry.path();
            let ty = entry.file_type().await?;
            if ty.is_dir() {
                stack.push(path);
            } else if ty.is_file() && path.extension().is_some_and(|ext| ext == "java") {
                paths.push(path);
            }
        }
    }
    paths.sort();

    let mut files = Vec::new();
    for path in paths {
        let text = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("read {}", path.display()))?;
        files.push(FileText {
            path: path.to_string_lossy().to_string(),
            text,
        });
    }
    Ok(files)
}

#[cfg(unix)]
async fn connect_unix_with_retry(path: &Path) -> Result<UnixStream> {
    Ok(tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match UnixStream::connect(path).await {
                Ok(stream) => return Ok(stream),
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    tokio::task::yield_now().await;
                }
                Err(err) => return Err(err),
            }
        }
    })
    .await??)
}

#[cfg(unix)]
async fn write_len_prefixed(stream: &mut UnixStream, payload: &[u8]) -> Result<()> {
    let len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| anyhow!("payload too large"))?;
    stream.write_u32_le(len).await?;
    stream.write_all(payload).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(unix)]
async fn spawn_worker(
    worker_bin: &Path,
    listen_path: &Path,
    cache_dir: &Path,
    shard_id: u32,
) -> Result<tokio::process::Child> {
    let connect_arg = format!("unix:{}", listen_path.display());
    let mut cmd = Command::new(worker_bin);
    cmd.kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .arg("--connect")
        .arg(connect_arg)
        .arg("--shard-id")
        .arg(shard_id.to_string())
        .arg("--cache-dir")
        .arg(cache_dir);
    Ok(cmd.spawn()?)
}
