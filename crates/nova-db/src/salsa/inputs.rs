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

    /// Whether the host considers the file content to be "dirty" (in-memory edits not yet saved
    /// to disk).
    ///
    /// This is used by warm-start / persistence logic:
    /// - avoid overwriting on-disk caches with transient editor text
    /// - allow indexing to distinguish on-disk state from in-memory overlays
    ///
    /// Hosts should set this to `true` when a file is modified in an editor buffer and back to
    /// `false` once the buffer state matches the on-disk content again.
    #[ra_salsa::input]
    fn file_is_dirty(&self, file: FileId) -> bool;

    /// Stable list of all files known to the host.
    ///
    /// This is intentionally an input so Salsa snapshots can enumerate files
    /// without consulting non-tracked host state.
    #[ra_salsa::input]
    fn all_file_ids(&self) -> Arc<Vec<FileId>>;

    /// Whether a file exists on disk (or in the VFS).
    #[ra_salsa::input]
    fn file_exists(&self, file: FileId) -> bool;

    /// Per-project configuration input (classpath, source roots, language level, ...).
    #[ra_salsa::input]
    fn project_config(&self, project: ProjectId) -> Arc<ProjectConfig>;

    /// Owning project for a file.
    #[ra_salsa::input]
    fn file_project(&self, file: FileId) -> ProjectId;

    /// Stable list of files belonging to `project`.
    ///
    /// Callers should provide the file IDs in stable, project-relative order
    /// (typically sorted by `file_rel_path`) to keep downstream results
    /// deterministic.
    #[ra_salsa::input]
    fn project_files(&self, project: ProjectId) -> Arc<Vec<FileId>>;

    /// Stable, project-relative file path used for persistence keys.
    #[ra_salsa::input]
    fn file_rel_path(&self, file: FileId) -> Arc<String>;

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
