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
