use crate::stdio_fs;
use crate::stdio_fs::LspFs;

use lsp_types::{TextDocumentContentChangeEvent, Uri};
use nova_decompile::DecompiledDocumentStore;
use nova_memory::MemoryManager;
use nova_vfs::{ChangeEvent, DocumentError, FileSystem, LocalFs, Vfs, VfsPath};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

pub(super) struct AnalysisState {
    pub(super) vfs: Vfs<LspFs>,
    pub(super) decompiled_store: Arc<DecompiledDocumentStore>,
    pub(super) file_paths: HashMap<nova_db::FileId, PathBuf>,
    pub(super) file_exists: HashMap<nova_db::FileId, bool>,
    pub(super) file_contents: HashMap<nova_db::FileId, Arc<String>>,
    pub(super) salsa: nova_db::SalsaDatabase,
}

impl std::fmt::Debug for AnalysisState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnalysisState")
            .field("file_count", &self.file_contents.len())
            .finish()
    }
}

impl AnalysisState {
    pub(super) fn path_for_uri(&self, uri: &Uri) -> VfsPath {
        VfsPath::from(uri)
    }

    pub(super) fn file_id_for_uri(&mut self, uri: &Uri) -> (nova_db::FileId, VfsPath) {
        let path = self.path_for_uri(uri);
        let file_id = self.vfs.file_id(path.clone());
        if let Some(local) = path.as_local_path() {
            self.file_paths.insert(file_id, local.to_path_buf());
            self.salsa
                .set_file_path(file_id, local.to_string_lossy().to_string());
        }
        (file_id, path)
    }

    pub(super) fn file_is_known(&self, file_id: nova_db::FileId) -> bool {
        self.file_exists.contains_key(&file_id)
    }

    pub(super) fn open_document(
        &mut self,
        uri: Uri,
        text: String,
        version: i32,
    ) -> nova_db::FileId {
        let text = Arc::new(text);
        let path = self.path_for_uri(&uri);
        let id = self
            .vfs
            .open_document_arc(path.clone(), Arc::clone(&text), version);
        if let Some(local) = path.as_local_path() {
            self.file_paths.insert(id, local.to_path_buf());
            self.salsa
                .set_file_path(id, local.to_string_lossy().to_string());
        }
        self.file_exists.insert(id, true);
        self.file_contents.insert(id, Arc::clone(&text));
        self.salsa.set_file_exists(id, true);
        self.salsa.set_file_text_arc(id, text);
        id
    }

    pub(super) fn apply_document_changes(
        &mut self,
        uri: &Uri,
        new_version: i32,
        changes: &[TextDocumentContentChangeEvent],
    ) -> Result<ChangeEvent, DocumentError> {
        let evt = self
            .vfs
            .apply_document_changes_lsp(uri, new_version, changes)?;
        if let ChangeEvent::DocumentChanged { file_id, path, .. } = &evt {
            self.file_exists.insert(*file_id, true);
            if let Some(text) = self.vfs.open_document_text_arc(path) {
                self.file_contents.insert(*file_id, Arc::clone(&text));
                self.salsa.set_file_exists(*file_id, true);
                self.salsa.set_file_text_arc(*file_id, text);
            } else if let Ok(text) = self.vfs.read_to_string(path) {
                let text = Arc::new(text);
                self.file_contents.insert(*file_id, Arc::clone(&text));
                self.salsa.set_file_exists(*file_id, true);
                self.salsa.set_file_text_arc(*file_id, text);
            }
        }
        Ok(evt)
    }

    pub(super) fn close_document(&mut self, uri: &Uri) {
        self.vfs.close_document_lsp(uri);
        self.refresh_from_disk(uri);
    }

    pub(super) fn mark_missing(&mut self, uri: &Uri) {
        let (file_id, _) = self.file_id_for_uri(uri);
        self.file_exists.insert(file_id, false);
        self.file_contents.remove(&file_id);
        self.salsa.set_file_text(file_id, String::new());
        self.salsa.set_file_exists(file_id, false);
    }

    pub(super) fn refresh_from_disk(&mut self, uri: &Uri) {
        let (file_id, path) = self.file_id_for_uri(uri);
        match self.vfs.read_to_string(&path) {
            Ok(text) => {
                let text = Arc::new(text);
                self.file_exists.insert(file_id, true);
                self.file_contents.insert(file_id, Arc::clone(&text));
                self.salsa.set_file_exists(file_id, true);
                self.salsa.set_file_text_arc(file_id, text);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                self.file_exists.insert(file_id, false);
                self.file_contents.remove(&file_id);
                self.salsa.set_file_text(file_id, String::new());
                self.salsa.set_file_exists(file_id, false);
            }
            Err(_) => {
                // Treat other IO errors as a cache miss; keep previous state.
            }
        }
    }

    pub(super) fn ensure_loaded(&mut self, uri: &Uri) -> nova_db::FileId {
        let (file_id, path) = self.file_id_for_uri(uri);

        // If we already have a view of the file (present or missing), keep it until we receive an
        // explicit notification (didChangeWatchedFiles) telling us it changed.
        if self.file_is_known(file_id) {
            // Decompiled virtual documents can transition from missing -> present without any
            // filesystem watcher event (they may be inserted into the VFS virtual document store
            // later, e.g. after `goto_definition_jdk`).
            //
            // For on-disk files we keep the "known missing" cache until we receive explicit
            // invalidation (didChangeWatchedFiles) to avoid unnecessary disk I/O.
            if !self.exists(file_id)
                && matches!(
                    &path,
                    VfsPath::Decompiled { .. } | VfsPath::LegacyDecompiled { .. }
                )
                && self.vfs.exists(&path)
            {
                self.refresh_from_disk(uri);
            }
            return file_id;
        }

        self.refresh_from_disk(uri);
        file_id
    }

    pub(super) fn exists(&self, file_id: nova_db::FileId) -> bool {
        self.file_exists.get(&file_id).copied().unwrap_or(false)
    }

    pub(super) fn rename_uri(&mut self, from: &Uri, to: &Uri) -> nova_db::FileId {
        let from_path = self.path_for_uri(from);
        let to_path = self.path_for_uri(to);
        let id = self.vfs.rename_path(&from_path, to_path.clone());
        if let Some(local) = to_path.as_local_path() {
            self.file_paths.insert(id, local.to_path_buf());
            self.salsa
                .set_file_path(id, local.to_string_lossy().to_string());
        } else {
            self.file_paths.remove(&id);
            self.salsa.set_file_path(id, String::new());
        }
        // Keep content/existence under the preserved id; callers should refresh content from disk if needed.
        id
    }

    pub(super) fn new_with_memory(memory: &MemoryManager) -> Self {
        let decompiled_store = stdio_fs::decompiled_store_from_env_best_effort();

        let fs = LspFs::new(LocalFs::new(), decompiled_store.clone());
        let vfs = Vfs::new(fs);
        let salsa = nova_db::SalsaDatabase::new_with_memory_manager_with_open_documents(
            memory,
            vfs.open_documents(),
        );
        let project = nova_db::ProjectId::from_raw(0);
        salsa.set_jdk_index(project, Arc::new(nova_jdk::JdkIndex::new()));
        salsa.set_classpath_index(project, None);
        Self {
            vfs,
            decompiled_store,
            file_paths: HashMap::new(),
            file_exists: HashMap::new(),
            file_contents: HashMap::new(),
            salsa,
        }
    }
}

impl Default for AnalysisState {
    fn default() -> Self {
        let decompiled_store = stdio_fs::decompiled_store_from_env_best_effort();

        let fs = LspFs::new(LocalFs::new(), decompiled_store.clone());
        let vfs = Vfs::new(fs);
        let salsa = nova_db::SalsaDatabase::new_with_open_documents(vfs.open_documents());
        let project = nova_db::ProjectId::from_raw(0);
        salsa.set_jdk_index(project, Arc::new(nova_jdk::JdkIndex::new()));
        salsa.set_classpath_index(project, None);
        Self {
            vfs,
            decompiled_store,
            file_paths: HashMap::new(),
            file_exists: HashMap::new(),
            file_contents: HashMap::new(),
            salsa,
        }
    }
}

impl nova_db::Database for AnalysisState {
    fn file_content(&self, file_id: nova_db::FileId) -> &str {
        self.file_contents
            .get(&file_id)
            .map(|text| text.as_str())
            .unwrap_or("")
    }

    fn file_path(&self, file_id: nova_db::FileId) -> Option<&std::path::Path> {
        self.file_paths.get(&file_id).map(PathBuf::as_path)
    }

    fn salsa_db(&self) -> Option<nova_db::SalsaDatabase> {
        Some(self.salsa.clone())
    }

    fn all_file_ids(&self) -> Vec<nova_db::FileId> {
        self.vfs.all_file_ids()
    }

    fn file_id(&self, path: &std::path::Path) -> Option<nova_db::FileId> {
        self.vfs.get_id(&VfsPath::local(path.to_path_buf()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::ServerState;
    use nova_db::Database as _;
    use nova_db::SourceDatabase;
    use nova_memory::MemoryBudgetOverrides;
    use nova_vfs::{ChangeEvent, VfsPath};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn editing_an_open_document_does_not_change_file_id() {
        let mut analysis = AnalysisState::default();
        let dir = tempfile::tempdir().unwrap();
        let abs = nova_core::AbsPathBuf::new(dir.path().join("Main.java")).unwrap();
        let uri: lsp_types::Uri = nova_core::path_to_file_uri(&abs).unwrap().parse().unwrap();

        let original = analysis.open_document(uri.clone(), "hello world".to_string(), 1);
        let change = lsp_types::TextDocumentContentChangeEvent {
            range: Some(lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 6,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 11,
                },
            }),
            range_length: None,
            text: "nova".to_string(),
        };
        let evt = analysis
            .apply_document_changes(&uri, 2, &[change])
            .expect("apply changes");
        match evt {
            ChangeEvent::DocumentChanged { file_id, .. } => assert_eq!(file_id, original),
            other => panic!("unexpected change event: {other:?}"),
        }

        let looked_up = analysis.ensure_loaded(&uri);
        assert_eq!(looked_up, original);
    }

    #[test]
    fn open_document_shares_text_arc_between_vfs_and_salsa() {
        let mut analysis = AnalysisState::default();
        let dir = tempfile::tempdir().unwrap();
        let abs = nova_core::AbsPathBuf::new(dir.path().join("Main.java")).unwrap();
        let uri: lsp_types::Uri = nova_core::path_to_file_uri(&abs).unwrap().parse().unwrap();

        let file_id = analysis.open_document(uri.clone(), "hello world".to_string(), 1);

        let path = analysis.path_for_uri(&uri);
        let overlay = analysis.vfs.open_document_text_arc(&path).unwrap();
        let salsa = analysis
            .salsa
            .with_snapshot(|snap| snap.file_content(file_id));
        assert!(Arc::ptr_eq(&overlay, &salsa));
    }

    #[test]
    fn apply_changes_updates_salsa_with_overlay_arc() {
        let mut analysis = AnalysisState::default();
        let dir = tempfile::tempdir().unwrap();
        let abs = nova_core::AbsPathBuf::new(dir.path().join("Main.java")).unwrap();
        let uri: lsp_types::Uri = nova_core::path_to_file_uri(&abs).unwrap().parse().unwrap();

        let file_id = analysis.open_document(uri.clone(), "hello world".to_string(), 1);

        let change = lsp_types::TextDocumentContentChangeEvent {
            range: Some(lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 6,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 11,
                },
            }),
            range_length: None,
            text: "nova".to_string(),
        };
        analysis
            .apply_document_changes(&uri, 2, &[change])
            .expect("apply changes");

        let path = analysis.path_for_uri(&uri);
        let overlay = analysis.vfs.open_document_text_arc(&path).unwrap();
        let salsa = analysis
            .salsa
            .with_snapshot(|snap| snap.file_content(file_id));
        assert_eq!(salsa.as_str(), "hello nova");
        assert!(Arc::ptr_eq(&overlay, &salsa));
    }

    #[test]
    fn ensure_loaded_can_reload_decompiled_virtual_document_after_store() {
        let mut state = ServerState::new(
            nova_config::NovaConfig::default(),
            None,
            MemoryBudgetOverrides::default(),
        );

        let uri: lsp_types::Uri = "nova:///decompiled/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef/com.example.Foo.java"
            .parse()
            .expect("valid decompiled URI");

        // Before the virtual document is stored, `ensure_loaded` caches the missing state.
        let file_id = state.analysis.ensure_loaded(&uri);
        assert!(state.analysis.file_is_known(file_id));
        assert!(!state.analysis.exists(file_id));

        let stored_text = "package com.example;\n\nclass Foo {}\n".to_string();
        state
            .analysis
            .vfs
            .store_virtual_document(VfsPath::from(&uri), stored_text.clone());

        // After storing the virtual document, `ensure_loaded` should be able to reload it even
        // though it was previously cached as missing.
        let reloaded = state.analysis.ensure_loaded(&uri);
        assert_eq!(reloaded, file_id);
        assert!(state.analysis.exists(file_id));
        assert!(
            state.analysis.file_content(file_id).contains(&stored_text),
            "expected reloaded content to contain stored text"
        );
    }

    #[test]
    fn lsp_analysis_state_reuses_salsa_memoization_for_type_diagnostics() {
        let mut analysis = AnalysisState::default();
        let dir = tempfile::tempdir().unwrap();
        let abs = nova_core::AbsPathBuf::new(dir.path().join("Main.java")).unwrap();
        let uri: lsp_types::Uri = nova_core::path_to_file_uri(&abs).unwrap().parse().unwrap();

        let text = "class Main { int add(int a, int b) { return a + b; } }".to_string();
        let file_id = analysis.open_document(uri, text, 1);

        analysis.salsa.clear_query_stats();

        let cancel = CancellationToken::new();
        let _ = nova_ide::core_file_diagnostics(&analysis, file_id, &cancel);
        let after_first = analysis.salsa.query_stats();
        let first = after_first
            .by_query
            .get("type_diagnostics")
            .copied()
            .unwrap_or_default();
        assert!(
            first.executions > 0,
            "expected type_diagnostics to execute at least once"
        );

        analysis.salsa.with_write(|db| {
            ra_salsa::Database::synthetic_write(db, ra_salsa::Durability::LOW);
        });

        let _ = nova_ide::core_file_diagnostics(&analysis, file_id, &cancel);
        let after_second = analysis.salsa.query_stats();
        let second = after_second
            .by_query
            .get("type_diagnostics")
            .copied()
            .unwrap_or_default();

        assert_eq!(
            second.executions, first.executions,
            "expected type_diagnostics to be memoized instead of re-executed"
        );
        assert!(
            second.validated_memoized > first.validated_memoized,
            "expected type_diagnostics memo to be validated after synthetic write"
        );
    }
}
