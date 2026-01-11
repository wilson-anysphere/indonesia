use std::collections::HashMap;
use std::path::PathBuf;

use nova_lsp::NovaLspFrontend;
use nova_remote_proto::WorkerStats;
use nova_router::{DistributedRouterConfig, ListenAddr};
use tempfile::TempDir;

#[cfg(unix)]
#[tokio::test]
async fn distributed_indexing_updates_only_one_shard() -> anyhow::Result<()> {
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

    let config = DistributedRouterConfig {
        listen_addr: ListenAddr::Unix(listen_path),
        worker_command: worker_bin,
        cache_dir,
        auth_token: None,
        spawn_workers: true,
    };

    let frontend =
        NovaLspFrontend::new_distributed(config, vec![module_a.clone(), module_b.clone()]).await?;
    frontend.index_workspace().await?;

    let symbols = frontend.workspace_symbols("").await;
    let names: Vec<_> = symbols.into_iter().map(|s| s.name).collect();
    assert!(names.contains(&"Alpha".to_string()));
    assert!(names.contains(&"Beta".to_string()));

    let before = frontend.worker_stats().await?;
    assert_eq!(before.len(), 2);

    let updated = "package a; public class Alpha {} class Gamma {}";
    tokio::fs::write(&file_a, updated).await?;
    frontend
        .did_change_file(file_a.clone(), updated.to_string())
        .await?;

    let after = frontend.worker_stats().await?;
    assert_worker_generations(&before, &after, 0, true);
    assert_worker_generations(&before, &after, 1, false);

    let symbols = frontend.workspace_symbols("").await;
    let names: Vec<_> = symbols.into_iter().map(|s| s.name).collect();
    assert!(names.contains(&"Alpha".to_string()));
    assert!(names.contains(&"Beta".to_string()));
    assert!(names.contains(&"Gamma".to_string()));

    frontend.shutdown().await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn worker_restart_rehydrates_shard_files_from_cache() -> anyhow::Result<()> {
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

    let listen_path = workspace_root.join("router.sock");
    let cache_dir = workspace_root.join("cache");
    let worker_bin = PathBuf::from(env!("CARGO_BIN_EXE_nova-worker"));

    let config = DistributedRouterConfig {
        listen_addr: ListenAddr::Unix(listen_path.clone()),
        worker_command: worker_bin.clone(),
        cache_dir: cache_dir.clone(),
        auth_token: None,
        spawn_workers: true,
    };

    let frontend =
        NovaLspFrontend::new_distributed(config, vec![module_a.clone(), module_b.clone()]).await?;
    frontend.index_workspace().await?;
    frontend.shutdown().await?;

    let config = DistributedRouterConfig {
        listen_addr: ListenAddr::Unix(listen_path),
        worker_command: worker_bin,
        cache_dir,
        auth_token: None,
        spawn_workers: true,
    };

    let frontend =
        NovaLspFrontend::new_distributed(config, vec![module_a.clone(), module_b.clone()]).await?;
    wait_for_file_counts(&frontend, &[(0, 2), (1, 1)]).await?;

    // This update should reindex shard 0 but still retain symbols from the other file in the shard.
    let updated = "package a; public class Alpha {} class Gamma {}";
    tokio::fs::write(&file_a1, updated).await?;
    frontend
        .did_change_file(file_a1.clone(), updated.to_string())
        .await?;

    let symbols = frontend.workspace_symbols("Delta").await;
    assert!(symbols.iter().any(|s| s.name == "Delta"));
    let symbols = frontend.workspace_symbols("Gamma").await;
    assert!(symbols.iter().any(|s| s.name == "Gamma"));

    frontend.shutdown().await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn distributed_workspace_symbols_merges_across_shards_deterministically() -> anyhow::Result<()> {
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

    let config = DistributedRouterConfig {
        listen_addr: ListenAddr::Unix(listen_path),
        worker_command: worker_bin,
        cache_dir,
        auth_token: None,
        spawn_workers: true,
    };

    let frontend =
        NovaLspFrontend::new_distributed(config, vec![module_a.clone(), module_b.clone()]).await?;
    frontend.index_workspace().await?;

    let symbols = frontend.workspace_symbols("FooBar").await;
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

    frontend.shutdown().await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn distributed_workspace_symbols_prefers_prefix_matches_across_shards() -> anyhow::Result<()> {
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

    let config = DistributedRouterConfig {
        listen_addr: ListenAddr::Unix(listen_path),
        worker_command: worker_bin,
        cache_dir,
        auth_token: None,
        spawn_workers: true,
    };

    let frontend =
        NovaLspFrontend::new_distributed(config, vec![module_a.clone(), module_b.clone()]).await?;
    frontend.index_workspace().await?;

    let symbols = frontend.workspace_symbols("foo").await;
    assert_eq!(symbols[0].name, "foobar");

    frontend.shutdown().await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn distributed_workspace_symbols_supports_acronym_queries_across_shards() -> anyhow::Result<()> {
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

    let config = DistributedRouterConfig {
        listen_addr: ListenAddr::Unix(listen_path),
        worker_command: worker_bin,
        cache_dir,
        auth_token: None,
        spawn_workers: true,
    };

    let frontend =
        NovaLspFrontend::new_distributed(config, vec![module_a.clone(), module_b.clone()]).await?;
    frontend.index_workspace().await?;

    let symbols = frontend.workspace_symbols("fb").await;
    assert_eq!(symbols[0].name, "FooBar");

    frontend.shutdown().await?;
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
async fn wait_for_file_counts(
    frontend: &NovaLspFrontend,
    expected: &[(u32, u32)],
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let stats = frontend.worker_stats().await?;
        if expected
            .iter()
            .all(|(shard, count)| stats.get(shard).is_some_and(|s| s.file_count == *count))
        {
            return Ok(());
        }

        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for worker file counts; last stats: {stats:?}");
        }

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
