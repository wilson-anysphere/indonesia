use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nova_db::{Database, FileId};
use nova_vfs::FileSystem;
use nova_vfs::VfsPath;

use crate::engine::WorkspaceEngine;

/// An owned, thread-safe view of the current workspace contents.
///
/// This is designed as a lightweight adapter for running `nova_ide::code_intelligence`
/// on top of the `nova_db::Database` trait while preserving the `FileId`s allocated
/// by the workspace VFS.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceSnapshot {
    /// Best-effort local path for each file.
    ///
    /// Non-local VFS paths (archives, decompiled files, arbitrary URIs) are stored
    /// as `None` because `nova_db::Database` models paths as `std::path::Path`.
    pub file_paths: HashMap<FileId, Option<PathBuf>>,
    /// File contents keyed by the *existing* VFS `FileId`.
    pub file_contents: HashMap<FileId, Arc<String>>,
    /// Stable ordering of all file IDs known to this snapshot.
    pub all_file_ids: Vec<FileId>,
    /// Reverse lookup for local filesystem paths.
    pub path_to_id: HashMap<PathBuf, FileId>,
}

impl WorkspaceSnapshot {
    /// Capture an owned snapshot from the in-memory workspace engine.
    ///
    /// This preserves the existing `FileId`s allocated by the VFS. Content is read
    /// from the VFS overlay when possible (so open documents win over disk). When
    /// the VFS cannot provide a file's text, we fall back to the Salsa input stored
    /// in the engine.
    pub(crate) fn from_engine(engine: &WorkspaceEngine) -> Self {
        let all_file_ids = engine.vfs().all_file_ids();

        let mut file_paths = HashMap::with_capacity(all_file_ids.len());
        let mut file_contents = HashMap::with_capacity(all_file_ids.len());
        let mut path_to_id = HashMap::with_capacity(all_file_ids.len());

        for file_id in &all_file_ids {
            let vfs_path = engine.vfs().path_for_id(*file_id);

            let local_path = vfs_path
                .as_ref()
                .and_then(VfsPath::as_local_path)
                .map(Path::to_path_buf);

            file_paths.insert(*file_id, local_path.clone());
            if let Some(path) = local_path {
                path_to_id.insert(path, *file_id);
            }

            let from_vfs = vfs_path
                .as_ref()
                .and_then(|path| engine.vfs().read_to_string(path).ok())
                .map(Arc::new);

            let content = from_vfs
                .or_else(|| engine.salsa_file_content(*file_id))
                .unwrap_or_else(|| Arc::new(String::new()));

            file_contents.insert(*file_id, content);
        }

        Self {
            file_paths,
            file_contents,
            all_file_ids,
            path_to_id,
        }
    }

    /// Build a deterministic snapshot from explicit `(path, contents)` pairs.
    ///
    /// This is primarily intended for batch CLI workflows (e.g. project-wide
    /// diagnostics) where we want stable `FileId`s without requiring a VFS.
    ///
    /// `FileId`s are assigned deterministically in sorted path order.
    #[must_use]
    pub fn from_sources(root: &Path, mut sources: Vec<(PathBuf, String)>) -> Self {
        for (path, _) in &mut sources {
            if path.is_relative() {
                *path = root.join(&*path);
            }
        }

        // Sort deterministically by (path, contents) so duplicates resolve deterministically.
        sources.sort_by(|(a_path, a_text), (b_path, b_text)| {
            a_path.cmp(b_path).then_with(|| a_text.cmp(b_text))
        });

        let mut file_paths = HashMap::with_capacity(sources.len());
        let mut file_contents = HashMap::with_capacity(sources.len());
        let mut path_to_id = HashMap::with_capacity(sources.len());
        let mut all_file_ids = Vec::with_capacity(sources.len());

        let mut next_raw: u32 = 0;
        for (path, text) in sources {
            if let Some(existing) = path_to_id.get(&path).copied() {
                // Deterministically keep the last entry for this path (see sorting above).
                file_contents.insert(existing, Arc::new(text));
                continue;
            }

            let file_id = FileId::from_raw(next_raw);
            next_raw = next_raw.saturating_add(1);

            all_file_ids.push(file_id);
            file_paths.insert(file_id, Some(path.clone()));
            file_contents.insert(file_id, Arc::new(text));
            path_to_id.insert(path, file_id);
        }

        Self {
            file_paths,
            file_contents,
            all_file_ids,
            path_to_id,
        }
    }
}

impl Database for WorkspaceSnapshot {
    fn file_content(&self, file_id: FileId) -> &str {
        self.file_contents
            .get(&file_id)
            .map(|s| s.as_str())
            .unwrap_or("")
    }

    fn file_path(&self, file_id: FileId) -> Option<&Path> {
        self.file_paths.get(&file_id).and_then(|p| p.as_deref())
    }

    fn all_file_ids(&self) -> Vec<FileId> {
        self.all_file_ids.clone()
    }

    fn file_id(&self, path: &Path) -> Option<FileId> {
        self.path_to_id.get(path).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use nova_core::AbsPathBuf;

    #[test]
    fn from_sources_roundtrips_file_id_lookup() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let a = root.join("src/A.java");
        let b = root.join("src/B.java");

        let snapshot = WorkspaceSnapshot::from_sources(
            root,
            vec![
                (
                    b.strip_prefix(root).unwrap().to_path_buf(),
                    "class B {}".to_string(),
                ),
                (
                    a.strip_prefix(root).unwrap().to_path_buf(),
                    "class A {}".to_string(),
                ),
            ],
        );

        let a_id = snapshot.file_id(&a).expect("file id for A");
        assert!(snapshot.all_file_ids().contains(&a_id));
        assert_eq!(snapshot.file_content(a_id), "class A {}");
        assert_eq!(snapshot.file_path(a_id), Some(a.as_path()));
    }

    #[test]
    fn from_engine_preserves_vfs_file_ids() {
        let workspace = crate::Workspace::new_in_memory();
        let tmp = tempfile::tempdir().unwrap();
        let abs = AbsPathBuf::new(tmp.path().join("Main.java")).unwrap();
        let uri = nova_core::path_to_file_uri(&abs).unwrap();
        let path = VfsPath::uri(uri);

        let file_id = workspace.open_document(path.clone(), "class Main {}".to_string(), 1);
        let engine = workspace.engine_for_tests();

        let snapshot = WorkspaceSnapshot::from_engine(engine);

        assert_eq!(engine.vfs().get_id(&path), Some(file_id));
        assert_eq!(snapshot.file_id(abs.as_path()), Some(file_id));
        assert!(snapshot.all_file_ids().contains(&file_id));
        assert_eq!(snapshot.file_content(file_id), "class Main {}");
    }
}
