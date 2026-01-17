use nova_config::NovaConfig;
use std::path::{Path, PathBuf};

pub(crate) fn load_workspace_config_with_path(root: &Path) -> (NovaConfig, Option<PathBuf>) {
    let root = match root.canonicalize() {
        Ok(root) => root,
        Err(err) => {
            tracing::debug!(
                target = "nova.lsp",
                root = %root.display(),
                err = %err,
                "failed to canonicalize workspace root; using raw path"
            );
            root.to_path_buf()
        }
    };
    let workspace_root = nova_project::workspace_root(&root).unwrap_or(root);

    match nova_config::load_for_workspace(&workspace_root) {
        Ok((config, path)) => (config, path),
        Err(err) => {
            tracing::warn!(
                root = %workspace_root.display(),
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
