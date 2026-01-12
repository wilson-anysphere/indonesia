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
use nova_cache::{normalize_rel_path, Fingerprint};
use nova_config::EffectiveConfig;
use nova_core::TextEdit;
use nova_db::persistence::PersistenceConfig;
use nova_db::{salsa, Database, NovaIndexing, NovaInputs, ProjectId, SourceRootId};
use nova_ide::{DebugConfiguration, Project};
use nova_index::ProjectIndexes;
use nova_memory::{
    BackgroundIndexingMode, EvictionRequest, EvictionResult, MemoryCategory, MemoryEvictor,
    MemoryManager, MemoryPressure, MemoryRegistration,
};
use nova_project::{
    BuildSystem, JavaConfig, LoadOptions, ProjectConfig, ProjectError, SourceRootOrigin,
};
use nova_scheduler::{Cancelled, Debouncer, KeyedDebouncer, PoolKind, Scheduler};
use nova_syntax::SyntaxTreeStore;
use nova_types::{CompletionItem, Diagnostic as NovaDiagnostic};
use nova_vfs::{
    ChangeEvent, ContentChange, DocumentError, FileChange, FileId, FileSystem, LocalFs,
    FileWatcher, NotifyFileWatcher, Vfs, VfsPath, WatchEvent,
};
use walkdir::WalkDir;

use crate::watch::{
    categorize_event, ChangeCategory, NormalizedEvent, WatchConfig,
};
use crate::watch_roots::{WatchRootError, WatchRootManager};

fn prune_overlapping_watch_roots(mut roots: Vec<PathBuf>) -> Vec<PathBuf> {
    // Deterministic ordering across platforms.
    roots.sort();
    roots.dedup();

    // The watcher currently only supports recursive roots, so any nested root is guaranteed to be
    // covered by its parent.
    let mut pruned: Vec<PathBuf> = Vec::new();
    'outer: for root in roots {
        for parent in &pruned {
            if root.starts_with(parent) {
                continue 'outer;
            }
        }
        pruned.push(root);
    }

    pruned
}

fn compute_watch_roots(workspace_root: &Path, watch_config: &WatchConfig) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    roots.push(workspace_root.to_path_buf());

    let explicit_external: Vec<PathBuf> = watch_config
        .source_roots
        .iter()
        .chain(watch_config.generated_source_roots.iter())
        // Roots under the workspace root are already covered by the workspace recursive watch.
        .filter(|root| !root.starts_with(workspace_root))
        .cloned()
        .collect();

    roots.extend(prune_overlapping_watch_roots(explicit_external));

    // Deterministic ordering for watcher setup.
    roots.sort();
    roots.dedup();

    roots
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexProgress {
    pub current: usize,
    pub total: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceStatus {
    IndexingStarted,
    IndexingReady,
    IndexingPaused(String),
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

#[derive(Debug)]
struct WorkspaceProjectIndexesEvictor {
    name: String,
    indexes: Arc<Mutex<ProjectIndexes>>,
    tracker: OnceLock<nova_memory::MemoryTracker>,
    registration: OnceLock<nova_memory::MemoryRegistration>,
}

impl WorkspaceProjectIndexesEvictor {
    fn new(manager: &MemoryManager, indexes: Arc<Mutex<ProjectIndexes>>) -> Arc<Self> {
        let evictor = Arc::new(Self {
            name: "workspace_project_indexes".to_string(),
            indexes,
            tracker: OnceLock::new(),
            registration: OnceLock::new(),
        });

        let registration = manager.register_evictor(
            evictor.name.clone(),
            MemoryCategory::Indexes,
            evictor.clone(),
        );
        evictor
            .tracker
            .set(registration.tracker())
            .expect("tracker only set once");
        evictor
            .registration
            .set(registration)
            .expect("registration only set once");

        evictor
    }

    fn replace_indexes(&self, new_indexes: ProjectIndexes) {
        let bytes = new_indexes.estimated_bytes();
        *self
            .indexes
            .lock()
            .expect("workspace indexes lock poisoned") = new_indexes;

        if let Some(tracker) = self.tracker.get() {
            tracker.set_bytes(bytes);
        }
    }

    fn clear_indexes(&self) {
        *self
            .indexes
            .lock()
            .expect("workspace indexes lock poisoned") = ProjectIndexes::default();
        if let Some(tracker) = self.tracker.get() {
            tracker.set_bytes(0);
        }
    }
}

impl MemoryEvictor for WorkspaceProjectIndexesEvictor {
    fn name(&self) -> &str {
        self.registration
            .get()
            .map(|registration| registration.name())
            .unwrap_or(&self.name)
    }

    fn category(&self) -> MemoryCategory {
        self.registration
            .get()
            .map(|registration| registration.category())
            .unwrap_or(MemoryCategory::Indexes)
    }

    fn evict(&self, request: EvictionRequest) -> EvictionResult {
        let before = self.tracker.get().map(|t| t.bytes()).unwrap_or(0);
        if before == 0 {
            return EvictionResult {
                before_bytes: 0,
                after_bytes: 0,
            };
        }

        // First iteration: treat any eviction request that asks us to shrink as a signal to drop
        // the in-memory indexes entirely. This is intentionally coarse-grained but ensures we can
        // shed potentially large allocations under pressure.
        let should_drop = request.target_bytes == 0
            || request.target_bytes < before
            || matches!(request.pressure, nova_memory::MemoryPressure::Critical);

        if should_drop {
            self.clear_indexes();
        }

        let after = self.tracker.get().map(|t| t.bytes()).unwrap_or(0);
        EvictionResult {
            before_bytes: before,
            after_bytes: after,
        }
    }
}

pub struct WatcherHandle {
    watcher_stop: channel::Sender<()>,
    watcher_thread: Option<thread::JoinHandle<()>>,
    driver_stop: channel::Sender<()>,
    driver_thread: Option<thread::JoinHandle<()>>,
    watcher_command_store: Arc<Mutex<Option<channel::Sender<WatchCommand>>>>,
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

        *self
            .watcher_command_store
            .lock()
            .expect("workspace watcher command store mutex poisoned") = None;
    }
}

const RAW_WATCH_QUEUE_CAPACITY: usize = 4096;
const BATCH_QUEUE_CAPACITY: usize = 256;
const OVERFLOW_RETRY_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, PartialEq, Eq)]
enum WatcherMessage {
    Batch(ChangeCategory, Vec<NormalizedEvent>),
    Rescan,
}

pub(crate) struct WorkspaceEngine {
    vfs: Vfs<LocalFs>,
    overlay_docs_memory_registration: MemoryRegistration,
    pub(crate) query_db: salsa::Database,
    indexes: Arc<Mutex<ProjectIndexes>>,
    indexes_evictor: Arc<WorkspaceProjectIndexesEvictor>,

    config: RwLock<EffectiveConfig>,
    memory: MemoryManager,
    scheduler: Scheduler,
    index_debouncer: KeyedDebouncer<&'static str>,
    project_reload_debouncer: KeyedDebouncer<&'static str>,
    memory_enforce_debouncer: KeyedDebouncer<&'static str>,
    subscribers: Arc<Mutex<Vec<Sender<WorkspaceEvent>>>>,

    project_state: Arc<Mutex<ProjectState>>,
    ide_project: RwLock<Option<Project>>,
    watch_config: Arc<RwLock<WatchConfig>>,
    watcher_command_store: Arc<Mutex<Option<channel::Sender<WatchCommand>>>>,

    #[cfg(test)]
    memory_enforce_observer: Arc<MemoryEnforceObserver>,
}

#[derive(Debug, Clone)]
enum WatchCommand {
    Watch(PathBuf),
}

#[derive(Debug)]
struct ProjectState {
    workspace_root: Option<PathBuf>,
    config: Arc<ProjectConfig>,
    load_options: LoadOptions,
    source_roots: Vec<SourceRootEntry>,
    classpath_fingerprint: Option<Fingerprint>,
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
            load_options: LoadOptions::default(),
            source_roots: Vec::new(),
            classpath_fingerprint: None,
            pending_build_changes: HashSet::new(),
            last_reload_started_at: None,
        }
    }
}

fn workspace_scheduler() -> Scheduler {
    static SCHEDULER: OnceLock<Scheduler> = OnceLock::new();
    SCHEDULER.get_or_init(Scheduler::default).clone()
}

#[cfg(test)]
#[derive(Debug, Default)]
struct MemoryEnforceObserver {
    inner: Mutex<usize>,
    cv: std::sync::Condvar,
}

#[cfg(test)]
impl MemoryEnforceObserver {
    fn record(&self) {
        let mut guard = self
            .inner
            .lock()
            .expect("memory enforce observer mutex poisoned");
        *guard += 1;
        self.cv.notify_all();
    }

    fn wait_for_at_least(&self, expected: usize, timeout: Duration) -> bool {
        let started = Instant::now();
        let mut guard = self
            .inner
            .lock()
            .expect("memory enforce observer mutex poisoned");
        while *guard < expected {
            let elapsed = started.elapsed();
            if elapsed >= timeout {
                return false;
            }
            let remaining = timeout - elapsed;
            let (next_guard, wait_res) = self
                .cv
                .wait_timeout(guard, remaining)
                .expect("memory enforce observer mutex poisoned");
            guard = next_guard;
            if wait_res.timed_out() {
                break;
            }
        }
        *guard >= expected
    }

    fn count(&self) -> usize {
        *self
            .inner
            .lock()
            .expect("memory enforce observer mutex poisoned")
    }
}

impl WorkspaceEngine {
    pub fn new(config: WorkspaceEngineConfig) -> Self {
        let scheduler = workspace_scheduler();
        let WorkspaceEngineConfig {
            workspace_root,
            persistence,
            memory,
        } = config;

        let vfs = Vfs::new(LocalFs::new());
        let open_docs = vfs.open_documents();

        let syntax_trees = SyntaxTreeStore::new(&memory, open_docs.clone());

        let query_db = salsa::Database::new_with_persistence(&workspace_root, persistence);
        query_db.register_salsa_memo_evictor(&memory);
        query_db.register_salsa_cancellation_on_memory_pressure(&memory);
        query_db.attach_item_tree_store(&memory, open_docs);
        query_db.set_syntax_tree_store(syntax_trees);

        let overlay_docs_memory_registration =
            memory.register_tracker("vfs_documents", MemoryCategory::Other);
        overlay_docs_memory_registration
            .tracker()
            .set_bytes(vfs.estimated_bytes() as u64);
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
        let indexes = Arc::new(Mutex::new(ProjectIndexes::default()));
        let indexes_evictor = WorkspaceProjectIndexesEvictor::new(&memory, Arc::clone(&indexes));

        let memory_enforce_debouncer = KeyedDebouncer::new(
            scheduler.clone(),
            PoolKind::Background,
            Duration::from_millis(750),
        );
        Self {
            vfs,
            overlay_docs_memory_registration,
            query_db,
            indexes,
            indexes_evictor,
            config: RwLock::new(EffectiveConfig::default()),
            memory,
            scheduler,
            index_debouncer,
            project_reload_debouncer,
            memory_enforce_debouncer,
            subscribers: Arc::new(Mutex::new(Vec::new())),
            project_state: Arc::new(Mutex::new(ProjectState::default())),
            ide_project: RwLock::new(None),
            watch_config: Arc::new(RwLock::new(WatchConfig::new(workspace_root))),
            watcher_command_store: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            memory_enforce_observer: Arc::new(MemoryEnforceObserver::default()),
        }
    }

    fn enforce_memory(&self) {
        let _ = self.memory.enforce();
        #[cfg(test)]
        self.memory_enforce_observer.record();
    }

    fn schedule_memory_enforcement(&self, delay: Duration) {
        let memory = self.memory.clone();
        #[cfg(test)]
        let observer = Arc::clone(&self.memory_enforce_observer);

        self.memory_enforce_debouncer.debounce_with_delay(
            "workspace-memory-enforce",
            delay,
            move |token| {
                Cancelled::check(&token)?;
                let _ = memory.enforce();
                #[cfg(test)]
                observer.record();
                Ok(())
            },
        );
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
        let (nova_config, nova_config_path) = nova_config::load_for_workspace(&root)
            .with_context(|| format!("failed to load nova config for {}", root.display()))?;
        {
            let mut state = self
                .project_state
                .lock()
                .expect("workspace project state mutex poisoned");
            state.workspace_root = Some(root.clone());
            state.load_options = LoadOptions {
                nova_config,
                nova_config_path,
                ..LoadOptions::default()
            };
            state.config = Arc::new(fallback_project_config(&root));
            state.source_roots = build_source_roots(&state.config);
            state.classpath_fingerprint = None;
            state.pending_build_changes.clear();
            state.last_reload_started_at = None;
        }

        // Load initial project state + file list.
        self.reload_project_now(&[])?;
        Ok(())
    }

    pub fn start_watching(self: &Arc<Self>) -> Result<WatcherHandle> {
        self.start_watching_with_watcher_factory(NotifyFileWatcher::new)
    }

    fn start_watching_with_watcher_factory<W, F>(self: &Arc<Self>, watcher_factory: F) -> Result<WatcherHandle>
    where
        W: FileWatcher + 'static,
        F: FnOnce() -> std::io::Result<W> + Send + 'static,
    {
        let watch_root = {
            let state = self
                .project_state
                .lock()
                .expect("workspace project state mutex poisoned");
            let root = state
                .workspace_root
                .clone()
                .context("workspace root not set")?;
            root
        };

        let watch_config = Arc::clone(&self.watch_config);
        let initial_config = watch_config
            .read()
            .expect("workspace watch config lock poisoned")
            .clone();

        let engine = Arc::clone(self);
        let (batch_tx, batch_rx) = channel::bounded::<WatcherMessage>(BATCH_QUEUE_CAPACITY);

        let (watcher_stop_tx, watcher_stop_rx) = channel::bounded::<()>(0);
        let (command_tx, command_rx) = channel::unbounded::<WatchCommand>();

        {
            *self
                .watcher_command_store
                .lock()
                .expect("workspace watcher command store mutex poisoned") = Some(command_tx.clone());
        }

        let subscribers = Arc::clone(&self.subscribers);
        let watcher_thread = thread::spawn(move || {
            let mut debouncer = Debouncer::new([
                (ChangeCategory::Source, Duration::from_millis(200)),
                (ChangeCategory::Build, Duration::from_millis(200)),
            ]);
            let mut watch_root_manager = WatchRootManager::new(Duration::from_secs(2));
            let retry_tick = channel::tick(Duration::from_secs(2));

            let mut watcher = match watcher_factory() {
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
            let watch_rx = watcher.receiver().clone();

            fn vfs_change_to_normalized(change: FileChange) -> Option<NormalizedEvent> {
                fn to_pathbuf(path: VfsPath) -> Option<PathBuf> {
                    path.as_local_path().map(|path| path.to_path_buf())
                }

                match change {
                    FileChange::Created { path } => Some(NormalizedEvent::Created(to_pathbuf(path)?)),
                    FileChange::Modified { path } => Some(NormalizedEvent::Modified(to_pathbuf(path)?)),
                    FileChange::Deleted { path } => Some(NormalizedEvent::Deleted(to_pathbuf(path)?)),
                    FileChange::Moved { from, to } => Some(NormalizedEvent::Moved {
                        from: to_pathbuf(from)?,
                        to: to_pathbuf(to)?,
                    }),
                }
            }

            let desired_roots =
                |workspace_root: &Path, config: &WatchConfig| -> HashSet<PathBuf> {
                    compute_watch_roots(workspace_root, config)
                        .into_iter()
                        .collect()
                };

            let now = Instant::now();
            for err in watch_root_manager.set_desired_roots(
                desired_roots(&watch_root, &initial_config),
                now,
                &mut watcher,
            ) {
                publish_watch_root_error(&subscribers, err);
            }

            let mut rescan_pending = false;

            loop {
                if rescan_pending {
                    match batch_tx.try_send(WatcherMessage::Rescan) {
                        Ok(()) => rescan_pending = false,
                        Err(channel::TrySendError::Full(_)) => {
                            // Downstream is behind; keep the rescan pending and retry soon.
                        }
                        Err(channel::TrySendError::Disconnected(_)) => break,
                    }
                }

                let now = Instant::now();
                let mut deadline = debouncer
                    .next_deadline()
                    .unwrap_or(now + Duration::from_secs(3600));
                if rescan_pending {
                    deadline = deadline.min(now + OVERFLOW_RETRY_INTERVAL);
                }
                let timeout = deadline.saturating_duration_since(now);
                let tick = channel::after(timeout);

                channel::select! {
                    recv(watcher_stop_rx) -> _ => {
                        for (cat, events) in debouncer.flush_all() {
                            let _ = batch_tx.try_send(WatcherMessage::Batch(cat, events));
                        }
                        break;
                    }
                    recv(command_rx) -> msg => {
                        let Ok(cmd) = msg else { break };
                        match cmd {
                            WatchCommand::Watch(_root) => {
                                let now = Instant::now();
                                let cfg = watch_config
                                    .read()
                                    .expect("workspace watch config lock poisoned")
                                    .clone();
                                for err in watch_root_manager.set_desired_roots(
                                    desired_roots(&watch_root, &cfg),
                                    now,
                                    &mut watcher,
                                ) {
                                    publish_watch_root_error(&subscribers, err);
                                }

                                for err in watch_root_manager.retry_pending(now, &mut watcher) {
                                    publish_watch_root_error(&subscribers, err);
                                }
                            }
                        }
                    }
                    recv(watch_rx) -> msg => {
                        let Ok(res) = msg else { break };
                        match res {
                            Ok(WatchEvent::Changes { changes }) => {
                                let now = Instant::now();
                                let config = watch_config
                                    .read()
                                    .expect("workspace watch config lock poisoned");
                                for change in changes {
                                    let Some(norm) = vfs_change_to_normalized(change) else {
                                        continue;
                                    };
                                    if let Some(cat) = categorize_event(&config, &norm) {
                                        debouncer.push(&cat, norm, now);
                                    }
                                }
                                for (cat, events) in debouncer.flush_due(now) {
                                    if let Err(err) = batch_tx.try_send(WatcherMessage::Batch(cat, events)) {
                                        if matches!(err, channel::TrySendError::Full(_)) {
                                            rescan_pending = true;
                                            debouncer = Debouncer::new([
                                                (ChangeCategory::Source, Duration::from_millis(200)),
                                                (ChangeCategory::Build, Duration::from_millis(200)),
                                            ]);
                                        } else {
                                            break;
                                        }
                                    }
                                }
                            }
                            Ok(WatchEvent::Rescan) | Err(_) => {
                                rescan_pending = true;
                                debouncer = Debouncer::new([
                                    (ChangeCategory::Source, Duration::from_millis(200)),
                                    (ChangeCategory::Build, Duration::from_millis(200)),
                                ]);
                            }
                        }
                    }
                    recv(tick) -> _ => {
                        let now = Instant::now();
                        for (cat, events) in debouncer.flush_due(now) {
                            if let Err(err) = batch_tx.try_send(WatcherMessage::Batch(cat, events)) {
                                if matches!(err, channel::TrySendError::Full(_)) {
                                    rescan_pending = true;
                                    debouncer = Debouncer::new([
                                        (ChangeCategory::Source, Duration::from_millis(200)),
                                        (ChangeCategory::Build, Duration::from_millis(200)),
                                    ]);
                                } else {
                                    break;
                                }
                            }
                        }
                    }
                    recv(retry_tick) -> _ => {
                        let now = Instant::now();

                        let cfg = watch_config
                            .read()
                            .expect("workspace watch config lock poisoned")
                            .clone();
                        for err in watch_root_manager.set_desired_roots(
                            desired_roots(&watch_root, &cfg),
                            now,
                            &mut watcher,
                        ) {
                            publish_watch_root_error(&subscribers, err);
                        }

                        for err in watch_root_manager.retry_pending(now, &mut watcher) {
                            publish_watch_root_error(&subscribers, err);
                        }

                        for (cat, events) in debouncer.flush_due(now) {
                            let _ = batch_tx.try_send(WatcherMessage::Batch(cat, events));
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
                    let Ok(msg) = msg else { break };
                    match msg {
                        WatcherMessage::Batch(category, events) => match category {
                            ChangeCategory::Source => engine.apply_filesystem_events(events),
                            ChangeCategory::Build => {
                                // Build/config changes normally don't need to flow through the VFS.
                                //
                                // However, we currently treat `module-info.java` as a build file so we
                                // can trigger a project reload to refresh the JPMS graph. We still want
                                // the VFS + Salsa inputs to see the updated file contents promptly for
                                // diagnostics and open-document behavior.
                                let mut java_events = Vec::new();
                                let mut changed = Vec::new();
                                for ev in events {
                                    let is_java = ev.paths().any(|path| {
                                        path.extension().and_then(|ext| ext.to_str()) == Some("java")
                                    });
                                    if is_java {
                                        java_events.push(ev.clone());
                                    }

                                    match ev {
                                        NormalizedEvent::Created(p)
                                        | NormalizedEvent::Modified(p)
                                        | NormalizedEvent::Deleted(p) => changed.push(p),
                                        NormalizedEvent::Moved { from, to } => {
                                            changed.push(from);
                                            changed.push(to);
                                        }
                                    }
                                }

                                if !java_events.is_empty() {
                                    engine.apply_filesystem_events(java_events);
                                }
                                engine.request_project_reload(changed);
                            }
                        },
                        WatcherMessage::Rescan => {
                            if let Err(err) = engine.reload_project_now(&[]) {
                                publish_to_subscribers(
                                    &engine.subscribers,
                                    WorkspaceEvent::Status(WorkspaceStatus::IndexingError(format!(
                                        "Project rescan failed: {err}"
                                    ))),
                                );
                            }
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
            watcher_command_store: Arc::clone(&self.watcher_command_store),
        })
    }

    pub fn apply_filesystem_events(&self, events: Vec<NormalizedEvent>) {
        if events.is_empty() {
            return;
        }

        // Coalesce noisy watcher streams by processing each path at most once per batch.
        let mut move_events: Vec<(PathBuf, PathBuf)> = Vec::new();
        let mut other_paths: HashSet<PathBuf> = HashSet::new();
        let mut module_info_changes: HashSet<PathBuf> = HashSet::new();

        for event in events {
            match event {
                NormalizedEvent::Moved { from, to } => {
                    if is_module_info_java(&from) {
                        module_info_changes.insert(from.clone());
                    }
                    if is_module_info_java(&to) {
                        module_info_changes.insert(to.clone());
                    }
                    move_events.push((from, to))
                }
                NormalizedEvent::Created(path)
                | NormalizedEvent::Modified(path)
                | NormalizedEvent::Deleted(path) => {
                    if is_module_info_java(&path) {
                        module_info_changes.insert(path.clone());
                    }
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

        if !module_info_changes.is_empty() {
            let mut paths: Vec<PathBuf> = module_info_changes.into_iter().collect();
            paths.sort();
            // `module-info.java` changes affect JPMS module discovery and should trigger a project
            // reload even though they are `.java` sources.
            self.request_project_reload(paths);
        }

        // Drive memory eviction once per batch, not once per file.
        self.schedule_memory_enforcement(Duration::from_millis(250));
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
        let watch_config = Arc::clone(&self.watch_config);
        let watcher_command_store = Arc::clone(&self.watcher_command_store);
        let memory = self.memory.clone();
        let memory_enforce_debouncer = self.memory_enforce_debouncer.clone();
        #[cfg(test)]
        let memory_observer = Arc::clone(&self.memory_enforce_observer);

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
                    reload_project_and_sync(
                        &root,
                        &changed,
                        &vfs,
                        &query_db,
                        &project_state,
                        &watch_config,
                        &watcher_command_store,
                    )
                {
                    publish_to_subscribers(
                        &subscribers,
                        WorkspaceEvent::Status(WorkspaceStatus::IndexingError(format!(
                            "Project reload failed: {err}"
                        ))),
                    );
                }

                // Project reload can update many file inputs at once; request an eviction pass.
                let memory_for_task = memory.clone();
                #[cfg(test)]
                let observer_for_task = Arc::clone(&memory_observer);
                memory_enforce_debouncer.debounce_with_delay(
                    "workspace-memory-enforce",
                    Duration::from_millis(0),
                    move |token| {
                        Cancelled::check(&token)?;
                        let _ = memory_for_task.enforce();
                        #[cfg(test)]
                        observer_for_task.record();
                        Ok(())
                    },
                );

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

        let result = reload_project_and_sync(
            &root,
            changed_files,
            &self.vfs,
            &self.query_db,
            &self.project_state,
            &self.watch_config,
            &self.watcher_command_store,
        );

        // Ensure we drive eviction after loading/updating a potentially large set of files.
        self.enforce_memory();

        result
    }

    pub fn open_document(&self, path: VfsPath, text: String, version: i32) -> FileId {
        let text_for_db = Arc::new(text.clone());
        let file_id = self.vfs.open_document(path.clone(), text, version);
        self.sync_overlay_documents_memory();
        self.ensure_file_inputs(file_id, &path);

        // Track whether this open document is "dirty" (differs from on-disk contents).
        //
        // This is used by Salsa warm-start indexing to ensure we don't reuse persisted index
        // shards when editor overlays have unsaved changes.
        let dirty = match path.as_local_path() {
            Some(local_path) => {
                if !local_path.exists() {
                    // New, unsaved file.
                    true
                } else {
                    // Prefer any existing Salsa `file_content` (which should reflect disk state for
                    // non-open files) to avoid redundant disk I/O.
                    let existing_content = self.query_db.with_snapshot(|snap| {
                        let has_content = snap
                            .all_file_ids()
                            .iter()
                            .any(|&existing_id| existing_id == file_id);
                        has_content.then(|| snap.file_content(file_id))
                    });

                    let disk_text = if let Some(existing) = existing_content {
                        Some(existing)
                    } else {
                        match fs::read_to_string(local_path) {
                            Ok(text) => Some(Arc::new(text)),
                            Err(_) => None,
                        }
                    };

                    match disk_text {
                        Some(disk_text) => disk_text.as_str() != text_for_db.as_str(),
                        None => true,
                    }
                }
            }
            None => {
                // Non-local paths (archives, virtual/decompiled docs, opaque URIs, etc) are
                // conservatively treated as dirty since we can't reliably compare with a stable
                // on-disk snapshot.
                true
            }
        };
        self.query_db.set_file_is_dirty(file_id, dirty);

        self.query_db.set_file_exists(file_id, true);
        self.query_db.set_file_content(file_id, text_for_db);
        self.update_project_files_membership(&path, file_id, true);

        self.publish(WorkspaceEvent::FileChanged { file: path.clone() });
        self.publish_diagnostics(path);
        // Opening a document can allocate large `Arc<String>` values and trigger diagnostics.
        self.schedule_memory_enforcement(Duration::from_millis(0));
        file_id
    }

    pub fn close_document(&self, path: &VfsPath) {
        let file_id = self.vfs.get_id(path);
        self.vfs.close_document(path);
        self.sync_overlay_documents_memory();

        if let Some(file_id) = file_id {
            self.ensure_file_inputs(file_id, path);
            let exists = self.vfs.exists(path);
            self.query_db.set_file_exists(file_id, exists);
            let mut restored_from_disk = false;
            if exists {
                if let Ok(text) = self.vfs.read_to_string(path) {
                    self.query_db.set_file_content(file_id, Arc::new(text));
                    restored_from_disk = true;
                }
            }
            if !exists || restored_from_disk {
                self.query_db.set_file_is_dirty(file_id, false);
            }
            self.update_project_files_membership(path, file_id, exists);
            // The document is no longer open; unpin its syntax tree so memory
            // accounting attributes it back to Salsa memoization.
            self.query_db.unpin_syntax_tree(file_id);
            self.query_db.unpin_item_tree(file_id);
        }

        // Closing can read disk contents back into Salsa, and often follows large edit sessions.
        self.schedule_memory_enforcement(Duration::from_millis(0));
    }

    pub fn apply_changes(
        &self,
        path: &VfsPath,
        new_version: i32,
        changes: &[ContentChange],
    ) -> Result<Vec<TextEdit>, DocumentError> {
        let evt = match self
            .vfs
            .apply_document_changes(path, new_version, changes)
        {
            Ok(evt) => evt,
            Err(err) => {
                // Best-effort: keep the memory report consistent even when edits fail.
                self.sync_overlay_documents_memory();
                return Err(err);
            }
        };
        self.sync_overlay_documents_memory();
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
            self.query_db.set_file_is_dirty(file_id, true);
        }

        self.publish(WorkspaceEvent::FileChanged { file: path.clone() });
        self.publish_diagnostics(path.clone());
        // Document edits can generate large transient allocations; debounce eviction to avoid
        // enforcing on every keystroke.
        self.schedule_memory_enforcement(Duration::from_millis(750));
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

    fn background_indexing_plan(
        degraded: BackgroundIndexingMode,
        all_files: Vec<FileId>,
        open_files: &HashSet<FileId>,
    ) -> Option<Vec<FileId>> {
        const MAX_REDUCED_FILES: usize = 128;

        match degraded {
            BackgroundIndexingMode::Paused => None,
            BackgroundIndexingMode::Full => Some(all_files),
            BackgroundIndexingMode::Reduced => {
                let mut files: Vec<FileId> = if open_files.is_empty() {
                    all_files.into_iter().take(MAX_REDUCED_FILES).collect()
                } else {
                    open_files.iter().copied().collect()
                };
                files.sort();
                files.dedup();
                Some(files)
            }
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
        // Gate background indexing based on the latest memory state.
        let report = self.memory.enforce();
        if report.degraded.background_indexing == BackgroundIndexingMode::Paused {
            self.publish(WorkspaceEvent::Status(WorkspaceStatus::IndexingPaused(
                "Indexing paused due to memory pressure".to_string(),
            )));
            return;
        }

        // Coalesce rapid edit bursts (e.g. didChange storms) and cancel in-flight indexing when
        // superseded by a newer request.
        let query_db = self.query_db.clone();
        let indexes_evictor = Arc::clone(&self.indexes_evictor);
        let subscribers = Arc::clone(&self.subscribers);
        let scheduler = self.scheduler.clone();
        let memory = self.memory.clone();
        let open_docs = self.vfs.open_documents();

        self.index_debouncer
            .debounce("workspace-index", move |token| {
                const ENFORCE_INTERVAL: Duration = Duration::from_millis(250);

                let token_for_cancel = token.clone();
                let ctx = scheduler.request_context_with_token("workspace/indexing", token);
                Cancelled::check(ctx.token())?;

                let report = memory.enforce();
                let degraded = report.degraded.background_indexing;
                if degraded == BackgroundIndexingMode::Paused {
                    publish_to_subscribers(
                        &subscribers,
                        WorkspaceEvent::Status(WorkspaceStatus::IndexingPaused(
                            "Indexing paused due to memory pressure".to_string(),
                        )),
                    );
                    return Ok(());
                }

                let progress = ctx.progress().start("Indexing workspace");

                publish_to_subscribers(
                    &subscribers,
                    WorkspaceEvent::Status(WorkspaceStatus::IndexingStarted),
                );

                let project = ProjectId::from_raw(0);
                let project_files: Vec<FileId> = query_db
                    .with_snapshot(|snap| snap.project_files(project).as_ref().clone());
                let open_files = open_docs.snapshot();

                let files = match degraded {
                    BackgroundIndexingMode::Full => project_files.clone(),
                    BackgroundIndexingMode::Reduced => {
                        Self::background_indexing_plan(degraded, project_files.clone(), &open_files)
                            .unwrap_or_default()
                    }
                    BackgroundIndexingMode::Paused => Vec::new(),
                };

                let total = files.len();

                // Ensure consumers always see at least one progress update, even if the project is
                // empty or progress is coarse-grained.
                progress.report(
                    Some(format!("0/{}", total)),
                    if total == 0 { None } else { Some(0) },
                );
                publish_to_subscribers(
                    &subscribers,
                    WorkspaceEvent::IndexProgress(IndexProgress { current: 0, total }),
                );

                // If this indexing request is cancelled while Salsa is in-flight, request Salsa
                // cancellation so queries unwind at the next checkpoint (best-effort).
                let query_db_for_cancel = query_db.clone();
                let cancel_handle = scheduler.io_handle().spawn(async move {
                    token_for_cancel.cancelled().await;
                    query_db_for_cancel.request_cancellation();
                });

                // Periodically enforce memory budgets while indexing runs. If we hit critical
                // pressure, request Salsa cancellation and mark the run as aborted.
                //
                // Use the scheduler's IO runtime instead of spawning a dedicated OS thread.
                // This keeps indexing robust in constrained environments (tests/CI) where thread
                // creation can fail (`EAGAIN`).
                let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let aborted_due_to_memory = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let stop_for_task = Arc::clone(&stop_flag);
                let aborted_for_task = Arc::clone(&aborted_due_to_memory);
                let memory_for_task = memory.clone();
                let query_db_for_task = query_db.clone();
                let enforcer_handle = scheduler.io_handle().spawn(async move {
                    loop {
                        tokio::time::sleep(ENFORCE_INTERVAL).await;
                        if stop_for_task.load(std::sync::atomic::Ordering::Relaxed) {
                            break;
                        }
                        let report = memory_for_task.enforce();
                        if report.pressure == MemoryPressure::Critical
                            || report.degraded.background_indexing == BackgroundIndexingMode::Paused
                        {
                            aborted_for_task.store(true, std::sync::atomic::Ordering::Relaxed);
                            query_db_for_task.request_cancellation();
                            break;
                        }
                    }
                });

                let indexing_result: std::result::Result<ProjectIndexes, Cancelled> = match degraded
                {
                    BackgroundIndexingMode::Full => query_db
                        .with_snapshot_catch_cancelled(|snap| snap.project_indexes(project))
                        .map(|indexes| (*indexes).clone())
                        .map_err(|_cancelled| Cancelled),
                    BackgroundIndexingMode::Reduced => {
                        let mut indexes = ProjectIndexes::default();
                        let mut cancelled = false;

                        for (idx, file_id) in files.iter().enumerate() {
                            if ctx.token().is_cancelled()
                                || aborted_due_to_memory.load(std::sync::atomic::Ordering::Relaxed)
                            {
                                cancelled = true;
                                break;
                            }

                            let delta = match query_db
                                .with_snapshot_catch_cancelled(|snap| snap.file_index_delta(*file_id))
                            {
                                Ok(delta) => delta,
                                Err(_) => {
                                    cancelled = true;
                                    break;
                                }
                            };

                            indexes.merge_from((*delta).clone());

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

                        if cancelled {
                            Err(Cancelled)
                        } else {
                            Ok(indexes)
                        }
                    }
                    BackgroundIndexingMode::Paused => Err(Cancelled),
                };

                stop_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                enforcer_handle.abort();
                // Always abort the cancellation watcher (even when Salsa cancels).
                cancel_handle.abort();

                let indexes = match indexing_result {
                    Ok(indexes) => indexes,
                    Err(Cancelled) => {
                        if aborted_due_to_memory.load(std::sync::atomic::Ordering::Relaxed) {
                            progress.finish(Some("Paused due to memory pressure".to_string()));
                            publish_to_subscribers(
                                &subscribers,
                                WorkspaceEvent::Status(WorkspaceStatus::IndexingPaused(
                                    "Indexing paused due to memory pressure".to_string(),
                                )),
                            );
                        }
                        return Err(Cancelled);
                    }
                };

                Cancelled::check(ctx.token())?;

                indexes_evictor.replace_indexes(indexes);

                progress.report(
                    Some(format!("{}/{}", total, total)),
                    if total == 0 { None } else { Some(100) },
                );
                publish_to_subscribers(
                    &subscribers,
                    WorkspaceEvent::IndexProgress(IndexProgress { current: total, total }),
                );

                if degraded == BackgroundIndexingMode::Full {
                    // Best-effort persistence: indexing results are still valid even if we fail to
                    // write them to disk.
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let _ = query_db.persist_project_indexes(project);
                    }));
                }

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
        let rel_path_arc = Arc::new(rel_path);
        self.query_db.set_file_rel_path(file_id, rel_path_arc);
        // `file_path` is a non-tracked persistence key used to warm-start Salsa queries
        // (parse/HIR/typeck/flow). It is intentionally kept project-relative and normalized for
        // cross-platform cache reuse.
        //
        // `nova-db` keeps it in sync with `file_rel_path` (sharing the same `Arc<String>`), so
        // workspace-managed files should only set `file_rel_path` here.
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
                Ok(text) => {
                    self.query_db.set_file_content(file_id, Arc::new(text));
                    self.query_db.set_file_is_dirty(file_id, false);
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    self.query_db.set_file_exists(file_id, false);
                    exists = false;
                    self.query_db.set_file_is_dirty(file_id, false);
                }
                Err(_) if !was_known => {
                    self.query_db
                        .set_file_content(file_id, Arc::new(String::new()));
                    self.query_db.set_file_is_dirty(file_id, false);
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
        self.sync_overlay_documents_memory();

        self.ensure_file_inputs(file_id, &to_vfs);
        let exists = self.vfs.exists(&to_vfs);
        self.query_db.set_file_exists(file_id, exists);

        if exists {
            if open_docs.is_open(file_id) {
                // The document is open in the editor (either because it was already open at `to`,
                // or because it was moved there from `from`). Ensure Salsa sees the overlay contents
                // so workspace analysis doesn't accidentally use stale disk state.
                if let Ok(text) = self.vfs.read_to_string(&to_vfs) {
                    self.query_db.set_file_content(file_id, Arc::new(text));
                }
            } else {
                match fs::read_to_string(to) {
                    Ok(text) => self.query_db.set_file_content(file_id, Arc::new(text)),
                    Err(_) if is_new_id => {
                        self.query_db
                            .set_file_content(file_id, Arc::new(String::new()));
                    }
                    Err(_) => {}
                }
            }
        }

        // A move can have two effects on ids:
        // - Typical case: preserve `id_from` at `to`.
        // - Destination already known: keep destination id and orphan `id_from`.
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

    fn sync_overlay_documents_memory(&self) {
        self.overlay_docs_memory_registration
            .tracker()
            .set_bytes(self.vfs.estimated_bytes() as u64);
    }

    pub(crate) fn vfs(&self) -> &Vfs<LocalFs> {
        &self.vfs
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

fn publish_watch_root_error(
    subscribers: &Arc<Mutex<Vec<Sender<WorkspaceEvent>>>>,
    err: WatchRootError,
) {
    match err {
        WatchRootError::WatchFailed { root, error } => {
            publish_to_subscribers(
                subscribers,
                WorkspaceEvent::Status(WorkspaceStatus::IndexingError(format!(
                    "Failed to watch {}: {error}",
                    root.display()
                ))),
            );
        }
        WatchRootError::UnwatchFailed { root, error } => {
            publish_to_subscribers(
                subscribers,
                WorkspaceEvent::Status(WorkspaceStatus::IndexingError(format!(
                    "Failed to unwatch {}: {error}",
                    root.display()
                ))),
            );
        }
    }
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

fn is_module_info_java(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name == "module-info.java")
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

fn watch_roots_from_project_config(config: &ProjectConfig) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut source_roots = Vec::new();
    let mut generated_source_roots = Vec::new();
    for root_entry in &config.source_roots {
        match root_entry.origin {
            SourceRootOrigin::Source => source_roots.push(root_entry.path.clone()),
            SourceRootOrigin::Generated => generated_source_roots.push(root_entry.path.clone()),
        }
    }
    source_roots.sort();
    source_roots.dedup();
    generated_source_roots.sort();
    generated_source_roots.dedup();
    (source_roots, generated_source_roots)
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

fn classpath_fingerprint(config: &ProjectConfig) -> Fingerprint {
    let mut bytes = Vec::new();
    // Classpath indexing behavior depends on the effective Java target release
    // (multi-release JAR selection), so include it in the fingerprint to ensure
    // the cached classpath index is invalidated when the language level changes.
    bytes.extend_from_slice(&config.java.target.0.to_le_bytes());
    for entry in config.classpath.iter().chain(config.module_path.iter()) {
        bytes.push(match entry.kind {
            nova_project::ClasspathEntryKind::Directory => b'D',
            nova_project::ClasspathEntryKind::Jar => b'J',
        });
        bytes.extend_from_slice(entry.path.to_string_lossy().as_bytes());
        bytes.push(0);
    }
    Fingerprint::from_bytes(bytes)
}

fn reload_project_and_sync(
    workspace_root: &Path,
    changed_files: &[PathBuf],
    vfs: &Vfs<LocalFs>,
    query_db: &salsa::Database,
    project_state: &Arc<Mutex<ProjectState>>,
    watch_config: &Arc<RwLock<WatchConfig>>,
    watcher_command_store: &Arc<Mutex<Option<channel::Sender<WatchCommand>>>>,
) -> Result<()> {
    let (previous_config, mut options, previous_classpath_fingerprint) = {
        let state = project_state
            .lock()
            .expect("workspace project state mutex poisoned");
        (
            Arc::clone(&state.config),
            state.load_options.clone(),
            state.classpath_fingerprint.clone(),
        )
    };

    let fallback_config = if previous_config.workspace_root.as_os_str().is_empty()
        || previous_config.workspace_root.as_path() != workspace_root
    {
        Some(fallback_project_config(workspace_root))
    } else {
        None
    };
    let base_config = fallback_config
        .as_ref()
        .unwrap_or_else(|| previous_config.as_ref());

    // `nova-project` only reloads the full project model when build files change. Generated-source
    // roots can change when nova-apt updates its snapshot at
    // `.nova/apt-cache/generated-roots.json`, so force a full reload when that file is in the
    // change set.
    let effective_changed_files = if changed_files.iter().any(|path| {
        path.file_name()
            .is_some_and(|name| name == "generated-roots.json")
            && path.ends_with(Path::new(".nova/apt-cache/generated-roots.json"))
    }) {
        &[] as &[PathBuf]
    } else {
        changed_files
    };

    let loaded = match nova_project::reload_project(base_config, &mut options, effective_changed_files)
    {
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

    let had_classpath_fingerprint = previous_classpath_fingerprint.is_some();
    let config_changed = &loaded != previous_config.as_ref();
    let config = if config_changed {
        Arc::new(loaded)
    } else {
        Arc::clone(&previous_config)
    };
    let source_roots = build_source_roots(&config);
    let (watch_source_roots, watch_generated_roots) = watch_roots_from_project_config(&config);
    let next_classpath_fingerprint = classpath_fingerprint(&config);
    let classpath_changed = match &previous_classpath_fingerprint {
        Some(prev) => prev != &next_classpath_fingerprint,
        None => true,
    };

    {
        let mut state = project_state
            .lock()
            .expect("workspace project state mutex poisoned");
        state.config = Arc::clone(&config);
        state.load_options = options;
        state.source_roots = source_roots.clone();
        state.classpath_fingerprint = Some(next_classpath_fingerprint.clone());
    }

    {
        let mut cfg = watch_config
            .write()
            .expect("workspace watch config lock poisoned");
        cfg.workspace_root = workspace_root.to_path_buf();
        cfg.source_roots = watch_source_roots.clone();
        cfg.generated_source_roots = watch_generated_roots.clone();
    }

    // If the watcher is running, ensure it begins watching any newly discovered roots outside the
    // workspace root. Roots under the workspace root are already covered by the recursive watch.
    if let Some(tx) = watcher_command_store
        .lock()
        .expect("workspace watcher command store mutex poisoned")
        .clone()
    {
        for root in watch_source_roots
            .iter()
            .chain(watch_generated_roots.iter())
        {
            if root.starts_with(workspace_root) {
                continue;
            }
            let _ = tx.send(WatchCommand::Watch(root.clone()));
        }
    }

    let project = ProjectId::from_raw(0);
    if !had_classpath_fingerprint || config_changed {
        query_db.set_project_config(project, Arc::clone(&config));
    }
    let requested_release = Some(config.java.target.0)
        .filter(|release| *release >= 1)
        .or_else(|| Some(config.java.source.0).filter(|release| *release >= 1));

    // Best-effort JDK index discovery.
    //
    // We intentionally do not fail workspace loading when JDK discovery or indexing fails: Nova can
    // still operate with a tiny built-in JDK index (used by unit tests / bootstrapping), but many
    // IDE features (decompilation, richer type info) benefit from a real platform index.
    let (workspace_config, _) =
        nova_config::load_for_workspace(workspace_root).unwrap_or_else(|_| {
            // If config loading fails, fall back to defaults; the workspace should still open.
            (nova_config::NovaConfig::default(), None)
    });
    let jdk_config = {
        // Nova config paths are expected to be relative to the workspace root when possible.
        // `NovaConfig::jdk_config` returns raw `PathBuf`s from the config file, so resolve them
        // here before handing them to `nova_jdk` discovery.
        let mut cfg = workspace_config.jdk_config();
        cfg.home = cfg.home.map(|p| {
            if p.is_absolute() {
                p
            } else {
                workspace_root.join(p)
            }
        });
        cfg.toolchains = cfg
            .toolchains
            .into_iter()
            .map(|(release, path)| {
                let resolved = if path.is_absolute() {
                    path
                } else {
                    workspace_root.join(path)
                };
                (release, resolved)
            })
            .collect();
        cfg
    };

    let jdk_index = nova_jdk::JdkIndex::discover_for_release(Some(&jdk_config), requested_release)
        .unwrap_or_else(|_| nova_jdk::JdkIndex::new());
    query_db.set_jdk_index(project, Arc::new(jdk_index));

    if !had_classpath_fingerprint || classpath_changed {
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
            let classpath_cache_dir = query_db.classpath_cache_dir();
            match nova_classpath::ClasspathIndex::build_with_options(
                &classpath_entries,
                classpath_cache_dir.as_deref(),
                nova_classpath::IndexOptions {
                    target_release: requested_release,
                },
            ) {
                Ok(index) => query_db.set_classpath_index(project, Some(Arc::new(index))),
                Err(_) => query_db.set_classpath_index(project, None),
            }
        }
    }

    // JPMS-only updates (i.e. `module-info.java` edits) do not affect workspace source roots or
    // membership, so we can avoid a full rescan of Java files. The file watcher already
    // propagates the on-disk edit into the VFS/Salsa file inputs.
    if !changed_files.is_empty()
        && changed_files.iter().all(|path| {
            path.file_name()
                .is_some_and(|name| name == "module-info.java")
        })
    {
        return Ok(());
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

        // `nova-db` keeps the non-tracked persistence `file_path` in sync with `file_rel_path`,
        // sharing the same `Arc<String>` for minimal allocations and stable persistence keys.
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
    // NOTE(file-watching tests):
    // Avoid tests that rely on real OS watcher timing (starting `notify`, touching the filesystem,
    // then sleeping and hoping an event arrives). They are flaky across platforms/CI.
    //
    // Prefer deterministic tests that either:
    // - inject a manual watcher (e.g. `nova_vfs::ManualFileWatcher`) into the workspace, or
    // - bypass the watcher entirely and call `apply_filesystem_events` with normalized events.
    //
    // See `docs/file-watching.md` for more background.

    use nova_cache::{CacheConfig, CacheDir};
    use nova_db::persistence::{PersistenceConfig, PersistenceMode};
    use nova_db::salsa::HasFilePaths;
    use nova_db::NovaInputs;
    use nova_index::{
        AnnotationLocation, IndexedSymbol, IndexSymbolKind, InheritanceEdge, ReferenceLocation,
        SymbolLocation,
    };
    use nova_memory::{
        EvictionRequest, EvictionResult, MemoryBudget, MemoryCategory, MemoryEvictor,
    };
    use nova_project::BuildSystem;
    use nova_vfs::{ManualFileWatcher, ManualFileWatcherHandle};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;
    use tokio::time::timeout;

    use super::*;

    #[test]
    fn open_document_keeps_file_rel_path_shared_with_persistent_file_path() {
        let workspace = crate::Workspace::new_in_memory();
        let dir = tempfile::tempdir().unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let root = dir.path().canonicalize().unwrap();

        // Initialize workspace root so `ensure_file_inputs` derives a stable project-relative path.
        let engine = workspace.engine_for_tests();
        engine.set_workspace_root(&root).unwrap();

        let file = workspace.open_document(
            VfsPath::local(root.join("A.java")),
            "class A {}".to_string(),
            1,
        );

        engine.query_db.with_snapshot(|snap| {
            let rel_path = snap.file_rel_path(file);
            let persistent_path = snap.file_path(file).expect("expected file path for FileId");

            assert_eq!(&*rel_path, &*persistent_path);
            assert!(
                Arc::ptr_eq(&rel_path, &persistent_path),
                "expected file_rel_path and file_path to share the same Arc"
            );
        });
    }

    async fn wait_for_indexing_ready(rx: &async_channel::Receiver<WorkspaceEvent>) {
        let mut saw_started = false;
        let mut saw_progress = false;

        loop {
            let event = rx
                .recv()
                .await
                .expect("workspace event channel unexpectedly closed");
            match event {
                WorkspaceEvent::Status(WorkspaceStatus::IndexingStarted) => {
                    saw_started = true;
                }
                WorkspaceEvent::IndexProgress(_) => {
                    saw_progress = true;
                }
                WorkspaceEvent::Status(WorkspaceStatus::IndexingReady) => {
                    break;
                }
                WorkspaceEvent::Status(WorkspaceStatus::IndexingPaused(reason)) => {
                    panic!("indexing paused unexpectedly: {reason}");
                }
                WorkspaceEvent::Status(WorkspaceStatus::IndexingError(err)) => {
                    panic!("indexing failed unexpectedly: {err}");
                }
                _ => {}
            }
        }

        assert!(saw_started, "expected IndexingStarted status event");
        assert!(saw_progress, "expected at least one IndexProgress event");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn trigger_indexing_uses_project_level_indexing_queries() {
        let workspace = crate::Workspace::new_in_memory();

        let dir = tempfile::tempdir().unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let root = dir.path().canonicalize().unwrap();

        // Open 2 Java files as in-memory overlays.
        workspace.open_document(
            VfsPath::local(root.join("A.java")),
            "class A {}".to_string(),
            1,
        );
        workspace.open_document(
            VfsPath::local(root.join("B.java")),
            "class B {}".to_string(),
            1,
        );

        // Initialize workspace root so `project_files` is populated (and indexing is scoped to the
        // project, not all known `FileId`s).
        let engine = workspace.engine_for_tests();
        engine.set_workspace_root(&root).unwrap();
        let project = ProjectId::from_raw(0);
        engine.query_db.with_snapshot(|snap| {
            assert_eq!(snap.project_files(project).len(), 2);
        });

        engine.query_db.clear_query_stats();

        let rx = workspace.subscribe();
        workspace.trigger_indexing();

        timeout(Duration::from_secs(10), wait_for_indexing_ready(&rx))
            .await
            .expect("timed out waiting for indexing");

        let stats = engine.query_db.query_stats();
        let project_shards_exec = stats
            .by_query
            .get("project_index_shards")
            .map(|stat| stat.executions)
            .unwrap_or(0);
        let project_indexes_exec = stats
            .by_query
            .get("project_indexes")
            .map(|stat| stat.executions)
            .unwrap_or(0);

        assert!(
            project_shards_exec > 0 || project_indexes_exec > 0,
            "expected project-level indexing query to execute; stats={stats:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn trigger_indexing_persists_project_index_shards_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().join("project");
        fs::create_dir_all(project_root.join("src")).unwrap();
        fs::write(
            project_root.join("src/Main.java"),
            "class Main {}".as_bytes(),
        )
        .unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let project_root = project_root.canonicalize().unwrap();

        let cache_root = dir.path().join("cache-root");

        let persistence = PersistenceConfig {
            mode: PersistenceMode::ReadWrite,
            cache: CacheConfig {
                cache_root_override: Some(cache_root.clone()),
            },
        };
        let memory = MemoryManager::new(MemoryBudget::default_for_system_with_env_overrides());
        let engine = WorkspaceEngine::new(WorkspaceEngineConfig {
            workspace_root: project_root.clone(),
            persistence: persistence.clone(),
            memory,
        });

        engine.set_workspace_root(&project_root).unwrap();

        let rx = engine.subscribe();
        engine.trigger_indexing();
        timeout(Duration::from_secs(20), wait_for_indexing_ready(&rx))
            .await
            .expect("timed out waiting for indexing");

        let cache_dir =
            CacheDir::new(&project_root, CacheConfig { cache_root_override: Some(cache_root) })
                .expect("cache dir should be computable");
        let shards_root = cache_dir.indexes_dir().join("shards");
        let manifest_path = shards_root.join("manifest.txt");
        assert!(
            manifest_path.is_file(),
            "expected shard manifest at {}",
            manifest_path.display()
        );
        let shard0_symbols = shards_root.join("0").join("symbols.idx");
        assert!(
            shard0_symbols.is_file(),
            "expected at least one persisted shard index file (missing {})",
            shard0_symbols.display()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn warm_start_reindexes_dirty_overlays_and_ignores_stale_persisted_shards() {
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().join("project");
        fs::create_dir_all(project_root.join("src")).unwrap();
        fs::write(
            project_root.join("src/Main.java"),
            "class Main {}".as_bytes(),
        )
        .unwrap();
        // Add a second file so warm-start can still reuse persisted shards for unchanged files.
        fs::write(
            project_root.join("src/Helper.java"),
            "class Helper {}".as_bytes(),
        )
        .unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let project_root = project_root.canonicalize().unwrap();

        let cache_root = dir.path().join("cache-root");
        let persistence = PersistenceConfig {
            mode: PersistenceMode::ReadWrite,
            cache: CacheConfig {
                cache_root_override: Some(cache_root.clone()),
            },
        };

        // First engine: index + persist.
        let memory = MemoryManager::new(MemoryBudget::default_for_system_with_env_overrides());
        let engine = WorkspaceEngine::new(WorkspaceEngineConfig {
            workspace_root: project_root.clone(),
            persistence: persistence.clone(),
            memory,
        });
        engine.set_workspace_root(&project_root).unwrap();

        let rx = engine.subscribe();
        engine.trigger_indexing();
        timeout(Duration::from_secs(20), wait_for_indexing_ready(&rx))
            .await
            .expect("timed out waiting for initial indexing");

        let cache_dir = CacheDir::new(
            &project_root,
            CacheConfig {
                cache_root_override: Some(cache_root.clone()),
            },
        )
        .expect("cache dir should be computable");
        let shards_root = cache_dir.indexes_dir().join("shards");
        assert!(
            shards_root.join("manifest.txt").is_file(),
            "expected persisted shard manifest at {}",
            shards_root.join("manifest.txt").display()
        );

        // Second engine: warm-start, then open a dirty overlay without touching disk.
        let memory = MemoryManager::new(MemoryBudget::default_for_system_with_env_overrides());
        let engine = WorkspaceEngine::new(WorkspaceEngineConfig {
            workspace_root: project_root.clone(),
            persistence,
            memory,
        });
        engine.set_workspace_root(&project_root).unwrap();

        let main_path = project_root.join("src/Main.java");
        engine.open_document(
            VfsPath::local(main_path),
            "class Dirty {}".to_string(),
            1,
        );

        engine.query_db.clear_query_stats();

        let rx = engine.subscribe();
        engine.trigger_indexing();
        timeout(Duration::from_secs(20), wait_for_indexing_ready(&rx))
            .await
            .expect("timed out waiting for indexing with dirty overlay");

        let stats = engine.query_db.query_stats();
        let delta_exec = stats
            .by_query
            .get("file_index_delta")
            .map(|stat| stat.executions)
            .unwrap_or(0);
        assert!(
            delta_exec > 0,
            "expected dirty file to be reindexed via file_index_delta; stats={stats:?}"
        );

        let disk_hits = stats
            .by_query
            .get("project_indexes")
            .map(|stat| stat.disk_hits)
            .unwrap_or(0);
        assert!(
            disk_hits > 0,
            "expected warm-start to reuse persisted shards for unchanged files; stats={stats:?}"
        );

        let indexes = engine
            .indexes
            .lock()
            .expect("workspace indexes lock poisoned")
            .clone();
        assert!(
            indexes.symbols.symbols.contains_key("Dirty"),
            "expected symbol `Dirty` to be present in indexes; symbols={:?}",
            indexes.symbols.symbols.keys().collect::<Vec<_>>()
        );
        assert!(
            !indexes.symbols.symbols.contains_key("Main"),
            "expected stale symbol `Main` to be removed from indexes; symbols={:?}",
            indexes.symbols.symbols.keys().collect::<Vec<_>>()
        );
        assert!(
            indexes.symbols.symbols.contains_key("Helper"),
            "expected unchanged file symbols to be preserved via warm-start; symbols={:?}",
            indexes.symbols.symbols.keys().collect::<Vec<_>>()
        );
    }

    fn current_rss_bytes() -> Option<u64> {
        #[cfg(target_os = "linux")]
        {
            let status = std::fs::read_to_string("/proc/self/status").ok()?;
            for line in status.lines() {
                let line = line.trim_start();
                if let Some(rest) = line.strip_prefix("VmRSS:") {
                    let kb = rest.trim().split_whitespace().next()?.parse::<u64>().ok()?;
                    return Some(kb.saturating_mul(1024));
                }
            }
            None
        }

        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    fn test_memory_budget_total() -> u64 {
        // `MemoryManager` considers process RSS as an upper bound over the tracked totals. Ensure
        // our synthetic tracker can dominate RSS by giving the budget ample headroom.
        let rss = current_rss_bytes().unwrap_or(0);
        (rss.saturating_mul(2)).max(512 * nova_memory::MB)
    }

    #[test]
    fn background_indexing_plan_is_paused_under_critical_pressure() {
        let budget_total = test_memory_budget_total();
        let memory = MemoryManager::new(MemoryBudget::from_total(budget_total));
        let registration = memory.register_tracker("pressure", MemoryCategory::Other);
        registration
            .tracker()
            .set_bytes(((budget_total as f64) * 0.99) as u64);
        let report = memory.enforce();
        assert_eq!(
            report.degraded.background_indexing,
            BackgroundIndexingMode::Paused
        );

        let all_files = vec![FileId::from_raw(1), FileId::from_raw(2)];
        let open_files: HashSet<FileId> = HashSet::from([FileId::from_raw(1)]);
        let plan = WorkspaceEngine::background_indexing_plan(
            report.degraded.background_indexing,
            all_files,
            &open_files,
        );
        assert!(plan.is_none());
    }

    #[test]
    fn background_indexing_plan_is_reduced_to_open_documents_under_high_pressure() {
        let budget_total = test_memory_budget_total();
        let memory = MemoryManager::new(MemoryBudget::from_total(budget_total));
        let registration = memory.register_tracker("pressure", MemoryCategory::Other);
        registration
            .tracker()
            .set_bytes(((budget_total as f64) * 0.90) as u64);
        let report = memory.enforce();
        assert_eq!(
            report.degraded.background_indexing,
            BackgroundIndexingMode::Reduced
        );

        let all_files = vec![FileId::from_raw(1), FileId::from_raw(2), FileId::from_raw(3)];
        let open_files: HashSet<FileId> = HashSet::from([FileId::from_raw(2)]);
        let files = WorkspaceEngine::background_indexing_plan(
            report.degraded.background_indexing,
            all_files,
            &open_files,
        )
        .expect("plan should be present in Reduced mode");
        assert_eq!(files, vec![FileId::from_raw(2)]);
    }

    struct TestEvictor {
        name: String,
        category: MemoryCategory,
        bytes: Mutex<u64>,
        _registration: OnceLock<nova_memory::MemoryRegistration>,
        tracker: OnceLock<nova_memory::MemoryTracker>,
        evict_calls: AtomicUsize,
    }

    impl TestEvictor {
        fn new(manager: &MemoryManager, name: &str, category: MemoryCategory) -> Arc<Self> {
            let evictor = Arc::new(Self {
                name: name.to_string(),
                category,
                bytes: Mutex::new(0),
                _registration: OnceLock::new(),
                tracker: OnceLock::new(),
                evict_calls: AtomicUsize::new(0),
            });

            let registration =
                manager.register_evictor(name.to_string(), category, evictor.clone());
            evictor
                .tracker
                .set(registration.tracker())
                .expect("tracker should only be set once");
            evictor
                ._registration
                .set(registration)
                .expect("registration should only be set once");

            evictor
        }

        fn set_bytes(&self, bytes: u64) {
            *self.bytes.lock().unwrap() = bytes;
            self.tracker.get().unwrap().set_bytes(bytes);
        }

        fn bytes(&self) -> u64 {
            *self.bytes.lock().unwrap()
        }

        fn evict_calls(&self) -> usize {
            self.evict_calls.load(Ordering::Relaxed)
        }
    }

    impl MemoryEvictor for TestEvictor {
        fn name(&self) -> &str {
            &self.name
        }

        fn category(&self) -> MemoryCategory {
            self.category
        }

        fn evict(&self, request: EvictionRequest) -> EvictionResult {
            self.evict_calls.fetch_add(1, Ordering::Relaxed);
            let mut bytes = self.bytes.lock().unwrap();
            let before = *bytes;
            let after = before.min(request.target_bytes);
            *bytes = after;
            self.tracker.get().unwrap().set_bytes(after);
            EvictionResult {
                before_bytes: before,
                after_bytes: after,
            }
        }
    }

    #[test]
    fn memory_enforcement_is_triggered_by_open_document() {
        let manager = MemoryManager::new(MemoryBudget::from_total(1_000));
        let evictor = TestEvictor::new(&manager, "test", MemoryCategory::Other);
        evictor.set_bytes(10_000);

        let engine = WorkspaceEngine::new(WorkspaceEngineConfig {
            workspace_root: PathBuf::new(),
            persistence: PersistenceConfig::default(),
            memory: manager.clone(),
        });

        let tmp = tempfile::tempdir().unwrap();
        let path = VfsPath::local(tmp.path().join("Main.java"));
        engine.open_document(path, "class Main {}".to_string(), 1);

        assert!(
            engine
                .memory_enforce_observer
                .wait_for_at_least(1, Duration::from_secs(2)),
            "timed out waiting for MemoryManager.enforce (count={})",
            engine.memory_enforce_observer.count(),
        );

        assert!(
            evictor.evict_calls() > 0,
            "expected test evictor to be invoked during enforcement"
        );
        assert_eq!(evictor.bytes(), 0, "expected eviction under tiny budget");
    }

    #[test]
    fn watch_roots_prune_nested_external_roots() {
        let workspace_root = PathBuf::from("/ws");
        let mut config = WatchConfig::new(workspace_root.clone());
        config.source_roots = vec![PathBuf::from("/ext/src"), PathBuf::from("/ext/src/generated")];

        let roots = compute_watch_roots(&workspace_root, &config);
        assert_eq!(
            roots,
            vec![PathBuf::from("/ext/src"), PathBuf::from("/ws")]
        );
    }

    #[test]
    fn watch_roots_are_deterministic_across_input_order() {
        let workspace_root = PathBuf::from("/ws");

        let mut config_a = WatchConfig::new(workspace_root.clone());
        config_a.source_roots =
            vec![PathBuf::from("/ext/src"), PathBuf::from("/ext/src/generated")];

        let mut config_b = WatchConfig::new(workspace_root.clone());
        config_b.source_roots =
            vec![PathBuf::from("/ext/src/generated"), PathBuf::from("/ext/src")];

        assert_eq!(
            compute_watch_roots(&workspace_root, &config_a),
            compute_watch_roots(&workspace_root, &config_b)
        );
    }

    #[test]
    fn watch_roots_under_workspace_root_are_never_added_explicitly() {
        let workspace_root = PathBuf::from("/ws");
        let workspace_src = workspace_root.join("src");

        let mut config = WatchConfig::new(workspace_root.clone());
        config.source_roots = vec![workspace_src.clone(), PathBuf::from("/ext/src")];

        let roots = compute_watch_roots(&workspace_root, &config);
        assert!(
            roots.iter().all(|root| root != &workspace_src),
            "expected {} to not be watched explicitly (workspace root watch should cover it)",
            workspace_src.display()
        );
    }

    #[test]
    fn project_indexes_are_tracked_and_evictable() {
        let mut indexes = ProjectIndexes::default();

        // Populate enough entries so `estimated_bytes()` is non-zero and large enough to exceed
        // the small test budget.
        for idx in 0..64u32 {
            let sym_name = format!("Symbol{idx}");
            indexes.symbols.insert(
                sym_name.clone(),
                IndexedSymbol {
                    qualified_name: sym_name,
                    kind: IndexSymbolKind::Class,
                    container_name: None,
                    location: SymbolLocation {
                        file: "src/Main.java".to_string(),
                        line: idx,
                        column: 0,
                    },
                    ast_id: idx,
                },
            );
            indexes.references.insert(
                format!("Symbol{idx}"),
                ReferenceLocation {
                    file: "src/Main.java".to_string(),
                    line: idx,
                    column: 1,
                },
            );
            indexes.annotations.insert(
                format!("Annotation{idx}"),
                AnnotationLocation {
                    file: "src/Main.java".to_string(),
                    line: idx,
                    column: 2,
                },
            );
            indexes.inheritance.insert(InheritanceEdge {
                file: "src/Main.java".to_string(),
                subtype: format!("Sub{idx}"),
                supertype: format!("Super{idx}"),
            });
        }

        let estimated_bytes = indexes.estimated_bytes();
        assert!(estimated_bytes > 0, "expected non-zero index size estimate");

        // Give the MemoryManager a smaller budget than our tracked usage so eviction triggers.
        let budget_total = estimated_bytes.saturating_sub(1).max(1);
        let memory = MemoryManager::new(MemoryBudget::from_total(budget_total));

        let engine = WorkspaceEngine::new(WorkspaceEngineConfig {
            workspace_root: PathBuf::new(),
            persistence: PersistenceConfig::from_env(),
            memory: memory.clone(),
        });

        engine.indexes_evictor.replace_indexes(indexes);

        let (_report, components) = memory.report_detailed();
        let component = components
            .iter()
            .find(|c| c.name == "workspace_project_indexes")
            .expect("workspace_project_indexes registered");
        assert!(
            component.bytes > 0,
            "expected workspace_project_indexes tracker > 0"
        );

        memory.enforce();

        let cleared = engine.indexes.lock().unwrap();
        assert!(cleared.symbols.symbols.is_empty());
        assert!(cleared.references.references.is_empty());
        assert!(cleared.annotations.annotations.is_empty());
        assert!(cleared.inheritance.subtypes.is_empty());
        assert!(cleared.inheritance.supertypes.is_empty());

        let (_report, components) = memory.report_detailed();
        let component = components
            .iter()
            .find(|c| c.name == "workspace_project_indexes")
            .expect("workspace_project_indexes registered");
        assert_eq!(component.bytes, 0);
    }

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
    fn open_document_sets_non_tracked_file_path_for_persistence_keys() {
        use nova_db::salsa::HasFilePaths;

        let workspace = crate::Workspace::new_in_memory();
        let engine = workspace.engine_for_tests();

        let dir = tempfile::tempdir().unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let root = dir.path().canonicalize().unwrap();
        engine.set_workspace_root(&root).unwrap();

        // Use a path with a `.` segment so we exercise `normalize_rel_path` via `rel_path_for_workspace`.
        let file_path = root.join("src/./Main.java");
        let vfs_path = VfsPath::local(file_path);
        let file_id = workspace.open_document(vfs_path, "class Main {}".to_string(), 1);

        let expected_rel_path = normalize_rel_path("src/./Main.java");
        engine.query_db.with_snapshot(|snap| {
            assert_eq!(snap.file_rel_path(file_id).as_str(), expected_rel_path.as_str());

            let file_path = snap.file_path(file_id).expect("file_path should be set");
            assert!(!file_path.is_empty());
            assert_eq!(file_path.as_str(), expected_rel_path.as_str());
        });
    }

    #[test]
    fn project_reload_sets_non_tracked_file_path_for_persistence_keys() {
        use nova_db::salsa::HasFilePaths;

        let dir = tempfile::tempdir().unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/Main.java"), "class Main {}".as_bytes()).unwrap();

        let workspace = crate::Workspace::open(&root).unwrap();
        let engine = workspace.engine_for_tests();

        let project = ProjectId::from_raw(0);
        engine.query_db.with_snapshot(|snap| {
            let files = snap.project_files(project);
            assert_eq!(
                files.len(),
                1,
                "expected project reload to discover one Java source file"
            );

            let file_id = files[0];
            let expected_rel_path = normalize_rel_path("src/Main.java");
            let rel_path = snap.file_rel_path(file_id);
            assert_eq!(rel_path.as_str(), expected_rel_path.as_str());

            let file_path = snap.file_path(file_id).expect("file_path should be set");
            assert!(!file_path.is_empty());
            assert_eq!(file_path.as_str(), expected_rel_path.as_str());
            assert!(
                Arc::ptr_eq(&rel_path, &file_path),
                "expected file_path to share the same Arc as file_rel_path"
            );
        });
    }

    #[test]
    fn overlay_document_memory_is_reported_via_memory_manager() {
        let memory = MemoryManager::new(MemoryBudget::from_total(256 * nova_memory::MB));
        let engine = WorkspaceEngine::new(WorkspaceEngineConfig {
            workspace_root: PathBuf::new(),
            persistence: PersistenceConfig {
                mode: PersistenceMode::Disabled,
                cache: CacheConfig::from_env(),
            },
            memory: memory.clone(),
        });

        // `MemoryCategory::Other` contains multiple components (e.g. salsa input tracking),
        // so use the detailed report to validate the overlay tracker specifically.
        fn overlay_bytes(memory: &MemoryManager) -> u64 {
            let (_report, components) = memory.report_detailed();
            components
                .iter()
                .find(|c| c.name == "vfs_documents")
                .map(|c| c.bytes)
                .unwrap_or(0)
        }

        let baseline = overlay_bytes(&memory);

        let dir = tempfile::tempdir().unwrap();
        let a_path = dir.path().join("a.java");
        let b_path = dir.path().join("b.java");
        let a = VfsPath::local(a_path.clone());
        let b = VfsPath::local(b_path.clone());

        engine.open_document(a.clone(), "aaaa".to_string(), 1);
        assert_eq!(overlay_bytes(&memory), baseline + 4);

        engine.open_document(b.clone(), "bb".to_string(), 1);
        assert_eq!(overlay_bytes(&memory), baseline + 6);

        engine
            .apply_changes(&a, 2, &[ContentChange::full("aaaaa")])
            .unwrap();
        assert_eq!(overlay_bytes(&memory), baseline + 7);

        engine.close_document(&b);
        assert_eq!(overlay_bytes(&memory), baseline + 5);

        // Rename handling: renaming an open document onto an already-open destination drops the
        // source document in the overlay, and the tracker should update accordingly.
        let src_path = dir.path().join("src.java");
        let dst_path = dir.path().join("dst.java");
        let src = VfsPath::local(src_path.clone());
        let dst = VfsPath::local(dst_path.clone());

        engine.open_document(src.clone(), "src".to_string(), 1); // 3 bytes
        engine.open_document(dst.clone(), "dstt".to_string(), 1); // 4 bytes
        assert_eq!(overlay_bytes(&memory), baseline + 12);

        engine.apply_filesystem_events(vec![NormalizedEvent::Moved {
            from: src_path,
            to: dst_path,
        }]);

        // Only `a` (5) + `dst` (4) remain in the overlay.
        assert_eq!(overlay_bytes(&memory), baseline + 9);
    }

    #[test]
    fn open_document_sets_file_is_dirty_false_when_text_matches_disk() {
        let dir = tempfile::tempdir().unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        let file_path = root.join("src/Main.java");
        fs::write(&file_path, "class Main {}".as_bytes()).unwrap();

        let memory = MemoryManager::new(MemoryBudget::default_for_system_with_env_overrides());
        let engine = WorkspaceEngine::new(WorkspaceEngineConfig {
            workspace_root: root.clone(),
            persistence: PersistenceConfig {
                mode: PersistenceMode::Disabled,
                cache: CacheConfig::from_env(),
            },
            memory,
        });
        engine.set_workspace_root(&root).unwrap();

        let file_id = engine.open_document(
            VfsPath::local(file_path),
            "class Main {}".to_string(),
            1,
        );
        engine.query_db.with_snapshot(|snap| {
            assert!(
                !snap.file_is_dirty(file_id),
                "expected file to be marked clean when overlay matches disk"
            );
        });
    }

    #[test]
    fn filesystem_events_update_salsa_and_preserve_file_ids_across_moves() {
        let dir = tempfile::tempdir().unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();

        let workspace = crate::Workspace::open(&root).unwrap();
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
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();

        let a = root.join("src/A.java");
        let b = root.join("src/B.java");
        fs::write(&a, "class A {}".as_bytes()).unwrap();
        fs::write(&b, "class B {}".as_bytes()).unwrap();

        let workspace = crate::Workspace::open(&root).unwrap();
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
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();

        let workspace = crate::Workspace::open(&root).unwrap();
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
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        let file_a = root.join("src/A.java");
        fs::write(&file_a, "class A { disk }".as_bytes()).unwrap();

        let workspace = crate::Workspace::open(&root).unwrap();
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
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();

        let file_a = root.join("src/A.java");
        let file_b = root.join("src/B.java");
        fs::write(&file_a, "class A {}".as_bytes()).unwrap();
        fs::write(&file_b, "class B {}".as_bytes()).unwrap();

        let workspace = crate::Workspace::open(&root).unwrap();
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
    fn move_open_document_to_known_destination_keeps_destination_id() {
        let dir = tempfile::tempdir().unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();

        let file_a = root.join("src/A.java");
        let file_b = root.join("src/B.java");
        fs::write(&file_a, "class A { disk }".as_bytes()).unwrap();
        fs::write(&file_b, "class B { disk }".as_bytes()).unwrap();

        let workspace = crate::Workspace::open(&root).unwrap();
        let engine = workspace.engine_for_tests();
        engine.apply_filesystem_events(vec![
            NormalizedEvent::Created(file_a.clone()),
            NormalizedEvent::Created(file_b.clone()),
        ]);

        let vfs_a = VfsPath::local(file_a.clone());
        let vfs_b = VfsPath::local(file_b.clone());
        let id_a = engine.vfs.get_id(&vfs_a).unwrap();
        let id_b = engine.vfs.get_id(&vfs_b).unwrap();

        // Open A (overlay). When the document is moved onto an already-known destination path, the
        // VFS keeps the destination `FileId` and updates the open-doc tracking to match.
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
        assert_eq!(engine.vfs.get_id(&vfs_b), Some(id_b));
        assert_eq!(engine.vfs.path_for_id(id_a), None);
        assert_eq!(
            engine.vfs.read_to_string(&vfs_b).unwrap(),
            "class A { overlay }"
        );
        assert!(!engine.vfs.open_documents().is_open(id_a));
        assert!(engine.vfs.open_documents().is_open(id_b));

        engine.query_db.with_snapshot(|snap| {
            assert!(!snap.file_exists(id_a));
            assert!(snap.file_exists(id_b));
            assert_eq!(snap.file_rel_path(id_b).as_str(), "src/B.java");
            assert_eq!(snap.file_content(id_b).as_str(), "class A { overlay }");
            assert!(!snap.project_files(ProjectId::from_raw(0)).contains(&id_a));
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
    fn project_reload_rebuilds_classpath_index_when_target_release_changes() {
        let dir = tempfile::tempdir().unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let root = dir.path().canonicalize().unwrap();
        let main_dir = root.join("src/main/java/com/example");
        fs::create_dir_all(&main_dir).unwrap();
        fs::write(
            main_dir.join("Main.java"),
            "package com.example; class Main {}".as_bytes(),
        )
        .unwrap();

        // Initial project config uses Java 17.
        let pom = root.join("pom.xml");
        fs::write(
            &pom,
            br#"<project><properties><maven.compiler.target>17</maven.compiler.target></properties></project>"#,
        )
        .unwrap();

        let workspace = crate::Workspace::open(&root).unwrap();
        let engine = workspace.engine_for_tests();
        let project = ProjectId::from_raw(0);

        let before = engine.query_db.with_snapshot(|snap| {
            assert_eq!(snap.project_config(project).java.target.0, 17);
            snap.classpath_index(project)
                .expect("classpath index should be set")
        });

        // Update Java target release; classpath entries are unchanged, but the index must be
        // rebuilt because multi-release JAR selection depends on the target release.
        fs::write(
            &pom,
            br#"<project><properties><maven.compiler.target>8</maven.compiler.target></properties></project>"#,
        )
        .unwrap();
        engine.reload_project_now(&[pom.clone()]).unwrap();

        engine.query_db.with_snapshot(|snap| {
            assert_eq!(snap.project_config(project).java.target.0, 8);
            let after = snap
                .classpath_index(project)
                .expect("classpath index should be set");
            assert!(
                !std::sync::Arc::ptr_eq(&before.0, &after.0),
                "expected classpath index to be rebuilt when target release changes"
            );
        });
    }

    #[test]
    fn project_reload_updates_jpms_modules_for_module_info_changes() {
        let dir = tempfile::tempdir().unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let root = dir.path().canonicalize().unwrap();
        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("Main.java"), "class Main {}".as_bytes()).unwrap();

        let module_info = root.join("module-info.java");
        fs::write(
            &module_info,
            "module com.example.one { }".as_bytes(),
        )
        .unwrap();

        let workspace = crate::Workspace::open(&root).unwrap();
        let engine = workspace.engine_for_tests();
        let project = ProjectId::from_raw(0);

        let old_name = engine.query_db.with_snapshot(|snap| {
            let config = snap.project_config(project);
            assert_eq!(config.jpms_modules.len(), 1);
            assert_eq!(config.jpms_modules[0].name.as_str(), "com.example.one");
            config.jpms_modules[0].name.clone()
        });

        engine.query_db.with_snapshot(|snap| {
            let config = snap.project_config(project);
            let workspace = config.jpms_workspace.as_ref().expect("jpms workspace present");
            assert!(workspace.graph.get(&old_name).is_some());
        });

        fs::write(&module_info, "module com.example.two { }".as_bytes()).unwrap();
        engine.reload_project_now(&[module_info.clone()]).unwrap();

        engine.query_db.with_snapshot(|snap| {
            let config = snap.project_config(project);
            assert_eq!(config.jpms_modules.len(), 1);
            assert_eq!(config.jpms_modules[0].name.as_str(), "com.example.two");

            let workspace = config.jpms_workspace.as_ref().expect("jpms workspace present");
            assert!(workspace.graph.get(&old_name).is_none());
            assert!(workspace.graph.get(&config.jpms_modules[0].name).is_some());
        });
    }

    #[test]
    fn project_reload_discovers_jdk_index_from_nova_config() {
        let dir = tempfile::tempdir().unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();

        // Ensure at least one file is indexed so project reload runs end-to-end.
        fs::write(root.join("src/Main.java"), "class Main {}".as_bytes()).unwrap();

        let fake_jdk_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../nova-jdk/testdata/fake-jdk")
            .canonicalize()
            .unwrap();
        let fake_jdk_root = fake_jdk_root.to_string_lossy().replace('\\', "\\\\");
        fs::write(
            root.join("nova.toml"),
            format!("[jdk]\nhome = \"{fake_jdk_root}\"\n"),
        )
        .unwrap();

        let workspace = crate::Workspace::open(&root).unwrap();
        let engine = workspace.engine_for_tests();
        let project = ProjectId::from_raw(0);

        engine.query_db.with_snapshot(|snap| {
            let index = snap.jdk_index(project);
            assert_eq!(index.info().backing, nova_jdk::JdkIndexBacking::Jmods);
            assert!(index
                .lookup_type("java.lang.String")
                .ok()
                .flatten()
                .is_some());
        });
    }

    #[test]
    fn project_reload_resolves_relative_jdk_home_to_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();

        fs::write(root.join("src/Main.java"), "class Main {}".as_bytes()).unwrap();

        let testdata_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../nova-jdk/testdata/fake-jdk")
            .canonicalize()
            .unwrap();
        let dest_jdk_root = root.join("fake-jdk");
        fs::create_dir_all(dest_jdk_root.join("jmods")).unwrap();
        fs::copy(
            testdata_root.join("jmods").join("java.base.jmod"),
            dest_jdk_root.join("jmods").join("java.base.jmod"),
        )
        .unwrap();

        fs::write(root.join("nova.toml"), "[jdk]\nhome = \"fake-jdk\"\n").unwrap();

        let workspace = crate::Workspace::open(&root).unwrap();
        let engine = workspace.engine_for_tests();
        let project = ProjectId::from_raw(0);

        engine.query_db.with_snapshot(|snap| {
            let index = snap.jdk_index(project);
            assert_eq!(index.info().backing, nova_jdk::JdkIndexBacking::Jmods);
            assert!(index
                .lookup_type("java.lang.String")
                .ok()
                .flatten()
                .is_some());
        });
    }

    #[test]
    fn project_reload_updates_watcher_config_for_new_generated_roots() {
        let dir = tempfile::tempdir().unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let root = dir.path().canonicalize().unwrap();

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

        let workspace = crate::Workspace::open(&root).unwrap();
        let engine = workspace.engine_for_tests();

        let generated_root = root.join("custom-generated");
        let generated_file = generated_root.join("Gen.java");
        let event = NormalizedEvent::Created(generated_file.clone());

        let stale_config = engine
            .watch_config
            .read()
            .expect("workspace watch config lock poisoned")
            .clone();
        assert!(
            !stale_config.source_roots.is_empty() || !stale_config.generated_source_roots.is_empty(),
            "expected Maven workspace to have configured roots"
        );
        assert!(
            !stale_config.generated_source_roots.contains(&generated_root),
            "test expects custom generated root to be absent before reload"
        );
        assert_eq!(categorize_event(&stale_config, &event), None);

        // Simulate nova-apt writing a snapshot of generated roots, then reload the project so the
        // newly discovered roots are incorporated into the watcher config.
        let snapshot_dir = root.join(".nova").join("apt-cache");
        fs::create_dir_all(&snapshot_dir).unwrap();
        let snapshot = serde_json::json!({
            "schema_version": 1,
            "modules": [{
                "module_root": root.to_string_lossy(),
                "roots": [{
                    "kind": "main",
                    "path": generated_root.to_string_lossy(),
                }]
            }]
        });
        let snapshot_path = snapshot_dir.join("generated-roots.json");
        fs::write(&snapshot_path, serde_json::to_string_pretty(&snapshot).unwrap()).unwrap();

        engine.reload_project_now(&[snapshot_path]).unwrap();

        let current_config = engine
            .watch_config
            .read()
            .expect("workspace watch config lock poisoned")
            .clone();
        assert!(
            current_config.generated_source_roots.contains(&generated_root),
            "expected watcher config to include newly generated root"
        );
        assert_eq!(
            categorize_event(&current_config, &event),
            Some(ChangeCategory::Source)
        );

        // Demonstrate why the watcher must consult the *current* WatchConfig: the original snapshot
        // would continue to drop events for the newly configured root.
        assert_eq!(categorize_event(&stale_config, &event), None);

        fs::create_dir_all(&generated_root).unwrap();
        fs::write(&generated_file, "class Gen {}".as_bytes()).unwrap();
        engine.apply_filesystem_events(vec![NormalizedEvent::Created(generated_file.clone())]);

        let file_id = engine
            .vfs
            .get_id(&VfsPath::local(generated_file.clone()))
            .expect("generated file id allocated");
        engine.query_db.with_snapshot(|snap| {
            let project = ProjectId::from_raw(0);
            assert!(snap.file_exists(file_id));
            assert!(snap.project_files(project).contains(&file_id));
        });
    }

    #[test]
    fn module_info_changes_trigger_project_reload_and_reuse_classpath_index() {
        let dir = tempfile::tempdir().unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let root = dir.path().canonicalize().unwrap();
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

        let workspace = crate::Workspace::open(&root).unwrap();
        let engine = workspace.engine_for_tests();
        let project = ProjectId::from_raw(0);

        let classpath_before = engine.query_db.with_snapshot(|snap| {
            let config = snap.project_config(project);
            assert_eq!(config.build_system, BuildSystem::Maven);
            assert!(config.jpms_modules.is_empty());
            snap.classpath_index(project)
                .expect("classpath index built for Maven output dirs")
                .0
        });

        // Creating `module-info.java` should enqueue a project reload (even though it's a `.java`
        // source file) so JPMS metadata stays fresh.
        let module_info = root.join("src/main/java/module-info.java");
        fs::write(&module_info, "module com.example { }".as_bytes()).unwrap();
        workspace.apply_filesystem_events(vec![NormalizedEvent::Created(module_info.clone())]);

        assert!(
            engine.project_reload_debouncer.cancel(&"workspace-reload"),
            "expected module-info change to enqueue a project reload"
        );

        let mut changed_files = {
            let mut state = engine
                .project_state
                .lock()
                .expect("workspace project state mutex poisoned");
            state.pending_build_changes.drain().collect::<Vec<_>>()
        };
        changed_files.sort();
        assert_eq!(changed_files, vec![module_info.clone()]);

        engine.reload_project_now(&changed_files).unwrap();

        let classpath_after = engine.query_db.with_snapshot(|snap| {
            let config = snap.project_config(project);
            assert_eq!(config.jpms_modules.len(), 1);
            assert_eq!(config.jpms_modules[0].name.as_str(), "com.example");
            assert!(config.jpms_workspace.is_some());
            snap.classpath_index(project)
                .expect("classpath index remains present")
                .0
        });

        assert!(
            Arc::ptr_eq(&classpath_before, &classpath_after),
            "expected classpath index to be reused when classpath/module-path are unchanged"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn manual_watcher_propagates_disk_edits_into_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().join("project");
        fs::create_dir_all(project_root.join("src")).unwrap();
        fs::write(project_root.join("src/Main.java"), "class Main {}".as_bytes()).unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let project_root = project_root.canonicalize().unwrap();
        let file = project_root.join("src/Main.java");

        let workspace = crate::Workspace::open(&project_root).unwrap();
        let rx = workspace.subscribe();
        let engine = workspace.engine.clone();

        let vfs_path = VfsPath::local(file.clone());
        let file_id = engine.vfs.get_id(&vfs_path).expect("file id allocated");

        let manual = ManualFileWatcher::new();
        let handle: ManualFileWatcherHandle = manual.handle();
        let _watcher = engine
            .start_watching_with_watcher_factory(move || Ok(manual))
            .unwrap();

        let updated = "class Main { int x; }";
        fs::write(&file, updated.as_bytes()).unwrap();
        handle
            .push(WatchEvent::Changes {
                changes: vec![FileChange::Modified { path: vfs_path.clone() }],
            })
            .unwrap();

        let vfs_path_for_wait = vfs_path.clone();
        timeout(Duration::from_secs(5), async move {
            loop {
                let event = rx
                    .recv()
                    .await
                    .expect("workspace event channel unexpectedly closed");
                match event {
                    WorkspaceEvent::FileChanged { file } if file == vfs_path_for_wait => break,
                    WorkspaceEvent::Status(WorkspaceStatus::IndexingError(err)) => {
                        panic!("workspace watcher error: {err}");
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for file watcher to update workspace");

        assert_eq!(
            engine.vfs.get_id(&vfs_path),
            Some(file_id),
            "FileId should remain stable across disk edits"
        );
        engine.query_db.with_snapshot(|snap| {
            assert_eq!(snap.file_content(file_id).as_str(), updated);
        });
    }
}
