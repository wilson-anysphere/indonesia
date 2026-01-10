use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

use async_channel::{Receiver, Sender};
use nova_config::EffectiveConfig;
use nova_core::TextEdit;
use nova_db::RootDatabase;
use nova_ide::{DebugConfiguration, Project};
use nova_index::{ProjectIndexes, SymbolLocation};
use nova_scheduler::Scheduler;
use nova_types::{CompletionItem, Diagnostic as NovaDiagnostic};
use nova_vfs::{ContentChange, DocumentError, FileIdRegistry, FileSystem, LocalFs, OverlayFs, VfsPath};

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

pub(crate) struct WorkspaceEngine {
    vfs: OverlayFs<LocalFs>,
    file_ids: Mutex<FileIdRegistry>,
    known_files: Mutex<HashSet<VfsPath>>,

    db: Mutex<RootDatabase>,
    indexes: Arc<Mutex<ProjectIndexes>>,

    config: RwLock<EffectiveConfig>,
    scheduler: Scheduler,
    subscribers: Arc<Mutex<Vec<Sender<WorkspaceEvent>>>>,

    project: RwLock<Option<Project>>,
}

impl WorkspaceEngine {
    pub fn new() -> Self {
        Self {
            vfs: OverlayFs::new(LocalFs::new()),
            file_ids: Mutex::new(FileIdRegistry::new()),
            known_files: Mutex::new(HashSet::new()),
            db: Mutex::new(RootDatabase::new()),
            indexes: Arc::new(Mutex::new(ProjectIndexes::default())),
            config: RwLock::new(EffectiveConfig::default()),
            scheduler: Scheduler::default(),
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

    pub fn open_document(&self, path: VfsPath, text: String, version: i32) {
        self.track_file(&path);
        self.update_db_text(&path, &text);
        self.vfs.open(path.clone(), text, version);

        self.publish(WorkspaceEvent::FileChanged { file: path.clone() });
        self.publish_diagnostics(path);
    }

    pub fn close_document(&self, path: &VfsPath) {
        self.vfs.close(path);
    }

    pub fn apply_changes(
        &self,
        path: &VfsPath,
        new_version: i32,
        changes: &[ContentChange],
    ) -> Result<Vec<TextEdit>, DocumentError> {
        let edits = self.vfs.apply_changes(path, new_version, changes)?;

        if let Some(text) = self.vfs.document_text(path) {
            self.update_db_text(path, &text);
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

        let files: Vec<VfsPath> = self
            .known_files
            .lock()
            .expect("workspace known files lock poisoned")
            .iter()
            .cloned()
            .collect();

        self.publish(WorkspaceEvent::Status(WorkspaceStatus::IndexingStarted));

        let vfs = self.vfs.clone();
        let indexes_arc = Arc::clone(&self.indexes);
        let subscribers = Arc::clone(&self.subscribers);
        self.scheduler.spawn_background(move |_| {
            let total = files.len();
            let mut new_indexes = ProjectIndexes::default();

            for (idx, path) in files.iter().enumerate() {
                if let Ok(text) = vfs.read_to_string(path) {
                    index_symbols(&mut new_indexes, path, &text);
                }
                publish_to_subscribers(
                    &subscribers,
                    WorkspaceEvent::IndexProgress(IndexProgress {
                        current: idx + 1,
                        total,
                    }),
                );
            }

            *indexes_arc
                .lock()
                .expect("workspace indexes lock poisoned") = new_indexes;

            publish_to_subscribers(
                &subscribers,
                WorkspaceEvent::Status(WorkspaceStatus::IndexingReady),
            );
            Ok(())
        });
    }

    pub fn debug_configurations(&self, root: &Path) -> Vec<DebugConfiguration> {
        let mut project = self.project.write().expect("workspace project lock poisoned");
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

    fn track_file(&self, path: &VfsPath) {
        self.known_files
            .lock()
            .expect("workspace known files lock poisoned")
            .insert(path.clone());
        self.file_ids
            .lock()
            .expect("workspace file id registry poisoned")
            .file_id(path.clone());
    }

    fn update_db_text(&self, path: &VfsPath, text: &str) {
        let Some(local) = path.as_local_path() else {
            return;
        };
        let mut db = self.db.lock().expect("workspace db lock poisoned");
        let file_id = db.file_id_for_path(local);
        db.set_file_text(file_id, text.to_string());
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

fn publish_to_subscribers(subscribers: &Arc<Mutex<Vec<Sender<WorkspaceEvent>>>>, event: WorkspaceEvent) {
    let mut subs = subscribers
        .lock()
        .expect("workspace subscriber mutex poisoned");
    subs.retain(|tx| tx.try_send(event.clone()).is_ok());
}

fn index_symbols(indexes: &mut ProjectIndexes, file: &VfsPath, text: &str) {
    for (line_no, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        let Some((before, after)) = trimmed.split_once("class ") else {
            continue;
        };
        if before
            .chars()
            .last()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            continue;
        }

        let name = after
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_matches('{')
            .trim_matches(';');
        if name.is_empty() {
            continue;
        }

        indexes.symbols.insert(
            name.to_string(),
            SymbolLocation {
                file: file.to_string(),
                line: line_no as u32,
                column: 0,
            },
        );
    }
}
