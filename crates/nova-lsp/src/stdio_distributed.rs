use crate::ServerState;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub(super) struct DistributedCliConfig {
    worker_command: PathBuf,
}

pub(super) fn parse_distributed_cli(args: &[String]) -> Option<DistributedCliConfig> {
    if !args.iter().any(|arg| arg == "--distributed") {
        return None;
    }
    let worker_command = parse_path_arg(args, "--distributed-worker-command")
        .unwrap_or_else(default_distributed_worker_command);
    Some(DistributedCliConfig { worker_command })
}

fn parse_path_arg(args: &[String], flag: &str) -> Option<PathBuf> {
    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        if arg == flag {
            let next = args.get(i + 1)?;
            return Some(PathBuf::from(next));
        }
        if let Some(value) = arg.strip_prefix(&format!("{flag}=")) {
            if !value.is_empty() {
                return Some(PathBuf::from(value));
            }
        }
        i += 1;
    }
    None
}

fn default_distributed_worker_command() -> PathBuf {
    let exe_name = if cfg!(windows) {
        "nova-worker.exe"
    } else {
        "nova-worker"
    };

    match std::env::current_exe() {
        Ok(exe) => {
            if let Some(dir) = exe.parent() {
                let candidate = dir.join(exe_name);
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
        Err(err) => {
            tracing::debug!(
                target = "nova.lsp",
                error = %err,
                "failed to resolve current executable path; falling back to PATH lookup"
            );
        }
    }

    PathBuf::from(exe_name)
}

fn distributed_run_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let ts = unix_epoch_millis_fallback_zero();
    base.join(format!("nova-lsp-distributed-{}-{ts}", std::process::id()))
}

fn unix_epoch_millis_fallback_zero() -> u128 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis(),
        Err(err) => {
            tracing::debug!(
                target = "nova.lsp",
                error = ?err,
                "system time is before UNIX_EPOCH; using ts=0 for distributed run dir"
            );
            0
        }
    }
}

#[cfg(unix)]
fn distributed_listen_addr(run_dir: &Path) -> nova_router::ListenAddr {
    nova_router::ListenAddr::Unix(run_dir.join("router.sock"))
}

#[cfg(windows)]
fn distributed_listen_addr(_run_dir: &Path) -> nova_router::ListenAddr {
    let ts = unix_epoch_millis_fallback_zero();
    nova_router::ListenAddr::NamedPipe(format!("nova-router-{}-{ts}", std::process::id()))
}

pub(super) struct DistributedServerState {
    pub(super) workspace_root: PathBuf,
    pub(super) source_roots: Vec<PathBuf>,
    pub(super) run_dir: PathBuf,
    pub(super) runtime: tokio::runtime::Runtime,
    pub(super) frontend: Arc<nova_lsp::NovaLspFrontend>,
    pub(super) initial_index: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

impl DistributedServerState {
    pub(super) fn contains_path(&self, path: &Path) -> bool {
        self.source_roots.iter().any(|root| path.starts_with(root))
    }
}

impl ServerState {
    pub(super) fn start_distributed_after_initialize(&mut self) {
        let Some(cli) = self.distributed_cli.clone() else {
            return;
        };
        if self.distributed.is_some() {
            return;
        }

        let Some(project_root) = self.project_root.clone() else {
            tracing::warn!(
                target = "nova.lsp",
                "distributed mode enabled but initialize.rootUri is missing; falling back to in-process workspace indexing"
            );
            return;
        };

        let (workspace_root, source_roots) = match nova_project::load_project_with_workspace_config(
            &project_root,
        ) {
            Ok(cfg) => {
                let roots = cfg
                    .source_roots
                    .into_iter()
                    .map(|r| r.path)
                    .collect::<Vec<_>>();
                (cfg.workspace_root, roots)
            }
            Err(nova_project::ProjectError::UnknownProjectType { .. }) => {
                (project_root.clone(), vec![project_root.clone()])
            }
            Err(err) => {
                tracing::warn!(
                    target = "nova.lsp",
                    error = ?err,
                    "failed to load project configuration for distributed mode; falling back to indexing workspace root"
                );
                (project_root.clone(), vec![project_root.clone()])
            }
        };

        let workspace_root = match workspace_root.canonicalize() {
            Ok(root) => root,
            Err(err) => {
                tracing::debug!(
                    target = "nova.lsp",
                    error = ?err,
                    path = %workspace_root.display(),
                    "failed to canonicalize workspace root for distributed mode; using provided path"
                );
                workspace_root
            }
        };
        let source_roots = source_roots
            .into_iter()
            .map(|root| match root.canonicalize() {
                Ok(root) => root,
                Err(err) => {
                    tracing::debug!(
                        target = "nova.lsp",
                        error = ?err,
                        path = %root.display(),
                        "failed to canonicalize source root for distributed mode; using provided path"
                    );
                    root
                }
            })
            .collect::<Vec<_>>();

        let cache_dir = match nova_cache::CacheDir::new(
            &workspace_root,
            nova_cache::CacheConfig::from_env(),
        ) {
            Ok(dir) => dir.indexes_dir(),
            Err(err) => {
                tracing::warn!(
                    target = "nova.lsp",
                    error = ?err,
                    "failed to open cache dir for distributed mode; disabling distributed router"
                );
                return;
            }
        };

        let run_dir = distributed_run_dir();
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            builder.mode(0o700);
            if let Err(err) = builder.create(&run_dir) {
                if err.kind() != std::io::ErrorKind::AlreadyExists {
                    tracing::warn!(
                        target = "nova.lsp",
                        run_dir = %run_dir.display(),
                        error = ?err,
                        "failed to create distributed run dir; disabling distributed router"
                    );
                    return;
                }
            }
            if let Err(err) =
                std::fs::set_permissions(&run_dir, std::fs::Permissions::from_mode(0o700))
            {
                tracing::warn!(
                    target = "nova.lsp",
                    run_dir = %run_dir.display(),
                    error = ?err,
                    "failed to set distributed run dir permissions; disabling distributed router"
                );
                return;
            }
        }

        #[cfg(not(unix))]
        {
            if let Err(err) = std::fs::create_dir_all(&run_dir) {
                tracing::warn!(
                    target = "nova.lsp",
                    run_dir = %run_dir.display(),
                    error = ?err,
                    "failed to create distributed run dir; disabling distributed router"
                );
                return;
            }
        }

        let listen_addr = distributed_listen_addr(&run_dir);

        // Keep thread counts bounded: distributed mode is mostly I/O + process supervision.
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(2)
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(err) => {
                tracing::warn!(
                    target = "nova.lsp",
                    error = ?err,
                    "failed to create tokio runtime for distributed router; disabling distributed mode"
                );
                return;
            }
        };

        let router_config = nova_router::DistributedRouterConfig::local_ipc(
            listen_addr,
            cli.worker_command.clone(),
            cache_dir,
        );

        let frontend = match runtime.block_on(nova_lsp::NovaLspFrontend::new_distributed(
            router_config,
            source_roots.clone(),
        )) {
            Ok(frontend) => Arc::new(frontend),
            Err(err) => {
                tracing::warn!(
                    target = "nova.lsp",
                    error = ?err,
                    "failed to start distributed router; falling back to in-process workspace indexing"
                );
                match std::fs::remove_dir_all(&run_dir) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => {
                        tracing::debug!(
                            target = "nova.lsp",
                            dir = %run_dir.display(),
                            error = %err,
                            "failed to remove distributed router run directory after startup failure"
                        );
                    }
                }
                return;
            }
        };

        let index_frontend = Arc::clone(&frontend);
        let initial_index =
            Some(runtime.spawn(async move { index_frontend.index_workspace().await }));

        self.distributed = Some(DistributedServerState {
            workspace_root,
            source_roots,
            run_dir,
            runtime,
            frontend,
            initial_index,
        });
    }

    pub(super) fn shutdown_distributed_router(&mut self, timeout: Duration) {
        let Some(dist) = self.distributed.take() else {
            return;
        };

        let frontend = Arc::clone(&dist.frontend);
        let shutdown = dist
            .runtime
            .block_on(async move { tokio::time::timeout(timeout, frontend.shutdown()).await });
        match shutdown {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                tracing::warn!(
                    target = "nova.lsp",
                    error = ?err,
                    "failed to shut down distributed router"
                );
            }
            Err(err) => {
                tracing::warn!(
                    target = "nova.lsp",
                    timeout = ?timeout,
                    error = ?err,
                    "timed out shutting down distributed router"
                );
            }
        }

        match std::fs::remove_dir_all(&dist.run_dir) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                tracing::debug!(
                    target = "nova.lsp",
                    dir = %dist.run_dir.display(),
                    error = %err,
                    "failed to remove distributed router run directory"
                );
            }
        }
        drop(dist);
    }
}
