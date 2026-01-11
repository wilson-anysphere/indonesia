use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use once_cell::sync::Lazy;

use nova_db::{Database, FileId};
use nova_framework_spring::SpringWorkspaceIndex;

use crate::framework_cache;
use crate::spring_di::SpringWorkspaceCache;

static SPRING_CONFIG_CACHE: Lazy<SpringWorkspaceCache<SpringWorkspaceIndex>> =
    Lazy::new(SpringWorkspaceCache::default);

pub(crate) fn workspace_index(
    db: &dyn Database,
    file: FileId,
) -> Option<Arc<SpringWorkspaceIndex>> {
    let path = db.file_path(file)?;
    let root = crate::spring_di::discover_project_root(path);

    let metadata = framework_cache::spring_metadata_index(&root);

    let files = collect_relevant_files(db, &root);
    let fingerprint = sources_fingerprint(db, &files, &metadata);

    Some(
        SPRING_CONFIG_CACHE.get_or_update_with(root, fingerprint, || {
            let mut index = SpringWorkspaceIndex::new(metadata);
            for entry in files {
                let text = db.file_content(entry.file_id);
                match entry.kind {
                    FileKind::Java => index.add_java_file(entry.path, text),
                    FileKind::Config => index.add_config_file(entry.path, text),
                }
            }
            index
        }),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileKind {
    Java,
    Config,
}

#[derive(Debug, Clone)]
struct FileEntry {
    path: PathBuf,
    file_id: FileId,
    kind: FileKind,
}

fn collect_relevant_files(db: &dyn Database, root: &Path) -> Vec<FileEntry> {
    let mut out = Vec::new();

    for file_id in db.all_file_ids() {
        let Some(path) = db.file_path(file_id) else {
            continue;
        };
        if !path.starts_with(root) {
            continue;
        }

        if path.extension().and_then(|e| e.to_str()) == Some("java") {
            let text = db.file_content(file_id);
            // Only scan Java sources that might contain config key usages.
            if !(text.contains("@Value") || text.contains("@ConfigurationProperties")) {
                continue;
            }
            out.push(FileEntry {
                path: path.to_path_buf(),
                file_id,
                kind: FileKind::Java,
            });
            continue;
        }

        if is_spring_properties_file(path) || is_spring_yaml_file(path) {
            out.push(FileEntry {
                path: path.to_path_buf(),
                file_id,
                kind: FileKind::Config,
            });
        }
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn sources_fingerprint(
    db: &dyn Database,
    files: &[FileEntry],
    metadata: &Arc<nova_config_metadata::MetadataIndex>,
) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();

    // Invalidate when the underlying metadata index changes (cache uses build markers).
    let meta_ptr = Arc::as_ptr(metadata) as usize;
    meta_ptr.hash(&mut hasher);

    for entry in files {
        entry.path.hash(&mut hasher);
        db.file_content(entry.file_id).hash(&mut hasher);
    }

    hasher.finish()
}

fn is_spring_properties_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    name.starts_with("application")
        && path.extension().and_then(|e| e.to_str()) == Some("properties")
}

fn is_spring_yaml_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if !name.starts_with("application") {
        return false;
    }
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("yml" | "yaml")
    )
}
