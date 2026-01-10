use std::path::PathBuf;

use anyhow::Result;
use nova_remote_proto::Symbol;
use nova_router::{DistributedRouterConfig, QueryRouter, SourceRoot, WorkspaceLayout};

/// Lightweight wrapper that keeps `nova-lsp` as the frontend while delegating heavy work to the
/// query router/worker layer.
pub struct NovaLspFrontend {
    router: QueryRouter,
}

impl NovaLspFrontend {
    pub fn new_in_process(source_roots: Vec<PathBuf>) -> Self {
        let layout = WorkspaceLayout {
            source_roots: source_roots.into_iter().map(|path| SourceRoot { path }).collect(),
        };
        Self {
            router: QueryRouter::new_in_process(layout),
        }
    }

    pub async fn new_distributed(config: DistributedRouterConfig, source_roots: Vec<PathBuf>) -> Result<Self> {
        let layout = WorkspaceLayout {
            source_roots: source_roots.into_iter().map(|path| SourceRoot { path }).collect(),
        };
        let router = QueryRouter::new_distributed(config, layout).await?;
        Ok(Self { router })
    }

    pub async fn index_workspace(&self) -> Result<()> {
        self.router.index_workspace().await
    }

    pub async fn did_change_file(&self, path: PathBuf, new_text: String) -> Result<()> {
        self.router.update_file(path, new_text).await
    }

    pub async fn workspace_symbols(&self, query: &str) -> Vec<Symbol> {
        self.router.workspace_symbols(query).await
    }

    pub async fn worker_stats(&self) -> Result<std::collections::HashMap<u32, nova_remote_proto::WorkerStats>> {
        self.router.worker_stats().await
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.router.shutdown().await
    }
}

