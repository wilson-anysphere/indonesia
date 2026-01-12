use std::sync::Arc;

use nova_classpath::ClasspathIndex;
use nova_core::ClassId;
use nova_jdk::JdkIndex;
use nova_project::ProjectConfig;
use nova_syntax::TextEdit;

use crate::{FileId, ProjectId, SourceRootId};

use super::ArcEq;

#[ra_salsa::query_group(NovaInputsStorage)]
pub trait NovaInputs: ra_salsa::Database {
    /// File content as last provided by the host (e.g. LSP text document sync).
    #[ra_salsa::input]
    fn file_content(&self, file: FileId) -> Arc<String>;

    /// Previous file content snapshot used for incremental parsing.
    ///
    /// When paired with [`NovaInputs::file_last_edit`], this can be used by
    /// syntax queries to reparse incrementally.
    #[ra_salsa::input]
    fn file_prev_content(&self, file: FileId) -> Arc<String>;

    /// The most recent edit applied to `file`, if known.
    ///
    /// When set, syntax queries may attempt to reparse incrementally using
    /// [`NovaInputs::file_prev_content`] as the "before" text.
    #[ra_salsa::input]
    fn file_last_edit(&self, file: FileId) -> Option<TextEdit>;

    /// Whether the host considers the file content to be "dirty" (in-memory edits not yet saved
    /// to disk).
    ///
    /// This is used by warm-start / persistence logic:
    /// - avoid overwriting on-disk caches with transient editor text
    /// - allow indexing to distinguish on-disk state from in-memory overlays
    ///
    /// Hosts should set this to `true` when a file is modified in an editor buffer and back to
    /// `false` once the buffer state matches the on-disk content again.
    ///
    /// The thread-safe `Database` wrapper ensures a default value of `false` is initialized for
    /// files referenced by `project_files`.
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

    /// Stable mapping of source type binary names to globally unique [`ClassId`]s for `project`.
    ///
    /// This mapping is **host-managed** (typically by [`crate::salsa::WorkspaceLoader`]) to keep
    /// `ClassId`s stable across Salsa memo eviction and across workspace reloads.
    ///
    /// Entries must be supplied in a deterministic order (sorted by `binary_name`) so that:
    /// - Salsa snapshots observe deterministic results.
    /// - Host updates that don't change the mapping won't spuriously invalidate downstream queries.
    #[ra_salsa::input]
    fn project_class_ids(&self, project: ProjectId) -> Arc<Vec<(Arc<str>, ClassId)>>;

    /// Look up a stable [`ClassId`] for a Java binary name (e.g. `com.example.Foo$Inner`).
    ///
    /// This is derived solely from [`NovaInputs::project_class_ids`].
    fn class_id_for_name(&self, project: ProjectId, binary_name: Arc<str>) -> Option<ClassId>;

    /// Reverse lookup of a Java binary name for a stable [`ClassId`].
    ///
    /// This is derived solely from [`NovaInputs::project_class_ids`].
    fn class_name_for_id(&self, project: ProjectId, id: ClassId) -> Option<Arc<str>>;
}

fn class_id_for_name(db: &dyn NovaInputs, project: ProjectId, binary_name: Arc<str>) -> Option<ClassId> {
    let mapping = db.project_class_ids(project);
    mapping
        .binary_search_by(|(name, _)| name.as_ref().cmp(binary_name.as_ref()))
        .ok()
        .map(|idx| mapping[idx].1)
}

fn class_name_for_id(db: &dyn NovaInputs, project: ProjectId, id: ClassId) -> Option<Arc<str>> {
    let mapping = db.project_class_ids(project);
    mapping
        .iter()
        .find_map(|(name, stored_id)| (*stored_id == id).then(|| name.clone()))
}
