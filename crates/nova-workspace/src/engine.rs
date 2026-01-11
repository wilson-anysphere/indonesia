use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_channel::{Receiver, Sender};
use nova_config::EffectiveConfig;
use nova_core::TextEdit;
use nova_db::persistence::PersistenceConfig;
use nova_db::salsa;
use nova_db::NovaIndexing;
use nova_ide::{DebugConfiguration, Project};
use nova_index::ProjectIndexes;
use nova_memory::MemoryManager;
use nova_scheduler::{Cancelled, KeyedDebouncer, PoolKind, Scheduler};
use nova_types::{CompletionItem, Diagnostic as NovaDiagnostic};
use nova_vfs::{
    ChangeEvent, ContentChange, DocumentError, FileId, FileSystem, LocalFs, Vfs, VfsPath,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexProgress {
    pub current: usize,
    pub total: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceStatus {
    IndexingStarted,
    IndexingReady,
    IndexingError(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceEvent {
    DiagnosticsUpdated {
        file: VfsPath,
        diagnostics: Vec<NovaDiagnostic>,
    },
    IndexProgress(IndexProgress),
    Status(WorkspaceStatus),
    FileChanged {
        file: VfsPath,
    },
}

#[derive(Clone)]
pub(crate) struct WorkspaceEngineConfig {
    pub workspace_root: PathBuf,
    pub persistence: PersistenceConfig,
    pub memory: MemoryManager,
}

pub(crate) struct WorkspaceEngine {
    vfs: Vfs<LocalFs>,
    query_db: salsa::Database,
    indexes: Arc<Mutex<ProjectIndexes>>,

    config: RwLock<EffectiveConfig>,
    scheduler: Scheduler,
    index_debouncer: KeyedDebouncer<&'static str>,
    subscribers: Arc<Mutex<Vec<Sender<WorkspaceEvent>>>>,

    project: RwLock<Option<Project>>,
}

impl WorkspaceEngine {
    pub fn new(config: WorkspaceEngineConfig) -> Self {
        let scheduler = Scheduler::default();
        let index_debouncer = KeyedDebouncer::new(
            scheduler.clone(),
            PoolKind::Background,
            // Match the default LSP diagnostics debounce so edits "win" over background work.
            Duration::from_millis(200),
        );

        let WorkspaceEngineConfig {
            workspace_root,
            persistence,
            memory,
        } = config;

        let query_db = salsa::Database::new_with_persistence(&workspace_root, persistence);
        query_db.register_salsa_memo_evictor(&memory);

        Self {
            vfs: Vfs::new(LocalFs::new()),
            query_db,
            indexes: Arc::new(Mutex::new(ProjectIndexes::default())),
            config: RwLock::new(EffectiveConfig::default()),
            scheduler,
            index_debouncer,
            subscribers: Arc::new(Mutex::new(Vec::new())),
            project: RwLock::new(None),
        }
    }

    pub fn subscribe(&self) -> Receiver<WorkspaceEvent> {
        let (tx, rx) = async_channel::unbounded();
        self.subscribers
            .lock()
            .expect("workspace subscriber mutex poisoned")
            .push(tx);
        rx
    }

    pub fn open_document(&self, path: VfsPath, text: String, version: i32) -> FileId {
        let text_for_db = text.clone();
        let file_id = self.vfs.open_document(path.clone(), text, version);
        self.query_db.set_file_text(file_id, text_for_db);
        self.query_db
            .set_file_rel_path(file_id, Arc::new(path.to_string()));

        self.publish(WorkspaceEvent::FileChanged { file: path.clone() });
        self.publish_diagnostics(path);
        file_id
    }

    pub fn close_document(&self, path: &VfsPath) {
        let file_id = self.vfs.get_id(path);
        self.vfs.close_document(path);

        if let Some(file_id) = file_id {
            let exists = self.vfs.exists(path);
            self.query_db.set_file_exists(file_id, exists);
            if exists {
                if let Ok(text) = self.vfs.read_to_string(path) {
                    self.query_db.set_file_text(file_id, text);
                }
            }
        }
    }

    pub fn apply_changes(
        &self,
        path: &VfsPath,
        new_version: i32,
        changes: &[ContentChange],
    ) -> Result<Vec<TextEdit>, DocumentError> {
        let evt = self
            .vfs
            .apply_document_changes(path, new_version, changes)?;
        let (file_id, edits) = match evt {
            ChangeEvent::DocumentChanged { file_id, edits, .. } => (file_id, edits),
            other => {
                unreachable!("apply_document_changes only returns DocumentChanged ({other:?})")
            }
        };

        if let Ok(text) = self.vfs.read_to_string(path) {
            self.query_db.set_file_text(file_id, text);
        }

        self.publish(WorkspaceEvent::FileChanged { file: path.clone() });
        self.publish_diagnostics(path.clone());
        Ok(edits)
    }

    pub fn completions(&self, path: &VfsPath, offset: usize) -> Vec<CompletionItem> {
        match self.vfs.read_to_string(path) {
            Ok(text) => nova_ide::analysis::completions(&text, offset),
            Err(_) => Vec::new(),
        }
    }

    pub fn trigger_indexing(&self) {
        let enable = self
            .config
            .read()
            .expect("workspace config lock poisoned")
            .enable_indexing;
        if !enable {
            return;
        }

        let files: Vec<FileId> = self.vfs.all_file_ids();

        // Coalesce rapid edit bursts (e.g. didChange storms) and cancel in-flight indexing when
        // superseded by a newer request.
        let query_db = self.query_db.clone();
        let indexes_arc = Arc::clone(&self.indexes);
        let subscribers = Arc::clone(&self.subscribers);
        let scheduler = self.scheduler.clone();

        self.index_debouncer
            .debounce("workspace-index", move |token| {
                let ctx = scheduler.request_context_with_token("workspace/indexing", token);
                let progress = ctx.progress().start("Indexing workspace");

                publish_to_subscribers(
                    &subscribers,
                    WorkspaceEvent::Status(WorkspaceStatus::IndexingStarted),
                );

                let total = files.len();
                let mut new_indexes = ProjectIndexes::default();

                for (idx, file_id) in files.iter().enumerate() {
                    Cancelled::check(ctx.token())?;

                    let delta = query_db.with_snapshot(|snap| snap.file_index_delta(*file_id));
                    new_indexes.merge_from((*delta).clone());

                    let percentage = if total == 0 {
                        None
                    } else {
                        Some(((idx + 1) * 100 / total).min(100) as u32)
                    };
                    progress.report(Some(format!("{}/{}", idx + 1, total)), percentage);
                    publish_to_subscribers(
                        &subscribers,
                        WorkspaceEvent::IndexProgress(IndexProgress {
                            current: idx + 1,
                            total,
                        }),
                    );
                }

                *indexes_arc.lock().expect("workspace indexes lock poisoned") = new_indexes;

                progress.finish(Some("Done".to_string()));
                publish_to_subscribers(
                    &subscribers,
                    WorkspaceEvent::Status(WorkspaceStatus::IndexingReady),
                );
                Ok(())
            });
    }

    pub fn debug_configurations(&self, root: &Path) -> Vec<DebugConfiguration> {
        let mut project = self
            .project
            .write()
            .expect("workspace project lock poisoned");
        if project.is_none() {
            if let Ok(loaded) = Project::load_from_dir(root) {
                *project = Some(loaded);
            }
        }

        project
            .as_ref()
            .map(Project::discover_debug_configurations)
            .unwrap_or_default()
    }

    fn publish_diagnostics(&self, file: VfsPath) {
        let diagnostics = match self.vfs.read_to_string(&file) {
            Ok(text) => nova_ide::analysis::diagnostics(&text),
            Err(_) => Vec::new(),
        };

        self.publish(WorkspaceEvent::DiagnosticsUpdated { file, diagnostics });
    }

    fn publish(&self, event: WorkspaceEvent) {
        publish_to_subscribers(&self.subscribers, event);
    }
}

fn publish_to_subscribers(
    subscribers: &Arc<Mutex<Vec<Sender<WorkspaceEvent>>>>,
    event: WorkspaceEvent,
) {
    let mut subs = subscribers
        .lock()
        .expect("workspace subscriber mutex poisoned");
    subs.retain(|tx| tx.try_send(event.clone()).is_ok());
}

#[cfg(test)]
mod tests {
    use nova_db::NovaInputs;

    use super::*;

    #[test]
    fn file_id_mapping_is_stable_and_drives_salsa_inputs() {
        let workspace = crate::Workspace::new_in_memory();
        let tmp = tempfile::tempdir().unwrap();
        let abs = nova_core::AbsPathBuf::new(tmp.path().join("Main.java")).unwrap();
        let uri = nova_core::path_to_file_uri(&abs).unwrap();
        let path = VfsPath::uri(uri);

        let file_id = workspace.open_document(path.clone(), "class Main {}".to_string(), 1);

        // Ensure VFS round-trips between FileId and VfsPath.
        let engine = workspace.engine_for_tests();
        assert_eq!(engine.vfs.get_id(&path), Some(file_id));
        assert_eq!(engine.vfs.path_for_id(file_id), Some(path.clone()));

        // Salsa inputs should be keyed by the *same* FileId.
        engine.query_db.with_snapshot(|snap| {
            assert!(snap.file_exists(file_id));
            assert_eq!(snap.file_content(file_id).as_str(), "class Main {}");
        });

        workspace
            .apply_changes(
                &path,
                2,
                &[ContentChange::full("class Main { int x; }".to_string())],
            )
            .unwrap();

        // FileId must remain stable after edits.
        assert_eq!(engine.vfs.get_id(&path), Some(file_id));
        assert_eq!(engine.vfs.path_for_id(file_id), Some(path.clone()));
        engine.query_db.with_snapshot(|snap| {
            assert!(snap.file_exists(file_id));
            assert_eq!(snap.file_content(file_id).as_str(), "class Main { int x; }");
        });
    }
}
