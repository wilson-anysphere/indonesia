use std::env;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(super) enum ConfigLoadWarning {
    ConfigPathCanonicalizeFailed {
        source: &'static str,
        path: PathBuf,
        error: String,
    },
    ExplicitConfigFailed {
        path: PathBuf,
        error: String,
    },
    EnvConfigFailed {
        path: PathBuf,
        error: String,
    },
    CwdFailed {
        error: String,
    },
    WorkspaceConfigFailed {
        root: PathBuf,
        error: String,
    },
}

#[derive(Debug)]
pub(super) struct LoadedConfig {
    pub(super) config: nova_config::NovaConfig,
    pub(super) warnings: Vec<ConfigLoadWarning>,
}

pub(super) fn load_config_from_args(args: &[String]) -> LoadedConfig {
    let mut warnings = Vec::new();
    // Prefer the explicit `--config` argument. This also ensures other crates
    // using `nova_config::load_for_workspace` see the same config via
    // `NOVA_CONFIG_PATH`.
    if let Some(path) = parse_config_arg(args) {
        let resolved = match path.canonicalize() {
            Ok(resolved) => resolved,
            Err(err) => {
                if err.kind() != io::ErrorKind::NotFound {
                    warnings.push(ConfigLoadWarning::ConfigPathCanonicalizeFailed {
                        source: "--config",
                        path: path.clone(),
                        error: err.to_string(),
                    });
                }
                path
            }
        };
        env::set_var("NOVA_CONFIG_PATH", &resolved);
        match nova_config::NovaConfig::load_from_path(&resolved) {
            Ok(config) => {
                return LoadedConfig { config, warnings };
            }
            Err(err) => {
                warnings.push(ConfigLoadWarning::ExplicitConfigFailed {
                    path: resolved,
                    error: err.to_string(),
                });
                return LoadedConfig {
                    config: nova_config::NovaConfig::default(),
                    warnings,
                };
            }
        }
    }

    // Fall back to `NOVA_CONFIG` env var (used by deployment wrappers). When set,
    // also mirror the value to `NOVA_CONFIG_PATH` so downstream workspace config
    // discovery uses the same file.
    if let Some(path) = env::var_os("NOVA_CONFIG").map(PathBuf::from) {
        let resolved = match path.canonicalize() {
            Ok(resolved) => resolved,
            Err(err) => {
                if err.kind() != io::ErrorKind::NotFound {
                    warnings.push(ConfigLoadWarning::ConfigPathCanonicalizeFailed {
                        source: "NOVA_CONFIG",
                        path: path.clone(),
                        error: err.to_string(),
                    });
                }
                path
            }
        };
        env::set_var("NOVA_CONFIG_PATH", &resolved);
        match nova_config::NovaConfig::load_from_path(&resolved) {
            Ok(config) => {
                return LoadedConfig { config, warnings };
            }
            Err(err) => {
                warnings.push(ConfigLoadWarning::EnvConfigFailed {
                    path: resolved,
                    error: err.to_string(),
                });
                return LoadedConfig {
                    config: nova_config::NovaConfig::default(),
                    warnings,
                };
            }
        }
    }

    // Fall back to workspace discovery (env var + workspace-root detection). We seed the
    // search from the current working directory.
    let cwd = match env::current_dir() {
        Ok(dir) => dir,
        Err(err) => {
            warnings.push(ConfigLoadWarning::CwdFailed {
                error: err.to_string(),
            });
            return LoadedConfig {
                config: nova_config::NovaConfig::default(),
                warnings,
            };
        }
    };

    let root = nova_project::workspace_root(&cwd).unwrap_or(cwd);

    match nova_config::load_for_workspace(&root) {
        Ok((config, path)) => {
            if let Some(path) = path {
                env::set_var("NOVA_CONFIG_PATH", &path);
            }
            LoadedConfig { config, warnings }
        }
        Err(err) => {
            warnings.push(ConfigLoadWarning::WorkspaceConfigFailed {
                root,
                error: err.to_string(),
            });
            LoadedConfig {
                config: nova_config::NovaConfig::default(),
                warnings,
            }
        }
    }
}

pub(super) fn reload_config_best_effort(
    project_root: Option<&Path>,
) -> Result<nova_config::NovaConfig, String> {
    // Prefer explicit `NOVA_CONFIG_PATH`, mirroring `load_config_from_args`.
    if let Some(path) = env::var_os("NOVA_CONFIG_PATH").map(PathBuf::from) {
        let resolved = match path.canonicalize() {
            Ok(resolved) => resolved,
            Err(err) => {
                tracing::debug!(
                    target = "nova.lsp",
                    path = %path.display(),
                    err = %err,
                    "failed to canonicalize NOVA_CONFIG_PATH; using raw path"
                );
                path
            }
        };
        match nova_config::NovaConfig::load_from_path(&resolved) {
            Ok(config) => return Ok(config),
            Err(nova_config::ConfigError::Io { source, .. })
                if source.kind() == io::ErrorKind::NotFound =>
            {
                // Best-effort: if the config file was deleted, fall back to defaults instead of
                // keeping stale state indefinitely.
                tracing::warn!(
                    target = "nova.lsp",
                    path = %resolved.display(),
                    "config file not found; falling back to defaults"
                );
                return Ok(nova_config::NovaConfig::default());
            }
            Err(err) => return Err(err.to_string()),
        }
    }

    // Fall back to `NOVA_CONFIG` env var (used by deployment wrappers). When set,
    // also mirror the value to `NOVA_CONFIG_PATH` so downstream workspace config
    // discovery uses the same file.
    if let Some(path) = env::var_os("NOVA_CONFIG").map(PathBuf::from) {
        let resolved = match path.canonicalize() {
            Ok(resolved) => resolved,
            Err(err) => {
                tracing::debug!(
                    target = "nova.lsp",
                    path = %path.display(),
                    err = %err,
                    "failed to canonicalize NOVA_CONFIG; using raw path"
                );
                path
            }
        };
        env::set_var("NOVA_CONFIG_PATH", &resolved);
        match nova_config::NovaConfig::load_from_path(&resolved) {
            Ok(config) => return Ok(config),
            Err(nova_config::ConfigError::Io { source, .. })
                if source.kind() == io::ErrorKind::NotFound =>
            {
                tracing::warn!(
                    target = "nova.lsp",
                    path = %resolved.display(),
                    "config file not found; falling back to defaults"
                );
                return Ok(nova_config::NovaConfig::default());
            }
            Err(err) => return Err(err.to_string()),
        }
    }

    // Fall back to workspace discovery (env var + workspace-root detection).
    let seed = match project_root.map(PathBuf::from) {
        Some(dir) => dir,
        None => match env::current_dir() {
            Ok(dir) => dir,
            Err(err) => return Err(format!("failed to determine current directory: {err}")),
        },
    };
    let root = nova_project::workspace_root(&seed).unwrap_or(seed);

    nova_config::load_for_workspace(&root)
        .map(|(config, path)| {
            if let Some(path) = path {
                env::set_var("NOVA_CONFIG_PATH", &path);
            }
            config
        })
        .map_err(|err| err.to_string())
}

fn parse_config_arg(args: &[String]) -> Option<PathBuf> {
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--config" {
            let next = args.get(i + 1)?;
            return Some(PathBuf::from(next));
        }
        if let Some(path) = arg.strip_prefix("--config=") {
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
        }
        i += 1;
    }
    None
}
