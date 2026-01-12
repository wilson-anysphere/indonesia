use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use nova_core::ProjectDatabase;
use nova_db::{Database, FileId, NovaInputs, SalsaDatabase, Snapshot};
use nova_vfs::FileSystem;
use nova_vfs::{LocalFs, Vfs};
use nova_vfs::VfsPath;

use crate::engine::WorkspaceEngine;

#[cfg(test)]
thread_local! {
    static WORKSPACE_SNAPSHOT_FROM_ENGINE_CALLS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
}

#[cfg(test)]
pub(crate) fn test_reset_workspace_snapshot_from_engine_calls() {
    WORKSPACE_SNAPSHOT_FROM_ENGINE_CALLS.with(|cell| cell.set(0));
}

#[cfg(test)]
pub(crate) fn test_workspace_snapshot_from_engine_calls() -> usize {
    WORKSPACE_SNAPSHOT_FROM_ENGINE_CALLS.with(std::cell::Cell::get)
}

/// An owned, thread-safe view of the current workspace contents.
///
/// This is designed as a lightweight adapter for running `nova_ide::code_intelligence`
/// on top of the `nova_db::Database` trait while preserving the `FileId`s allocated
/// by the workspace VFS.
#[derive(Clone, Default)]
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
    /// Optional long-lived Salsa query database.
    ///
    /// When this snapshot is created from a [`WorkspaceEngine`], this is set to
    /// the engine's shared query database so higher layers (e.g. `nova-ide`
    /// diagnostics) can reuse memoized results.
    salsa_db: Option<SalsaDatabase>,
}

impl fmt::Debug for WorkspaceSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkspaceSnapshot")
            .field("file_paths", &self.file_paths)
            .field("file_contents", &self.file_contents)
            .field("all_file_ids", &self.all_file_ids)
            .field("path_to_id", &self.path_to_id)
            .field("has_salsa_db", &self.salsa_db.is_some())
            .finish()
    }
}

impl WorkspaceSnapshot {
    /// Capture an owned snapshot from the in-memory workspace engine.
    ///
    /// This preserves the existing `FileId`s allocated by the VFS. Content is read
    /// primarily from the Salsa inputs stored in the engine, which already include
    /// open-document overlays. If a `FileId` exists in the VFS registry but its
    /// Salsa inputs haven't been initialized yet, we fall back to reading from the
    /// VFS (and ultimately disk for non-open files).
    pub(crate) fn from_engine(engine: &WorkspaceEngine) -> Self {
        #[cfg(test)]
        WORKSPACE_SNAPSHOT_FROM_ENGINE_CALLS.with(|cell| cell.set(cell.get() + 1));

        let vfs = engine.vfs();
        let all_file_ids = vfs.all_file_ids();
        let query_db = engine.query_db();

        let mut file_paths = HashMap::with_capacity(all_file_ids.len());
        let mut file_contents = HashMap::with_capacity(all_file_ids.len());
        let mut path_to_id = HashMap::with_capacity(all_file_ids.len());
        let empty = Arc::new(String::new());

        query_db.with_snapshot(|snap| {
            let salsa_file_ids = snap.all_file_ids();
            let salsa_file_ids: &[FileId] = salsa_file_ids.as_ref().as_slice();
            let mut salsa_idx = 0usize;

            for file_id in &all_file_ids {
                let vfs_path = vfs.path_for_id(*file_id);

                let local_path = vfs_path
                    .as_ref()
                    .and_then(VfsPath::as_local_path)
                    .map(Path::to_path_buf);

                file_paths.insert(*file_id, local_path.clone());
                if let Some(path) = local_path {
                    path_to_id.insert(path, *file_id);
                }

                let fallback_to_vfs = || {
                    vfs_path
                        .as_ref()
                        .and_then(|path| vfs.read_to_string(path).ok())
                        .map(Arc::new)
                        .unwrap_or_else(|| empty.clone())
                };

                while salsa_idx < salsa_file_ids.len() && salsa_file_ids[salsa_idx] < *file_id {
                    salsa_idx += 1;
                }
                let has_salsa_content =
                    salsa_idx < salsa_file_ids.len() && salsa_file_ids[salsa_idx] == *file_id;

                let content = if has_salsa_content {
                    // Prefer the Salsa input contents (already include open document overlays).
                    if snap.file_exists(*file_id) {
                        snap.file_content(*file_id)
                    } else {
                        empty.clone()
                    }
                } else {
                    fallback_to_vfs()
                };

                file_contents.insert(*file_id, content);
            }
        });

        Self {
            file_paths,
            file_contents,
            all_file_ids,
            path_to_id,
            salsa_db: Some(query_db),
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
            salsa_db: None,
        }
    }
}

/// A lightweight, snapshot-backed [`nova_db::Database`] view for IDE requests.
///
/// Unlike [`WorkspaceSnapshot`], this type avoids eagerly materializing all file contents.
/// It holds a single Salsa snapshot and lazily caches file text (`Arc<String>`) on demand.
///
/// ## Safety notes
///
/// The legacy [`nova_db::Database`] trait returns borrowed `&str`/`&Path` references.
/// To support lazily-populated caches, we store snapshot-owned values (`Arc<String>`,
/// `PathBuf`) in internal maps and return references to their stable heap allocations.
///
/// This requires a small amount of `unsafe` code to extend the borrow outside the
/// cache lock's lifetime. This is sound because:
/// - Cache entries are only ever inserted (never removed or replaced).
/// - The returned references point into `Arc<String>` / `PathBuf` heap allocations,
///   which remain valid for the lifetime of the view.
pub(crate) struct WorkspaceDbView {
    snapshot: Snapshot,
    vfs: Vfs<LocalFs>,
    file_contents: Mutex<HashMap<FileId, Arc<String>>>,
    file_paths: Mutex<HashMap<FileId, Option<PathBuf>>>,
    all_file_ids: OnceLock<Vec<FileId>>,
}

impl WorkspaceDbView {
    pub(crate) fn new(snapshot: Snapshot, vfs: Vfs<LocalFs>) -> Self {
        Self {
            snapshot,
            vfs,
            file_contents: Mutex::new(HashMap::new()),
            file_paths: Mutex::new(HashMap::new()),
            all_file_ids: OnceLock::new(),
        }
    }

    pub(crate) fn semantic_db(&self) -> &Snapshot {
        &self.snapshot
    }

    fn lock_unpoison<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        match mutex.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn snapshot_file_content(&self, file_id: FileId) -> Arc<String> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            nova_db::SourceDatabase::file_content(&self.snapshot, file_id)
        }))
            .ok()
            .unwrap_or_else(|| Arc::new(String::new()))
    }

    fn cached_file_content_ptr(&self, file_id: FileId) -> *const str {
        let mut cache = Self::lock_unpoison(&self.file_contents);
        let entry = cache
            .entry(file_id)
            .or_insert_with(|| self.snapshot_file_content(file_id));
        entry.as_str() as *const str
    }

    fn cached_file_path_ptr(&self, file_id: FileId) -> Option<*const Path> {
        let mut cache = Self::lock_unpoison(&self.file_paths);
        let entry = cache.entry(file_id).or_insert_with(|| {
            self.vfs
                .path_for_id(file_id)
                .as_ref()
                .and_then(VfsPath::as_local_path)
                .map(Path::to_path_buf)
        });
        entry.as_ref().map(|p| p.as_path() as *const Path)
    }
}

impl Database for WorkspaceDbView {
    fn file_content(&self, file_id: FileId) -> &str {
        let ptr = self.cached_file_content_ptr(file_id);
        // SAFETY: See type-level safety notes. The returned `&str` points into a heap allocation
        // owned by an `Arc<String>` stored in `self.file_contents`, which is never removed.
        unsafe { &*ptr }
    }

    fn file_path(&self, file_id: FileId) -> Option<&Path> {
        let ptr = self.cached_file_path_ptr(file_id)?;
        // SAFETY: See type-level safety notes. The returned `&Path` points into a heap allocation
        // owned by a `PathBuf` stored in `self.file_paths`, which is never removed.
        Some(unsafe { &*ptr })
    }

    fn all_file_ids(&self) -> Vec<FileId> {
        self.all_file_ids
            .get_or_init(|| self.vfs.all_file_ids())
            .clone()
    }

    fn file_id(&self, path: &Path) -> Option<FileId> {
        self.vfs.get_id(&VfsPath::local(path.to_path_buf()))
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

    fn salsa_db(&self) -> Option<nova_db::SalsaDatabase> {
        self.salsa_db.clone()
    }
}

impl ProjectDatabase for WorkspaceSnapshot {
    fn project_files(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self
            .file_paths
            .values()
            .filter_map(|path| path.clone())
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }

    fn file_text(&self, path: &Path) -> Option<String> {
        let file_id = self.file_id(path)?;
        Some(self.file_content(file_id).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use nova_core::AbsPathBuf;
    use nova_db::NovaInputs;
    use nova_db::NovaSyntax;
    use std::fs;

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

    #[test]
    fn from_engine_exposes_workspace_salsa_db() {
        let workspace = crate::Workspace::new_in_memory();
        let tmp = tempfile::tempdir().unwrap();
        let abs = AbsPathBuf::new(tmp.path().join("Main.java")).unwrap();
        let uri = nova_core::path_to_file_uri(&abs).unwrap();
        let path = VfsPath::uri(uri);

        let file_id = workspace.open_document(path, "class Main {}".to_string(), 1);

        let snapshot = workspace.snapshot();
        let salsa = snapshot
            .salsa_db()
            .expect("snapshot should expose workspace salsa db");

        // Basic smoke test: the DB should be usable for Salsa-backed queries without panicking.
        salsa.with_snapshot(|snap| {
            let _ = snap.parse_java(file_id);
        });
    }

    #[test]
    fn snapshot_reuses_workspace_salsa_for_cross_file_import_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src/foo")).unwrap();
        fs::create_dir_all(root.join("src/bar")).unwrap();
        let file_a = root.join("src/foo/A.java");
        let file_b = root.join("src/bar/B.java");
        fs::write(&file_a, "package foo; public class A {}".as_bytes()).unwrap();
        fs::write(
            &file_b,
            "package bar; import foo.A; public class B { A a; }".as_bytes(),
        )
        .unwrap();

        let workspace = crate::Workspace::open(&root).unwrap();
        let snapshot = workspace.snapshot();
        let file_id_b = snapshot.file_id(&file_b).expect("file id for B");

        let diagnostics = nova_ide::file_diagnostics(&snapshot, file_id_b);
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.code.as_ref() == "unresolved-import"),
            "expected cross-file import to resolve when using workspace snapshot salsa db, got: {diagnostics:?}"
        );
    }

    #[test]
    fn from_engine_prefers_salsa_file_contents_over_vfs_reads() {
        let dir = tempfile::tempdir().unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        let file_a = root.join("src/A.java");
        let file_b = root.join("src/B.java");
        fs::write(&file_a, "class A {}".as_bytes()).unwrap();
        fs::write(&file_b, "class B {}".as_bytes()).unwrap();

        let workspace = crate::Workspace::open(&root).unwrap();
        let engine = workspace.engine_for_tests();
        let vfs_a = VfsPath::local(file_a);
        let vfs_b = VfsPath::local(file_b);
        let file_id_a = engine.vfs().get_id(&vfs_a).expect("file id for A");
        let file_id_b = engine.vfs().get_id(&vfs_b).expect("file id for B");

        let from_salsa_a = engine
            .query_db
            .with_snapshot(|snap| snap.file_content(file_id_a));
        let from_salsa_b = engine
            .query_db
            .with_snapshot(|snap| snap.file_content(file_id_b));

        let snapshot = workspace.snapshot();
        let from_snapshot_a = snapshot
            .file_contents
            .get(&file_id_a)
            .expect("snapshot contents for A");
        let from_snapshot_b = snapshot
            .file_contents
            .get(&file_id_b)
            .expect("snapshot contents for B");

        assert!(
            Arc::ptr_eq(from_snapshot_a, &from_salsa_a),
            "expected snapshot to reuse the existing Salsa Arc<String> without re-reading from disk"
        );
        assert!(
            Arc::ptr_eq(from_snapshot_b, &from_salsa_b),
            "expected snapshot to reuse the existing Salsa Arc<String> without re-reading from disk"
        );
    }

    #[test]
    fn from_engine_falls_back_to_vfs_when_salsa_inputs_are_uninitialized() {
        let workspace = crate::Workspace::new_in_memory();
        let engine = workspace.engine_for_tests();

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Main.java");
        fs::write(&file, "class Main { disk }".as_bytes()).unwrap();

        // Allocate a `FileId` in the VFS registry without initializing Salsa inputs.
        let vfs_path = VfsPath::local(file.clone());
        let file_id = engine.vfs().file_id(vfs_path);

        // Snapshotting should not panic and should fall back to reading through the VFS.
        let snapshot = WorkspaceSnapshot::from_engine(engine);
        assert_eq!(snapshot.file_id(&file), Some(file_id));
        assert_eq!(snapshot.file_content(file_id), "class Main { disk }");
    }

    #[test]
    fn from_engine_exposes_salsa_db_for_diagnostics_reuse() {
        let workspace = crate::Workspace::new_in_memory();
        let tmp = tempfile::tempdir().unwrap();
        let abs = AbsPathBuf::new(tmp.path().join("Main.java")).unwrap();
        let uri = nova_core::path_to_file_uri(&abs).unwrap();
        let path = VfsPath::uri(uri);

        let file_id = workspace.open_document(path.clone(), "class Main {}".to_string(), 1);
        let engine = workspace.engine_for_tests();

        let snapshot = WorkspaceSnapshot::from_engine(engine);
        assert!(
            snapshot.salsa_db().is_some(),
            "engine-backed snapshots should expose a Salsa DB for memoized diagnostics"
        );

        // Ensure `nova_ide::file_diagnostics_with_semantic_db` can run on the
        // engine-backed Salsa DB by checking that the DB records memoization
        // validation across repeated calls.
        let salsa = snapshot
            .salsa_db()
            .expect("snapshot should expose the engine Salsa DB");
        salsa.clear_query_stats();

        // Force a revision bump so Salsa validates memoized values and emits
        // `DidValidateMemoizedValue` events.
        salsa.request_cancellation();
        let _ = salsa.with_snapshot(|semantic| {
            nova_ide::file_diagnostics_with_semantic_db(&snapshot, semantic, file_id)
        });
        let first = salsa.query_stats();
        let validated_first: u64 = first
            .by_query
            .values()
            .map(|stat| stat.validated_memoized)
            .sum();
        let executed_first: u64 = first.by_query.values().map(|stat| stat.executions).sum();
        assert!(
            validated_first > 0 || executed_first > 0,
            "expected Salsa queries to run on the engine DB when diagnostics are computed"
        );

        salsa.request_cancellation();
        let _ = salsa.with_snapshot(|semantic| {
            nova_ide::file_diagnostics_with_semantic_db(&snapshot, semantic, file_id)
        });
        let second = salsa.query_stats();
        let validated_second: u64 = second
            .by_query
            .values()
            .map(|stat| stat.validated_memoized)
            .sum();
        assert!(
            validated_second > validated_first,
            "expected memoized Salsa values to be validated on the second diagnostics run"
        );
    }

    #[test]
    fn from_engine_uses_open_document_overlay_text() {
        let dir = tempfile::tempdir().unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        let file = root.join("src/Main.java");
        fs::write(&file, "class Main { disk }".as_bytes()).unwrap();

        let workspace = crate::Workspace::open(&root).unwrap();
        let vfs_path = VfsPath::local(file);
        let file_id = workspace.open_document(vfs_path, "class Main { overlay }".to_string(), 1);

        let engine = workspace.engine_for_tests();
        let from_salsa = engine
            .query_db
            .with_snapshot(|snap| snap.file_content(file_id));

        let snapshot = workspace.snapshot();
        assert_eq!(snapshot.file_content(file_id), "class Main { overlay }");
        assert!(
            Arc::ptr_eq(
                snapshot
                    .file_contents
                    .get(&file_id)
                    .expect("snapshot contents for overlay"),
                &from_salsa
            ),
            "expected snapshot to reuse the existing Salsa Arc<String> for overlay contents"
        );
    }

    #[test]
    fn workspace_snapshot_implements_project_database() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let snapshot = WorkspaceSnapshot::from_sources(
            root,
            vec![
                (PathBuf::from("src/B.java"), "class B {}".to_string()),
                (PathBuf::from("src/A.java"), "class A {}".to_string()),
            ],
        );

        let files = ProjectDatabase::project_files(&snapshot);
        assert_eq!(
            files,
            vec![root.join("src/A.java"), root.join("src/B.java")]
        );

        let text = ProjectDatabase::file_text(&snapshot, &files[0]).expect("file text");
        assert_eq!(text, "class A {}");
    }
}
