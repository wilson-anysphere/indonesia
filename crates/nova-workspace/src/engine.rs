use std::collections::{HashMap, HashSet};
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
use nova_core::{TextEdit, TextRange, TextSize};
use nova_db::persistence::PersistenceConfig;
use nova_db::{salsa, Database, NovaIndexing, NovaInputs, NovaSyntax, ProjectId, SourceRootId};
use nova_ide::{DebugConfiguration, Project};
use nova_index::ProjectIndexes;
use nova_memory::{
    BackgroundIndexingMode, DegradedSettings, EvictionRequest, EvictionResult, MemoryCategory,
    MemoryEvictor, MemoryManager, MemoryPressure, MemoryRegistration, MemoryReport,
};
use nova_project::{
    BuildSystem, ClasspathEntry, ClasspathEntryKind, JavaConfig, LoadOptions, OutputDir,
    OutputDirKind, ProjectConfig, ProjectError, SourceRoot, SourceRootKind, SourceRootOrigin,
};
#[cfg(test)]
use nova_scheduler::SchedulerConfig;
use nova_scheduler::{Cancelled, Debouncer, KeyedDebouncer, PoolKind, Scheduler};
use nova_syntax::{JavaParseStore, SyntaxTreeStore};
use nova_types::{CompletionItem, Diagnostic as NovaDiagnostic, Span};
use nova_vfs::{
    ChangeEvent, ContentChange, DocumentError, FileChange, FileId, FileSystem, FileWatcher,
    LocalFs, NotifyFileWatcher, OpenDocuments, Vfs, VfsPath, WatchEvent, WatchMode,
};
use walkdir::WalkDir;

use nova_build::{BuildManager, CommandRunner};

use crate::snapshot::WorkspaceDbView;
use crate::watch::{categorize_event, ChangeCategory, NormalizedEvent, WatchConfig};
use crate::watch_roots::{WatchRootError, WatchRootManager};

fn compute_watch_roots(
    workspace_root: &Path,
    watch_config: &WatchConfig,
) -> Vec<(PathBuf, WatchMode)> {
    let mut roots: Vec<(PathBuf, WatchMode)> = Vec::new();
    roots.push((workspace_root.to_path_buf(), WatchMode::Recursive));

    // Explicit external roots are watched recursively. Roots under the workspace root are already
    // covered by the workspace recursive watch.
    for root in watch_config
        .source_roots
        .iter()
        .chain(watch_config.generated_source_roots.iter())
        .chain(watch_config.module_roots.iter())
    {
        if root.starts_with(workspace_root) {
            continue;
        }
        roots.push((root.clone(), WatchMode::Recursive));
    }

    // Watch the discovered config file when it lives outside the workspace root. Use a
    // non-recursive watch so we don't accidentally watch huge trees like `$HOME`.
    if let Some(config_path) = watch_config.nova_config_path.as_ref() {
        if !config_path.starts_with(workspace_root) {
            roots.push((config_path.clone(), WatchMode::NonRecursive));
        }
    }

    // Deterministic ordering.
    roots.sort_by(|(a, _), (b, _)| a.cmp(b));

    // Deduplicate paths, preferring Recursive mode.
    roots.dedup_by(|(a, mode_a), (b, mode_b)| {
        if a != b {
            return false;
        }
        if *mode_a == WatchMode::Recursive || *mode_b == WatchMode::Recursive {
            *mode_a = WatchMode::Recursive;
        }
        true
    });

    // Prune paths covered by a prior recursive watch.
    let mut pruned: Vec<(PathBuf, WatchMode)> = Vec::new();
    'outer: for (root, mode) in roots {
        for (parent, parent_mode) in &pruned {
            if *parent_mode == WatchMode::Recursive && root.starts_with(parent) {
                continue 'outer;
            }
        }
        pruned.push((root, mode));
    }

    pruned
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
    /// Optional command runner override for build tool integration (Maven/Gradle).
    ///
    /// When unset, Nova uses a default runner that executes external commands. In
    /// `cfg(test)` builds we default to a runner that returns `NotFound` to avoid
    /// invoking real build tools during unit tests.
    pub build_runner: Option<Arc<dyn CommandRunner>>,
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

#[derive(Debug, Default)]
struct ClosedFileTextState {
    bytes_by_file: HashMap<FileId, u64>,
    evicted: HashSet<FileId>,
}

/// Workspace-owned accounting + eviction for Salsa `file_content` inputs of *closed* files.
///
/// Open documents must remain pinned and are excluded from tracking/eviction.
struct ClosedFileTextStore {
    name: String,
    query_db: salsa::Database,
    open_docs: Arc<OpenDocuments>,
    state: Mutex<ClosedFileTextState>,
    tracker: OnceLock<nova_memory::MemoryTracker>,
    registration: OnceLock<nova_memory::MemoryRegistration>,
}

impl ClosedFileTextStore {
    fn new(
        manager: &MemoryManager,
        query_db: salsa::Database,
        open_docs: Arc<OpenDocuments>,
    ) -> Arc<Self> {
        let store = Arc::new(Self {
            name: "workspace_closed_file_texts".to_string(),
            query_db,
            open_docs,
            state: Mutex::new(ClosedFileTextState::default()),
            tracker: OnceLock::new(),
            registration: OnceLock::new(),
        });

        let registration = manager.register_evictor(
            store.name.clone(),
            MemoryCategory::QueryCache,
            store.clone(),
        );
        store
            .tracker
            .set(registration.tracker())
            .expect("tracker only set once");
        store
            .registration
            .set(registration)
            .expect("registration only set once");

        store
    }

    fn is_evicted(&self, file_id: FileId) -> bool {
        self.state
            .lock()
            .expect("workspace closed file text store mutex poisoned")
            .evicted
            .contains(&file_id)
    }

    fn on_open_document(&self, file_id: FileId) {
        let mut state = self
            .state
            .lock()
            .expect("workspace closed file text store mutex poisoned");
        let removed = state.bytes_by_file.remove(&file_id).unwrap_or(0);
        state.evicted.remove(&file_id);

        if removed != 0 {
            if let Some(tracker) = self.tracker.get() {
                tracker.add_bytes(-(removed as i64));
            }
        }
    }

    fn track_closed_file_content(&self, file_id: FileId, text: &Arc<String>) {
        if self.open_docs.is_open(file_id) {
            self.on_open_document(file_id);
            return;
        }

        let new_bytes = text.len() as u64;
        let mut state = self
            .state
            .lock()
            .expect("workspace closed file text store mutex poisoned");

        state.evicted.remove(&file_id);
        let removed = state.bytes_by_file.remove(&file_id).unwrap_or(0);
        if removed != 0 {
            if let Some(tracker) = self.tracker.get() {
                tracker.add_bytes(-(removed as i64));
            }
        }

        if new_bytes != 0 {
            state.bytes_by_file.insert(file_id, new_bytes);
            if let Some(tracker) = self.tracker.get() {
                tracker.add_bytes(new_bytes as i64);
            }
        }
    }

    fn clear(&self, file_id: FileId) {
        let mut state = self
            .state
            .lock()
            .expect("workspace closed file text store mutex poisoned");
        let removed = state.bytes_by_file.remove(&file_id).unwrap_or(0);
        state.evicted.remove(&file_id);
        if removed != 0 {
            if let Some(tracker) = self.tracker.get() {
                tracker.add_bytes(-(removed as i64));
            }
        }
    }

    fn restore_if_evicted(&self, vfs: &Vfs<LocalFs>, file_id: FileId) {
        if self.open_docs.is_open(file_id) {
            return;
        }

        let should_restore = {
            let state = self
                .state
                .lock()
                .expect("workspace closed file text store mutex poisoned");
            state.evicted.contains(&file_id)
        };

        if !should_restore {
            return;
        }

        let Some(path) = vfs.path_for_id(file_id) else {
            return;
        };

        // Avoid clobbering overlay documents if the file was opened while we were loading.
        if self.open_docs.is_open(file_id) {
            return;
        }

        let Ok(text) = vfs.read_to_string(&path) else {
            return;
        };

        if self.open_docs.is_open(file_id) {
            return;
        }

        let text_arc = Arc::new(text);
        self.query_db.set_file_exists(file_id, vfs.exists(&path));
        self.query_db
            .set_file_content(file_id, Arc::clone(&text_arc));
        self.query_db.set_file_is_dirty(file_id, false);
        self.track_closed_file_content(file_id, &text_arc);
    }
}

impl MemoryEvictor for ClosedFileTextStore {
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
            .unwrap_or(MemoryCategory::QueryCache)
    }

    fn evict(&self, request: EvictionRequest) -> EvictionResult {
        let before = self.tracker.get().map(|t| t.bytes()).unwrap_or(0);
        if before <= request.target_bytes {
            return EvictionResult {
                before_bytes: before,
                after_bytes: before,
            };
        }

        let open_files = self.open_docs.snapshot();
        let mut candidates: Vec<(FileId, u64)> = {
            let state = self
                .state
                .lock()
                .expect("workspace closed file text store mutex poisoned");
            state
                .bytes_by_file
                .iter()
                .filter(|(file_id, _)| !open_files.contains(file_id))
                .map(|(file_id, bytes)| (*file_id, *bytes))
                .collect()
        };
        candidates.sort_by(|a, b| b.1.cmp(&a.1));

        for (file_id, _bytes) in candidates {
            let current = self.tracker.get().map(|t| t.bytes()).unwrap_or(0);
            if current <= request.target_bytes {
                break;
            }

            if self.open_docs.is_open(file_id) {
                continue;
            }

            // Replace with a shared empty `Arc<String>` to drop the strong reference to the large
            // allocation while keeping Salsa inputs well-formed.
            self.query_db
                .set_file_content(file_id, empty_file_content());
            // Mark dirty so persistence logic doesn't overwrite on-disk caches with the evicted
            // placeholder contents.
            self.query_db.set_file_is_dirty(file_id, true);

            let mut state = self
                .state
                .lock()
                .expect("workspace closed file text store mutex poisoned");
            let removed = state.bytes_by_file.remove(&file_id).unwrap_or(0);
            state.evicted.insert(file_id);
            drop(state);

            if removed != 0 {
                if let Some(tracker) = self.tracker.get() {
                    tracker.add_bytes(-(removed as i64));
                }
            }
        }

        let after = self.tracker.get().map(|t| t.bytes()).unwrap_or(0);
        EvictionResult {
            before_bytes: before,
            after_bytes: after,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct WatchDebounceConfig {
    pub source: Duration,
    pub build: Duration,
}

impl Default for WatchDebounceConfig {
    fn default() -> Self {
        Self {
            source: Duration::from_millis(200),
            build: Duration::from_millis(200),
        }
    }
}

#[cfg(test)]
impl WatchDebounceConfig {
    pub(crate) const ZERO: Self = Self {
        source: Duration::ZERO,
        build: Duration::ZERO,
    };
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

const BATCH_QUEUE_CAPACITY: usize = 256;
const WATCH_COMMAND_QUEUE_CAPACITY: usize = 1;
const SUBSCRIBER_QUEUE_CAPACITY: usize = 1024;
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
    closed_file_texts: Arc<ClosedFileTextStore>,
    indexes: Arc<Mutex<ProjectIndexes>>,
    indexes_evictor: Arc<WorkspaceProjectIndexesEvictor>,

    build_runner: Arc<dyn CommandRunner>,

    config: RwLock<EffectiveConfig>,
    scheduler: Scheduler,
    memory: MemoryManager,
    last_memory_enforce: Mutex<Option<Instant>>,
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
    jdk_fingerprint: Option<Fingerprint>,
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
            jdk_fingerprint: None,
            pending_build_changes: HashSet::new(),
            last_reload_started_at: None,
        }
    }
}

fn workspace_scheduler() -> Scheduler {
    static SCHEDULER: OnceLock<Scheduler> = OnceLock::new();
    SCHEDULER
        .get_or_init(|| {
            // Unit tests create many short-lived workspaces. Keep the scheduler conservative so we
            // don't exhaust OS thread limits when the test harness runs with high parallelism.
            #[cfg(test)]
            {
                Scheduler::new(SchedulerConfig {
                    compute_threads: 1,
                    background_threads: 1,
                    io_threads: 1,
                    progress_channel_capacity: 1024,
                })
            }
            #[cfg(not(test))]
            {
                Scheduler::default()
            }
        })
        .clone()
}

fn empty_file_content() -> Arc<String> {
    static EMPTY: OnceLock<Arc<String>> = OnceLock::new();
    EMPTY.get_or_init(|| Arc::new(String::new())).clone()
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

fn default_build_runner() -> Arc<dyn CommandRunner> {
    #[cfg(test)]
    {
        Arc::new(DenyBuildRunner)
    }

    #[cfg(not(test))]
    {
        Arc::new(nova_build::DefaultCommandRunner::default())
    }
}

/// Prevent unit tests from invoking real build tools (Maven/Gradle) from the host environment.
///
/// Tests that need build-tool output should pass an explicit `build_runner` via
/// `WorkspaceEngineConfig`.
#[cfg(test)]
#[derive(Debug)]
struct DenyBuildRunner;

#[cfg(test)]
impl CommandRunner for DenyBuildRunner {
    fn run(
        &self,
        _cwd: &Path,
        _program: &Path,
        _args: &[String],
    ) -> std::io::Result<nova_build::CommandOutput> {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "external build tool invocations are disabled in nova-workspace unit tests",
        ))
    }
}

impl WorkspaceEngine {
    pub fn new(config: WorkspaceEngineConfig) -> Self {
        let scheduler = workspace_scheduler();
        let WorkspaceEngineConfig {
            workspace_root,
            persistence,
            memory,
            build_runner,
        } = config;
        let build_runner = build_runner.unwrap_or_else(default_build_runner);

        let vfs = Vfs::new(LocalFs::new());
        let open_docs = vfs.open_documents();

        let syntax_trees = SyntaxTreeStore::new(&memory, open_docs.clone());

        let query_db = salsa::Database::new_with_persistence(&workspace_root, persistence);
        query_db.register_salsa_memo_evictor(&memory);
        query_db.register_salsa_cancellation_on_memory_pressure(&memory);
        query_db.attach_item_tree_store(&memory, open_docs.clone());
        query_db.set_syntax_tree_store(syntax_trees);

        // Pin full-fidelity Java parse trees for open documents across Salsa memo eviction.
        let java_parse_store = JavaParseStore::new(&memory, open_docs.clone());
        query_db.set_java_parse_store(Some(java_parse_store));

        let closed_file_texts =
            ClosedFileTextStore::new(&memory, query_db.clone(), open_docs.clone());

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
            closed_file_texts,
            indexes,
            indexes_evictor,
            build_runner,
            config: RwLock::new(EffectiveConfig::default()),
            scheduler,
            memory,
            last_memory_enforce: Mutex::new(None),
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

    /// Subscribe to workspace events.
    ///
    /// This channel is bounded to avoid unbounded memory growth; if a subscriber does not keep up,
    /// events may be dropped.
    pub fn subscribe(&self) -> Receiver<WorkspaceEvent> {
        let (tx, rx) = async_channel::bounded(SUBSCRIBER_QUEUE_CAPACITY);
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
            state.jdk_fingerprint = None;
            state.pending_build_changes.clear();
            state.last_reload_started_at = None;
        }

        // Load initial project state + file list.
        self.reload_project_now(&[])?;
        Ok(())
    }

    pub fn start_watching(self: &Arc<Self>) -> Result<WatcherHandle> {
        self.start_watching_with_watcher_factory(
            NotifyFileWatcher::new,
            WatchDebounceConfig::default(),
        )
    }

    #[cfg(test)]
    pub(crate) fn start_watching_with_watcher(
        self: &Arc<Self>,
        watcher: Box<dyn FileWatcher>,
        debounce: WatchDebounceConfig,
    ) -> Result<WatcherHandle> {
        self.start_watching_with_watcher_factory(move || Ok(watcher), debounce)
    }

    fn start_watching_with_watcher_factory<W, F>(
        self: &Arc<Self>,
        watcher_factory: F,
        debounce: WatchDebounceConfig,
    ) -> Result<WatcherHandle>
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
            match VfsPath::local(root) {
                VfsPath::Local(path) => path,
                _ => unreachable!("VfsPath::local produced a non-local path"),
            }
        };

        let watch_config = Arc::clone(&self.watch_config);
        let initial_config = watch_config
            .read()
            .expect("workspace watch config lock poisoned")
            .clone();

        let engine = Arc::clone(self);
        let (batch_tx, batch_rx) = channel::bounded::<WatcherMessage>(BATCH_QUEUE_CAPACITY);

        let (watcher_stop_tx, watcher_stop_rx) = channel::bounded::<()>(0);
        let (command_tx, command_rx) =
            channel::bounded::<WatchCommand>(WATCH_COMMAND_QUEUE_CAPACITY);

        {
            *self
                .watcher_command_store
                .lock()
                .expect("workspace watcher command store mutex poisoned") =
                Some(command_tx.clone());
        }

        let subscribers = Arc::clone(&self.subscribers);
        let watcher_thread = thread::Builder::new()
            .name("workspace-watcher".to_string())
            .spawn(move || {
            let mut debouncer = Debouncer::new([
                (ChangeCategory::Source, debounce.source),
                (ChangeCategory::Build, debounce.build),
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

            let desired_roots = |workspace_root: &Path,
                                 config: &WatchConfig|
             -> std::collections::HashMap<PathBuf, WatchMode> {
                compute_watch_roots(workspace_root, config).into_iter().collect()
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
                                let mut saw_directory_event = false;
                                {
                                    let config = watch_config
                                        .read()
                                        .expect("workspace watch config lock poisoned");
                                    for change in changes {
                                        // NOTE: Directory-level watcher events cannot be safely
                                        // mapped into per-file operations in the VFS. Falling back
                                        // to a full rescan keeps the workspace consistent.
                                        let is_heuristic_directory_change_for_missing_path =
                                            |local: &Path| {
                                                // Safety net when `fs::metadata` fails (e.g. the
                                                // path was deleted before we observed it). We only
                                                // apply this to extension-less paths outside
                                                // ignored directories to reduce the risk of
                                                // triggering full rescans for normal file edits.
                                                let in_ignored_dir = local.components().any(|c| {
                                                    c.as_os_str() == std::ffi::OsStr::new(".git")
                                                        || c.as_os_str()
                                                            == std::ffi::OsStr::new(".gradle")
                                                        || c.as_os_str()
                                                            == std::ffi::OsStr::new("build")
                                                        || c.as_os_str()
                                                            == std::ffi::OsStr::new("target")
                                                        || c.as_os_str()
                                                            == std::ffi::OsStr::new(".nova")
                                                        || c.as_os_str()
                                                            == std::ffi::OsStr::new(".idea")
                                                });

                                                local.extension().is_none() && !in_ignored_dir
                                            };
                                        let is_directory_change = match &change {
                                            FileChange::Created { path }
                                            | FileChange::Modified { path } => path
                                                .as_local_path()
                                                .and_then(|p| fs::metadata(p).ok())
                                                .is_some_and(|meta| meta.is_dir()),
                                            FileChange::Deleted { path } => {
                                                match path.as_local_path() {
                                                    Some(local) => match fs::metadata(local) {
                                                        Ok(meta) => meta.is_dir(),
                                                        Err(err)
                                                            if err.kind()
                                                                == std::io::ErrorKind::NotFound =>
                                                        {
                                                            // Directory deletes are often observed
                                                            // *after* the directory is removed, so
                                                            // metadata fails. As a safety net,
                                                            // treat extension-less paths outside
                                                            // ignored directories as potential
                                                            // directory-level operations and fall
                                                            // back to a rescan.
                                                            is_heuristic_directory_change_for_missing_path(local)
                                                        }
                                                        Err(_) => false,
                                                    },
                                                    None => false,
                                                }
                                            }
                                            FileChange::Moved { from, to } => {
                                                let from_local = from.as_local_path();
                                                let to_local = to.as_local_path();
                                                let from_meta = from_local.map(fs::metadata);
                                                let to_meta = to_local.map(fs::metadata);

                                                let from_is_dir = matches!(
                                                    from_meta.as_ref(),
                                                    Some(Ok(meta)) if meta.is_dir()
                                                );
                                                let to_is_dir = matches!(
                                                    to_meta.as_ref(),
                                                    Some(Ok(meta)) if meta.is_dir()
                                                );
                                                if from_is_dir || to_is_dir {
                                                    true
                                                } else {
                                                    // If we can't stat either path because both are
                                                    // already gone, treat it as a directory-level
                                                    // operation when the paths look like directories.
                                                    // This prevents silently ignoring directory moves
                                                    // that are immediately followed by deletion (and
                                                    // only surface as a move event).
                                                    let from_missing = matches!(
                                                        from_meta.as_ref(),
                                                        Some(Err(err))
                                                            if err.kind() == std::io::ErrorKind::NotFound
                                                    );
                                                    let to_missing = matches!(
                                                        to_meta.as_ref(),
                                                        Some(Err(err))
                                                            if err.kind() == std::io::ErrorKind::NotFound
                                                    );
                                                    from_missing
                                                        && to_missing
                                                        && from_local.is_some_and(
                                                            is_heuristic_directory_change_for_missing_path,
                                                        )
                                                        && to_local.is_some_and(
                                                            is_heuristic_directory_change_for_missing_path,
                                                        )
                                                }
                                            }
                                        };

                                        if is_directory_change {
                                            saw_directory_event = true;
                                            break;
                                        }

                                        let Some(norm) =
                                            NormalizedEvent::from_file_change(&change)
                                        else {
                                            continue;
                                        };
                                        if let Some(cat) = categorize_event(&config, &norm) {
                                            debouncer.push(&cat, norm, now);
                                        }
                                    }
                                }

                                if saw_directory_event {
                                    rescan_pending = true;
                                    // Drop any pending debounced batches; we will reconcile via a
                                    // full project reload instead.
                                    debouncer = Debouncer::new([
                                        (ChangeCategory::Source, debounce.source),
                                        (ChangeCategory::Build, debounce.build),
                                    ]);
                                    continue;
                                }

                                for (cat, events) in debouncer.flush_due(now) {
                                    if let Err(err) = batch_tx.try_send(WatcherMessage::Batch(cat, events)) {
                                        if matches!(err, channel::TrySendError::Full(_)) {
                                            rescan_pending = true;
                                            debouncer = Debouncer::new([
                                                (ChangeCategory::Source, debounce.source),
                                                (ChangeCategory::Build, debounce.build),
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
                                    (ChangeCategory::Source, debounce.source),
                                    (ChangeCategory::Build, debounce.build),
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
                                        (ChangeCategory::Source, debounce.source),
                                        (ChangeCategory::Build, debounce.build),
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
        })
            .map_err(|err| {
                // If watcher creation fails, make sure the workspace doesn't think a watcher is
                // running (the `WatcherHandle` won't be returned so `Drop` can't clear the store).
                *self
                    .watcher_command_store
                    .lock()
                    .expect("workspace watcher command store mutex poisoned") = None;
                err
            })
            .context("failed to spawn workspace watcher thread")?;

        let (driver_stop_tx, driver_stop_rx) = channel::bounded::<()>(0);
        let driver_thread = match thread::Builder::new()
            .name("workspace-watcher-driver".to_string())
            .spawn(move || loop {
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
            }) {
            Ok(thread) => thread,
            Err(err) => {
                // Clean up the already-running watcher thread since we won't be returning a handle.
                let _ = watcher_stop_tx.send(());
                let _ = watcher_thread.join();
                *self
                    .watcher_command_store
                    .lock()
                    .expect("workspace watcher command store mutex poisoned") = None;
                return Err(err).context("failed to spawn workspace watcher driver thread");
            }
        };

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

        // Normalize incoming watcher paths so:
        // - drive letter case on Windows (`c:` vs `C:`) doesn't affect prefix checks
        // - dot segments (`a/../b`) don't prevent directory-event expansion
        //
        // This is purely lexical normalization via `VfsPath::local` (does not resolve symlinks).
        let normalize_local_path = |path: PathBuf| -> PathBuf {
            match VfsPath::local(path) {
                VfsPath::Local(path) => path,
                // `VfsPath::local` always returns the local variant.
                _ => unreachable!("VfsPath::local produced a non-local path"),
            }
        };

        // Coalesce noisy watcher streams by processing each path at most once per batch.
        //
        // Note: watcher backends can emit directory-level events (folder move/delete) without
        // emitting per-file events for the contained files. We expand those directory events into
        // file-level operations here to preserve stable `FileId` mappings and open-document
        // overlays.
        let mut move_events: Vec<(PathBuf, PathBuf)> = Vec::new();
        let mut other_paths: HashSet<PathBuf> = HashSet::new();
        let mut module_info_changes: HashSet<PathBuf> = HashSet::new();

        // Helper: return the set of *known* local file paths that are under `dir`.
        // This must not allocate new `FileId`s.
        let known_files_under_dir = |dir: &Path| -> Vec<PathBuf> {
            let dir = normalize_local_path(dir.to_path_buf());
            let mut files = Vec::new();
            for file_id in self.vfs.all_file_ids() {
                let Some(vfs_path) = self.vfs.path_for_id(file_id) else {
                    continue;
                };
                let Some(local) = vfs_path.as_local_path() else {
                    continue;
                };
                if local == dir.as_path() {
                    // Defensive: skip ids accidentally allocated for directories.
                    continue;
                }
                if local.starts_with(&dir) {
                    files.push(local.to_path_buf());
                }
            }
            files.sort();
            files.dedup();
            files
        };

        for event in events {
            match event {
                NormalizedEvent::Moved { from, to } => {
                    let from = normalize_local_path(from);
                    let to = normalize_local_path(to);

                    if is_module_info_java(&from) {
                        module_info_changes.insert(from.clone());
                    }
                    if is_module_info_java(&to) {
                        module_info_changes.insert(to.clone());
                    }

                    // Directory moves can arrive as a single watcher event. Expand to per-file moves
                    // using the currently-known VFS paths under `from`.
                    let from_is_dir = fs::metadata(&from).map(|m| m.is_dir()).unwrap_or(false);
                    let to_is_dir = fs::metadata(&to).map(|m| m.is_dir()).unwrap_or(false);
                    let from_vfs = VfsPath::local(from.clone());
                    let to_vfs = VfsPath::local(to.clone());
                    let from_known_as_file = self.vfs.get_id(&from_vfs).is_some();
                    let to_known_as_file = self.vfs.get_id(&to_vfs).is_some();

                    let dir_move_files = if from_is_dir || to_is_dir {
                        known_files_under_dir(&from)
                    } else if !from_known_as_file {
                        // The directory might already be gone by the time we observe the event. Fall
                        // back to checking whether we have any known file paths nested under `from`.
                        known_files_under_dir(&from)
                    } else {
                        Vec::new()
                    };

                    if !dir_move_files.is_empty() {
                        for from_file in dir_move_files {
                            let rel = match from_file.strip_prefix(&from) {
                                Ok(rel) => rel.to_path_buf(),
                                Err(_) => continue,
                            };
                            let to_file = to.join(rel);
                            if is_module_info_java(&from_file) {
                                module_info_changes.insert(from_file.clone());
                            }
                            if is_module_info_java(&to_file) {
                                module_info_changes.insert(to_file.clone());
                            }
                            move_events.push((from_file, to_file));
                        }
                    } else {
                        let from_java =
                            from.extension().and_then(|ext| ext.to_str()) == Some("java");
                        let to_java = to.extension().and_then(|ext| ext.to_str()) == Some("java");
                        // Avoid allocating ids for non-Java, untracked file moves. Directory moves
                        // are handled above by expanding into moves for already-known paths.
                        if from_java || to_java || from_known_as_file || to_known_as_file {
                            move_events.push((from, to));
                        }
                    }
                }
                NormalizedEvent::Created(path) | NormalizedEvent::Modified(path) => {
                    let path = normalize_local_path(path);
                    if is_module_info_java(&path) {
                        module_info_changes.insert(path.clone());
                    }

                    // If a directory event makes it through categorization, ignore it. We only
                    // care about directory moves/deletes; treating a directory as a file would
                    // allocate a bogus `FileId`.
                    if fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false) {
                        continue;
                    }

                    let vfs_path = VfsPath::local(path.clone());
                    let is_known = self.vfs.get_id(&vfs_path).is_some();
                    let is_java = path.extension().and_then(|ext| ext.to_str()) == Some("java");
                    if is_java || is_known {
                        other_paths.insert(path);
                    }
                }
                NormalizedEvent::Deleted(path) => {
                    let path = normalize_local_path(path);
                    if is_module_info_java(&path) {
                        module_info_changes.insert(path.clone());
                    }

                    // Directory deletes can arrive as a single watcher event without per-file
                    // deletes. If the deleted path isn't a known file but *is* a prefix of known
                    // file paths, expand it as a directory deletion.
                    let vfs_path = VfsPath::local(path.clone());
                    let is_known_file = self.vfs.get_id(&vfs_path).is_some();
                    if !is_known_file {
                        let dir_files = known_files_under_dir(&path);
                        if !dir_files.is_empty() {
                            for file in dir_files {
                                if is_module_info_java(&file) {
                                    module_info_changes.insert(file.clone());
                                }
                                other_paths.insert(file);
                            }
                            continue;
                        }
                    }

                    let is_java = path.extension().and_then(|ext| ext.to_str()) == Some("java");
                    if is_java || is_known_file {
                        other_paths.insert(path);
                    }
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
        let closed_file_texts = Arc::clone(&self.closed_file_texts);
        let subscribers = Arc::clone(&self.subscribers);
        let build_runner = Arc::clone(&self.build_runner);
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

                if let Err(err) = reload_project_and_sync(
                    &root,
                    &changed,
                    &vfs,
                    &query_db,
                    closed_file_texts.as_ref(),
                    &project_state,
                    &watch_config,
                    &watcher_command_store,
                    &subscribers,
                    &build_runner,
                ) {
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
            self.closed_file_texts.as_ref(),
            &self.project_state,
            &self.watch_config,
            &self.watcher_command_store,
            &self.subscribers,
            &self.build_runner,
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
        let was_evicted = self.closed_file_texts.is_evicted(file_id);
        self.closed_file_texts.on_open_document(file_id);

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
                    let existing_content = if was_evicted {
                        None
                    } else {
                        self.query_db.with_snapshot(|snap| {
                            let has_content = snap
                                .all_file_ids()
                                .iter()
                                .any(|&existing_id| existing_id == file_id);
                            has_content.then(|| snap.file_content(file_id))
                        })
                    };

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

        self.query_db.set_file_exists(file_id, true);
        self.query_db.set_file_content(file_id, text_for_db);
        self.query_db.set_file_is_dirty(file_id, dirty);
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
                    let text_arc = Arc::new(text);
                    self.query_db
                        .set_file_content(file_id, Arc::clone(&text_arc));
                    self.closed_file_texts
                        .track_closed_file_content(file_id, &text_arc);
                    restored_from_disk = true;
                }
            } else {
                // The overlay was closed and the file doesn't exist on disk; drop the last-known
                // contents to avoid holding onto large inputs for deleted/unsaved buffers.
                self.query_db
                    .set_file_content(file_id, empty_file_content());
                self.closed_file_texts.clear(file_id);
            }
            if !exists || restored_from_disk {
                self.query_db.set_file_is_dirty(file_id, false);
            }
            self.update_project_files_membership(path, file_id, exists);
            // The document is no longer open; unpin open-document caches so memory
            // accounting attributes them back to Salsa memoization.
            self.query_db.unpin_syntax_tree(file_id);
            self.query_db.unpin_java_parse_tree(file_id);
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
        let old_text = self.vfs.read_to_string(path).ok();
        let evt = match self.vfs.apply_document_changes(path, new_version, changes) {
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
            let text_for_db = Arc::new(text);
            self.ensure_file_inputs(file_id, path);
            self.closed_file_texts.on_open_document(file_id);
            self.query_db.set_file_exists(file_id, true);
            match edits.as_slice() {
                [edit] => {
                    self.query_db.apply_file_text_edit(
                        file_id,
                        edit.clone(),
                        Arc::clone(&text_for_db),
                    );
                }
                [] => {
                    // No-op change batch (shouldn't happen). Treat as a full update.
                    self.query_db.set_file_content(file_id, text_for_db);
                }
                _ => {
                    let synthetic = old_text
                        .as_deref()
                        .and_then(|old| synthetic_single_edit(old, text_for_db.as_str()));
                    if let Some(edit) = synthetic {
                        self.query_db
                            .apply_file_text_edit(file_id, edit, Arc::clone(&text_for_db));
                    } else {
                        self.query_db.set_file_content(file_id, text_for_db);
                    }
                }
            }
        }
        // Any in-memory edit makes the file "dirty" relative to disk for warm-start indexing.
        self.query_db.set_file_is_dirty(file_id, true);

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
        let report = self.memory_report_for_work();
        // Under critical pressure, even producing a truncated list can allocate
        // a large intermediate candidate set. Prefer returning no completions to
        // reduce memory churn and avoid worsening tail latency.
        if matches!(report.pressure, MemoryPressure::Critical) {
            return Vec::new();
        }
        let cap = report.degraded.completion_candidate_cap;
        self.closed_file_texts
            .restore_if_evicted(&self.vfs, file_id);
        let view = WorkspaceDbView::new(self.query_db.snapshot(), self.vfs.clone());
        let text = view.file_content(file_id);
        let position = offset_to_lsp_position(text, offset);
        let mut items: Vec<CompletionItem> = nova_ide::completions(&view, file_id, position)
            .into_iter()
            .map(|item| CompletionItem {
                label: item.label,
                detail: item.detail,
                replace_span: None,
            })
            .collect();
        items.truncate(cap);
        items
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
        let vfs = self.vfs.clone();
        let closed_file_texts = Arc::clone(&self.closed_file_texts);

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
                let project_files: Vec<FileId> =
                    query_db.with_snapshot(|snap| snap.project_files(project).as_ref().clone());
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

                if degraded == BackgroundIndexingMode::Full {
                    for file_id in project_files.iter().copied() {
                        closed_file_texts.restore_if_evicted(&vfs, file_id);
                    }
                }

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

                            closed_file_texts.restore_if_evicted(&vfs, *file_id);

                            let delta = match query_db.with_snapshot_catch_cancelled(|snap| {
                                snap.file_index_delta(*file_id)
                            }) {
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
                    WorkspaceEvent::IndexProgress(IndexProgress {
                        current: total,
                        total,
                    }),
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

    pub(crate) fn salsa_file_content(&self, file_id: FileId) -> Option<Arc<String>> {
        self.query_db.with_snapshot(|snap| {
            let has_content = snap.all_file_ids().iter().any(|&id| id == file_id);
            has_content.then(|| snap.file_content(file_id))
        })
    }

    pub(crate) fn salsa_parse_java(&self, file_id: FileId) -> Arc<nova_syntax::JavaParseResult> {
        self.closed_file_texts
            .restore_if_evicted(&self.vfs, file_id);
        self.query_db.with_snapshot(|snap| snap.parse_java(file_id))
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
        if exists {
            if open_docs.is_open(file_id) {
                self.closed_file_texts.on_open_document(file_id);
                // The file is open in the editor; keep overlay contents but update dirty state if the
                // file was saved to disk (or modified externally).
                let disk_text = fs::read_to_string(path);
                let overlay_text = self.vfs.overlay().document_text(&vfs_path);
                let is_dirty = match (disk_text, overlay_text) {
                    (Ok(disk), Some(overlay)) => disk != overlay,
                    _ => true,
                };
                self.query_db.set_file_is_dirty(file_id, is_dirty);
            } else {
                match fs::read_to_string(path) {
                    Ok(text) => {
                        let text_arc = Arc::new(text);
                        self.query_db
                            .set_file_content(file_id, Arc::clone(&text_arc));
                        self.query_db.set_file_is_dirty(file_id, false);
                        self.closed_file_texts
                            .track_closed_file_content(file_id, &text_arc);
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        self.query_db.set_file_exists(file_id, false);
                        exists = false;
                        self.query_db.set_file_is_dirty(file_id, false);
                        self.query_db
                            .set_file_content(file_id, empty_file_content());
                        self.closed_file_texts.clear(file_id);
                    }
                    Err(_) if !was_known => {
                        self.query_db
                            .set_file_content(file_id, Arc::new(String::new()));
                        self.query_db.set_file_is_dirty(file_id, true);
                        self.closed_file_texts.clear(file_id);
                    }
                    Err(_) => {
                        // Best-effort: keep the previous contents if we fail to read during a transient
                        // IO error.
                    }
                }
            }
        } else if !open_docs.is_open(file_id) {
            self.query_db.set_file_is_dirty(file_id, false);
            self.query_db
                .set_file_content(file_id, empty_file_content());
            self.closed_file_texts.clear(file_id);
        }

        if !exists && was_known && !open_docs.is_open(file_id) {
            self.query_db
                .set_file_content(file_id, empty_file_content());
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
        if !exists && !open_docs.is_open(file_id) {
            self.query_db.set_file_is_dirty(file_id, false);
        }

        if exists {
            if open_docs.is_open(file_id) {
                self.closed_file_texts.on_open_document(file_id);
                // The document is open in the editor (either because it was already open at `to`,
                // or because it was moved there from `from`). Ensure Salsa sees the overlay contents
                // so workspace analysis doesn't accidentally use stale disk state.
                if let Ok(text) = self.vfs.read_to_string(&to_vfs) {
                    self.query_db.set_file_content(file_id, Arc::new(text));
                }
                let disk_text = fs::read_to_string(to);
                let overlay_text = self.vfs.overlay().document_text(&to_vfs);
                let is_dirty = match (disk_text, overlay_text) {
                    (Ok(disk), Some(overlay)) => disk != overlay,
                    _ => true,
                };
                self.query_db.set_file_is_dirty(file_id, is_dirty);
            } else {
                match fs::read_to_string(to) {
                    Ok(text) => {
                        let text_arc = Arc::new(text);
                        self.query_db
                            .set_file_content(file_id, Arc::clone(&text_arc));
                        self.query_db.set_file_is_dirty(file_id, false);
                        self.closed_file_texts
                            .track_closed_file_content(file_id, &text_arc);
                    }
                    Err(_) if is_new_id => {
                        self.query_db
                            .set_file_content(file_id, Arc::new(String::new()));
                        self.query_db.set_file_is_dirty(file_id, true);
                        self.closed_file_texts.clear(file_id);
                    }
                    Err(_) => {}
                }
            }
        } else if !open_docs.is_open(file_id) {
            self.query_db
                .set_file_content(file_id, empty_file_content());
            self.closed_file_texts.clear(file_id);
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
                // The destination id is no longer reachable and cannot be open in the editor.
                // Drop any pinned open-document caches promptly.
                self.query_db.unpin_syntax_tree(id_to);
                self.query_db.unpin_java_parse_tree(id_to);
                self.query_db.unpin_item_tree(id_to);
                self.query_db.set_file_exists(id_to, false);
                if !open_docs.is_open(id_to) {
                    self.query_db.set_file_content(id_to, empty_file_content());
                    self.closed_file_texts.clear(id_to);
                }
                self.update_project_files_membership(&to_vfs, id_to, false);
            }
        }

        // If we renamed onto an already-known destination and `rename_path` returned the
        // destination id, the source id has been cleared from the registry.
        if let Some(id_from) = id_from {
            if to_was_known && Some(file_id) == id_to && id_from != file_id {
                // The move orphaned the source id, so it is no longer open in the editor.
                // Drop any pinned open-document caches promptly.
                self.query_db.unpin_syntax_tree(id_from);
                self.query_db.unpin_java_parse_tree(id_from);
                self.query_db.unpin_item_tree(id_from);
                self.query_db.set_file_exists(id_from, false);
                if !open_docs.is_open(id_from) {
                    self.query_db
                        .set_file_content(id_from, empty_file_content());
                    self.closed_file_texts.clear(id_from);
                }
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
        let report = self.memory_report_for_work();
        let diagnostics = self.compute_diagnostics(&file, file_id, report.degraded);
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
    fn memory_report_for_work(&self) -> MemoryReport {
        // Keep eviction and degraded settings reasonably fresh without running the
        // (potentially expensive) eviction loop on every diagnostics/completions request.
        const ENFORCE_INTERVAL: Duration = Duration::from_secs(1);

        let now = Instant::now();
        let should_enforce = {
            let mut last = self
                .last_memory_enforce
                .lock()
                .expect("workspace memory enforce mutex poisoned");
            match *last {
                Some(prev) if now.duration_since(prev) < ENFORCE_INTERVAL => false,
                _ => {
                    *last = Some(now);
                    true
                }
            }
        };

        if should_enforce {
            self.memory.enforce()
        } else {
            self.memory.report()
        }
    }

    fn compute_diagnostics(
        &self,
        file: &VfsPath,
        file_id: FileId,
        degraded: DegradedSettings,
    ) -> Vec<NovaDiagnostic> {
        if degraded.skip_expensive_diagnostics {
            return self.syntax_diagnostics_only(file, file_id);
        }

        self.closed_file_texts
            .restore_if_evicted(&self.vfs, file_id);
        let view = WorkspaceDbView::new(self.query_db.snapshot(), self.vfs.clone());
        nova_ide::file_diagnostics_with_semantic_db(&view, view.semantic_db(), file_id)
    }

    fn syntax_diagnostics_only(&self, file: &VfsPath, file_id: FileId) -> Vec<NovaDiagnostic> {
        if !is_java_vfs_path(file) {
            return Vec::new();
        }
        // Avoid duplicating the file contents (and any disk I/O) by reusing the
        // Salsa input text, which already includes open-document overlays.
        let Some(text) = self.query_db.with_snapshot(|snap| {
            if snap.file_exists(file_id) {
                Some(snap.file_content(file_id))
            } else {
                None
            }
        }) else {
            return Vec::new();
        };

        let mut diagnostics = Vec::new();

        let parse = nova_syntax::parse(text.as_str());
        diagnostics.extend(parse.errors.into_iter().map(|e| {
            NovaDiagnostic::error(
                "SYNTAX",
                e.message,
                Some(Span::new(e.range.start as usize, e.range.end as usize)),
            )
        }));

        let java_parse = nova_syntax::parse_java(text.as_str());
        diagnostics.extend(java_parse.errors.into_iter().map(|e| {
            NovaDiagnostic::error(
                "SYNTAX",
                e.message,
                Some(Span::new(e.range.start as usize, e.range.end as usize)),
            )
        }));

        diagnostics
    }

    pub(crate) fn query_db(&self) -> salsa::Database {
        self.query_db.clone()
    }

    pub(crate) fn vfs(&self) -> &Vfs<LocalFs> {
        &self.vfs
    }
}

fn is_java_vfs_path(path: &VfsPath) -> bool {
    match path {
        VfsPath::Local(local) => local.extension().and_then(|ext| ext.to_str()) == Some("java"),
        VfsPath::Archive(archive) => archive.entry.ends_with(".java"),
        // Decompiled virtual docs are always Java.
        VfsPath::Decompiled { .. } | VfsPath::LegacyDecompiled { .. } => true,
        VfsPath::Uri(uri) => uri.ends_with(".java"),
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

fn synthetic_single_edit(old: &str, new: &str) -> Option<TextEdit> {
    let old_len = old.len();
    let new_len = new.len();
    let old_bytes = old.as_bytes();
    let new_bytes = new.as_bytes();

    // Find the longest common prefix.
    let mut prefix = 0usize;
    let max_prefix = old_len.min(new_len);
    while prefix < max_prefix && old_bytes[prefix] == new_bytes[prefix] {
        prefix += 1;
    }

    // Find the longest common suffix without overlapping the prefix.
    let mut suffix = 0usize;
    while suffix < old_len.saturating_sub(prefix) && suffix < new_len.saturating_sub(prefix) {
        if old_bytes[old_len - 1 - suffix] != new_bytes[new_len - 1 - suffix] {
            break;
        }
        suffix += 1;
    }

    // Ensure the prefix boundary is a UTF-8 character boundary by walking it backwards. This
    // ensures we never split a multi-byte character (in case the mismatch occurs mid-codepoint).
    while prefix > 0 && (!old.is_char_boundary(prefix) || !new.is_char_boundary(prefix)) {
        prefix -= 1;
    }

    // Ensure the suffix boundary is a UTF-8 character boundary by shrinking it (walking it
    // forwards). This keeps the suffix bytes a strict subset of the original common suffix.
    while suffix > 0 {
        let old_start = old_len - suffix;
        let new_start = new_len - suffix;
        if old.is_char_boundary(old_start) && new.is_char_boundary(new_start) {
            break;
        }
        suffix -= 1;
    }

    let old_end = old_len.saturating_sub(suffix);
    let new_end = new_len.saturating_sub(suffix);

    if prefix > old_end || prefix > new_end {
        return None;
    }
    if !old.is_char_boundary(old_end) || !new.is_char_boundary(new_end) {
        return None;
    }

    let start = u32::try_from(prefix).ok()?;
    let end = u32::try_from(old_end).ok()?;

    Some(TextEdit::new(
        TextRange::new(TextSize::from(start), TextSize::from(end)),
        new[prefix..new_end].to_string(),
    ))
}

fn publish_to_subscribers(
    subscribers: &Arc<Mutex<Vec<Sender<WorkspaceEvent>>>>,
    event: WorkspaceEvent,
) {
    let mut subs = subscribers
        .lock()
        .expect("workspace subscriber mutex poisoned");
    subs.retain(|tx| match tx.try_send(event.clone()) {
        Ok(()) => true,
        Err(async_channel::TrySendError::Full(_)) => true,
        Err(async_channel::TrySendError::Closed(_)) => false,
    });
}

fn publish_watch_root_error(
    subscribers: &Arc<Mutex<Vec<Sender<WorkspaceEvent>>>>,
    err: WatchRootError,
) {
    match err {
        WatchRootError::WatchFailed {
            root,
            mode: _,
            error,
        } => {
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
    let mut roots: Vec<PathBuf> = config
        .source_roots
        .iter()
        .map(|r| match VfsPath::local(r.path.clone()) {
            VfsPath::Local(path) => path,
            _ => unreachable!("VfsPath::local produced a non-local path"),
        })
        .collect();
    if roots.is_empty() {
        roots.push(match VfsPath::local(config.workspace_root.clone()) {
            VfsPath::Local(path) => path,
            _ => unreachable!("VfsPath::local produced a non-local path"),
        });
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

fn watch_roots_from_project_config(
    config: &ProjectConfig,
) -> (Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>) {
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

    let mut module_roots: Vec<PathBuf> = config
        .modules
        .iter()
        .map(|module| module.root.clone())
        .collect();
    module_roots.sort();
    module_roots.dedup();

    (source_roots, generated_source_roots, module_roots)
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

fn jdk_fingerprint(config: &nova_core::JdkConfig, requested_release: Option<u16>) -> Fingerprint {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&requested_release.unwrap_or(0).to_le_bytes());
    bytes.extend_from_slice(&config.release.unwrap_or(0).to_le_bytes());
    if let Some(home) = &config.home {
        bytes.extend_from_slice(home.to_string_lossy().as_bytes());
    }
    bytes.push(0);
    for (release, home) in &config.toolchains {
        bytes.extend_from_slice(&release.to_le_bytes());
        bytes.extend_from_slice(home.to_string_lossy().as_bytes());
        bytes.push(0);
    }

    // When no explicit toolchain/home override is configured, JDK discovery falls back to the
    // environment (`JAVA_HOME` / `java` on PATH). Include `JAVA_HOME` in the fingerprint so
    // switching toolchains mid-process triggers a rediscovery on the next reload.
    if config.preferred_home(requested_release).is_none() {
        if let Some(java_home) = std::env::var_os("JAVA_HOME") {
            bytes.extend_from_slice(java_home.to_string_lossy().as_bytes());
        }
        bytes.push(0);
    }

    Fingerprint::from_bytes(bytes)
}

fn build_cache_dir(workspace_root: &Path, query_db: &salsa::Database) -> PathBuf {
    // Prefer Nova's project-hash cache dir when persistence is enabled, so nova-build and other
    // subsystems share a single on-disk cache root.
    if let Some(classpath_dir) = query_db.classpath_cache_dir() {
        if let Some(project_cache_dir) = classpath_dir.parent() {
            return project_cache_dir.join("build");
        }
    }

    workspace_root.join(".nova").join("build-cache")
}

fn is_build_tool_input_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };

    // Mirror `nova-build`'s build-file fingerprinting exclusions to avoid treating build output /
    // cache directories as project-changing inputs.
    let in_ignored_dir = path.components().any(|c| {
        c.as_os_str() == std::ffi::OsStr::new(".git")
            || c.as_os_str() == std::ffi::OsStr::new(".gradle")
            || c.as_os_str() == std::ffi::OsStr::new("build")
            || c.as_os_str() == std::ffi::OsStr::new("target")
            || c.as_os_str() == std::ffi::OsStr::new(".nova")
            || c.as_os_str() == std::ffi::OsStr::new(".idea")
    });

    // Gradle script plugins can influence dependencies and tasks.
    if !in_ignored_dir && (name.ends_with(".gradle") || name.ends_with(".gradle.kts")) {
        return true;
    }

    // Gradle version catalogs can define dependency versions.
    if !in_ignored_dir && name.ends_with(".versions.toml") {
        return true;
    }

    // Gradle dependency locking can change resolved classpaths without modifying build scripts.
    //
    // Patterns:
    // - `gradle.lockfile` at any depth.
    // - `*.lockfile` under any `dependency-locks/` directory (covers Gradle's default
    //   `gradle/dependency-locks/` location).
    if !in_ignored_dir && name == "gradle.lockfile" {
        return true;
    }
    if !in_ignored_dir
        && name.ends_with(".lockfile")
        && path
            .ancestors()
            .any(|dir| dir.file_name().is_some_and(|name| name == "dependency-locks"))
    {
        return true;
    }

    if name == "pom.xml" {
        return true;
    }

    match name {
        "gradle.properties" | "gradlew" | "gradlew.bat" => true,
        "gradle-wrapper.properties" => {
            path.ends_with(Path::new("gradle/wrapper/gradle-wrapper.properties"))
        }
        "gradle-wrapper.jar" => path.ends_with(Path::new("gradle/wrapper/gradle-wrapper.jar")),
        "mvnw" | "mvnw.cmd" => true,
        "maven-wrapper.properties" => {
            path.ends_with(Path::new(".mvn/wrapper/maven-wrapper.properties"))
        }
        "maven-wrapper.jar" => path.ends_with(Path::new(".mvn/wrapper/maven-wrapper.jar")),
        "extensions.xml" => path.ends_with(Path::new(".mvn/extensions.xml")),
        "maven.config" => path.ends_with(Path::new(".mvn/maven.config")),
        "jvm.config" => path.ends_with(Path::new(".mvn/jvm.config")),
        _ => false,
    }
}

fn should_refresh_build_config(workspace_root: &Path, changed_files: &[PathBuf]) -> bool {
    changed_files.is_empty()
        || changed_files.iter().any(|path| {
            // Many build inputs are detected based on path components (e.g. ignoring `build/`
            // output directories). Use paths relative to the workspace root so absolute parent
            // directories (like `/home/user/build/...`) don't accidentally trip ignore heuristics.
            let rel = path.strip_prefix(workspace_root).unwrap_or(path.as_path());
            is_build_tool_input_file(rel)
        })
}

fn classpath_entry_kind_for_path(path: &Path) -> ClasspathEntryKind {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("jar") || ext.eq_ignore_ascii_case("jmod") => {
            ClasspathEntryKind::Jar
        }
        _ => ClasspathEntryKind::Directory,
    }
}

fn classpath_entries_from_paths(paths: impl IntoIterator<Item = PathBuf>) -> Vec<ClasspathEntry> {
    let mut out = Vec::new();
    let mut seen: HashSet<(ClasspathEntryKind, PathBuf)> = HashSet::new();
    for path in paths {
        if path.as_os_str().is_empty() {
            continue;
        }
        let kind = classpath_entry_kind_for_path(&path);
        if seen.insert((kind, path.clone())) {
            out.push(ClasspathEntry { kind, path });
        }
    }
    out
}

fn infer_source_root_origin(path: &Path) -> SourceRootOrigin {
    let lower = path.to_string_lossy().to_ascii_lowercase();
    if lower.contains("generated") || lower.contains(".nova") {
        SourceRootOrigin::Generated
    } else {
        SourceRootOrigin::Source
    }
}

fn source_roots_from_java_compile_config(
    base: &ProjectConfig,
    cfg: &nova_build::JavaCompileConfig,
) -> Vec<SourceRoot> {
    fn origin_for_path(
        base: &ProjectConfig,
        kind: SourceRootKind,
        path: &Path,
    ) -> SourceRootOrigin {
        base.source_roots
            .iter()
            .find(|root| root.kind == kind && root.path == path)
            .or_else(|| base.source_roots.iter().find(|root| root.path == path))
            .map(|root| root.origin)
            .unwrap_or_else(|| infer_source_root_origin(path))
    }

    let mut out = Vec::new();
    let mut seen: HashSet<(SourceRootKind, PathBuf)> = HashSet::new();

    for path in &cfg.main_source_roots {
        let origin = origin_for_path(base, SourceRootKind::Main, path);
        if seen.insert((SourceRootKind::Main, path.clone())) {
            out.push(SourceRoot {
                kind: SourceRootKind::Main,
                origin,
                path: path.clone(),
            });
        }
    }

    for path in &cfg.test_source_roots {
        let origin = origin_for_path(base, SourceRootKind::Test, path);
        if seen.insert((SourceRootKind::Test, path.clone())) {
            out.push(SourceRoot {
                kind: SourceRootKind::Test,
                origin,
                path: path.clone(),
            });
        }
    }

    // Preserve any additional roots discovered by `nova-project` (especially
    // `.nova/apt-cache/generated-roots.json`).
    for root in &base.source_roots {
        if seen.insert((root.kind, root.path.clone())) {
            out.push(root.clone());
        }
    }

    out
}

fn output_dirs_from_java_compile_config(
    base: &ProjectConfig,
    cfg: &nova_build::JavaCompileConfig,
) -> Vec<OutputDir> {
    let mut out = Vec::new();
    let mut seen: HashSet<(OutputDirKind, PathBuf)> = HashSet::new();

    if let Some(dir) = &cfg.main_output_dir {
        if seen.insert((OutputDirKind::Main, dir.clone())) {
            out.push(OutputDir {
                kind: OutputDirKind::Main,
                path: dir.clone(),
            });
        }
    }

    if let Some(dir) = &cfg.test_output_dir {
        if seen.insert((OutputDirKind::Test, dir.clone())) {
            out.push(OutputDir {
                kind: OutputDirKind::Test,
                path: dir.clone(),
            });
        }
    }

    // Preserve any additional output dirs (e.g. multi-module workspaces) discovered by
    // `nova-project` heuristics.
    for dir in &base.output_dirs {
        if seen.insert((dir.kind, dir.path.clone())) {
            out.push(dir.clone());
        }
    }

    out
}

fn apply_java_compile_config_to_project_config(
    mut config: ProjectConfig,
    cfg: &nova_build::JavaCompileConfig,
    base: &ProjectConfig,
) -> ProjectConfig {
    config.classpath = classpath_entries_from_paths(
        cfg.compile_classpath
            .iter()
            .chain(cfg.test_classpath.iter())
            .cloned(),
    );
    config.module_path = classpath_entries_from_paths(cfg.module_path.iter().cloned());
    config.source_roots = source_roots_from_java_compile_config(base, cfg);
    config.output_dirs = output_dirs_from_java_compile_config(base, cfg);
    config
}

fn reuse_previous_build_config_fields(
    mut loaded: ProjectConfig,
    previous: &ProjectConfig,
) -> ProjectConfig {
    loaded.classpath = previous.classpath.clone();
    loaded.module_path = previous.module_path.clone();
    loaded.output_dirs = previous.output_dirs.clone();

    // Prefer the previous source roots (which may have been populated from nova-build), but also
    // incorporate any newly discovered roots from `nova-project` (e.g. apt generated roots).
    let mut merged = previous.source_roots.clone();
    let mut seen: HashSet<(SourceRootKind, PathBuf)> = merged
        .iter()
        .map(|root| (root.kind, root.path.clone()))
        .collect();
    for root in &loaded.source_roots {
        if seen.insert((root.kind, root.path.clone())) {
            merged.push(root.clone());
        }
    }
    loaded.source_roots = merged;

    loaded
}

fn reload_project_and_sync(
    workspace_root: &Path,
    changed_files: &[PathBuf],
    vfs: &Vfs<LocalFs>,
    query_db: &salsa::Database,
    closed_file_texts: &ClosedFileTextStore,
    project_state: &Arc<Mutex<ProjectState>>,
    watch_config: &Arc<RwLock<WatchConfig>>,
    watcher_command_store: &Arc<Mutex<Option<channel::Sender<WatchCommand>>>>,
    subscribers: &Arc<Mutex<Vec<Sender<WorkspaceEvent>>>>,
    build_runner: &Arc<dyn CommandRunner>,
) -> Result<()> {
    let (previous_config, mut options, previous_classpath_fingerprint, previous_jdk_fingerprint) = {
        let state = project_state
            .lock()
            .expect("workspace project state mutex poisoned");
        (
            Arc::clone(&state.config),
            state.load_options.clone(),
            state.classpath_fingerprint.clone(),
            state.jdk_fingerprint.clone(),
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

    let mut loaded =
        match nova_project::reload_project(base_config, &mut options, effective_changed_files) {
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

    if matches!(
        loaded.build_system,
        BuildSystem::Maven | BuildSystem::Gradle
    ) {
        let refresh_build = should_refresh_build_config(workspace_root, changed_files);
        // `.nova/queries/gradle.json` is a build-tool-produced Gradle snapshot that can update
        // resolved classpaths/source roots without modifying build scripts. When it changes we
        // want to re-load the project config (so `nova-project` can consume the snapshot), but we
        // do NOT want to overwrite the newly loaded classpath with stale build-derived fields.
        let gradle_snapshot_changed = changed_files
            .iter()
            .any(|path| path.ends_with(Path::new(".nova/queries/gradle.json")));

        if refresh_build {
            let cache_dir = build_cache_dir(workspace_root, query_db);
            let build = BuildManager::with_runner(cache_dir, Arc::clone(build_runner));

            let compile_config = match loaded.build_system {
                BuildSystem::Maven => build.java_compile_config_maven(workspace_root, None),
                BuildSystem::Gradle => build.java_compile_config_gradle(workspace_root, None),
                _ => unreachable!("build config refresh only applies to Maven/Gradle"),
            };

            match compile_config {
                Ok(cfg) => {
                    // Preserve generated roots and other workspace metadata discovered by
                    // `nova-project` while replacing classpath/module-path/source roots with the
                    // build-tool-derived configuration.
                    let base = loaded.clone();
                    loaded = apply_java_compile_config_to_project_config(loaded, &cfg, &base);
                }
                Err(err) => {
                    publish_to_subscribers(
                        subscribers,
                        WorkspaceEvent::Status(WorkspaceStatus::IndexingError(format!(
                            "Build tool classpath extraction failed; falling back to heuristic project config: {err}"
                        ))),
                    );
                }
            }
        } else if !gradle_snapshot_changed
            && previous_config.build_system == loaded.build_system
            && previous_config.workspace_root == loaded.workspace_root
            && !previous_config.workspace_root.as_os_str().is_empty()
        {
            loaded = reuse_previous_build_config_fields(loaded, previous_config.as_ref());
        }
    }

    let had_classpath_fingerprint = previous_classpath_fingerprint.is_some();
    let config_changed = &loaded != previous_config.as_ref();
    let config = if config_changed {
        Arc::new(loaded)
    } else {
        Arc::clone(&previous_config)
    };
    let source_roots = build_source_roots(&config);
    let (watch_source_roots, watch_generated_roots, watch_module_roots) =
        watch_roots_from_project_config(&config);
    let nova_config_path = options.nova_config_path.clone();
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
        *cfg = WatchConfig::with_roots(
            workspace_root.to_path_buf(),
            watch_source_roots.clone(),
            watch_generated_roots.clone(),
        );
        cfg.module_roots = watch_module_roots.clone();
        cfg.nova_config_path = nova_config_path.clone();
    }

    // If the watcher is running, schedule a refresh so it reconciles watched paths/roots with the
    // latest `watch_config` (adds new external roots, removes stale ones, and picks up changes to
    // the config-path watch).
    if let Some(tx) = watcher_command_store
        .lock()
        .expect("workspace watcher command store mutex poisoned")
        .clone()
    {
        // Never block the calling thread (this can run inside debounced background tasks).
        // If the queue is full, a watch update is already pending and will pick up the latest
        // `watch_config` state when it runs.
        let _ = tx.try_send(WatchCommand::Watch(workspace_root.to_path_buf()));
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

    let next_jdk_fingerprint = jdk_fingerprint(&jdk_config, requested_release);
    let jdk_changed = match &previous_jdk_fingerprint {
        Some(prev) => prev != &next_jdk_fingerprint,
        None => true,
    };
    let current_jdk_backing = query_db.with_snapshot(|snap| snap.jdk_index(project).info().backing);
    let should_discover_jdk =
        jdk_changed || current_jdk_backing == nova_jdk::JdkIndexBacking::Builtin;

    {
        let mut state = project_state
            .lock()
            .expect("workspace project state mutex poisoned");
        state.jdk_fingerprint = Some(next_jdk_fingerprint);
    }

    if should_discover_jdk {
        // JDK discovery is best-effort. In addition to returning `Err`, some discovery paths may
        // panic under extreme resource pressure (e.g. failing to spawn output reader threads while
        // probing `java` on PATH). Treat any panic as a discovery failure and fall back to the
        // built-in stub index so the workspace can still load.
        let jdk_index = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            nova_jdk::JdkIndex::discover_for_release(Some(&jdk_config), requested_release)
        }))
        .ok()
        .and_then(|res| res.ok())
        .unwrap_or_else(nova_jdk::JdkIndex::new);
        query_db.set_jdk_index(project, Arc::new(jdk_index));
    }

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
    let mut next_entries: Vec<(Arc<String>, FileId)> = Vec::new();

    for path in paths {
        let vfs_path = VfsPath::local(path.clone());
        let file_id = vfs.file_id(vfs_path);

        let Some(rel_path) = rel_path_for_workspace(workspace_root, &path) else {
            continue;
        };

        let rel_path = Arc::new(rel_path);
        // `set_file_rel_path` keeps the non-tracked persistence `file_path` in sync with
        // `file_rel_path`, sharing the same `Arc<String>` for stable persistence keys without
        // duplicating the underlying string.
        query_db.set_file_rel_path(file_id, rel_path.clone());
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
            if !open_docs.is_open(file_id) {
                query_db.set_file_content(file_id, empty_file_content());
                closed_file_texts.clear(file_id);
            }
            continue;
        }

        if !open_docs.is_open(file_id) {
            match fs::read_to_string(&path) {
                Ok(text) => {
                    let text_arc = Arc::new(text);
                    query_db.set_file_content(file_id, Arc::clone(&text_arc));
                    query_db.set_file_is_dirty(file_id, false);
                    closed_file_texts.track_closed_file_content(file_id, &text_arc);
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    query_db.set_file_exists(file_id, false);
                    query_db.set_file_content(file_id, empty_file_content());
                    closed_file_texts.clear(file_id);
                    continue;
                }
                Err(_) if !previous_set.contains(&file_id) => {
                    // Ensure the input is initialized for new files even if we cannot read them.
                    query_db.set_file_content(file_id, empty_file_content());
                    query_db.set_file_is_dirty(file_id, true);
                    closed_file_texts.clear(file_id);
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
            if !open_docs.is_open(old) {
                query_db.set_file_content(old, empty_file_content());
                closed_file_texts.clear(old);
            }
        }
    }

    next_entries.sort_by(|(a_path, a_id), (b_path, b_id)| {
        a_path.as_str().cmp(b_path.as_str()).then(a_id.cmp(b_id))
    });
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
        AnnotationLocation, IndexSymbolKind, IndexedSymbol, InheritanceEdge, ReferenceLocation,
        SymbolLocation,
    };
    use nova_memory::{
        EvictionRequest, EvictionResult, MemoryBudget, MemoryCategory, MemoryEvictor,
    };
    use nova_project::BuildSystem;
    use nova_test_utils::EnvVarGuard;
    use nova_vfs::{FileChange, ManualFileWatcher, ManualFileWatcherHandle};
    use std::fs;
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;
    use tokio::task::yield_now;
    use tokio::time::timeout;

    use super::*;

    #[test]
    fn build_config_refresh_is_triggered_by_gradle_version_catalogs_script_plugins_and_lockfiles() {
        let root = PathBuf::from("/tmp/workspace");

        assert!(
            should_refresh_build_config(&root, &[root.join("libs.versions.toml")]),
            "expected root libs.versions.toml to trigger build-tool refresh"
        );

        assert!(
            should_refresh_build_config(&root, &[root.join("deps.versions.toml")]),
            "expected root deps.versions.toml to trigger build-tool refresh"
        );

        assert!(
            should_refresh_build_config(&root, &[root.join("gradle").join("foo.versions.toml")]),
            "expected gradle/foo.versions.toml to trigger build-tool refresh"
        );

        assert!(
            should_refresh_build_config(
                &root,
                &[root.join("gradle").join("sub").join("nested.versions.toml")]
            ),
            "expected gradle/sub/nested.versions.toml to trigger build-tool refresh"
        );

        assert!(
            should_refresh_build_config(&root, &[root.join("dependencies.gradle")]),
            "expected dependencies.gradle to trigger build-tool refresh"
        );

        assert!(
            should_refresh_build_config(&root, &[root.join("dependencies.gradle.kts")]),
            "expected dependencies.gradle.kts to trigger build-tool refresh"
        );

        assert!(
            should_refresh_build_config(&root, &[root.join("gradle.lockfile")]),
            "expected gradle.lockfile to trigger build-tool refresh"
        );

        assert!(
            should_refresh_build_config(
                &root,
                &[root
                    .join("gradle")
                    .join("dependency-locks")
                    .join("compileClasspath.lockfile")]
            ),
            "expected gradle/dependency-locks/*.lockfile to trigger build-tool refresh"
        );

        assert!(
            !should_refresh_build_config(&root, &[root.join("foo.lockfile")]),
            "expected foo.lockfile (outside dependency-locks) to be ignored"
        );

        assert!(
            should_refresh_build_config(
                &root,
                &[root.join("dependency-locks").join("custom.lockfile")]
            ),
            "expected dependency-locks/*.lockfile to trigger build-tool refresh"
        );

        // Build output directories should not trigger build-tool refresh.
        assert!(
            !should_refresh_build_config(&root, &[root.join("build").join("dependencies.gradle")]),
            "expected build/dependencies.gradle to be ignored"
        );

        assert!(
            !should_refresh_build_config(&root, &[root.join("build").join("gradle.lockfile")]),
            "expected build/gradle.lockfile to be ignored"
        );

        // Ensure absolute paths don't spuriously hit ignore heuristics due to parent directories
        // named `build/`.
        let root_under_build = PathBuf::from("/tmp/build/workspace");
        assert!(
            should_refresh_build_config(
                &root_under_build,
                &[root_under_build.join("gradle.lockfile")]
            ),
            "expected gradle.lockfile under /tmp/build/... to trigger refresh"
        );
    }

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

    #[test]
    fn build_config_refresh_triggers_for_gradle_dependency_lockfiles() {
        let root = PathBuf::from("/tmp/workspace");
        assert!(
            should_refresh_build_config(&root, &[root.join("gradle.lockfile")]),
            "expected gradle.lockfile to trigger build config refresh"
        );
        assert!(
            should_refresh_build_config(
                &root,
                &[root.join("gradle/dependency-locks/compileClasspath.lockfile")]
            ),
            "expected dependency-locks/*.lockfile to trigger build config refresh"
        );
        assert!(
            !should_refresh_build_config(&root, &[root.join("foo.lockfile")]),
            "expected unrelated *.lockfile not to trigger build config refresh"
        );
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
            build_runner: None,
        });

        engine.set_workspace_root(&project_root).unwrap();

        let rx = engine.subscribe();
        engine.trigger_indexing();
        timeout(Duration::from_secs(20), wait_for_indexing_ready(&rx))
            .await
            .expect("timed out waiting for indexing");

        let cache_dir = CacheDir::new(
            &project_root,
            CacheConfig {
                cache_root_override: Some(cache_root),
            },
        )
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
        let main_path = project_root.join("src/Main.java");

        let cache_root = dir.path().join("cache-root");
        let persistence = PersistenceConfig {
            mode: PersistenceMode::ReadWrite,
            cache: CacheConfig {
                cache_root_override: Some(cache_root.clone()),
            },
        };

        // Engine #1: index from scratch and persist shards to disk.
        let memory = MemoryManager::new(MemoryBudget::default_for_system_with_env_overrides());
        let engine1 = WorkspaceEngine::new(WorkspaceEngineConfig {
            workspace_root: project_root.clone(),
            persistence: persistence.clone(),
            memory,
            build_runner: None,
        });
        engine1.set_workspace_root(&project_root).unwrap();

        let rx1 = engine1.subscribe();
        engine1.trigger_indexing();
        timeout(Duration::from_secs(20), wait_for_indexing_ready(&rx1))
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

        drop(engine1);

        // Engine #2: warm-start from persisted shards, then edit Main.java in-memory only.
        let memory = MemoryManager::new(MemoryBudget::default_for_system_with_env_overrides());
        let engine2 = WorkspaceEngine::new(WorkspaceEngineConfig {
            workspace_root: project_root.clone(),
            persistence,
            memory,
            build_runner: None,
        });
        engine2.set_workspace_root(&project_root).unwrap();

        let vfs_path = VfsPath::local(main_path.clone());
        let disk_text = fs::read_to_string(&main_path).unwrap();
        let file_id = engine2.open_document(vfs_path.clone(), disk_text, 1);
        engine2.query_db.with_snapshot(|snap| {
            assert!(
                !snap.file_is_dirty(file_id),
                "expected freshly opened overlay to be clean when it matches disk"
            );
        });

        engine2
            .apply_changes(
                &vfs_path,
                2,
                &[ContentChange::full("class Dirty {}".to_string())],
            )
            .unwrap();
        engine2.query_db.with_snapshot(|snap| {
            assert!(
                snap.file_is_dirty(file_id),
                "expected didChange to mark overlay dirty"
            );
        });

        engine2.query_db.clear_query_stats();

        let rx2 = engine2.subscribe();
        engine2.trigger_indexing();
        timeout(Duration::from_secs(20), wait_for_indexing_ready(&rx2))
            .await
            .expect("timed out waiting for indexing with dirty overlay");

        let stats = engine2.query_db.query_stats();
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

        let indexes = engine2
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

        let all_files = vec![
            FileId::from_raw(1),
            FileId::from_raw(2),
            FileId::from_raw(3),
        ];
        let open_files: HashSet<FileId> = HashSet::from([FileId::from_raw(2)]);
        let files = WorkspaceEngine::background_indexing_plan(
            report.degraded.background_indexing,
            all_files,
            &open_files,
        )
        .expect("plan should be present in Reduced mode");
        assert_eq!(files, vec![FileId::from_raw(2)]);
    }

    fn new_test_engine(memory: MemoryManager) -> WorkspaceEngine {
        WorkspaceEngine::new(WorkspaceEngineConfig {
            workspace_root: PathBuf::new(),
            persistence: PersistenceConfig {
                mode: PersistenceMode::Disabled,
                cache: CacheConfig::from_env(),
            },
            memory,
            build_runner: None,
        })
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
            build_runner: None,
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
        config.source_roots = vec![
            PathBuf::from("/ext/src"),
            PathBuf::from("/ext/src/generated"),
        ];

        let roots = compute_watch_roots(&workspace_root, &config);
        assert_eq!(
            roots,
            vec![
                (PathBuf::from("/ext/src"), WatchMode::Recursive),
                (PathBuf::from("/ws"), WatchMode::Recursive)
            ]
        );
    }

    #[test]
    fn watch_roots_are_deterministic_across_input_order() {
        let workspace_root = PathBuf::from("/ws");

        let mut config_a = WatchConfig::new(workspace_root.clone());
        config_a.source_roots = vec![
            PathBuf::from("/ext/src"),
            PathBuf::from("/ext/src/generated"),
        ];

        let mut config_b = WatchConfig::new(workspace_root.clone());
        config_b.source_roots = vec![
            PathBuf::from("/ext/src/generated"),
            PathBuf::from("/ext/src"),
        ];

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
            roots.iter().all(|(root, _)| root != &workspace_src),
            "expected {} to not be watched explicitly (workspace root watch should cover it)",
            workspace_src.display()
        );
    }

    #[test]
    fn watch_roots_include_external_module_roots() {
        use std::collections::HashMap;

        let dir = tempfile::tempdir().unwrap();
        let workspace_root = dir.path().join("root");
        let external_root = dir.path().join("external");
        fs::create_dir_all(&workspace_root).unwrap();
        fs::create_dir_all(&external_root).unwrap();

        fs::write(
            workspace_root.join("pom.xml"),
            br#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>root</artifactId>
  <version>1.0</version>
  <packaging>pom</packaging>
  <modules>
    <module>../external</module>
  </modules>
</project>
"#,
        )
        .unwrap();
        fs::write(
            external_root.join("pom.xml"),
            br#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>external</artifactId>
  <version>1.0</version>
</project>
"#,
        )
        .unwrap();

        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let workspace_root = workspace_root.canonicalize().unwrap();
        let external_root = external_root.canonicalize().unwrap();

        let workspace = crate::Workspace::open(&workspace_root).unwrap();
        let engine = workspace.engine_for_tests();

        let watch_config = engine
            .watch_config
            .read()
            .expect("workspace watch config lock poisoned")
            .clone();

        assert!(
            watch_config.module_roots.contains(&external_root),
            "expected WatchConfig.module_roots to include {} (got {:?})",
            external_root.display(),
            watch_config.module_roots
        );

        let roots = compute_watch_roots(&workspace_root, &watch_config);
        assert!(
            roots.contains(&(external_root.clone(), WatchMode::Recursive)),
            "expected watch roots {roots:?} to include external module root {}",
            external_root.display()
        );

        // Prove the watch-root manager would request this root.
        let mut manager = WatchRootManager::new(Duration::from_millis(0));
        let mut manual = ManualFileWatcher::new();
        let desired: HashMap<PathBuf, WatchMode> = roots.into_iter().collect();
        let errors = manager.set_desired_roots(desired, Instant::now(), &mut manual);
        assert!(
            errors.is_empty(),
            "unexpected errors while applying watch roots: {errors:?}"
        );
        assert!(
            manual
                .watch_calls()
                .iter()
                .any(|(path, mode)| path == &external_root && *mode == WatchMode::Recursive),
            "expected manual watcher calls {:?} to include {}",
            manual.watch_calls(),
            external_root.display()
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
            build_runner: None,
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
    fn external_config_path_adds_non_recursive_watch_for_parent_directory() {
        nova_config::with_config_env_lock(|| {
            let workspace_dir = tempfile::tempdir().unwrap();
            let workspace_root = workspace_dir.path().canonicalize().unwrap();

            let config_dir = tempfile::tempdir().unwrap();
            let config_path = config_dir.path().join("myconfig.toml");
            fs::write(&config_path, b"[generated_sources]\nenabled = true\n").unwrap();
            let config_path = config_path.canonicalize().unwrap();

            let _config_guard = EnvVarGuard::set(nova_config::NOVA_CONFIG_ENV_VAR, &config_path);

            let mut watch_config = WatchConfig::new(workspace_root.clone());
            watch_config.nova_config_path = nova_config::discover_config_path(&workspace_root);
            assert_eq!(
                watch_config.nova_config_path.as_deref(),
                Some(config_path.as_path())
            );

            let roots = compute_watch_roots(&workspace_root, &watch_config);
            assert!(roots.contains(&(workspace_root.clone(), WatchMode::Recursive)));
            assert!(
                roots.contains(&(config_path.clone(), WatchMode::NonRecursive)),
                "expected roots {roots:?} to include non-recursive watch for config path"
            );
            assert!(
                !roots.iter().any(|(root, _)| root == config_dir.path()),
                "expected watch roots not to include the entire config directory; roots: {roots:?}"
            );
        });
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
            assert_eq!(
                snap.file_rel_path(file_id).as_str(),
                expected_rel_path.as_str()
            );

            let file_path = snap.file_path(file_id).expect("file_path should be set");
            assert!(!file_path.is_empty());
            assert_eq!(file_path.as_str(), expected_rel_path.as_str());
        });
    }

    #[test]
    fn degraded_mode_skips_expensive_diagnostics_but_retains_syntax_errors() {
        let memory = MemoryManager::new(MemoryBudget::from_total(10 * nova_memory::GB));
        let engine = new_test_engine(memory.clone());

        let registration = engine
            .memory
            .register_tracker("test-pressure", MemoryCategory::Other);
        let tracker = registration.tracker();

        // Start in low pressure so we get the full diagnostic set.
        tracker.set_bytes(0);

        let dir = tempfile::tempdir().unwrap();
        let path = VfsPath::local(dir.path().join("Main.java"));
        let text = "class Main { void foo() { bar(); int x = ; } }".to_string();
        let file_id = engine.open_document(path.clone(), text, 1);

        let full = engine.compute_diagnostics(&path, file_id, memory.report().degraded);
        assert!(
            full.iter()
                .any(|d| d.code.as_ref() == "UNRESOLVED_REFERENCE"),
            "expected full diagnostics to include unresolved reference, got: {full:#?}"
        );

        // Drive the memory manager into high pressure; `skip_expensive_diagnostics` should turn on.
        tracker.set_bytes(memory.budget().total * 86 / 100);
        let degraded_report = memory.report();
        assert!(
            degraded_report.degraded.skip_expensive_diagnostics,
            "expected skip_expensive_diagnostics under high pressure, got: {degraded_report:?}"
        );

        let degraded = engine.compute_diagnostics(&path, file_id, degraded_report.degraded);
        assert!(
            degraded.iter().any(|d| d.code.as_ref() == "SYNTAX"),
            "expected degraded diagnostics to include syntax errors, got: {degraded:#?}"
        );
        assert!(
            !degraded
                .iter()
                .any(|d| d.code.as_ref() == "UNRESOLVED_REFERENCE"),
            "expected degraded diagnostics to skip expensive checks, got: {degraded:#?}"
        );
    }

    #[test]
    fn degraded_mode_caps_completion_candidates() {
        let memory = MemoryManager::new(MemoryBudget::from_total(10 * nova_memory::GB));
        let engine = new_test_engine(memory.clone());

        let registration = engine
            .memory
            .register_tracker("test-pressure", MemoryCategory::Other);
        let tracker = registration.tracker();
        tracker.set_bytes(0);

        let dir = tempfile::tempdir().unwrap();
        let path = VfsPath::local(dir.path().join("Main.java"));

        let mut text = String::new();
        text.push_str("class Main {\n");
        for idx in 0..120usize {
            text.push_str(&format!("  void m{idx}() {{}}\n"));
        }
        text.push_str("  void test() {\n    /*cursor*/\n  }\n}\n");
        let offset = text.find("/*cursor*/").unwrap();

        engine.open_document(path.clone(), text, 1);

        let baseline = engine.completions(&path, offset);
        assert!(
            baseline.len() > 50,
            "expected baseline completions to be large enough to truncate, got {}",
            baseline.len()
        );

        tracker.set_bytes(memory.budget().total * 86 / 100);
        let report = memory.report();
        let cap = report.degraded.completion_candidate_cap;
        assert!(
            report.degraded.skip_expensive_diagnostics,
            "expected high pressure degraded settings, got {report:?}"
        );

        let capped = engine.completions(&path, offset);
        assert_eq!(
            capped.len(),
            cap,
            "expected completions to be truncated to cap={cap}, got {}",
            capped.len()
        );

        // Truncation should preserve ordering (prefix of the baseline list).
        let baseline_labels: Vec<&str> = baseline.iter().map(|item| item.label.as_str()).collect();
        let capped_labels: Vec<&str> = capped.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(
            capped_labels,
            baseline_labels.into_iter().take(cap).collect::<Vec<_>>()
        );
    }

    #[test]
    fn critical_pressure_returns_empty_completions() {
        let memory = MemoryManager::new(MemoryBudget::from_total(10 * nova_memory::GB));
        let engine = new_test_engine(memory.clone());

        let registration = engine
            .memory
            .register_tracker("test-pressure", MemoryCategory::Other);
        let tracker = registration.tracker();

        let dir = tempfile::tempdir().unwrap();
        let path = VfsPath::local(dir.path().join("Main.java"));

        let mut text = String::new();
        text.push_str("class Main {\n");
        for idx in 0..120usize {
            text.push_str(&format!("  void m{idx}() {{}}\n"));
        }
        text.push_str("  void test() {\n    /*cursor*/\n  }\n}\n");
        let offset = text.find("/*cursor*/").unwrap();

        engine.open_document(path.clone(), text, 1);

        tracker.set_bytes(0);
        assert!(
            !engine.completions(&path, offset).is_empty(),
            "expected baseline completions under low pressure"
        );

        // Enter critical pressure (>95% budget usage).
        tracker.set_bytes(memory.budget().total * 96 / 100);
        let report = memory.report();
        assert!(
            matches!(report.pressure, MemoryPressure::Critical),
            "expected critical pressure, got: {report:?}"
        );

        let completions = engine.completions(&path, offset);
        assert!(
            completions.is_empty(),
            "expected no completions under critical pressure, got {}",
            completions.len()
        );
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
    fn open_document_sets_file_is_dirty_and_clears_on_save() {
        let dir = tempfile::tempdir().unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();

        let file_path = root.join("src/A.java");
        fs::write(&file_path, "class A {}").unwrap();

        let workspace = crate::Workspace::open(&root).unwrap();
        let engine = workspace.engine_for_tests();

        let vfs_path = VfsPath::local(file_path.clone());
        let file_id = workspace.open_document(vfs_path.clone(), "class A {}".to_string(), 1);

        engine
            .query_db
            .with_snapshot(|snap| assert!(!snap.file_is_dirty(file_id)));

        workspace
            .apply_changes(&vfs_path, 2, &[ContentChange::full("class A { int x; }")])
            .unwrap();

        engine
            .query_db
            .with_snapshot(|snap| assert!(snap.file_is_dirty(file_id)));

        // Simulate saving the open document to disk: update the file and inject a watcher event.
        fs::write(&file_path, "class A { int x; }").unwrap();
        engine.apply_filesystem_events(vec![NormalizedEvent::Modified(file_path)]);

        engine
            .query_db
            .with_snapshot(|snap| assert!(!snap.file_is_dirty(file_id)));
    }

    #[test]
    fn overlay_document_memory_is_reported_via_memory_manager() {
        // `MemoryCategory::Other` contains multiple components (e.g. salsa input tracking), so use
        // the detailed report to validate the VFS document tracker specifically.
        fn overlay_bytes(memory: &MemoryManager) -> u64 {
            let (_report, components) = memory.report_detailed();
            components
                .iter()
                .find(|c| c.name == "vfs_documents")
                .map(|c| {
                    assert_eq!(c.category, MemoryCategory::Other);
                    c.bytes
                })
                .unwrap_or(0)
        }

        let memory = MemoryManager::new(MemoryBudget::from_total(256 * nova_memory::MB));
        let engine = WorkspaceEngine::new(WorkspaceEngineConfig {
            workspace_root: PathBuf::new(),
            persistence: PersistenceConfig {
                mode: PersistenceMode::Disabled,
                cache: CacheConfig::from_env(),
            },
            memory: memory.clone(),
            build_runner: None,
        });
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
    fn close_document_releases_open_document_pins() {
        use nova_db::salsa::{
            HasItemTreeStore, HasJavaParseStore, HasSyntaxTreeStore, NovaSemantic, NovaSyntax,
        };

        let workspace = crate::Workspace::new_in_memory();
        let engine = workspace.engine_for_tests();

        let tmp = tempfile::tempdir().unwrap();
        let abs = nova_core::AbsPathBuf::new(tmp.path().join("Main.java")).unwrap();
        let uri = nova_core::path_to_file_uri(&abs).unwrap();
        let path = VfsPath::uri(uri);

        let file_id = workspace.open_document(path.clone(), "class Main {}".to_string(), 1);

        let (syntax_store, java_store, item_store) = engine.query_db.with_snapshot(|snap| {
            (
                snap.syntax_tree_store()
                    .expect("syntax tree store should be attached"),
                snap.java_parse_store()
                    .expect("java parse store should be attached"),
                snap.item_tree_store()
                    .expect("item tree store should be attached"),
            )
        });

        // Trigger the memoized queries that write into the open-document pin stores.
        engine.query_db.with_snapshot(|snap| {
            let _ = snap.parse(file_id);
            let _ = snap.parse_java(file_id);
            let _ = snap.item_tree(file_id);
        });

        assert!(syntax_store.contains(file_id));
        assert!(java_store.contains(file_id));
        assert!(item_store.contains(file_id));

        let syntax_bytes_before = syntax_store.tracked_bytes();
        let java_bytes_before = java_store.tracked_bytes();
        let item_tree_bytes_before = item_store.tracked_bytes();
        assert!(syntax_bytes_before > 0);
        assert!(java_bytes_before > 0);
        assert!(item_tree_bytes_before > 0);

        workspace.close_document(&path);

        assert!(!syntax_store.contains(file_id));
        assert!(!java_store.contains(file_id));
        assert!(!item_store.contains(file_id));

        assert!(
            syntax_store.tracked_bytes() < syntax_bytes_before,
            "expected syntax store bytes to decrease after close"
        );
        assert!(
            java_store.tracked_bytes() < java_bytes_before,
            "expected java parse store bytes to decrease after close"
        );
        assert!(
            item_store.tracked_bytes() < item_tree_bytes_before,
            "expected item tree store bytes to decrease after close"
        );
    }

    #[test]
    fn move_events_drop_open_document_pins_for_closed_file_ids() {
        use nova_db::salsa::{
            HasItemTreeStore, HasJavaParseStore, HasSyntaxTreeStore, NovaSemantic, NovaSyntax,
        };

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

        let (syntax_store, java_store, item_store) = engine.query_db.with_snapshot(|snap| {
            (
                snap.syntax_tree_store()
                    .expect("syntax tree store should be attached"),
                snap.java_parse_store()
                    .expect("java parse store should be attached"),
                snap.item_tree_store()
                    .expect("item tree store should be attached"),
            )
        });

        // Open + compute pinned artifacts for A.
        let opened = workspace.open_document(vfs_a.clone(), "class A { overlay }".to_string(), 1);
        assert_eq!(opened, id_a);
        engine.query_db.with_snapshot(|snap| {
            let _ = snap.parse(id_a);
            let _ = snap.parse_java(id_a);
            let _ = snap.item_tree(id_a);
        });

        assert!(syntax_store.contains(id_a));
        assert!(java_store.contains(id_a));
        assert!(item_store.contains(id_a));

        // Move the open document onto an already-known destination id. This will orphan `id_a`.
        fs::remove_file(&file_b).unwrap();
        fs::rename(&file_a, &file_b).unwrap();
        workspace.apply_filesystem_events(vec![NormalizedEvent::Moved {
            from: file_a.clone(),
            to: file_b.clone(),
        }]);

        assert!(
            !syntax_store.contains(id_a),
            "expected syntax tree pin to be dropped for orphaned id"
        );
        assert!(
            !java_store.contains(id_a),
            "expected java parse pin to be dropped for orphaned id"
        );
        assert!(
            !item_store.contains(id_a),
            "expected item tree pin to be dropped for orphaned id"
        );

        // Sanity check that the overlay is still open (under a different id).
        let id_b = engine
            .vfs
            .get_id(&vfs_b)
            .expect("expected B to have a file id");
        assert!(engine.vfs.open_documents().is_open(id_b));

        // Orphaned ids should release their Salsa file contents to avoid retaining large buffers.
        engine.query_db.with_snapshot(|snap| {
            assert!(!snap.file_exists(id_a));
            assert_eq!(snap.file_content(id_a).as_str(), "");
        });
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
            build_runner: None,
        });
        engine.set_workspace_root(&root).unwrap();

        let file_id =
            engine.open_document(VfsPath::local(file_path), "class Main {}".to_string(), 1);
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
            assert_eq!(snap.file_content(file_id).as_str(), "");
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

        // Avoid `Workspace::open` (project discovery + indexing) in this low-level test: we only
        // need `workspace_root` to be set so `project_files` membership logic can run.
        let memory = MemoryManager::new(MemoryBudget::default_for_system_with_env_overrides());
        let engine = WorkspaceEngine::new(WorkspaceEngineConfig {
            workspace_root: root.clone(),
            persistence: PersistenceConfig {
                mode: PersistenceMode::Disabled,
                cache: CacheConfig::from_env(),
            },
            memory,
            build_runner: None,
        });
        {
            let mut state = engine
                .project_state
                .lock()
                .expect("workspace project state mutex poisoned");
            state.workspace_root = Some(root.clone());
            state.source_roots.clear();
        }
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
    fn filesystem_delete_event_does_not_clear_open_document_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        let file = root.join("src/Main.java");
        fs::write(&file, "class Main { disk }".as_bytes()).unwrap();

        let workspace = crate::Workspace::open(root).unwrap();
        let vfs_path = VfsPath::local(file.clone());
        let file_id =
            workspace.open_document(vfs_path.clone(), "class Main { overlay }".to_string(), 1);

        fs::remove_file(&file).unwrap();
        workspace.apply_filesystem_events(vec![NormalizedEvent::Deleted(file.clone())]);

        let engine = workspace.engine_for_tests();
        engine.query_db.with_snapshot(|snap| {
            assert!(snap.file_exists(file_id));
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
    fn directory_move_events_preserve_file_ids_and_open_document_overlays() {
        let dir = tempfile::tempdir().unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let root = dir.path().canonicalize().unwrap();

        let from_dir = root.join("src/main/java/com/foo");
        fs::create_dir_all(&from_dir).unwrap();

        let a_from = from_dir.join("A.java");
        let b_from = from_dir.join("B.java");
        fs::write(&a_from, "class A { disk }".as_bytes()).unwrap();
        fs::write(&b_from, "class B { disk }".as_bytes()).unwrap();

        let workspace = crate::Workspace::open(&root).unwrap();
        let engine = workspace.engine_for_tests();

        let vfs_a_from = VfsPath::local(a_from.clone());
        let vfs_b_from = VfsPath::local(b_from.clone());
        let id_a = engine.vfs.get_id(&vfs_a_from).expect("A file id allocated");
        let id_b = engine.vfs.get_id(&vfs_b_from).expect("B file id allocated");

        // Open A so we can verify overlay preservation across a directory move.
        let opened =
            workspace.open_document(vfs_a_from.clone(), "class A { overlay }".to_string(), 1);
        assert_eq!(opened, id_a);
        assert!(engine.vfs.open_documents().is_open(id_a));

        let to_dir = root.join("src/main/java/com/bar");
        fs::create_dir_all(to_dir.parent().unwrap()).unwrap();
        fs::rename(&from_dir, &to_dir).unwrap();

        let from_event = from_dir.join("..").join("foo");
        let to_event = to_dir.join("..").join("bar");
        workspace.apply_filesystem_events(vec![NormalizedEvent::Moved {
            from: from_event,
            to: to_event,
        }]);

        let a_to = to_dir.join("A.java");
        let b_to = to_dir.join("B.java");
        let vfs_a_to = VfsPath::local(a_to.clone());
        let vfs_b_to = VfsPath::local(b_to.clone());

        assert_eq!(engine.vfs.get_id(&vfs_a_from), None);
        assert_eq!(engine.vfs.get_id(&vfs_b_from), None);
        assert_eq!(engine.vfs.get_id(&vfs_a_to), Some(id_a));
        assert_eq!(engine.vfs.get_id(&vfs_b_to), Some(id_b));

        // Directory moves should not allocate ids for the directory paths themselves.
        assert_eq!(engine.vfs.get_id(&VfsPath::local(from_dir.clone())), None);
        assert_eq!(engine.vfs.get_id(&VfsPath::local(to_dir.clone())), None);

        // Overlay contents should move along with the file.
        assert_eq!(
            engine.vfs.read_to_string(&vfs_a_to).unwrap(),
            "class A { overlay }"
        );

        engine.query_db.with_snapshot(|snap| {
            assert!(snap.file_exists(id_a));
            assert!(snap.file_exists(id_b));
            assert_eq!(
                snap.file_rel_path(id_a).as_str(),
                "src/main/java/com/bar/A.java"
            );
            assert_eq!(
                snap.file_rel_path(id_b).as_str(),
                "src/main/java/com/bar/B.java"
            );
            assert!(snap.project_files(ProjectId::from_raw(0)).contains(&id_a));
            assert!(snap.project_files(ProjectId::from_raw(0)).contains(&id_b));
        });
    }

    #[test]
    fn directory_deletion_events_mark_nested_files_as_missing_and_remove_from_project_files() {
        let dir = tempfile::tempdir().unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let root = dir.path().canonicalize().unwrap();

        let src_dir = root.join("src/main/java/com/example");
        fs::create_dir_all(&src_dir).unwrap();

        let a = src_dir.join("A.java");
        let b = src_dir.join("B.java");
        fs::write(&a, "class A {}".as_bytes()).unwrap();
        fs::write(&b, "class B {}".as_bytes()).unwrap();

        let workspace = crate::Workspace::open(&root).unwrap();
        let engine = workspace.engine_for_tests();
        let project = ProjectId::from_raw(0);

        let id_a = engine.vfs.get_id(&VfsPath::local(a.clone())).expect("A id");
        let id_b = engine.vfs.get_id(&VfsPath::local(b.clone())).expect("B id");
        engine.query_db.with_snapshot(|snap| {
            assert!(snap.project_files(project).contains(&id_a));
            assert!(snap.project_files(project).contains(&id_b));
            assert!(snap.file_exists(id_a));
            assert!(snap.file_exists(id_b));
        });

        fs::remove_dir_all(&src_dir).unwrap();
        let delete_event = src_dir.join("..").join("example");
        workspace.apply_filesystem_events(vec![NormalizedEvent::Deleted(delete_event)]);

        // Directory deletes should not allocate ids for the directory path itself.
        assert_eq!(engine.vfs.get_id(&VfsPath::local(src_dir.clone())), None);

        engine.query_db.with_snapshot(|snap| {
            assert!(!snap.file_exists(id_a));
            assert!(!snap.file_exists(id_b));
            assert!(!snap.project_files(project).contains(&id_a));
            assert!(!snap.project_files(project).contains(&id_b));
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
        engine.query_db.with_snapshot(|snap| {
            assert_eq!(snap.file_content(id_a).as_str(), "class A {}");
            assert_eq!(snap.file_content(id_b).as_str(), "class B {}");
        });

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
            assert_eq!(snap.file_content(id_a).as_str(), "");
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
            assert_eq!(snap.file_content(id_a).as_str(), "");
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
    fn maven_build_integration_populates_classpath_from_nova_build() {
        use std::{collections::HashMap, process::ExitStatus};

        #[derive(Debug)]
        struct MavenEvaluateRoutingRunner {
            outputs: HashMap<String, nova_build::CommandOutput>,
        }

        impl MavenEvaluateRoutingRunner {
            fn new(outputs: HashMap<String, nova_build::CommandOutput>) -> Self {
                Self { outputs }
            }
        }

        impl nova_build::CommandRunner for MavenEvaluateRoutingRunner {
            fn run(
                &self,
                _cwd: &Path,
                _program: &Path,
                args: &[String],
            ) -> std::io::Result<nova_build::CommandOutput> {
                let expression = args
                    .iter()
                    .find_map(|arg| arg.strip_prefix("-Dexpression="))
                    .unwrap_or_default();

                Ok(self.outputs.get(expression).cloned().unwrap_or_else(|| {
                    nova_build::CommandOutput {
                        status: success_status(),
                        stdout: String::new(),
                        stderr: String::new(),
                        truncated: false,
                    }
                }))
            }
        }

        fn success_status() -> ExitStatus {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                ExitStatus::from_raw(0)
            }
            #[cfg(windows)]
            {
                use std::os::windows::process::ExitStatusExt;
                ExitStatus::from_raw(0)
            }
        }

        fn list_output(values: &[&str]) -> nova_build::CommandOutput {
            nova_build::CommandOutput {
                status: success_status(),
                stdout: format!("[{}]\n", values.join(", ")),
                stderr: String::new(),
                truncated: false,
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();

        fs::write(
            root.join("pom.xml"),
            br#"<project><modelVersion>4.0.0</modelVersion></project>"#,
        )
        .unwrap();

        let main_dir = root.join("src/main/java/com/example");
        fs::create_dir_all(&main_dir).unwrap();
        fs::write(
            main_dir.join("Main.java"),
            "package com.example; class Main {}".as_bytes(),
        )
        .unwrap();

        let dep_jar = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../nova-classpath/testdata/dep.jar")
            .canonicalize()
            .unwrap();
        let dep_jar_str = dep_jar.to_string_lossy().to_string();

        let mut outputs = HashMap::new();
        outputs.insert(
            "project.compileClasspathElements".to_string(),
            list_output(&[dep_jar_str.as_str()]),
        );
        outputs.insert(
            "project.testClasspathElements".to_string(),
            list_output(&[dep_jar_str.as_str()]),
        );
        outputs.insert(
            "project.compileSourceRoots".to_string(),
            list_output(&["src/main/java"]),
        );
        outputs.insert(
            "project.testCompileSourceRoots".to_string(),
            list_output(&["src/test/java"]),
        );

        let runner: Arc<dyn nova_build::CommandRunner> =
            Arc::new(MavenEvaluateRoutingRunner::new(outputs));

        let memory = MemoryManager::new(MemoryBudget::default_for_system_with_env_overrides());
        let engine = WorkspaceEngine::new(WorkspaceEngineConfig {
            workspace_root: root.clone(),
            persistence: PersistenceConfig {
                mode: PersistenceMode::Disabled,
                cache: CacheConfig::from_env(),
            },
            memory,
            build_runner: Some(runner),
        });

        engine.set_workspace_root(&root).unwrap();

        engine.query_db.with_snapshot(|snap| {
            let project = ProjectId::from_raw(0);
            let config = snap.project_config(project);
            assert_eq!(config.build_system, BuildSystem::Maven);
            assert!(
                config
                    .classpath
                    .iter()
                    .any(|entry| entry.kind == ClasspathEntryKind::Jar && entry.path == dep_jar),
                "expected build-derived classpath to include {}",
                dep_jar.display()
            );

            let index = snap
                .classpath_index(project)
                .expect("classpath index should be built when nova-build provides jars");
            assert!(
                index.lookup_binary("com.example.dep.Foo").is_some(),
                "expected classpath index to contain classes from {}",
                dep_jar.display()
            );
        });
    }

    #[test]
    fn gradle_build_integration_populates_classpath_from_nova_build() {
        use std::process::ExitStatus;

        #[derive(Debug)]
        struct GradleConfigRunner {
            stdout: String,
        }

        impl nova_build::CommandRunner for GradleConfigRunner {
            fn run(
                &self,
                _cwd: &Path,
                _program: &Path,
                _args: &[String],
            ) -> std::io::Result<nova_build::CommandOutput> {
                Ok(nova_build::CommandOutput {
                    status: success_status(),
                    stdout: self.stdout.clone(),
                    stderr: String::new(),
                    truncated: false,
                })
            }
        }

        fn success_status() -> ExitStatus {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                ExitStatus::from_raw(0)
            }
            #[cfg(windows)]
            {
                use std::os::windows::process::ExitStatusExt;
                ExitStatus::from_raw(0)
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();

        fs::write(root.join("settings.gradle"), "rootProject.name = 'demo'").unwrap();
        fs::write(root.join("build.gradle"), "plugins { id 'java' }").unwrap();

        let main_dir = root.join("src/main/java/com/example");
        fs::create_dir_all(&main_dir).unwrap();
        fs::write(
            main_dir.join("Main.java"),
            "package com.example; class Main {}".as_bytes(),
        )
        .unwrap();

        let dep_jar = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../nova-classpath/testdata/dep.jar")
            .canonicalize()
            .unwrap();

        let json = serde_json::json!({
            "projectDir": root.to_string_lossy(),
            "compileClasspath": [dep_jar.to_string_lossy()],
            "testCompileClasspath": [],
            "mainSourceRoots": [root.join("src/main/java").to_string_lossy()],
            "testSourceRoots": [],
            "mainOutputDirs": [root.join("build/classes/java/main").to_string_lossy()],
            "testOutputDirs": [root.join("build/classes/java/test").to_string_lossy()],
            "sourceCompatibility": "17",
            "targetCompatibility": "17",
            "toolchainLanguageVersion": "17",
            "compileCompilerArgs": [],
            "testCompilerArgs": [],
            "inferModulePath": false,
        });

        let stdout = format!(
            "NOVA_JSON_BEGIN\n{}\nNOVA_JSON_END\n",
            serde_json::to_string(&json).unwrap()
        );

        let runner: Arc<dyn nova_build::CommandRunner> = Arc::new(GradleConfigRunner { stdout });

        let memory = MemoryManager::new(MemoryBudget::default_for_system_with_env_overrides());
        let engine = WorkspaceEngine::new(WorkspaceEngineConfig {
            workspace_root: root.clone(),
            persistence: PersistenceConfig {
                mode: PersistenceMode::Disabled,
                cache: CacheConfig::from_env(),
            },
            memory,
            build_runner: Some(runner),
        });

        engine.set_workspace_root(&root).unwrap();

        engine.query_db.with_snapshot(|snap| {
            let project = ProjectId::from_raw(0);
            let config = snap.project_config(project);
            assert_eq!(config.build_system, BuildSystem::Gradle);
            assert!(
                config
                    .classpath
                    .iter()
                    .any(|entry| entry.kind == ClasspathEntryKind::Jar && entry.path == dep_jar),
                "expected build-derived classpath to include {}",
                dep_jar.display()
            );

            let index = snap
                .classpath_index(project)
                .expect("classpath index should be built when nova-build provides jars");
            assert!(
                index.lookup_binary("com.example.dep.Foo").is_some(),
                "expected classpath index to contain classes from {}",
                dep_jar.display()
            );
        });
    }

    #[test]
    fn gradle_snapshot_reload_updates_classpath_without_reusing_stale_build_config_fields() {
        use std::process::ExitStatus;

        #[derive(Debug)]
        struct GradleConfigRunner {
            stdout: String,
        }

        impl nova_build::CommandRunner for GradleConfigRunner {
            fn run(
                &self,
                _cwd: &Path,
                _program: &Path,
                _args: &[String],
            ) -> std::io::Result<nova_build::CommandOutput> {
                Ok(nova_build::CommandOutput {
                    status: success_status(),
                    stdout: self.stdout.clone(),
                    stderr: String::new(),
                    truncated: false,
                })
            }
        }

        fn success_status() -> ExitStatus {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                ExitStatus::from_raw(0)
            }
            #[cfg(windows)]
            {
                use std::os::windows::process::ExitStatusExt;
                ExitStatus::from_raw(0)
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();

        fs::write(root.join("settings.gradle"), "rootProject.name = 'demo'").unwrap();
        fs::write(root.join("build.gradle"), "plugins { id 'java' }").unwrap();

        let main_dir = root.join("src/main/java/com/example");
        fs::create_dir_all(&main_dir).unwrap();
        fs::write(
            main_dir.join("Main.java"),
            "package com.example; class Main {}".as_bytes(),
        )
        .unwrap();

        let dep_jar = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../nova-classpath/testdata/dep.jar")
            .canonicalize()
            .unwrap();
        let named_module_jar = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../nova-classpath/testdata/named-module.jar")
            .canonicalize()
            .unwrap();

        // Initial nova-build-derived config includes dep.jar.
        let build_json = serde_json::json!({
            "projectDir": root.to_string_lossy(),
            "compileClasspath": [dep_jar.to_string_lossy()],
            "testCompileClasspath": [],
            "mainSourceRoots": [root.join("src/main/java").to_string_lossy()],
            "testSourceRoots": [],
            "mainOutputDirs": [root.join("build/classes/java/main").to_string_lossy()],
            "testOutputDirs": [root.join("build/classes/java/test").to_string_lossy()],
            "sourceCompatibility": "17",
            "targetCompatibility": "17",
            "toolchainLanguageVersion": "17",
            "compileCompilerArgs": [],
            "testCompilerArgs": [],
            "inferModulePath": false,
        });

        let stdout = format!(
            "NOVA_JSON_BEGIN\n{}\nNOVA_JSON_END\n",
            serde_json::to_string(&build_json).unwrap()
        );

        let runner: Arc<dyn nova_build::CommandRunner> = Arc::new(GradleConfigRunner { stdout });

        let memory = MemoryManager::new(MemoryBudget::default_for_system_with_env_overrides());
        let engine = WorkspaceEngine::new(WorkspaceEngineConfig {
            workspace_root: root.clone(),
            persistence: PersistenceConfig {
                mode: PersistenceMode::Disabled,
                cache: CacheConfig::from_env(),
            },
            memory,
            build_runner: Some(runner),
        });

        engine.set_workspace_root(&root).unwrap();
        engine.query_db.with_snapshot(|snap| {
            let project = ProjectId::from_raw(0);
            let config = snap.project_config(project);
            assert_eq!(config.build_system, BuildSystem::Gradle);
            assert!(
                config
                    .classpath
                    .iter()
                    .any(|entry| entry.kind == ClasspathEntryKind::Jar && entry.path == dep_jar),
                "expected initial build-derived classpath to include {}",
                dep_jar.display()
            );
        });

        // Now simulate an updated `.nova/queries/gradle.json` snapshot being written (e.g. by a
        // separate nova-build invocation), changing the resolved classpath.
        let build_files = nova_build::collect_gradle_build_files(&root).unwrap();
        let fingerprint = nova_build::BuildFileFingerprint::from_files(&root, build_files).unwrap();

        let snapshot_path = root.join(".nova").join("queries").join("gradle.json");
        fs::create_dir_all(snapshot_path.parent().unwrap()).unwrap();

        let snapshot_json = serde_json::json!({
            "schemaVersion": 1,
            "buildFingerprint": fingerprint.digest,
            "projects": [{
                "path": ":",
                "projectDir": root.to_string_lossy(),
            }],
            "javaCompileConfigs": {
                ":": {
                    "projectDir": root.to_string_lossy(),
                    "compileClasspath": [named_module_jar.to_string_lossy()],
                    "testClasspath": [],
                    "modulePath": [],
                    "mainSourceRoots": [root.join("src/main/java").to_string_lossy()],
                    "testSourceRoots": [],
                    "mainOutputDir": root.join("build/classes/java/main").to_string_lossy(),
                    "testOutputDir": root.join("build/classes/java/test").to_string_lossy(),
                    "source": "17",
                    "target": "17",
                    "release": "17",
                    "enablePreview": false,
                }
            }
        });
        fs::write(&snapshot_path, serde_json::to_vec_pretty(&snapshot_json).unwrap()).unwrap();

        engine.reload_project_now(&[snapshot_path.clone()]).unwrap();

        engine.query_db.with_snapshot(|snap| {
            let project = ProjectId::from_raw(0);
            let config = snap.project_config(project);
            assert_eq!(config.build_system, BuildSystem::Gradle);
            assert!(
                config.classpath.iter().any(|entry| {
                    entry.kind == ClasspathEntryKind::Jar && entry.path == named_module_jar
                }),
                "expected snapshot-derived classpath to include {}",
                named_module_jar.display()
            );
            assert!(
                !config
                    .classpath
                    .iter()
                    .any(|entry| entry.kind == ClasspathEntryKind::Jar && entry.path == dep_jar),
                "expected snapshot-derived classpath to NOT include stale {}",
                dep_jar.display()
            );

            let index = snap
                .classpath_index(project)
                .expect("classpath index should exist when snapshot provides jars");
            assert!(
                index.lookup_binary("com.example.api.Api").is_some(),
                "expected classpath index to contain classes from {}",
                named_module_jar.display()
            );
        });
    }

    #[test]
    fn project_reload_clears_file_content_for_removed_files() {
        let dir = tempfile::tempdir().unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        let java = root.join("src/Main.java");
        fs::write(&java, "class Main {}".as_bytes()).unwrap();

        let workspace = crate::Workspace::open(&root).unwrap();
        let engine = workspace.engine_for_tests();
        let vfs_path = VfsPath::local(java.clone());
        let file_id = engine.vfs.get_id(&vfs_path).expect("file id allocated");

        engine.query_db.with_snapshot(|snap| {
            assert!(snap.file_exists(file_id));
            assert_eq!(snap.file_content(file_id).as_str(), "class Main {}");
        });

        fs::remove_file(&java).unwrap();
        engine.reload_project_now(&[]).unwrap();

        engine.query_db.with_snapshot(|snap| {
            assert!(!snap.file_exists(file_id));
            assert_eq!(snap.file_content(file_id).as_str(), "");
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
        fs::write(&module_info, "module com.example.one { }".as_bytes()).unwrap();

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
            let workspace = config
                .jpms_workspace
                .as_ref()
                .expect("jpms workspace present");
            assert!(workspace.graph.get(&old_name).is_some());
        });

        fs::write(&module_info, "module com.example.two { }".as_bytes()).unwrap();
        engine.reload_project_now(&[module_info.clone()]).unwrap();

        engine.query_db.with_snapshot(|snap| {
            let config = snap.project_config(project);
            assert_eq!(config.jpms_modules.len(), 1);
            assert_eq!(config.jpms_modules[0].name.as_str(), "com.example.two");

            let workspace = config
                .jpms_workspace
                .as_ref()
                .expect("jpms workspace present");
            assert!(workspace.graph.get(&old_name).is_none());
            assert!(workspace.graph.get(&config.jpms_modules[0].name).is_some());
        });
    }

    #[test]
    fn project_reload_discovers_jdk_index_from_nova_config() {
        nova_config::with_config_env_lock(|| {
            let _config_guard = EnvVarGuard::unset(nova_config::NOVA_CONFIG_ENV_VAR);

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
        });
    }

    #[test]
    fn workspace_load_options_include_build_integration_config_from_nova_config() {
        nova_config::with_config_env_lock(|| {
            let _config_guard = EnvVarGuard::unset(nova_config::NOVA_CONFIG_ENV_VAR);

            let dir = tempfile::tempdir().unwrap();
            // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
            let root = dir.path().canonicalize().unwrap();
            fs::create_dir_all(root.join("src")).unwrap();
            fs::write(root.join("src/Main.java"), "class Main {}".as_bytes()).unwrap();
            fs::write(
                root.join("nova.toml"),
                r#"
[build]
enabled = true
timeout_ms = 1234

[build.maven]
enabled = false
"#,
            )
            .unwrap();

            let workspace = crate::Workspace::open(&root).unwrap();
            let engine = workspace.engine_for_tests();

            let state = engine
                .project_state
                .lock()
                .expect("workspace project state mutex poisoned");
            let build = &state.load_options.nova_config.build;
            assert!(build.enabled);
            assert_eq!(build.timeout_ms, 1234);
            assert!(!build.maven.enabled);
            assert!(build.gradle.enabled);
            assert!(!build.maven_enabled());
            assert!(build.gradle_enabled());
        });
    }

    #[test]
    fn project_reload_resolves_relative_jdk_home_to_workspace_root() {
        nova_config::with_config_env_lock(|| {
            let _config_guard = EnvVarGuard::unset(nova_config::NOVA_CONFIG_ENV_VAR);

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
        // Intentionally include `..` segments to ensure logical normalization doesn't break
        // source-root membership checks.
        let generated_root_non_canonical = root.join("module").join("..").join("custom-generated");
        let generated_file = generated_root.join("Gen.java");
        let event = NormalizedEvent::Created(generated_file.clone());

        let stale_config = engine
            .watch_config
            .read()
            .expect("workspace watch config lock poisoned")
            .clone();
        assert!(
            !stale_config.source_roots.is_empty()
                || !stale_config.generated_source_roots.is_empty(),
            "expected Maven workspace to have configured roots"
        );
        assert!(
            !stale_config
                .generated_source_roots
                .contains(&generated_root),
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
                    "path": generated_root_non_canonical.to_string_lossy(),
                }]
            }]
        });
        let snapshot_path = snapshot_dir.join("generated-roots.json");
        fs::write(
            &snapshot_path,
            serde_json::to_string_pretty(&snapshot).unwrap(),
        )
        .unwrap();

        engine.reload_project_now(&[snapshot_path]).unwrap();

        let current_config = engine
            .watch_config
            .read()
            .expect("workspace watch config lock poisoned")
            .clone();
        assert!(
            current_config
                .generated_source_roots
                .contains(&generated_root),
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

    #[test]
    fn open_documents_reuse_parse_java_across_salsa_memo_eviction() {
        use std::sync::Arc;

        use nova_db::NovaSyntax;
        use nova_memory::MemoryPressure;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        let file = root.join("src/Main.java");
        fs::write(&file, "class Main {}".as_bytes()).unwrap();

        let workspace = crate::Workspace::open(root).unwrap();
        let engine = workspace.engine_for_tests();
        let vfs_path = VfsPath::local(file.clone());

        let file_id =
            workspace.open_document(vfs_path.clone(), "class Main { int x; }".to_string(), 1);
        let text_arc = engine
            .query_db
            .with_snapshot(|snap| snap.file_content(file_id));

        let first = engine
            .query_db
            .with_snapshot(|snap| snap.parse_java(file_id));
        engine.query_db.evict_salsa_memos(MemoryPressure::Critical);
        let second = engine
            .query_db
            .with_snapshot(|snap| snap.parse_java(file_id));
        assert!(
            Arc::ptr_eq(&first, &second),
            "expected parse_java results to be reused for open documents across memo eviction"
        );

        // Closing the document should disable pinning even if the text `Arc` matches.
        workspace.close_document(&vfs_path);
        engine.query_db.set_file_content(file_id, text_arc);
        engine.query_db.evict_salsa_memos(MemoryPressure::Critical);
        let third = engine
            .query_db
            .with_snapshot(|snap| snap.parse_java(file_id));
        assert!(
            !Arc::ptr_eq(&first, &third),
            "expected parse_java results to be recomputed for closed documents after memo eviction"
        );
    }

    #[test]
    fn open_documents_reuse_item_tree_across_salsa_memo_eviction() {
        use std::sync::Arc;

        use nova_db::NovaSemantic;
        use nova_memory::MemoryPressure;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        let file = root.join("src/Main.java");
        fs::write(&file, "class Main {}".as_bytes()).unwrap();

        let workspace = crate::Workspace::open(root).unwrap();
        let engine = workspace.engine_for_tests();
        let vfs_path = VfsPath::local(file.clone());

        let file_id =
            workspace.open_document(vfs_path.clone(), "class Main { int x; }".to_string(), 1);
        let text_arc = engine
            .query_db
            .with_snapshot(|snap| snap.file_content(file_id));

        let first = engine
            .query_db
            .with_snapshot(|snap| snap.item_tree(file_id));
        engine.query_db.evict_salsa_memos(MemoryPressure::Critical);
        let second = engine
            .query_db
            .with_snapshot(|snap| snap.item_tree(file_id));
        assert!(
            Arc::ptr_eq(&first, &second),
            "expected item_tree results to be reused for open documents across memo eviction"
        );

        // Closing the document should disable pinning even if the text `Arc` matches.
        workspace.close_document(&vfs_path);
        engine.query_db.set_file_content(file_id, text_arc);
        engine.query_db.evict_salsa_memos(MemoryPressure::Critical);
        let third = engine
            .query_db
            .with_snapshot(|snap| snap.item_tree(file_id));
        assert!(
            !Arc::ptr_eq(&first, &third),
            "expected item_tree results to be recomputed for closed documents after memo eviction"
        );
    }

    #[test]
    fn diagnostics_use_lazy_db_view_without_building_workspace_snapshot() {
        crate::snapshot::test_reset_workspace_snapshot_from_engine_calls();

        let workspace = crate::Workspace::new_in_memory();
        let rx = workspace.subscribe();
        let dir = tempfile::tempdir().unwrap();
        let file = VfsPath::local(dir.path().join("Main.java"));

        workspace.open_document(
            file.clone(),
            "class Main { void f() { missingSymbol(); } }".to_string(),
            1,
        );

        let mut diagnostics = None;
        for _ in 0..10 {
            let evt = rx.recv_blocking().expect("workspace event");
            if let WorkspaceEvent::DiagnosticsUpdated {
                file: updated,
                diagnostics: diags,
            } = evt
            {
                if updated == file {
                    diagnostics = Some(diags);
                    break;
                }
            }
        }

        let diagnostics = diagnostics.expect("diagnostics published for open document");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.code.as_ref() == "UNRESOLVED_REFERENCE"),
            "expected unresolved reference diagnostic, got: {diagnostics:?}"
        );

        assert_eq!(
            crate::snapshot::test_workspace_snapshot_from_engine_calls(),
            0,
            "diagnostics should not build WorkspaceSnapshot::from_engine"
        );
    }

    #[test]
    fn completions_use_salsa_for_non_open_files_without_building_workspace_snapshot() {
        crate::snapshot::test_reset_workspace_snapshot_from_engine_calls();

        let dir = tempfile::tempdir().unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src/com/example/lib")).unwrap();

        let main_path = root.join("src/Main.java");
        let foo_path = root.join("src/com/example/lib/Foo.java");

        let main_source = r#"import com.example.lib.

class Main {}"#;
        let foo_source_old = r#"package com.example.lib;

public class Foo {}"#;

        fs::write(&main_path, main_source.as_bytes()).unwrap();
        fs::write(&foo_path, foo_source_old.as_bytes()).unwrap();

        let workspace = crate::Workspace::open(&root).unwrap();
        let engine = workspace.engine_for_tests();

        // Ensure both files are registered in the VFS and have Salsa inputs initialized.
        engine.apply_filesystem_events(vec![
            NormalizedEvent::Created(main_path.clone()),
            NormalizedEvent::Created(foo_path.clone()),
        ]);

        let vfs_foo = VfsPath::local(foo_path.clone());
        let foo_id = engine.vfs.get_id(&vfs_foo).expect("file id for Foo");

        // Update Foo.java on disk without notifying the workspace. Completion requests must still
        // see the *old* content via Salsa inputs (no disk IO).
        let foo_source_new = r#"package com.example.lib;

public class Bar {}"#;
        fs::write(&foo_path, foo_source_new.as_bytes()).unwrap();

        // Sanity check: the lazy DB view must still serve the old Salsa contents.
        let view =
            crate::snapshot::WorkspaceDbView::new(engine.query_db.snapshot(), engine.vfs.clone());
        assert!(
            view.file_content(foo_id).contains("class Foo"),
            "expected view to serve old Salsa contents, got: {:?}",
            view.file_content(foo_id)
        );
        assert!(
            !view.file_content(foo_id).contains("class Bar"),
            "did not expect view to read updated disk contents, got: {:?}",
            view.file_content(foo_id)
        );

        // Trigger import completions from Main.java; this scans workspace Java sources and should
        // still see the old `Foo` type name.
        let vfs_main = VfsPath::local(main_path.clone());
        let offset = main_source
            .find("com.example.lib.")
            .expect("main source contains import prefix")
            + "com.example.lib.".len();
        let items = engine.completions(&vfs_main, offset);
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();

        // Under critical memory pressure, the engine may intentionally truncate to 0 candidates.
        // Only assert on concrete items when completions are not capped to zero.
        let cap = engine
            .memory_report_for_work()
            .degraded
            .completion_candidate_cap;
        if cap > 0 {
            assert!(
                labels.iter().any(|l| *l == "Foo"),
                "expected completions to include Foo from Salsa; got {labels:?}"
            );
            assert!(
                !labels.iter().any(|l| *l == "Bar"),
                "did not expect completions to include Bar from disk; got {labels:?}"
            );
        }

        assert_eq!(
            crate::snapshot::test_workspace_snapshot_from_engine_calls(),
            0,
            "completions should not build WorkspaceSnapshot::from_engine"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn manual_watcher_propagates_disk_edits_into_workspace() {
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
        let file = project_root.join("src/Main.java");

        let workspace = crate::Workspace::open(&project_root).unwrap();
        let rx = workspace.subscribe();
        let engine = workspace.engine.clone();

        let vfs_path = VfsPath::local(file.clone());
        let file_id = engine.vfs.get_id(&vfs_path).expect("file id allocated");

        let manual = ManualFileWatcher::new();
        let handle: ManualFileWatcherHandle = manual.handle();
        let _watcher = engine
            .start_watching_with_watcher(Box::new(manual), WatchDebounceConfig::ZERO)
            .unwrap();

        let updated = "class Main { int x; }";
        fs::write(&file, updated.as_bytes()).unwrap();
        handle
            .push(WatchEvent::Changes {
                changes: vec![FileChange::Modified {
                    path: vfs_path.clone(),
                }],
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

    #[tokio::test(flavor = "current_thread")]
    async fn manual_watcher_rescan_triggers_project_reload_and_file_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().join("project");
        fs::create_dir_all(project_root.join("src")).unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let project_root = project_root.canonicalize().unwrap();

        let workspace = crate::Workspace::open(&project_root).unwrap();
        let engine = workspace.engine.clone();

        let manual = ManualFileWatcher::new();
        let handle: ManualFileWatcherHandle = manual.handle();
        let _watcher = engine
            .start_watching_with_watcher(Box::new(manual), WatchDebounceConfig::ZERO)
            .unwrap();

        // Create a new Java file on disk, but simulate the watcher dropping events by sending a
        // `Rescan` signal. The workspace should fall back to a project reload and discover the file.
        let file = project_root.join("src/Main.java");
        let text = "class Main { int x; }";
        fs::write(&file, text.as_bytes()).unwrap();

        handle.push(WatchEvent::Rescan).unwrap();

        let vfs_path = VfsPath::local(file.clone());
        timeout(Duration::from_secs(5), async {
            loop {
                let Some(file_id) = engine.vfs.get_id(&vfs_path) else {
                    yield_now().await;
                    continue;
                };

                let ready = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    engine.query_db.with_snapshot(|snap| {
                        snap.file_exists(file_id)
                            && snap.file_content(file_id).as_str() == text
                            && snap.file_rel_path(file_id).as_str() == "src/Main.java"
                            && snap
                                .project_files(ProjectId::from_raw(0))
                                .contains(&file_id)
                    })
                }))
                .unwrap_or(false);

                if ready {
                    break;
                }

                yield_now().await;
            }
        })
        .await
        .expect("timed out waiting for rescan-triggered project reload");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn manual_watcher_directory_move_triggers_rescan_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().join("project");
        fs::create_dir_all(project_root.join("src")).unwrap();
        let text = "class Main {}";
        fs::write(project_root.join("src/Main.java"), text.as_bytes()).unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let project_root = project_root.canonicalize().unwrap();

        let workspace = crate::Workspace::open(&project_root).unwrap();
        let engine = workspace.engine.clone();

        let old_path = VfsPath::local(project_root.join("src/Main.java"));
        let old_id = engine
            .vfs
            .get_id(&old_path)
            .expect("file id allocated for initial project scan");

        let manual = ManualFileWatcher::new();
        let handle: ManualFileWatcherHandle = manual.handle();
        let _watcher = engine
            .start_watching_with_watcher(Box::new(manual), WatchDebounceConfig::ZERO)
            .unwrap();

        // Rename the `src/` directory on disk and send a watcher event for the directory move.
        let src_dir = project_root.join("src");
        let src2_dir = project_root.join("src2");
        fs::rename(&src_dir, &src2_dir).unwrap();

        handle
            .push(WatchEvent::Changes {
                changes: vec![FileChange::Moved {
                    from: VfsPath::local(src_dir),
                    to: VfsPath::local(src2_dir),
                }],
            })
            .unwrap();

        // The watcher should detect the directory-level move and fall back to a full rescan,
        // discovering the file at its new path.
        let vfs_path = VfsPath::local(project_root.join("src2/Main.java"));
        timeout(Duration::from_secs(5), async move {
            loop {
                let Some(file_id) = engine.vfs.get_id(&vfs_path) else {
                    yield_now().await;
                    continue;
                };

                let ready = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    engine.query_db.with_snapshot(|snap| {
                        snap.file_exists(file_id)
                            && snap.file_content(file_id).as_str() == text
                            && snap.file_rel_path(file_id).as_str() == "src2/Main.java"
                            && snap
                                .project_files(ProjectId::from_raw(0))
                                .contains(&file_id)
                            && (file_id == old_id
                                || (!snap.file_exists(old_id)
                                    && !snap
                                        .project_files(ProjectId::from_raw(0))
                                        .contains(&old_id)))
                    })
                }))
                .unwrap_or(false);

                if ready {
                    break;
                }

                yield_now().await;
            }
        })
        .await
        .expect("timed out waiting for directory-move-triggered project reload");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn manual_watcher_directory_move_with_missing_metadata_triggers_rescan_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().join("project");
        fs::create_dir_all(project_root.join("src")).unwrap();
        fs::write(project_root.join("src/Main.java"), "class Main {}".as_bytes()).unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let project_root = project_root.canonicalize().unwrap();

        let workspace = crate::Workspace::open(&project_root).unwrap();
        let engine = workspace.engine.clone();

        let main_path = VfsPath::local(project_root.join("src/Main.java"));
        let file_id = engine
            .vfs
            .get_id(&main_path)
            .expect("file id allocated for initial project scan");

        let manual = ManualFileWatcher::new();
        let handle: ManualFileWatcherHandle = manual.handle();
        let _watcher = engine
            .start_watching_with_watcher(Box::new(manual), WatchDebounceConfig::ZERO)
            .unwrap();

        // Rename the directory and delete it before emitting the watcher event. This ensures
        // `fs::metadata` returns NotFound for both the `from` and `to` paths when the watcher thread
        // processes the event.
        let src_dir = project_root.join("src");
        let src2_dir = project_root.join("src2");
        fs::rename(&src_dir, &src2_dir).unwrap();
        fs::remove_dir_all(&src2_dir).unwrap();

        handle
            .push(WatchEvent::Changes {
                changes: vec![FileChange::Moved {
                    from: VfsPath::local(src_dir),
                    to: VfsPath::local(src2_dir),
                }],
            })
            .unwrap();

        timeout(Duration::from_secs(5), async move {
            loop {
                let ready = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    engine.query_db.with_snapshot(|snap| {
                        !snap.file_exists(file_id)
                            && !snap
                                .project_files(ProjectId::from_raw(0))
                                .contains(&file_id)
                    })
                }))
                .unwrap_or(false);

                if ready {
                    break;
                }

                yield_now().await;
            }
        })
        .await
        .expect("timed out waiting for missing-metadata directory move to trigger rescan");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn manual_watcher_directory_create_triggers_rescan_fallback() {
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

        let workspace = crate::Workspace::open(&project_root).unwrap();
        let engine = workspace.engine.clone();

        let manual = ManualFileWatcher::new();
        let handle: ManualFileWatcherHandle = manual.handle();
        let _watcher = engine
            .start_watching_with_watcher(Box::new(manual), WatchDebounceConfig::ZERO)
            .unwrap();

        let gen_dir = project_root.join("src").join("gen");
        let gen_file = gen_dir.join("Generated.java");
        let gen_text = "class Generated { int x; }";
        fs::create_dir_all(&gen_dir).unwrap();
        fs::write(&gen_file, gen_text.as_bytes()).unwrap();

        // Simulate the watcher emitting only a directory-level create event. The workspace should
        // fall back to a rescan and discover the new file.
        handle
            .push(WatchEvent::Changes {
                changes: vec![FileChange::Created {
                    path: VfsPath::local(gen_dir),
                }],
            })
            .unwrap();

        let vfs_path = VfsPath::local(gen_file.clone());
        timeout(Duration::from_secs(5), async move {
            loop {
                let Some(file_id) = engine.vfs.get_id(&vfs_path) else {
                    yield_now().await;
                    continue;
                };

                let ready = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    engine.query_db.with_snapshot(|snap| {
                        snap.file_exists(file_id)
                            && snap.file_content(file_id).as_str() == gen_text
                            && snap.file_rel_path(file_id).as_str() == "src/gen/Generated.java"
                            && snap
                                .project_files(ProjectId::from_raw(0))
                                .contains(&file_id)
                    })
                }))
                .unwrap_or(false);

                if ready {
                    break;
                }

                yield_now().await;
            }
        })
        .await
        .expect("timed out waiting for directory-create-triggered project reload");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn manual_watcher_directory_delete_triggers_rescan_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().join("project");
        fs::create_dir_all(project_root.join("src")).unwrap();
        let main_text = "class Main {}";
        fs::write(project_root.join("src/Main.java"), main_text.as_bytes()).unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let project_root = project_root.canonicalize().unwrap();

        let workspace = crate::Workspace::open(&project_root).unwrap();
        let engine = workspace.engine.clone();

        let vfs_path = VfsPath::local(project_root.join("src/Main.java"));
        let file_id = engine.vfs.get_id(&vfs_path).expect("file id allocated");

        let manual = ManualFileWatcher::new();
        let handle: ManualFileWatcherHandle = manual.handle();
        let _watcher = engine
            .start_watching_with_watcher(Box::new(manual), WatchDebounceConfig::ZERO)
            .unwrap();

        // Delete the directory on disk, then inject a directory-level watcher delete event. The
        // workspace should fall back to a full rescan and remove the file from `project_files`.
        let src_dir = project_root.join("src");
        fs::remove_dir_all(&src_dir).unwrap();

        handle
            .push(WatchEvent::Changes {
                changes: vec![FileChange::Deleted {
                    path: VfsPath::local(src_dir),
                }],
            })
            .unwrap();

        timeout(Duration::from_secs(5), async move {
            loop {
                let ready = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    engine.query_db.with_snapshot(|snap| {
                        !snap.file_exists(file_id)
                            && !snap
                                .project_files(ProjectId::from_raw(0))
                                .contains(&file_id)
                    })
                }))
                .unwrap_or(false);

                if ready {
                    break;
                }

                yield_now().await;
            }
        })
        .await
        .expect("timed out waiting for directory-delete-triggered project reload");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn external_nova_config_path_is_watched_and_triggers_reload() {
        let workspace_dir = tempfile::tempdir().unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let workspace_root = workspace_dir.path().canonicalize().unwrap();
        fs::create_dir_all(workspace_root.join("src")).unwrap();
        fs::write(
            workspace_root.join("src/Main.java"),
            "class Main {}".as_bytes(),
        )
        .unwrap();

        let external_dir = tempfile::tempdir().unwrap();
        let external_config_path = external_dir.path().join("external-config.toml");
        fs::write(&external_config_path, "").unwrap();
        let external_config_path = external_config_path.canonicalize().unwrap();

        let workspace = nova_config::with_config_env_lock(|| {
            let _config_guard =
                EnvVarGuard::set(nova_config::NOVA_CONFIG_ENV_VAR, &external_config_path);
            crate::Workspace::open(&workspace_root).unwrap()
        });
        let engine = workspace.engine.clone();

        let config_path = engine
            .watch_config
            .read()
            .expect("workspace watch config lock poisoned")
            .nova_config_path
            .clone()
            .expect("expected discover_config_path to use NOVA_CONFIG_PATH");
        assert_eq!(config_path, external_config_path);

        // Wrap a ManualFileWatcher so tests can observe watch_path calls even after the watcher is
        // moved into the workspace thread.
        struct RecordingWatcher {
            inner: ManualFileWatcher,
            calls: Arc<Mutex<Vec<(PathBuf, WatchMode)>>>,
        }

        impl FileWatcher for RecordingWatcher {
            fn watch_path(&mut self, path: &Path, mode: WatchMode) -> std::io::Result<()> {
                self.calls
                    .lock()
                    .expect("recording watcher calls mutex poisoned")
                    .push((path.to_path_buf(), mode));
                self.inner.watch_path(path, mode)
            }

            fn unwatch_path(&mut self, path: &Path) -> std::io::Result<()> {
                self.inner.unwatch_path(path)
            }

            fn receiver(&self) -> &channel::Receiver<nova_vfs::WatchMessage> {
                self.inner.receiver()
            }
        }

        let watch_calls: Arc<Mutex<Vec<(PathBuf, WatchMode)>>> = Arc::new(Mutex::new(Vec::new()));
        let manual = ManualFileWatcher::new();
        let handle: ManualFileWatcherHandle = manual.handle();
        let watcher = RecordingWatcher {
            inner: manual,
            calls: Arc::clone(&watch_calls),
        };

        let _watcher_handle = engine
            .start_watching_with_watcher(Box::new(watcher), WatchDebounceConfig::ZERO)
            .unwrap();

        // Ensure the watcher is asked to watch the external config file (or its parent) explicitly.
        let watch_calls_for_wait = Arc::clone(&watch_calls);
        let config_path_for_wait = config_path.clone();
        timeout(Duration::from_secs(5), async move {
            loop {
                let calls = watch_calls_for_wait
                    .lock()
                    .expect("recording watcher calls mutex poisoned");
                let parent = config_path_for_wait.parent();
                if calls.iter().any(|(path, mode)| {
                    *mode == WatchMode::NonRecursive
                        && (path == &config_path_for_wait
                            || parent.is_some_and(|p| path.as_path() == p))
                }) {
                    break;
                }
                drop(calls);
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("timed out waiting for watcher to watch external nova config path");

        // Inject a config file change and ensure it is treated as a build change (project reload).
        handle
            .push(WatchEvent::Changes {
                changes: vec![FileChange::Modified {
                    path: VfsPath::local(config_path.clone()),
                }],
            })
            .unwrap();

        let engine_for_wait = Arc::clone(&engine);
        timeout(Duration::from_secs(5), async move {
            loop {
                if engine_for_wait
                    .project_reload_debouncer
                    .cancel(&"workspace-reload")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("timed out waiting for config change to enqueue project reload");

        let mut changed_files = {
            let mut state = engine
                .project_state
                .lock()
                .expect("workspace project state mutex poisoned");
            state.pending_build_changes.drain().collect::<Vec<_>>()
        };
        changed_files.sort();
        assert_eq!(changed_files, vec![config_path]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rescan_event_reloads_project_and_refreshes_file_contents() {
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
        let file = project_root.join("src/Main.java");

        let workspace = crate::Workspace::open(&project_root).unwrap();
        let engine = workspace.engine.clone();
        let vfs_path = VfsPath::local(file.clone());
        let file_id = engine.vfs.get_id(&vfs_path).expect("file id allocated");
        engine.query_db.with_snapshot(|snap| {
            assert_eq!(snap.file_content(file_id).as_str(), "class Main {}");
        });

        let manual = ManualFileWatcher::new();
        let handle: ManualFileWatcherHandle = manual.handle();
        let _watcher = engine
            .start_watching_with_watcher(Box::new(manual), WatchDebounceConfig::ZERO)
            .unwrap();

        // Modify the file on disk without injecting a normal file-change event; the workspace
        // should remain stale until it receives a rescan request.
        fs::write(&file, "class Main { int x; }".as_bytes()).unwrap();
        engine.query_db.with_snapshot(|snap| {
            assert_eq!(snap.file_content(file_id).as_str(), "class Main {}");
        });

        handle.push(WatchEvent::Rescan).unwrap();

        timeout(Duration::from_secs(5), async {
            loop {
                let text = engine
                    .query_db
                    .with_snapshot(|snap| snap.file_content(file_id));
                if text.as_str() == "class Main { int x; }" {
                    break;
                }
                yield_now().await;
            }
        })
        .await
        .expect("timed out waiting for rescan to refresh file_content");
    }
}
