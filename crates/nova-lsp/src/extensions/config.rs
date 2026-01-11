use nova_config::NovaConfig;
use std::path::{Path, PathBuf};

pub(crate) fn load_workspace_config_with_path(root: &Path) -> (NovaConfig, Option<PathBuf>) {
    match nova_config::load_for_workspace(root) {
        Ok((config, path)) => (config, path),
        Err(err) => {
            tracing::warn!(
                root = %root.display(),
                error = %err,
                "failed to load Nova config; falling back to defaults"
            );
            (NovaConfig::default(), None)
        }
    }
}

pub(crate) fn load_workspace_config(root: &Path) -> NovaConfig {
    load_workspace_config_with_path(root).0
}
