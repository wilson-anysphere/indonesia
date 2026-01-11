use std::sync::Arc;

use nova_project::ProjectConfig;

use crate::{FileId, ProjectId, SourceRootId};

#[ra_salsa::query_group(NovaInputsStorage)]
pub trait NovaInputs: ra_salsa::Database {
    /// File content as last provided by the host (e.g. LSP text document sync).
    #[ra_salsa::input]
    fn file_content(&self, file: FileId) -> Arc<String>;

    /// Whether a file exists on disk (or in the VFS).
    #[ra_salsa::input]
    fn file_exists(&self, file: FileId) -> bool;

    /// Per-project configuration input (classpath, source roots, language level, ...).
    #[ra_salsa::input]
    fn project_config(&self, project: ProjectId) -> Arc<ProjectConfig>;

    /// Source root identifier for a file.
    ///
    /// This is typically assigned by the workspace/project loader after mapping
    /// `FileId` to a `ProjectConfig` source root.
    #[ra_salsa::input]
    fn source_root(&self, file: FileId) -> SourceRootId;
}
