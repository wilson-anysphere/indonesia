use std::sync::Arc;

use nova_classpath::ClasspathIndex;
use nova_jdk::JdkIndex;
use nova_project::ProjectConfig;

use crate::{FileId, ProjectId, SourceRootId};

use super::ArcEq;

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

    /// Owning project for a file.
    #[ra_salsa::input]
    fn file_project(&self, file: FileId) -> ProjectId;

    /// Source root identifier for a file.
    ///
    /// This is typically assigned by the workspace/project loader after mapping
    /// `FileId` to a `ProjectConfig` source root.
    #[ra_salsa::input]
    fn source_root(&self, file: FileId) -> SourceRootId;

    /// External JDK type index for the given project.
    #[ra_salsa::input]
    fn jdk_index(&self, project: ProjectId) -> ArcEq<JdkIndex>;

    /// Optional external project/classpath type index for the given project.
    #[ra_salsa::input]
    fn classpath_index(&self, project: ProjectId) -> Option<ArcEq<ClasspathIndex>>;
}
