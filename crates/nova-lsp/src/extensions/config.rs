use nova_config::NovaConfig;
use std::path::Path;

pub(crate) fn load_workspace_config(root: &Path) -> NovaConfig {
    match nova_config::load_for_workspace(root) {
        Ok(config) => config,
        Err(err) => {
            tracing::warn!(
                root = %root.display(),
                error = %err,
                "failed to load Nova config; falling back to defaults"
            );
            NovaConfig::default()
        }
    }
}

