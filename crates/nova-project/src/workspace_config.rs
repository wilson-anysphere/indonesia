use std::path::{Path, PathBuf};

use nova_config::NovaConfig;

use crate::discover::ProjectError;

pub(crate) fn canonicalize_workspace_root(root: impl AsRef<Path>) -> Result<PathBuf, ProjectError> {
    let root = root.as_ref();
    std::fs::canonicalize(root).map_err(|source| ProjectError::Io {
        path: root.to_path_buf(),
        source,
    })
}

pub(crate) fn load_nova_config(workspace_root: &Path) -> Result<NovaConfig, ProjectError> {
    Ok(nova_config::load_for_workspace(workspace_root)?)
}
