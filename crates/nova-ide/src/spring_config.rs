use std::sync::Arc;

use nova_db::{Database, FileId};
use nova_framework_spring::SpringWorkspaceIndex;

/// Spring config workspace index entrypoint used by `code_intelligence`.
///
/// The implementation lives in `spring_config_intel` so it can maintain a root-scoped cache and
/// reuse the cached Spring Boot `spring-configuration-metadata.json` index from `framework_cache`.
pub(crate) fn workspace_index(
    db: &dyn Database,
    file: FileId,
) -> Option<Arc<SpringWorkspaceIndex>> {
    db.file_path(file)?;
    Some(crate::spring_config_intel::workspace_index_for_file(
        db, file,
    ))
}
