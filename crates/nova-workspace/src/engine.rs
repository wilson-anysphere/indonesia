use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_channel::{Receiver, Sender};
use crossbeam_channel as channel;
use lsp_types::Position;
use notify::{RecursiveMode, Watcher};
use nova_cache::normalize_rel_path;
use nova_config::EffectiveConfig;
use nova_core::TextEdit;
use nova_db::persistence::PersistenceConfig;
use nova_db::{salsa, Database, NovaIndexing, NovaInputs, ProjectId, SourceRootId};
use nova_ide::{DebugConfiguration, Project};
use nova_index::ProjectIndexes;
use nova_memory::MemoryManager;
use nova_project::{BuildSystem, JavaConfig, ProjectConfig, ProjectError, SourceRootOrigin};
use nova_scheduler::{Cancelled, Debouncer, KeyedDebouncer, PoolKind, Scheduler};
use nova_types::{CompletionItem, Diagnostic as NovaDiagnostic};
use nova_vfs::{
    ChangeEvent, ContentChange, DocumentError, FileId, FileSystem, LocalFs, Vfs, VfsPath,
};
use walkdir::WalkDir;

use crate::watch::{
    categorize_event, ChangeCategory, EventNormalizer, NormalizedEvent, WatchConfig,
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

pub struct WatcherHandle {
    watcher_stop: channel::Sender<()>,
    watcher_thread: Option<thread::JoinHandle<()>>,
    driver_stop: channel::Sender<()>,
    driver_thread: Option<thread::JoinHandle<()>>,
}

impl Drop for WatcherHandle {
    fn drop(&mut self) {
        let _ = self.watcher_stop.send(());
        if let Some(handle) = self.watcher_thread.take() {
            let _ = handle.join();
        }

        let _ = self.driver_stop.send(());
        if let Some(handle) = self.driver_thread.take() {
            let _ = handle.join();
        }
    }
}

pub(crate) struct WorkspaceEngine {
    vfs: Vfs<LocalFs>,
    query_db: salsa::Database,
    indexes: Arc<Mutex<ProjectIndexes>>,

    config: RwLock<EffectiveConfig>,
    scheduler: Scheduler,
    index_debouncer: KeyedDebouncer<&'static str>,
    project_reload_debouncer: KeyedDebouncer<&'static str>,
    subscribers: Arc<Mutex<Vec<Sender<WorkspaceEvent>>>>,

    project_state: Arc<Mutex<ProjectState>>,
    ide_project: RwLock<Option<Project>>,
}

#[derive(Debug)]
struct ProjectState {
    workspace_root: Option<PathBuf>,
    config: Arc<ProjectConfig>,
    source_roots: Vec<SourceRootEntry>,
    pending_build_changes: HashSet<PathBuf>,
    last_reload_started_at: Option<Instant>,
}

#[derive(Debug, Clone)]
struct SourceRootEntry {
    path: PathBuf,
    id: SourceRootId,
    path_components: usize,
}

impl Default for ProjectState {
    fn default() -> Self {
        Self {
            workspace_root: None,
            config: Arc::new(ProjectConfig {
                workspace_root: PathBuf::new(),
                build_system: BuildSystem::Simple,
                java: JavaConfig::default(),
                modules: Vec::new(),
                jpms_modules: Vec::new(),
                jpms_workspace: None,
                source_roots: Vec::new(),
                module_path: Vec::new(),
                classpath: Vec::new(),
                output_dirs: Vec::new(),
                dependencies: Vec::new(),
                workspace_model: None,
            }),
            source_roots: Vec::new(),
            pending_build_changes: HashSet::new(),
            last_reload_started_at: None,
        }
    }
}

fn workspace_scheduler() -> Scheduler {
    static SCHEDULER: OnceLock<Scheduler> = OnceLock::new();
    SCHEDULER.get_or_init(Scheduler::default).clone()
}

impl WorkspaceEngine {
    pub fn new(config: WorkspaceEngineConfig) -> Self {
        let scheduler = workspace_scheduler();
        let WorkspaceEngineConfig {
            workspace_root,
            persistence,
            memory,
        } = config;

        let query_db = salsa::Database::new_with_persistence(&workspace_root, persistence);
        query_db.register_salsa_memo_evictor(&memory);
        let default_project = ProjectId::from_raw(0);
        // Ensure fundamental project inputs are always initialized so callers can safely
        // start with an empty/in-memory workspace.
        query_db.set_project_files(default_project, Arc::new(Vec::new()));
        query_db.set_jdk_index(default_project, Arc::new(nova_jdk::JdkIndex::new()));
        query_db.set_classpath_index(default_project, None);
        let index_debouncer = KeyedDebouncer::new(
            scheduler.clone(),
            PoolKind::Background,
            // Match the default LSP diagnostics debounce so edits "win" over background work.
            Duration::from_millis(200),
        );
        let project_reload_debouncer = KeyedDebouncer::new(
            scheduler.clone(),
            PoolKind::Background,
            Duration::from_millis(1200),
        );
        Self {
            vfs: Vfs::new(LocalFs::new()),
            query_db,
            indexes: Arc::new(Mutex::new(ProjectIndexes::default())),
            config: RwLock::new(EffectiveConfig::default()),
            scheduler,
            index_debouncer,
            project_reload_debouncer,
            subscribers: Arc::new(Mutex::new(Vec::new())),
            project_state: Arc::new(Mutex::new(ProjectState::default())),
            ide_project: RwLock::new(None),
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

    pub fn set_workspace_root(&self, root: impl AsRef<Path>) -> Result<()> {
        let root = fs::canonicalize(root.as_ref())
            .with_context(|| format!("failed to canonicalize {}", root.as_ref().display()))?;
        {
            let mut state = self
                .project_state
                .lock()
                .expect("workspace project state mutex poisoned");
            state.workspace_root = Some(root.clone());
        }

        // Load initial project state + file list.
        self.reload_project_now(&[])?;
        Ok(())
    }

    pub fn start_watching(self: &Arc<Self>) -> Result<WatcherHandle> {
        let (watch_config, watch_root) = {
            let state = self
                .project_state
                .lock()
                .expect("workspace project state mutex poisoned");
            let root = state
                .workspace_root
                .clone()
                .context("workspace root not set")?;

            let mut source_roots = Vec::new();
            let mut generated_roots = Vec::new();
            for root_entry in &state.config.source_roots {
                match root_entry.origin {
                    SourceRootOrigin::Source => source_roots.push(root_entry.path.clone()),
                    SourceRootOrigin::Generated => generated_roots.push(root_entry.path.clone()),
                }
            }

            (
                WatchConfig {
                    workspace_root: root.clone(),
                    source_roots,
                    generated_source_roots: generated_roots,
                },
                root,
            )
        };

        let engine = Arc::clone(self);
        let (batch_tx, batch_rx) = channel::unbounded::<(ChangeCategory, Vec<NormalizedEvent>)>();

        let (watcher_stop_tx, watcher_stop_rx) = channel::bounded::<()>(0);
        let (raw_tx, raw_rx) = channel::unbounded::<notify::Result<notify::Event>>();

        let subscribers = Arc::clone(&self.subscribers);
        let watcher_thread = thread::spawn(move || {
            let mut normalizer = EventNormalizer::new();
            let mut debouncer = Debouncer::new([
                (ChangeCategory::Source, Duration::from_millis(200)),
                (ChangeCategory::Build, Duration::from_millis(200)),
            ]);

            let mut watcher = match notify::recommended_watcher(move |res| {
                let _ = raw_tx.send(res);
            }) {
                Ok(watcher) => watcher,
                Err(err) => {
                    publish_to_subscribers(
                        &subscribers,
                        WorkspaceEvent::Status(WorkspaceStatus::IndexingError(format!(
                            "Failed to start file watcher: {err}"
                        ))),
                    );
                    return;
                }
            };

            let mut roots: Vec<(PathBuf, RecursiveMode)> = Vec::new();
            roots.push((watch_root.clone(), RecursiveMode::Recursive));
            for root in watch_config
                .source_roots
                .iter()
                .chain(watch_config.generated_source_roots.iter())
            {
                // If the configured root is outside the workspace root, we need to watch it
                // explicitly. Roots under the workspace root are already covered by the recursive
                // watch.
                if root.starts_with(&watch_root) {
                    continue;
                }
                roots.push((root.clone(), RecursiveMode::Recursive));
            }

            roots.sort_by(|(a, _), (b, _)| a.cmp(b));
            roots.dedup_by(|(a, mode_a), (b, mode_b)| {
                if a != b {
                    return false;
                }
                // Prefer recursive mode when duplicates exist.
                if *mode_a == RecursiveMode::Recursive || *mode_b == RecursiveMode::Recursive {
                    *mode_a = RecursiveMode::Recursive;
                }
                true
            });

            for (root, mode) in roots {
                if !root.exists() {
                    continue;
                }
                if let Err(err) = watcher.watch(&root, mode) {
                    publish_to_subscribers(
                        &subscribers,
                        WorkspaceEvent::Status(WorkspaceStatus::IndexingError(format!(
                            "Failed to watch {}: {err}",
                            root.display()
                        ))),
                    );
                }
            }

            loop {
                let now = Instant::now();
                let deadline = debouncer
                    .next_deadline()
                    .unwrap_or(now + Duration::from_secs(3600));
                let timeout = deadline.saturating_duration_since(now);
                let tick = channel::after(timeout);

                channel::select! {
                    recv(watcher_stop_rx) -> _ => {
                        for (cat, events) in debouncer.flush_all() {
                            let _ = batch_tx.send((cat, events));
                        }
                        break;
                    }
                    recv(raw_rx) -> msg => {
                        let Ok(res) = msg else { break };
                        match res {
                            Ok(event) => {
                                let now = Instant::now();
                                for norm in normalizer.push(event, now) {
                                    if let Some(cat) = categorize_event(&watch_config, &norm) {
                                        debouncer.push(&cat, norm, now);
                                    }
                                }
                                for (cat, events) in debouncer.flush_due(now) {
                                    let _ = batch_tx.send((cat, events));
                                }
                            }
                            Err(err) => {
                                publish_to_subscribers(
                                    &subscribers,
                                    WorkspaceEvent::Status(WorkspaceStatus::IndexingError(format!(
                                        "File watcher error: {err}"
                                    ))),
                                );
                            }
                        }
                    }
                    recv(tick) -> _ => {
                        let now = Instant::now();
                        for (cat, events) in debouncer.flush_due(now) {
                            let _ = batch_tx.send((cat, events));
                        }
                    }
                }
            }
        });

        let (driver_stop_tx, driver_stop_rx) = channel::bounded::<()>(0);
        let driver_thread = thread::spawn(move || loop {
            channel::select! {
                recv(driver_stop_rx) -> _ => break,
                recv(batch_rx) -> msg => {
                    let Ok((category, events)) = msg else { break };
                    match category {
                        ChangeCategory::Source => engine.apply_filesystem_events(events),
                        ChangeCategory::Build => {
                            let mut changed = Vec::new();
                            for ev in &events {
                                match ev {
                                    NormalizedEvent::Created(p)
                                    | NormalizedEvent::Modified(p)
                                    | NormalizedEvent::Deleted(p) => changed.push(p.clone()),
                                    NormalizedEvent::Moved { from, to } => {
                                        changed.push(from.clone());
                                        changed.push(to.clone());
                                    }
                                }
                            }
                            engine.request_project_reload(changed);
                        }
                    }
                }
            }
        });

        Ok(WatcherHandle {
            watcher_stop: watcher_stop_tx,
            watcher_thread: Some(watcher_thread),
            driver_stop: driver_stop_tx,
            driver_thread: Some(driver_thread),
        })
    }

    pub fn apply_filesystem_events(&self, events: Vec<NormalizedEvent>) {
        if events.is_empty() {
            return;
        }

        // Coalesce noisy watcher streams by processing each path at most once per batch.
        let mut move_events: Vec<(PathBuf, PathBuf)> = Vec::new();
        let mut other_paths: HashSet<PathBuf> = HashSet::new();

        for event in events {
            match event {
                NormalizedEvent::Moved { from, to } => move_events.push((from, to)),
                NormalizedEvent::Created(path)
                | NormalizedEvent::Modified(path)
                | NormalizedEvent::Deleted(path) => {
                    other_paths.insert(path);
                }
            }
        }

        // Apply moves first to keep FileId mapping stable before we touch destination files.
        // We order moves so that if a destination path is also a source path, we move it out
        // first. This avoids `Vfs::rename_path` treating a move as "modify destination" and
        // dropping the source `FileId` (common during rename chains like `B -> C` and `A -> B`).
        move_events.sort();
        move_events.dedup();
        let ordered_moves = order_move_events(move_events);
        for (from, to) in ordered_moves {
            other_paths.remove(&from);
            other_paths.remove(&to);

            self.apply_move_event(&from, &to);
        }

        let mut remaining: Vec<PathBuf> = other_paths.into_iter().collect();
        remaining.sort();
        for path in remaining {
            self.apply_path_event(&path);
        }
    }

    pub fn request_project_reload(&self, changed_files: Vec<PathBuf>) {
        if changed_files.is_empty() {
            return;
        }

        let (root, delay) = {
            let now = Instant::now();
            let mut state = self
                .project_state
                .lock()
                .expect("workspace project state mutex poisoned");
            state.pending_build_changes.extend(changed_files);
            let root = state.workspace_root.clone();

            const MIN_RELOAD_INTERVAL: Duration = Duration::from_secs(2);
            let mut delay = Duration::from_millis(1200);
            if let Some(prev) = state.last_reload_started_at {
                let elapsed = now.duration_since(prev);
                if elapsed < MIN_RELOAD_INTERVAL {
                    delay = delay.max(MIN_RELOAD_INTERVAL - elapsed);
                }
            }

            (root, delay)
        };

        let Some(root) = root else {
            return;
        };

        let project_state = Arc::clone(&self.project_state);
        let vfs = self.vfs.clone();
        let query_db = self.query_db.clone();
        let subscribers = Arc::clone(&self.subscribers);
        let scheduler = self.scheduler.clone();

        self.project_reload_debouncer.debounce_with_delay(
            "workspace-reload",
            delay,
            move |token| {
                let _ctx = scheduler.request_context_with_token("workspace/reload_project", token);

                let changed = {
                    let mut state = project_state
                        .lock()
                        .expect("workspace project state mutex poisoned");
                    state.last_reload_started_at = Some(Instant::now());
                    state.pending_build_changes.drain().collect::<Vec<_>>()
                };

                if let Err(err) =
                    reload_project_and_sync(&root, &changed, &vfs, &query_db, &project_state)
                {
                    publish_to_subscribers(
                        &subscribers,
                        WorkspaceEvent::Status(WorkspaceStatus::IndexingError(format!(
                            "Project reload failed: {err}"
                        ))),
                    );
                }

                Ok(())
            },
        );
    }

    pub fn reload_project_now(&self, changed_files: &[PathBuf]) -> Result<()> {
        let root = {
            let state = self
                .project_state
                .lock()
                .expect("workspace project state mutex poisoned");
            state
                .workspace_root
                .clone()
                .context("workspace root not set")?
        };

        reload_project_and_sync(
            &root,
            changed_files,
            &self.vfs,
            &self.query_db,
            &self.project_state,
        )
    }

    pub fn open_document(&self, path: VfsPath, text: String, version: i32) -> FileId {
        let text_for_db = Arc::new(text.clone());
        let file_id = self.vfs.open_document(path.clone(), text, version);
        self.ensure_file_inputs(file_id, &path);
        self.query_db.set_file_exists(file_id, true);
        self.query_db.set_file_content(file_id, text_for_db);
        self.update_project_files_membership(&path, file_id, true);

        self.publish(WorkspaceEvent::FileChanged { file: path.clone() });
        self.publish_diagnostics(path);
        file_id
    }

    pub fn close_document(&self, path: &VfsPath) {
        let file_id = self.vfs.get_id(path);
        self.vfs.close_document(path);

        if let Some(file_id) = file_id {
            self.ensure_file_inputs(file_id, path);
            let exists = self.vfs.exists(path);
            self.query_db.set_file_exists(file_id, exists);
            if exists {
                if let Ok(text) = self.vfs.read_to_string(path) {
                    self.query_db.set_file_content(file_id, Arc::new(text));
                }
            }
            self.update_project_files_membership(path, file_id, exists);
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
            self.ensure_file_inputs(file_id, path);
            self.query_db.set_file_exists(file_id, true);
            self.query_db.set_file_content(file_id, Arc::new(text));
        }

        self.publish(WorkspaceEvent::FileChanged { file: path.clone() });
        self.publish_diagnostics(path.clone());
        Ok(edits)
    }

    pub fn completions(&self, path: &VfsPath, offset: usize) -> Vec<CompletionItem> {
        let Some(file_id) = self.vfs.get_id(path) else {
            return Vec::new();
        };
        let snapshot = crate::WorkspaceSnapshot::from_engine(self);
        let text = snapshot.file_content(file_id);
        let position = offset_to_lsp_position(text, offset);
        nova_ide::completions(&snapshot, file_id, position)
            .into_iter()
            .map(|item| CompletionItem {
                label: item.label,
                detail: item.detail,
                replace_span: None,
            })
            .collect()
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
            .ide_project
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

    fn ensure_file_inputs(&self, file_id: FileId, path: &VfsPath) {
        let project = ProjectId::from_raw(0);
        self.query_db.set_file_project(file_id, project);

        let (workspace_root, source_roots) = {
            let state = self
                .project_state
                .lock()
                .expect("workspace project state mutex poisoned");
            (state.workspace_root.clone(), state.source_roots.clone())
        };

        let local_path = path.as_local_path();
        let source_root = local_path
            .and_then(|local| source_root_for_path(&source_roots, local))
            .unwrap_or_else(|| SourceRootId::from_raw(0));
        self.query_db.set_source_root(file_id, source_root);

        let rel_path = if let (Some(workspace_root), Some(local)) = (workspace_root, local_path) {
            rel_path_for_workspace(&workspace_root, local)
        } else {
            None
        }
        .unwrap_or_else(|| normalize_rel_path(&path.to_string()));
        self.query_db.set_file_rel_path(file_id, Arc::new(rel_path));
    }

    fn apply_path_event(&self, path: &Path) {
        let vfs_path = VfsPath::local(path.to_path_buf());
        let was_known = self.vfs.get_id(&vfs_path).is_some();
        let file_id = self.vfs.file_id(vfs_path.clone());
        self.ensure_file_inputs(file_id, &vfs_path);

        let mut exists = self.vfs.exists(&vfs_path);
        self.query_db.set_file_exists(file_id, exists);

        let open_docs = self.vfs.open_documents();
        if exists && !open_docs.is_open(file_id) {
            match fs::read_to_string(path) {
                Ok(text) => self.query_db.set_file_content(file_id, Arc::new(text)),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    self.query_db.set_file_exists(file_id, false);
                    exists = false;
                }
                Err(_) if !was_known => {
                    self.query_db
                        .set_file_content(file_id, Arc::new(String::new()));
                }
                Err(_) => {
                    // Best-effort: keep the previous contents if we fail to read during a transient
                    // IO error.
                }
            }
        }

        self.update_project_files_membership(&vfs_path, file_id, exists);
        self.publish(WorkspaceEvent::FileChanged {
            file: vfs_path.clone(),
        });
        self.publish_diagnostics(vfs_path);
    }

    fn apply_move_event(&self, from: &Path, to: &Path) {
        let from_vfs = VfsPath::local(from.to_path_buf());
        let to_vfs = VfsPath::local(to.to_path_buf());

        let id_from = self.vfs.get_id(&from_vfs);
        let id_to = self.vfs.get_id(&to_vfs);
        let to_was_known = id_to.is_some();
        let open_docs = self.vfs.open_documents();
        let is_new_id = id_from.is_none() && !to_was_known;

        let file_id = self.vfs.rename_path(&from_vfs, to_vfs.clone());

        self.ensure_file_inputs(file_id, &to_vfs);
        let exists = self.vfs.exists(&to_vfs);
        self.query_db.set_file_exists(file_id, exists);

        if exists && !open_docs.is_open(file_id) {
            match fs::read_to_string(to) {
                Ok(text) => self.query_db.set_file_content(file_id, Arc::new(text)),
                Err(_) if is_new_id => {
                    self.query_db
                        .set_file_content(file_id, Arc::new(String::new()));
                }
                Err(_) => {}
            }
        }

        // A move can have three effects on ids:
        // - Typical case: preserve `id_from` at `to`.
        // - Destination already known: keep destination id and orphan `id_from`.
        // - Open document moved onto an existing destination: preserve `id_from` and orphan `id_to`.
        //
        // Keep Salsa inputs consistent by explicitly marking orphaned ids as deleted and removing
        // them from `project_files`.

        // If the destination already had an id and `rename_path` returned a different one, the
        // previous id is no longer reachable from any path.
        if let Some(id_to) = id_to {
            if id_to != file_id {
                self.query_db.set_file_exists(id_to, false);
                self.update_project_files_membership(&to_vfs, id_to, false);
            }
        }

        // If we renamed onto an already-known destination and `rename_path` returned the
        // destination id, the source id has been cleared from the registry.
        if let Some(id_from) = id_from {
            if to_was_known && Some(file_id) == id_to && id_from != file_id {
                self.query_db.set_file_exists(id_from, false);
                self.update_project_files_membership(&from_vfs, id_from, false);
            } else {
                // Update membership for the moved id (handles leaving the Java set / root).
                self.update_project_files_membership(&from_vfs, file_id, false);
            }
        }

        self.update_project_files_membership(&to_vfs, file_id, exists);

        // Surface both paths as changed so consumers can react to deletions at `from` and
        // creations at `to`.
        self.publish(WorkspaceEvent::FileChanged {
            file: from_vfs.clone(),
        });
        self.publish(WorkspaceEvent::FileChanged {
            file: to_vfs.clone(),
        });
        self.publish_diagnostics(from_vfs);
        self.publish_diagnostics(to_vfs);
    }

    fn update_project_files_membership(&self, path: &VfsPath, file_id: FileId, exists: bool) {
        let Some(local) = path.as_local_path() else {
            return;
        };
        let is_java = local.extension().and_then(|ext| ext.to_str()) == Some("java");

        let should_track = {
            let state = self
                .project_state
                .lock()
                .expect("workspace project state mutex poisoned");
            let Some(workspace_root) = state.workspace_root.as_ref() else {
                return;
            };
            let in_workspace = local.starts_with(workspace_root);
            let in_source_root = state.source_roots.is_empty()
                || source_root_for_path(&state.source_roots, local).is_some();
            is_java && in_workspace && in_source_root
        };

        let project = ProjectId::from_raw(0);
        let current: Vec<FileId> = self
            .query_db
            .with_snapshot(|snap| snap.project_files(project).as_ref().clone());
        let mut ids: HashSet<FileId> = current.into_iter().collect();
        if exists && should_track {
            ids.insert(file_id);
        } else {
            ids.remove(&file_id);
        }

        let mut entries: Vec<(String, FileId)> = Vec::new();
        self.query_db.with_snapshot(|snap| {
            for id in ids.iter().copied() {
                if !snap.file_exists(id) {
                    continue;
                }
                let rel = snap.file_rel_path(id);
                entries.push((rel.as_ref().clone(), id));
            }
        });
        entries.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        let ordered: Vec<FileId> = entries.into_iter().map(|(_, id)| id).collect();
        self.query_db.set_project_files(project, Arc::new(ordered));
    }

    fn publish_diagnostics(&self, file: VfsPath) {
        // Publishing diagnostics for a path that no longer has a `FileId` (e.g. the source path
        // during a rename) should *not* allocate a new id. Downstream consumers typically want an
        // empty diagnostics set to clear stale results, but we can't run code intelligence without
        // a stable `FileId`.
        let Some(file_id) = self.vfs.get_id(&file) else {
            self.publish(WorkspaceEvent::DiagnosticsUpdated {
                file,
                diagnostics: Vec::new(),
            });
            return;
        };
        let snapshot = crate::WorkspaceSnapshot::from_engine(self);
        let diagnostics = nova_ide::file_diagnostics(&snapshot, file_id);

        self.publish(WorkspaceEvent::DiagnosticsUpdated { file, diagnostics });
    }

    fn publish(&self, event: WorkspaceEvent) {
        publish_to_subscribers(&self.subscribers, event);
    }

    pub(crate) fn vfs(&self) -> &Vfs<LocalFs> {
        &self.vfs
    }

    pub(crate) fn salsa_file_content(&self, file: FileId) -> Option<std::sync::Arc<String>> {
        self.query_db.with_snapshot(|snap| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if snap.file_exists(file) {
                    Some(snap.file_content(file))
                } else {
                    None
                }
            }))
            .ok()
            .flatten()
        })
    }
}

fn offset_to_lsp_position(text: &str, offset: usize) -> Position {
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    let mut cur: usize = 0;

    for ch in text.chars() {
        if cur >= offset {
            break;
        }
        cur += ch.len_utf8();
        if ch == '\n' {
            line += 1;
            col_utf16 = 0;
        } else {
            col_utf16 += ch.len_utf16() as u32;
        }
    }

    Position {
        line,
        character: col_utf16,
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

fn order_move_events(mut moves: Vec<(PathBuf, PathBuf)>) -> Vec<(PathBuf, PathBuf)> {
    if moves.len() <= 1 {
        return moves;
    }

    let mut from_set: HashSet<PathBuf> = moves.iter().map(|(from, _)| from.clone()).collect();
    let mut ordered = Vec::with_capacity(moves.len());

    while !moves.is_empty() {
        // Prefer moves whose destination is not a remaining source path. Because `moves` is sorted,
        // the first eligible move yields deterministic output.
        let mut idx = None;
        for (i, (_, to)) in moves.iter().enumerate() {
            if !from_set.contains(to) {
                idx = Some(i);
                break;
            }
        }

        let (from, to) = match idx {
            Some(i) => moves.remove(i),
            None => {
                // Cycle: fall back to a deterministic order. This can still lose `FileId`s if the
                // filesystem performed an in-place overwrite, but we prefer a stable result.
                moves.remove(0)
            }
        };
        from_set.remove(&from);
        ordered.push((from, to));
    }

    ordered
}

fn rel_path_for_workspace(workspace_root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(workspace_root).ok()?;
    let rel = rel.to_string_lossy();
    Some(normalize_rel_path(rel.as_ref()))
}

fn source_root_for_path(roots: &[SourceRootEntry], path: &Path) -> Option<SourceRootId> {
    let mut best: Option<&SourceRootEntry> = None;
    for root in roots {
        if !path.starts_with(&root.path) {
            continue;
        }
        match best {
            None => best = Some(root),
            Some(prev) if root.path_components > prev.path_components => best = Some(root),
            Some(prev) if root.path_components == prev.path_components && root.path < prev.path => {
                best = Some(root)
            }
            Some(_) => {}
        }
    }
    best.map(|root| root.id)
}

fn build_source_roots(config: &ProjectConfig) -> Vec<SourceRootEntry> {
    let mut roots: Vec<PathBuf> = config.source_roots.iter().map(|r| r.path.clone()).collect();
    if roots.is_empty() {
        roots.push(config.workspace_root.clone());
    }
    roots.sort();
    roots.dedup();

    roots
        .into_iter()
        .enumerate()
        .map(|(idx, path)| SourceRootEntry {
            path_components: path.components().count(),
            path,
            id: SourceRootId::from_raw(idx as u32),
        })
        .collect()
}

fn java_files_under(root: &Path) -> Result<Vec<PathBuf>> {
    match fs::metadata(root) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Ok(Vec::new()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("failed to read {}", root.display())),
    }

    let mut files = Vec::new();
    for entry in WalkDir::new(root).follow_links(true) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("java") {
            files.push(path.to_path_buf());
        }
    }
    Ok(files)
}

fn fallback_project_config(workspace_root: &Path) -> ProjectConfig {
    use nova_project::{SourceRoot, SourceRootKind};

    let root = workspace_root.to_path_buf();
    let module_name = root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("root")
        .to_string();

    ProjectConfig {
        workspace_root: root.clone(),
        build_system: BuildSystem::Simple,
        java: JavaConfig::default(),
        modules: vec![nova_project::Module {
            name: module_name,
            root,
            annotation_processing: Default::default(),
        }],
        jpms_modules: Vec::new(),
        jpms_workspace: None,
        source_roots: vec![SourceRoot {
            kind: SourceRootKind::Main,
            origin: SourceRootOrigin::Source,
            path: workspace_root.to_path_buf(),
        }],
        module_path: Vec::new(),
        classpath: Vec::new(),
        output_dirs: Vec::new(),
        dependencies: Vec::new(),
        workspace_model: None,
    }
}

fn reload_project_and_sync(
    workspace_root: &Path,
    _changed_files: &[PathBuf],
    vfs: &Vfs<LocalFs>,
    query_db: &salsa::Database,
    project_state: &Arc<Mutex<ProjectState>>,
) -> Result<()> {
    let loaded = match nova_project::load_project_with_workspace_config(workspace_root) {
        Ok(config) => config,
        Err(ProjectError::UnknownProjectType { .. }) => fallback_project_config(workspace_root),
        Err(err) => {
            return Err(anyhow::Error::new(err)).with_context(|| {
                format!(
                    "failed to load project configuration at {}",
                    workspace_root.display()
                )
            });
        }
    };

    let config = Arc::new(loaded);
    let source_roots = build_source_roots(&config);

    {
        let mut state = project_state
            .lock()
            .expect("workspace project state mutex poisoned");
        state.config = Arc::clone(&config);
        state.source_roots = source_roots.clone();
    }

    let project = ProjectId::from_raw(0);
    query_db.set_project_config(project, Arc::clone(&config));
    query_db.set_jdk_index(project, Arc::new(nova_jdk::JdkIndex::new()));

    // Best-effort classpath index. This can be expensive, so fall back to `None` if it fails.
    let classpath_entries: Vec<nova_classpath::ClasspathEntry> = config
        .classpath
        .iter()
        .chain(config.module_path.iter())
        .map(nova_classpath::ClasspathEntry::from)
        .collect();
    if classpath_entries.is_empty() {
        query_db.set_classpath_index(project, None);
    } else {
        match nova_classpath::ClasspathIndex::build(&classpath_entries, None) {
            Ok(index) => query_db.set_classpath_index(project, Some(Arc::new(index))),
            Err(_) => query_db.set_classpath_index(project, None),
        }
    }

    // Snapshot previous project files so we can mark removed ones as non-existent.
    let previous_files: Vec<FileId> =
        query_db.with_snapshot(|snap| snap.project_files(project).as_ref().clone());
    let previous_set: HashSet<FileId> = previous_files.iter().copied().collect();

    // Scan Java files under declared roots.
    let mut paths = Vec::new();
    for root in &config.source_roots {
        paths.extend(java_files_under(&root.path)?);
    }

    // Preserve open documents even if they temporarily disappear from disk.
    for file in vfs.open_documents().snapshot() {
        let Some(path) = vfs.path_for_id(file) else {
            continue;
        };
        let Some(local) = path.as_local_path() else {
            continue;
        };
        if local.extension().and_then(|ext| ext.to_str()) != Some("java") {
            continue;
        }
        if local.starts_with(workspace_root) {
            paths.push(local.to_path_buf());
        }
    }

    paths.sort();
    paths.dedup();

    let open_docs = vfs.open_documents();
    let mut next_entries: Vec<(String, FileId)> = Vec::new();

    for path in paths {
        let vfs_path = VfsPath::local(path.clone());
        let file_id = vfs.file_id(vfs_path);

        let Some(rel_path) = rel_path_for_workspace(workspace_root, &path) else {
            continue;
        };

        query_db.set_file_rel_path(file_id, Arc::new(rel_path.clone()));
        query_db.set_file_project(file_id, project);
        let root_id = source_root_for_path(&source_roots, &path).unwrap_or_else(|| {
            source_roots
                .first()
                .map(|r| r.id)
                .unwrap_or_else(|| SourceRootId::from_raw(0))
        });
        query_db.set_source_root(file_id, root_id);

        let exists = vfs.exists(&VfsPath::local(path.clone()));
        query_db.set_file_exists(file_id, exists);
        if !exists {
            continue;
        }

        if !open_docs.is_open(file_id) {
            match fs::read_to_string(&path) {
                Ok(text) => query_db.set_file_content(file_id, Arc::new(text)),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    query_db.set_file_exists(file_id, false);
                    continue;
                }
                Err(_) if !previous_set.contains(&file_id) => {
                    // Ensure the input is initialized for new files even if we cannot read them.
                    query_db.set_file_content(file_id, Arc::new(String::new()));
                }
                Err(_) => {
                    // Keep previous contents for existing files on transient errors.
                }
            }
        }

        next_entries.push((rel_path, file_id));
    }

    // Mark removed files as deleted.
    let next_set: HashSet<FileId> = next_entries.iter().map(|(_, id)| *id).collect();
    for old in previous_files {
        if !next_set.contains(&old) {
            query_db.set_file_exists(old, false);
        }
    }

    next_entries.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let ordered: Vec<FileId> = next_entries.into_iter().map(|(_, id)| id).collect();
    query_db.set_project_files(project, Arc::new(ordered));

    Ok(())
}

#[cfg(test)]
mod tests {
    use nova_db::NovaInputs;
    use nova_project::BuildSystem;
    use std::fs;

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

    #[test]
    fn filesystem_events_update_salsa_and_preserve_file_ids_across_moves() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();

        let workspace = crate::Workspace::open(root).unwrap();
        let engine = workspace.engine_for_tests();

        let project = ProjectId::from_raw(0);
        engine.query_db.with_snapshot(|snap| {
            assert!(snap.project_files(project).is_empty());
        });

        let file_a = root.join("src/A.java");
        fs::write(&file_a, "class A {}".as_bytes()).unwrap();

        engine.apply_filesystem_events(vec![NormalizedEvent::Created(file_a.clone())]);

        let vfs_a = VfsPath::local(file_a.clone());
        let file_id = engine.vfs.get_id(&vfs_a).expect("file id allocated");

        engine.query_db.with_snapshot(|snap| {
            assert!(snap.file_exists(file_id));
            assert_eq!(snap.file_content(file_id).as_str(), "class A {}");
            assert_eq!(snap.file_rel_path(file_id).as_str(), "src/A.java");
            assert!(snap.project_files(project).contains(&file_id));
        });

        let file_b = root.join("src/B.java");
        fs::rename(&file_a, &file_b).unwrap();
        engine.apply_filesystem_events(vec![NormalizedEvent::Moved {
            from: file_a.clone(),
            to: file_b.clone(),
        }]);

        let vfs_b = VfsPath::local(file_b.clone());
        assert_eq!(engine.vfs.get_id(&vfs_a), None);
        assert_eq!(engine.vfs.get_id(&vfs_b), Some(file_id));

        engine.query_db.with_snapshot(|snap| {
            assert!(snap.file_exists(file_id));
            assert_eq!(snap.file_rel_path(file_id).as_str(), "src/B.java");
        });

        fs::write(&file_b, "class B {}".as_bytes()).unwrap();
        engine.apply_filesystem_events(vec![NormalizedEvent::Modified(file_b.clone())]);
        engine.query_db.with_snapshot(|snap| {
            assert_eq!(snap.file_content(file_id).as_str(), "class B {}");
        });

        fs::remove_file(&file_b).unwrap();
        engine.apply_filesystem_events(vec![NormalizedEvent::Deleted(file_b.clone())]);
        engine.query_db.with_snapshot(|snap| {
            assert!(!snap.file_exists(file_id));
            assert!(!snap.project_files(project).contains(&file_id));
        });
    }

    #[test]
    fn move_event_ordering_preserves_file_ids_for_rename_chains() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();

        let a = root.join("src/A.java");
        let b = root.join("src/B.java");
        fs::write(&a, "class A {}".as_bytes()).unwrap();
        fs::write(&b, "class B {}".as_bytes()).unwrap();

        let workspace = crate::Workspace::open(root).unwrap();
        let engine = workspace.engine_for_tests();

        engine.apply_filesystem_events(vec![
            NormalizedEvent::Created(a.clone()),
            NormalizedEvent::Created(b.clone()),
        ]);

        let id_a = engine.vfs.get_id(&VfsPath::local(a.clone())).unwrap();
        let id_b = engine.vfs.get_id(&VfsPath::local(b.clone())).unwrap();

        // Perform a rename chain on disk: B -> C, then A -> B.
        let c = root.join("src/C.java");
        fs::rename(&b, &c).unwrap();
        fs::rename(&a, &b).unwrap();

        // Feed watcher events in an order that would break stable ids if we naively processed moves
        // in sorted order (A -> B before B -> C).
        engine.apply_filesystem_events(vec![
            NormalizedEvent::Moved {
                from: a.clone(),
                to: b.clone(),
            },
            NormalizedEvent::Moved {
                from: b.clone(),
                to: c.clone(),
            },
        ]);

        assert_eq!(engine.vfs.get_id(&VfsPath::local(b.clone())), Some(id_a));
        assert_eq!(engine.vfs.get_id(&VfsPath::local(c.clone())), Some(id_b));

        engine.query_db.with_snapshot(|snap| {
            assert_eq!(snap.file_rel_path(id_a).as_str(), "src/B.java");
            assert_eq!(snap.file_rel_path(id_b).as_str(), "src/C.java");
            assert_eq!(snap.file_content(id_a).as_str(), "class A {}");
            assert_eq!(snap.file_content(id_b).as_str(), "class B {}");
        });
    }

    #[test]
    fn renaming_java_file_to_non_java_removes_it_from_project_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();

        let workspace = crate::Workspace::open(root).unwrap();
        let engine = workspace.engine_for_tests();
        let project = ProjectId::from_raw(0);

        let java_path = root.join("src/A.java");
        fs::write(&java_path, "class A {}".as_bytes()).unwrap();
        engine.apply_filesystem_events(vec![NormalizedEvent::Created(java_path.clone())]);

        let vfs_java = VfsPath::local(java_path.clone());
        let file_id = engine.vfs.get_id(&vfs_java).expect("file id allocated");

        engine.query_db.with_snapshot(|snap| {
            assert!(snap.project_files(project).contains(&file_id));
        });

        let txt_path = root.join("src/A.txt");
        fs::rename(&java_path, &txt_path).unwrap();
        engine.apply_filesystem_events(vec![NormalizedEvent::Moved {
            from: java_path.clone(),
            to: txt_path.clone(),
        }]);

        let vfs_txt = VfsPath::local(txt_path.clone());
        assert_eq!(engine.vfs.get_id(&vfs_java), None);
        assert_eq!(engine.vfs.get_id(&vfs_txt), Some(file_id));

        engine.query_db.with_snapshot(|snap| {
            assert!(snap.file_exists(file_id));
            assert!(!snap.project_files(project).contains(&file_id));
        });
    }

    #[test]
    fn filesystem_events_do_not_overwrite_open_document_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        let file = root.join("src/Main.java");
        fs::write(&file, "class Main {}".as_bytes()).unwrap();

        let workspace = crate::Workspace::open(root).unwrap();
        let vfs_path = VfsPath::local(file.clone());
        workspace.open_document(vfs_path.clone(), "class Main { overlay }".to_string(), 1);

        // Disk edits should be ignored while the document is open.
        fs::write(&file, "class Main { disk }".as_bytes()).unwrap();
        workspace.apply_filesystem_events(vec![NormalizedEvent::Modified(file.clone())]);

        let engine = workspace.engine_for_tests();
        let file_id = engine.vfs.get_id(&vfs_path).unwrap();
        engine.query_db.with_snapshot(|snap| {
            assert_eq!(
                snap.file_content(file_id).as_str(),
                "class Main { overlay }"
            );
        });
    }

    #[test]
    fn move_events_preserve_open_document_overlay_and_file_id() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        let file_a = root.join("src/A.java");
        fs::write(&file_a, "class A { disk }".as_bytes()).unwrap();

        let workspace = crate::Workspace::open(root).unwrap();
        let vfs_a = VfsPath::local(file_a.clone());
        let file_id = workspace.open_document(vfs_a.clone(), "class A { overlay }".to_string(), 1);

        let engine = workspace.engine_for_tests();
        assert!(engine.vfs.open_documents().is_open(file_id));

        let file_b = root.join("src/B.java");
        fs::rename(&file_a, &file_b).unwrap();
        workspace.apply_filesystem_events(vec![NormalizedEvent::Moved {
            from: file_a.clone(),
            to: file_b.clone(),
        }]);

        let vfs_b = VfsPath::local(file_b.clone());
        assert_eq!(engine.vfs.get_id(&vfs_a), None);
        assert_eq!(engine.vfs.get_id(&vfs_b), Some(file_id));
        assert_eq!(
            engine.vfs.read_to_string(&vfs_b).unwrap(),
            "class A { overlay }"
        );

        engine.query_db.with_snapshot(|snap| {
            assert!(snap.file_exists(file_id));
            assert_eq!(snap.file_content(file_id).as_str(), "class A { overlay }");
            assert_eq!(snap.file_rel_path(file_id).as_str(), "src/B.java");
        });
    }

    #[test]
    fn move_to_known_destination_deletes_source_file_id() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();

        let file_a = root.join("src/A.java");
        let file_b = root.join("src/B.java");
        fs::write(&file_a, "class A {}".as_bytes()).unwrap();
        fs::write(&file_b, "class B {}".as_bytes()).unwrap();

        let workspace = crate::Workspace::open(root).unwrap();
        let engine = workspace.engine_for_tests();

        engine.apply_filesystem_events(vec![
            NormalizedEvent::Created(file_a.clone()),
            NormalizedEvent::Created(file_b.clone()),
        ]);

        let id_a = engine.vfs.get_id(&VfsPath::local(file_a.clone())).unwrap();
        let id_b = engine.vfs.get_id(&VfsPath::local(file_b.clone())).unwrap();

        // Make the destination path "known" but removable on all platforms: delete it before the
        // rename, without informing the workspace yet.
        fs::remove_file(&file_b).unwrap();
        fs::rename(&file_a, &file_b).unwrap();

        engine.apply_filesystem_events(vec![NormalizedEvent::Moved {
            from: file_a.clone(),
            to: file_b.clone(),
        }]);

        let vfs_a = VfsPath::local(file_a.clone());
        let vfs_b = VfsPath::local(file_b.clone());
        assert_eq!(engine.vfs.get_id(&vfs_a), None);
        assert_eq!(engine.vfs.get_id(&vfs_b), Some(id_b));
        assert_eq!(engine.vfs.path_for_id(id_a), None);

        engine.query_db.with_snapshot(|snap| {
            assert!(!snap.file_exists(id_a));
            assert!(snap.file_exists(id_b));
            assert_eq!(snap.file_rel_path(id_b).as_str(), "src/B.java");
            assert_eq!(snap.file_content(id_b).as_str(), "class A {}");
            assert!(!snap.project_files(ProjectId::from_raw(0)).contains(&id_a));
        });
    }

    #[test]
    fn move_open_document_to_known_destination_displaces_destination_id() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();

        let file_a = root.join("src/A.java");
        let file_b = root.join("src/B.java");
        fs::write(&file_a, "class A { disk }".as_bytes()).unwrap();
        fs::write(&file_b, "class B { disk }".as_bytes()).unwrap();

        let workspace = crate::Workspace::open(root).unwrap();
        let engine = workspace.engine_for_tests();
        engine.apply_filesystem_events(vec![
            NormalizedEvent::Created(file_a.clone()),
            NormalizedEvent::Created(file_b.clone()),
        ]);

        let vfs_a = VfsPath::local(file_a.clone());
        let vfs_b = VfsPath::local(file_b.clone());
        let id_a = engine.vfs.get_id(&vfs_a).unwrap();
        let id_b = engine.vfs.get_id(&vfs_b).unwrap();

        // Open A (overlay). The rename should preserve A's id and mark B's old id as deleted.
        let opened = workspace.open_document(vfs_a.clone(), "class A { overlay }".to_string(), 1);
        assert_eq!(opened, id_a);
        assert!(engine.vfs.open_documents().is_open(id_a));

        // Ensure the destination path still has a known id in the registry, but make the rename
        // portable by removing the file on disk first.
        fs::remove_file(&file_b).unwrap();
        fs::rename(&file_a, &file_b).unwrap();

        workspace.apply_filesystem_events(vec![NormalizedEvent::Moved {
            from: file_a.clone(),
            to: file_b.clone(),
        }]);

        assert_eq!(engine.vfs.get_id(&vfs_a), None);
        assert_eq!(engine.vfs.get_id(&vfs_b), Some(id_a));
        assert_eq!(engine.vfs.path_for_id(id_b), None);
        assert!(engine.vfs.open_documents().is_open(id_a));
        assert!(!engine.vfs.open_documents().is_open(id_b));

        engine.query_db.with_snapshot(|snap| {
            assert!(snap.file_exists(id_a));
            assert!(!snap.file_exists(id_b));
            assert_eq!(snap.file_rel_path(id_a).as_str(), "src/B.java");
            assert_eq!(snap.file_content(id_a).as_str(), "class A { overlay }");
            assert!(!snap.project_files(ProjectId::from_raw(0)).contains(&id_b));
        });
    }

    #[test]
    fn java_file_outside_source_roots_is_not_added_to_project_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("pom.xml"),
            br#"<?xml version="1.0"?><project></project>"#,
        )
        .unwrap();

        let main_dir = root.join("src/main/java/com/example");
        fs::create_dir_all(&main_dir).unwrap();
        fs::write(
            main_dir.join("Main.java"),
            "package com.example; class Main {}".as_bytes(),
        )
        .unwrap();

        let workspace = crate::Workspace::open(root).unwrap();
        let engine = workspace.engine_for_tests();
        let project = ProjectId::from_raw(0);

        engine.query_db.with_snapshot(|snap| {
            assert_eq!(
                snap.project_config(project).build_system,
                BuildSystem::Maven
            );
            assert_eq!(snap.project_files(project).len(), 1);
        });

        // A Java file under the workspace root but outside Maven source roots should not be added.
        let scratch = root.join("Scratch.java");
        fs::write(&scratch, "class Scratch {}".as_bytes()).unwrap();
        workspace.apply_filesystem_events(vec![NormalizedEvent::Created(scratch.clone())]);

        let scratch_id = engine.vfs.get_id(&VfsPath::local(scratch.clone())).unwrap();
        engine.query_db.with_snapshot(|snap| {
            assert!(snap.file_exists(scratch_id));
            assert!(!snap.project_files(project).contains(&scratch_id));
        });
    }

    #[test]
    fn project_reload_updates_project_config_and_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let main_dir = root.join("src/main/java/com/example");
        fs::create_dir_all(&main_dir).unwrap();
        let java = main_dir.join("Main.java");
        fs::write(&java, "package com.example; class Main {}".as_bytes()).unwrap();

        let workspace = crate::Workspace::open(root).unwrap();
        let engine = workspace.engine_for_tests();
        let project = ProjectId::from_raw(0);

        engine.query_db.with_snapshot(|snap| {
            assert_eq!(
                snap.project_config(project).build_system,
                BuildSystem::Simple
            );
            assert_eq!(snap.project_files(project).len(), 1);
        });

        let pom = root.join("pom.xml");
        fs::write(&pom, br#"<?xml version="1.0"?><project></project>"#).unwrap();

        engine.reload_project_now(&[pom]).unwrap();

        engine.query_db.with_snapshot(|snap| {
            assert_eq!(
                snap.project_config(project).build_system,
                BuildSystem::Maven
            );
            assert_eq!(snap.project_files(project).len(), 1);
            let file_id = snap.project_files(project)[0];
            assert!(snap.file_exists(file_id));
            assert_eq!(
                snap.file_rel_path(file_id).as_str(),
                "src/main/java/com/example/Main.java"
            );
        });
    }

    #[test]
    #[ignore = "relies on OS file watcher timings"]
    fn notify_watcher_propagates_disk_edits_into_workspace() {
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        let file = root.join("src/Main.java");
        fs::write(&file, "class Main {}".as_bytes()).unwrap();

        let workspace = crate::Workspace::open(root).unwrap();
        let _handle = workspace.start_watching().unwrap();

        fs::write(&file, "class Main { int x; }".as_bytes()).unwrap();
        std::thread::sleep(Duration::from_millis(250));

        let engine = workspace.engine_for_tests();
        let vfs_path = VfsPath::local(file.clone());
        let file_id = engine.vfs.get_id(&vfs_path).unwrap();
        engine.query_db.with_snapshot(|snap| {
            assert_eq!(snap.file_content(file_id).as_str(), "class Main { int x; }");
        });
    }
}
