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
use nova_build::{BuildCache, BuildFileFingerprint, BuildManager, BuildSystemKind, CommandRunner};
use nova_cache::normalize_rel_path;
use nova_config::{BuildIntegrationMode, EffectiveConfig};
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
    BuildSystem, ClasspathEntry, ClasspathEntryKind, JavaConfig, JavaVersion, LoadOptions,
    OutputDir, OutputDirKind, ProjectConfig, SourceRoot, SourceRootKind, SourceRootOrigin,
};
#[cfg(test)]
use nova_scheduler::SchedulerConfig;
use nova_scheduler::{
    CancellationToken, Cancelled, Debouncer, KeyedDebouncer, PoolKind, Scheduler,
};
use nova_syntax::{JavaParseStore, SyntaxTreeStore};
use nova_types::{CompletionItem, Diagnostic as NovaDiagnostic, Span};
use nova_vfs::{
    ChangeEvent, ContentChange, DocumentError, FileChange, FileId, FileSystem, FileWatcher,
    LocalFs, NotifyFileWatcher, OpenDocuments, Vfs, VfsPath, WatchEvent, WatchMode,
};

use crate::snapshot::WorkspaceDbView;
use crate::watch::{categorize_event, ChangeCategory, WatchConfig};
use crate::watch_roots::{WatchRootError, WatchRootManager};

fn normalize_vfs_local_path(path: PathBuf) -> PathBuf {
    match VfsPath::local(path) {
        VfsPath::Local(path) => path,
        // `VfsPath::local` always returns the local variant.
        _ => unreachable!("VfsPath::local produced a non-local path"),
    }
}

fn compute_watch_roots(
    workspace_root: &Path,
    watch_config: &WatchConfig,
) -> Vec<(PathBuf, WatchMode)> {
    let workspace_root = normalize_vfs_local_path(workspace_root.to_path_buf());

    let mut roots: Vec<(PathBuf, WatchMode)> = Vec::new();
    roots.push((workspace_root.clone(), WatchMode::Recursive));

    // Explicit external roots are watched recursively. Roots under the workspace root are already
    // covered by the workspace recursive watch.
    for root in watch_config
        .source_roots
        .iter()
        .chain(watch_config.generated_source_roots.iter())
        .chain(watch_config.module_roots.iter())
    {
        let root = normalize_vfs_local_path(root.clone());
        if root.starts_with(&workspace_root) {
            continue;
        }
        roots.push((root, WatchMode::Recursive));
    }

    // Watch the discovered config file when it lives outside the workspace root. Use a
    // non-recursive watch so we don't accidentally watch huge trees like `$HOME`.
    if let Some(config_path) = watch_config.nova_config_path.as_ref() {
        let config_path = normalize_vfs_local_path(config_path.clone());
        if !config_path.starts_with(&workspace_root) {
            roots.push((config_path, WatchMode::NonRecursive));
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
    /// Intended for tests; production callers should generally leave this unset so the workspace
    /// can apply config-driven timeouts.
    ///
    /// When unset, Nova uses a default runner that executes external commands. In `cfg(test)` builds
    /// we default to a runner that returns `NotFound` to avoid invoking real build tools during unit
    /// tests.
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

    fn eviction_priority(&self) -> u8 {
        // Dropping the full in-memory project indexes is a high-UX-impact,
        // expensive-to-rebuild operation; prefer evicting other index caches first.
        10
    }

    fn evict(&self, request: EvictionRequest) -> EvictionResult {
        let before = self.tracker.get().map(|t| t.bytes()).unwrap_or(0);
        if before == 0 {
            return EvictionResult {
                before_bytes: 0,
                after_bytes: 0,
            };
        }

        if before <= request.target_bytes {
            return EvictionResult {
                before_bytes: before,
                after_bytes: before,
            };
        }

        // Under critical pressure, drop everything.
        if request.target_bytes == 0
            || matches!(request.pressure, nova_memory::MemoryPressure::Critical)
        {
            self.clear_indexes();
            let after = self.tracker.get().map(|t| t.bytes()).unwrap_or(0);
            return EvictionResult {
                before_bytes: before,
                after_bytes: after,
            };
        }

        // Best-effort partial retention: keep the most useful subsets (symbols)
        // when we can fit them within the requested target.
        let after_bytes = {
            let mut guard = self
                .indexes
                .lock()
                .expect("workspace indexes lock poisoned");

            let symbols_bytes = guard.symbols.estimated_bytes();
            if symbols_bytes > request.target_bytes {
                // Even the symbol index doesn't fit; fall back to clearing everything.
                *guard = ProjectIndexes::default();
                0
            } else {
                let references_bytes = guard.references.estimated_bytes();
                let inheritance_bytes = guard.inheritance.estimated_bytes();
                let annotations_bytes = guard.annotations.estimated_bytes();

                #[derive(Clone, Copy)]
                enum OptionalIndex {
                    References,
                    Inheritance,
                    Annotations,
                }

                impl OptionalIndex {
                    fn bit(self) -> u8 {
                        match self {
                            Self::References => 1,
                            Self::Inheritance => 2,
                            Self::Annotations => 4,
                        }
                    }

                    fn score(self) -> u8 {
                        // Prefer keeping symbols; secondary indexes are all
                        // "optional". The relative ordering here is a heuristic.
                        match self {
                            Self::References => 1,
                            Self::Inheritance => 1,
                            Self::Annotations => 1,
                        }
                    }
                }

                let optionals = [
                    (OptionalIndex::References, references_bytes),
                    (OptionalIndex::Inheritance, inheritance_bytes),
                    (OptionalIndex::Annotations, annotations_bytes),
                ];

                // Choose the best subset of optional indexes to keep such that
                // `symbols + optionals <= target`.
                //
                // This is a tiny knapsack (3 items), so brute-force is fine and deterministic.
                let mut best_mask: u8 = 0;
                let mut best_score: u8 = 0;
                let mut best_bytes: u64 = symbols_bytes;

                for mask in 0u8..8 {
                    let mut bytes = symbols_bytes;
                    let mut score = 0u8;

                    for (kind, kind_bytes) in optionals {
                        if mask & kind.bit() != 0 {
                            bytes = bytes.saturating_add(kind_bytes);
                            score = score.saturating_add(kind.score());
                        }
                    }

                    if bytes > request.target_bytes {
                        continue;
                    }

                    if score > best_score || (score == best_score && bytes > best_bytes) {
                        best_mask = mask;
                        best_score = score;
                        best_bytes = bytes;
                    }
                }

                // Apply the chosen subset.
                let keep_references = best_mask & OptionalIndex::References.bit() != 0;
                let keep_inheritance = best_mask & OptionalIndex::Inheritance.bit() != 0;
                let keep_annotations = best_mask & OptionalIndex::Annotations.bit() != 0;

                if !keep_references {
                    guard.references = Default::default();
                }
                if !keep_inheritance {
                    guard.inheritance = Default::default();
                }
                if !keep_annotations {
                    guard.annotations = Default::default();
                }

                best_bytes
            }
        };

        if let Some(tracker) = self.tracker.get() {
            tracker.set_bytes(after_bytes);
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
        // Closed-file contents are accounted by `workspace_closed_file_texts`. When the document is
        // opened, restore accounting to the `salsa_inputs` tracker to avoid undercounting and keep
        // the "non-evictable inputs" report accurate.
        self.query_db.set_file_text_suppressed(file_id, false);

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

        // Avoid double-counting closed-file contents: the workspace tracks them in
        // `workspace_closed_file_texts`, so suppress them from the `salsa_inputs` tracker.
        self.query_db.set_file_text_suppressed(file_id, true);

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
        // No longer tracked/evictable: ensure the Salsa input tracker returns to default behavior.
        self.query_db.set_file_text_suppressed(file_id, false);

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

    fn eviction_priority(&self) -> u8 {
        // Closed-file texts are needed to compute semantic queries (imports, typechecking, etc).
        // Prefer evicting memoized query results and other cheap caches before replacing file
        // contents with empty placeholders (which would otherwise force disk reloads).
        20
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
    Batch(ChangeCategory, Vec<FileChange>),
    Rescan,
}

pub(crate) struct WorkspaceEngine {
    vfs: Vfs<LocalFs>,
    overlay_docs_memory_registration: MemoryRegistration,
    pub(crate) query_db: salsa::Database,
    closed_file_texts: Arc<ClosedFileTextStore>,
    workspace_loader: Arc<Mutex<salsa::WorkspaceLoader>>,
    indexes: Arc<Mutex<ProjectIndexes>>,
    indexes_evictor: Arc<WorkspaceProjectIndexesEvictor>,
    build_runner: Arc<dyn CommandRunner>,
    build_runner_is_default: bool,
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
    Refresh,
}

#[derive(Debug)]
struct ProjectState {
    workspace_root: Option<PathBuf>,
    load_options: LoadOptions,
    projects: Vec<ProjectId>,
    project_roots: Vec<ProjectRoots>,
    pending_build_changes: HashSet<PathBuf>,
    last_reload_started_at: Option<Instant>,
}

#[derive(Debug, Clone)]
struct ProjectRoots {
    project: ProjectId,
    source_roots: Vec<SourceRootEntry>,
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
            load_options: LoadOptions::default(),
            projects: Vec::new(),
            project_roots: Vec::new(),
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

#[derive(Debug)]
struct DeadlineCommandRunner {
    deadline: Instant,
    cancellation: Option<CancellationToken>,
    inner: DeadlineCommandRunnerInner,
}

#[derive(Debug)]
enum DeadlineCommandRunnerInner {
    /// Use Nova's default command runner with a per-command timeout equal to the remaining
    /// time budget.
    Default,
    /// Delegate to a caller-supplied runner (primarily for tests).
    Custom(Arc<dyn CommandRunner>),
}

impl CommandRunner for DeadlineCommandRunner {
    fn run(
        &self,
        cwd: &Path,
        program: &Path,
        args: &[String],
    ) -> std::io::Result<nova_build::CommandOutput> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let command = format_command(program, args);
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("command `{command}` skipped because request time budget was exhausted"),
            ));
        }

        match &self.inner {
            DeadlineCommandRunnerInner::Default => {
                let runner = nova_build::DefaultCommandRunner {
                    timeout: Some(remaining),
                    cancellation: self.cancellation.clone(),
                };
                runner.run(cwd, program, args)
            }
            DeadlineCommandRunnerInner::Custom(inner) => inner.run(cwd, program, args),
        }
    }
}

fn format_command(program: &Path, args: &[String]) -> String {
    let mut out = format_command_part(&program.to_string_lossy());
    for arg in args {
        out.push(' ');
        out.push_str(&format_command_part(arg));
    }
    out
}

fn format_command_part(part: &str) -> String {
    if part.contains(' ') || part.contains('\t') {
        format!("\"{}\"", part.replace('"', "\\\""))
    } else {
        part.to_string()
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
        let build_runner_is_default = build_runner.is_none();
        let build_runner = build_runner.unwrap_or_else(default_build_runner);

        let vfs = Vfs::new(LocalFs::new());
        let open_docs = vfs.open_documents();

        let syntax_trees = SyntaxTreeStore::new(&memory, open_docs.clone());

        let query_db = salsa::Database::new_with_persistence_with_open_documents(
            &workspace_root,
            persistence,
            open_docs.clone(),
        );
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
            memory.register_tracker("vfs_overlay_documents", MemoryCategory::Other);
        overlay_docs_memory_registration
            .tracker()
            .set_bytes(vfs.overlay().estimated_bytes() as u64);
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
            workspace_loader: Arc::new(Mutex::new(salsa::WorkspaceLoader::new())),
            indexes,
            indexes_evictor,
            build_runner,
            build_runner_is_default,
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
        {
            let mut state = self
                .project_state
                .lock()
                .expect("workspace project state mutex poisoned");
            state.workspace_root = Some(root.clone());
            state.load_options = LoadOptions::default();
            state.projects.clear();
            state.project_roots.clear();
            state.pending_build_changes.clear();
            state.last_reload_started_at = None;
        }

        {
            let mut loader = self
                .workspace_loader
                .lock()
                .expect("workspace loader mutex poisoned");
            *loader = salsa::WorkspaceLoader::new();
        }

        {
            let mut cfg = self
                .watch_config
                .write()
                .expect("workspace watch config lock poisoned");
            cfg.workspace_root = root.clone();
            cfg.source_roots.clear();
            cfg.generated_source_roots.clear();
            cfg.module_roots.clear();
            cfg.nova_config_path = None;
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
                            WatchCommand::Refresh => {
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
                                    let has_configured_roots = !config.source_roots.is_empty()
                                        || !config.generated_source_roots.is_empty();
                                    let is_within_any_source_root = |path: &Path| {
                                        if has_configured_roots {
                                            config
                                                .source_roots
                                                .iter()
                                                .chain(config.generated_source_roots.iter())
                                                .any(|root| path.starts_with(root))
                                        } else {
                                            path.starts_with(&config.workspace_root)
                                        }
                                    };
                                    let is_ancestor_of_any_source_root = |path: &Path| {
                                        if has_configured_roots {
                                            config
                                                .source_roots
                                                .iter()
                                                .chain(config.generated_source_roots.iter())
                                                .any(|root| root.starts_with(path))
                                        } else {
                                            false
                                        }
                                    };
                                    let is_relevant_dir_for_move_or_delete = |path: &Path| {
                                        is_within_any_source_root(path)
                                            || is_ancestor_of_any_source_root(path)
                                    };
                                    for change in changes {
                                        // NOTE: Directory-level watcher events cannot be safely
                                        // mapped into per-file operations in the VFS. Falling back
                                        // to a full rescan keeps the workspace consistent.
                                        let is_heuristic_directory_change_for_missing_path =
                                            |local: &Path| {
                                                // Safety net when `fs::metadata` fails (e.g. the
                                                // path was deleted before we observed it). Only
                                                // treat extension-less paths that are within (or
                                                // ancestors of) known source roots as potential
                                                // directory-level operations.
                                                local.extension().is_none()
                                                    && is_relevant_dir_for_move_or_delete(local)
                                            };
                                        let mut category: Option<ChangeCategory> = None;
                                        let is_directory_change = match &change {
                                            FileChange::Created { path }
                                            | FileChange::Modified { path } => {
                                                match path.as_local_path() {
                                                    Some(local) => fs::metadata(local)
                                                        .map(|meta| {
                                                            meta.is_dir()
                                                                && is_within_any_source_root(local)
                                                        })
                                                        .unwrap_or(false),
                                                    None => false,
                                                }
                                            }
                                            FileChange::Deleted { path } => {
                                                match path.as_local_path() {
                                                    Some(local) => match fs::metadata(local) {
                                                        Ok(meta) => {
                                                            meta.is_dir()
                                                                && is_relevant_dir_for_move_or_delete(local)
                                                        }
                                                        Err(err)
                                                            if err.kind()
                                                                == std::io::ErrorKind::NotFound =>
                                                        {
                                                            // The directory is already gone, so
                                                            // `metadata` can't tell whether this was
                                                            // a file or directory deletion. Use
                                                            // categorization only to avoid treating
                                                            // build-file deletes (e.g. `BUILD`,
                                                            // `WORKSPACE`) as directory operations.
                                                            category =
                                                                categorize_event(&config, &change);
                                                            // Directory deletes are often observed
                                                            // *after* the directory is removed, so
                                                            // metadata fails. As a safety net,
                                                            // treat extension-less paths that are
                                                            // within (or are ancestors of) known
                                                            // source roots as potential
                                                            // directory-level operations and fall
                                                            // back to a rescan.
                                                            category != Some(ChangeCategory::Build)
                                                                && is_heuristic_directory_change_for_missing_path(local)
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
                                                let any_relevant = from_local
                                                    .is_some_and(is_relevant_dir_for_move_or_delete)
                                                    || to_local
                                                        .is_some_and(is_relevant_dir_for_move_or_delete);
                                                if (from_is_dir || to_is_dir) && any_relevant {
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
                                                    if from_missing && to_missing {
                                                        category =
                                                            categorize_event(&config, &change);
                                                        category != Some(ChangeCategory::Build)
                                                            && (from_local.is_some_and(
                                                                is_heuristic_directory_change_for_missing_path,
                                                            ) || to_local.is_some_and(
                                                                is_heuristic_directory_change_for_missing_path,
                                                            ))
                                                    } else {
                                                        false
                                                    }
                                                }
                                            }
                                        };

                                        if is_directory_change {
                                            saw_directory_event = true;
                                            break;
                                        }

                                        if category.is_none() {
                                            category = categorize_event(&config, &change);
                                        }
                                        if let Some(cat) = category {
                                            debouncer.push(&cat, change, now);
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
                                            path.as_local_path().is_some_and(|path| {
                                                path.extension().and_then(|ext| ext.to_str()) == Some("java")
                                            })
                                        });
                                        if is_java {
                                            java_events.push(ev.clone());
                                        }

                                        for path in ev.paths() {
                                            if let Some(path) = path.as_local_path() {
                                                changed.push(path.to_path_buf());
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

    pub fn apply_filesystem_events(&self, events: Vec<FileChange>) {
        if events.is_empty() {
            return;
        }

        // Normalize incoming watcher paths so:
        // - drive letter case on Windows (`c:` vs `C:`) doesn't affect prefix checks
        // - dot segments (`a/../b`) don't prevent directory-event expansion
        //
        // This is purely lexical normalization via `VfsPath::local` (does not resolve symlinks).

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
            let dir = normalize_vfs_local_path(dir.to_path_buf());
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
                FileChange::Moved { from, to } => {
                    let (Some(from), Some(to)) = (from.as_local_path(), to.as_local_path()) else {
                        continue;
                    };
                    let from = normalize_vfs_local_path(from.to_path_buf());
                    let to = normalize_vfs_local_path(to.to_path_buf());
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
                FileChange::Created { path } | FileChange::Modified { path } => {
                    let Some(path) = path.as_local_path() else {
                        continue;
                    };
                    let path = normalize_vfs_local_path(path.to_path_buf());
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
                FileChange::Deleted { path } => {
                    let Some(path) = path.as_local_path() else {
                        continue;
                    };
                    let path = normalize_vfs_local_path(path.to_path_buf());
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
        let workspace_loader = Arc::clone(&self.workspace_loader);
        let subscribers = Arc::clone(&self.subscribers);
        let build_runner = Arc::clone(&self.build_runner);
        let build_runner_is_default = self.build_runner_is_default;
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
                let cancellation = token.clone();
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
                    &workspace_loader,
                    &project_state,
                    &watch_config,
                    &watcher_command_store,
                    &subscribers,
                    &build_runner,
                    build_runner_is_default,
                    Some(cancellation),
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
            &self.workspace_loader,
            &self.project_state,
            &self.watch_config,
            &self.watcher_command_store,
            &self.subscribers,
            &self.build_runner,
            self.build_runner_is_default,
            None,
        );

        // Ensure we drive eviction after loading/updating a potentially large set of files.
        self.enforce_memory();

        result
    }

    pub fn open_document(&self, path: VfsPath, text: String, version: i32) -> FileId {
        let text = Arc::new(text);
        let file_id = self
            .vfs
            .open_document_arc(path.clone(), Arc::clone(&text), version);
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
                        Some(disk_text) => disk_text.as_str() != text.as_str(),
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
        self.query_db.set_file_content(file_id, text);
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
        // Capture the current overlay contents (if open) so we can transfer them into Salsa without
        // cloning on close.
        let overlay_text = self.vfs.open_document_text_arc(path);
        self.vfs.close_document(path);
        self.sync_overlay_documents_memory();

        if let Some(file_id) = file_id {
            self.ensure_file_inputs(file_id, path);
            let exists = self.vfs.exists(path);
            self.query_db.set_file_exists(file_id, exists);
            let mut synced_to_disk = false;
            if exists {
                // If the document was not dirty when it was closed, the overlay contents should
                // match disk and we can avoid an expensive `read_to_string` by reusing the existing
                // `Arc<String>` allocation.
                let is_dirty = self
                    .query_db
                    .with_snapshot(|snap| snap.file_is_dirty(file_id));
                if !is_dirty {
                    if let Some(text_arc) = overlay_text.as_ref() {
                        self.query_db
                            .set_file_content(file_id, Arc::clone(text_arc));
                        self.closed_file_texts
                            .track_closed_file_content(file_id, text_arc);
                        synced_to_disk = true;
                    }
                }

                if !synced_to_disk {
                    match self.vfs.read_to_string(path) {
                        Ok(text) => {
                            let text_arc = Arc::new(text);
                            self.query_db
                                .set_file_content(file_id, Arc::clone(&text_arc));
                            self.closed_file_texts
                                .track_closed_file_content(file_id, &text_arc);
                            synced_to_disk = true;
                        }
                        Err(_) => {
                            // Best-effort: keep the previous contents if we fail to read during a
                            // transient IO error.
                            if let Some(text_arc) = overlay_text.as_ref() {
                                self.query_db
                                    .set_file_content(file_id, Arc::clone(text_arc));
                                self.closed_file_texts
                                    .track_closed_file_content(file_id, text_arc);
                            }
                        }
                    }
                }
            } else {
                // The overlay was closed and the file doesn't exist on disk; drop the last-known
                // contents to avoid holding onto large inputs for deleted/unsaved buffers.
                self.query_db
                    .set_file_content(file_id, empty_file_content());
                self.closed_file_texts.clear(file_id);
            }
            if !exists || synced_to_disk {
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
        let old_text = self.vfs.open_document_text_arc(path);
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

        let text_for_db = self
            .vfs
            .open_document_text_arc(path)
            .or_else(|| self.vfs.read_to_string(path).ok().map(Arc::new));
        if let Some(text_for_db) = text_for_db {
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
                        .and_then(|old| synthetic_single_edit(old.as_str(), text_for_db.as_str()));
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
        let view = WorkspaceDbView::new(self.query_db.clone(), self.vfs.clone());
        let text = view.file_content(file_id);
        let position = offset_to_lsp_position(text, offset);
        let mut lsp_items = nova_ide::completions(&view, file_id, position);
        // Truncate before mapping into `nova_types::CompletionItem` so we avoid allocating
        // intermediate completion structs (and their strings) that would be dropped immediately.
        lsp_items.truncate(cap);
        lsp_items
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
        let vfs = self.vfs.clone();
        let closed_file_texts = Arc::clone(&self.closed_file_texts);
        let project_state = Arc::clone(&self.project_state);

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

                let projects: Vec<ProjectId> = {
                    let state = project_state
                        .lock()
                        .expect("workspace project state mutex poisoned");
                    if state.projects.is_empty() {
                        vec![ProjectId::from_raw(0)]
                    } else {
                        state.projects.clone()
                    }
                };

                let mut project_files: Vec<FileId> = Vec::new();
                query_db.with_snapshot(|snap| {
                    for &project in &projects {
                        project_files.extend(snap.project_files(project).iter().copied());
                    }
                });
                project_files.sort();
                project_files.dedup();
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
                    BackgroundIndexingMode::Full => {
                        let mut indexes = ProjectIndexes::default();
                        let mut cancelled = false;

                        for &project in &projects {
                            if ctx.token().is_cancelled()
                                || aborted_due_to_memory.load(std::sync::atomic::Ordering::Relaxed)
                            {
                                cancelled = true;
                                break;
                            }

                            let project_indexes = match query_db
                                .with_snapshot_catch_cancelled(|snap| snap.project_indexes(project))
                            {
                                Ok(indexes) => indexes,
                                Err(_) => {
                                    cancelled = true;
                                    break;
                                }
                            };

                            indexes.merge_from((*project_indexes).clone());
                        }

                        if cancelled {
                            Err(Cancelled)
                        } else {
                            Ok(indexes)
                        }
                    }
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
                        if projects.len() == 1 {
                            let _ = query_db.persist_project_indexes(projects[0]);
                        }
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
        ensure_file_inputs(file_id, path, &self.query_db, &self.project_state);
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
                let overlay_text = self.vfs.open_document_text_arc(&vfs_path);
                let is_dirty = match (disk_text, overlay_text) {
                    (Ok(disk), Some(overlay)) => disk.as_str() != overlay.as_str(),
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
                let overlay_text = self.vfs.open_document_text_arc(&to_vfs);
                let text = overlay_text
                    .as_ref()
                    .map(Arc::clone)
                    .or_else(|| self.vfs.read_to_string(&to_vfs).ok().map(Arc::new));
                if let Some(text) = text {
                    self.query_db.set_file_content(file_id, text);
                }
                let disk_text = fs::read_to_string(to);
                let is_dirty = match (disk_text, overlay_text) {
                    (Ok(disk), Some(overlay)) => disk.as_str() != overlay.as_str(),
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
        update_project_files_membership(path, file_id, exists, &self.query_db, &self.project_state);
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
            .set_bytes(self.vfs.overlay().estimated_bytes() as u64);
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
        let view = WorkspaceDbView::new(self.query_db.clone(), self.vfs.clone());
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

        // When closed-file `file_content` is evicted under memory pressure, the Salsa input is
        // replaced with an empty placeholder. In degraded-diagnostics mode we avoid eagerly
        // restoring that input into Salsa (which would retain the allocation), but we still want
        // best-effort syntax diagnostics for the current on-disk contents. Fall back to reading
        // from the VFS for evicted files only.
        let vfs_text = (text.is_empty() && self.closed_file_texts.is_evicted(file_id))
            .then(|| self.vfs.read_to_string(file).ok())
            .flatten();
        let text = vfs_text.as_deref().unwrap_or(text.as_str());

        let mut diagnostics = Vec::new();

        let parse = nova_syntax::parse(text);
        diagnostics.extend(parse.errors.into_iter().map(|e| {
            NovaDiagnostic::error(
                "SYNTAX",
                e.message,
                Some(Span::new(e.range.start as usize, e.range.end as usize)),
            )
        }));

        let java_parse = nova_syntax::parse_java(text);
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
        WatchRootError::WatchFailed { root, mode, error } => {
            publish_to_subscribers(
                subscribers,
                WorkspaceEvent::Status(WorkspaceStatus::IndexingError(format!(
                    "Failed to watch {} ({mode:?}): {error}",
                    root.display(),
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

fn project_source_root_for_path(
    projects: &[ProjectRoots],
    path: &Path,
) -> Option<(ProjectId, SourceRootId)> {
    let mut best: Option<(ProjectId, &SourceRootEntry)> = None;
    for project in projects {
        for root in &project.source_roots {
            if !path.starts_with(&root.path) {
                continue;
            }
            match best {
                None => best = Some((project.project, root)),
                Some((_best_project, best_root))
                    if root.path_components > best_root.path_components =>
                {
                    best = Some((project.project, root));
                }
                Some((_best_project, best_root))
                    if root.path_components == best_root.path_components
                        && root.path < best_root.path =>
                {
                    best = Some((project.project, root));
                }
                Some((best_project, best_root))
                    if root.path_components == best_root.path_components
                        && root.path == best_root.path
                        && project.project.to_raw() < best_project.to_raw() =>
                {
                    best = Some((project.project, root));
                }
                Some(_) => {}
            }
        }
    }
    best.map(|(project, root)| (project, root.id))
}

fn ensure_file_inputs(
    file_id: FileId,
    path: &VfsPath,
    query_db: &salsa::Database,
    project_state: &Arc<Mutex<ProjectState>>,
) {
    let (workspace_root, project_roots) = {
        let state = project_state
            .lock()
            .expect("workspace project state mutex poisoned");
        (state.workspace_root.clone(), state.project_roots.clone())
    };

    let default_project = ProjectId::from_raw(0);
    let default_root = SourceRootId::from_raw(0);

    let (project, source_root) = path
        .as_local_path()
        .and_then(|local| project_source_root_for_path(&project_roots, local))
        .unwrap_or((default_project, default_root));
    query_db.set_file_project(file_id, project);
    query_db.set_source_root(file_id, source_root);

    let rel_path =
        if let (Some(workspace_root), Some(local)) = (workspace_root, path.as_local_path()) {
            rel_path_for_workspace(&workspace_root, local)
                .unwrap_or_else(|| normalize_rel_path(&local.to_string_lossy()))
        } else if let Some(local) = path.as_local_path() {
            normalize_rel_path(&local.to_string_lossy())
        } else {
            normalize_rel_path(&path.to_string())
        };

    // `set_file_rel_path` keeps the non-tracked file-path persistence key in sync.
    query_db.set_file_rel_path(file_id, Arc::new(rel_path));
}

fn update_project_files_membership(
    path: &VfsPath,
    file_id: FileId,
    exists: bool,
    query_db: &salsa::Database,
    project_state: &Arc<Mutex<ProjectState>>,
) {
    let Some(local) = path.as_local_path() else {
        return;
    };
    let is_java = local.extension().and_then(|ext| ext.to_str()) == Some("java");

    let (workspace_root, project_roots, mut projects) = {
        let state = project_state
            .lock()
            .expect("workspace project state mutex poisoned");
        (
            state.workspace_root.clone(),
            state.project_roots.clone(),
            state.projects.clone(),
        )
    };

    if projects.is_empty() {
        projects.push(ProjectId::from_raw(0));
    }

    let target_project = if !is_java {
        None
    } else if let Some((project, _root)) = project_source_root_for_path(&project_roots, local) {
        Some(project)
    } else if project_roots.is_empty()
        && workspace_root
            .as_ref()
            .is_some_and(|workspace_root| local.starts_with(workspace_root))
    {
        Some(ProjectId::from_raw(0))
    } else {
        None
    };

    let should_track = exists && is_java && target_project.is_some();

    if let Some(target) = target_project {
        if !projects.contains(&target) {
            projects.push(target);
        }
    }

    for project in projects {
        let present = should_track && target_project == Some(project);
        update_project_files_for_project(query_db, project, file_id, present);
    }
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

fn update_project_files_for_project(
    query_db: &salsa::Database,
    project: ProjectId,
    file_id: FileId,
    present: bool,
) {
    let current: Vec<FileId> =
        query_db.with_snapshot(|snap| snap.project_files(project).as_ref().clone());
    let mut ids: HashSet<FileId> = current.into_iter().collect();
    if present {
        ids.insert(file_id);
    } else {
        ids.remove(&file_id);
    }

    let mut entries: Vec<(String, FileId)> = Vec::new();
    query_db.with_snapshot(|snap| {
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
    query_db.set_project_files(project, Arc::new(ordered));
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

fn is_nova_config_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    if matches!(name, "nova.toml" | ".nova.toml" | "nova.config.toml") {
        return true;
    }
    name == "config.toml"
        && path
            .strip_prefix(".nova")
            .ok()
            .is_some_and(|rest| rest == Path::new("config.toml"))
}

fn is_build_tool_input_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };

    // Mirror `nova-build`'s build-file fingerprinting exclusions to avoid treating build output /
    // cache directories as project-changing inputs.
    // Bazel output trees can be enormous. Skip any `bazel-*` directories (`bazel-out`, `bazel-bin`,
    // `bazel-testlogs`, `bazel-<workspace>`, etc) at any depth.
    let in_ignored_dir = path.components().any(|c| {
        c.as_os_str() == std::ffi::OsStr::new(".git")
            || c.as_os_str() == std::ffi::OsStr::new(".gradle")
            || c.as_os_str() == std::ffi::OsStr::new("build")
            || c.as_os_str() == std::ffi::OsStr::new("target")
            || c.as_os_str() == std::ffi::OsStr::new(".nova")
            || c.as_os_str() == std::ffi::OsStr::new(".idea")
            || c.as_os_str() == std::ffi::OsStr::new("node_modules")
            || c.as_os_str()
                .to_str()
                .is_some_and(|component| component.starts_with("bazel-"))
    });

    // Gradle script plugins can influence dependencies and tasks.
    if !in_ignored_dir && (name.ends_with(".gradle") || name.ends_with(".gradle.kts")) {
        return true;
    }

    // Gradle version catalogs can define dependency versions.
    //
    // Keep semantics aligned with Gradle build-file fingerprinting (`nova-build-model`), which:
    // - always includes the conventional `libs.versions.toml`
    // - includes additional catalogs only when they are direct children of a `gradle/` directory
    if !in_ignored_dir {
        if name == "libs.versions.toml" {
            return true;
        }
        if name.ends_with(".versions.toml")
            && path
                .parent()
                .and_then(|parent| parent.file_name())
                .is_some_and(|dir| dir == "gradle")
        {
            return true;
        }
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
        && path.ancestors().any(|dir| {
            dir.file_name()
                .is_some_and(|name| name == "dependency-locks")
        })
    {
        return true;
    }

    if name == "pom.xml" {
        return true;
    }

    match name {
        "gradle.properties" => true,
        // Gradle wrapper scripts should only be treated as build inputs at the workspace root (this
        // matches Gradle build-file fingerprinting semantics in `nova-build-model`).
        "gradlew" | "gradlew.bat" => path == Path::new(name),
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

fn should_refresh_build_config(
    workspace_root: &Path,
    module_roots: &[PathBuf],
    changed_files: &[PathBuf],
) -> bool {
    if changed_files.is_empty() {
        return true;
    }

    // Normalize roots to match `nova-vfs` path normalization semantics (drive letter casing +
    // lexical `.`/`..` resolution). This avoids missing prefix matches when module roots are
    // recorded with `..` segments (e.g. `../included`) but file change events arrive in a
    // normalized form.
    let workspace_root = normalize_vfs_local_path(workspace_root.to_path_buf());
    let module_roots: Vec<(PathBuf, usize)> = module_roots
        .iter()
        .cloned()
        .map(normalize_vfs_local_path)
        .map(|root| {
            let len = root.components().count();
            (root, len)
        })
        .collect();

    changed_files.iter().any(|path| {
        let path = normalize_vfs_local_path(path.clone());

        // Many build inputs are detected based on path components (e.g. ignoring `build/` output
        // directories). Use paths relative to the workspace root (or module roots) so absolute
        // parent directories (like `/home/user/build/...`) don't accidentally trip ignore
        // heuristics.
        //
        // Prefer the workspace root when the changed file is inside it. This preserves the
        // intended semantics for root-only build inputs like `gradlew`/`gradlew.bat`.
        let rel = match path.strip_prefix(&workspace_root) {
            Ok(rel) => rel,
            Err(_) => {
                let mut best: Option<(&Path, usize)> = None;
                for (root, root_len) in &module_roots {
                    if let Ok(stripped) = path.strip_prefix(root) {
                        if best.map(|(_, best_len)| *root_len > best_len).unwrap_or(true) {
                            best = Some((stripped, *root_len));
                        }
                    }
                }
                best.map(|(rel, _)| rel).unwrap_or(path.as_path())
            }
        };

        is_build_tool_input_file(rel) || is_nova_config_file(rel)
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
    config.java = java_config_from_java_compile_config(base.java, cfg);
    config
}

fn java_config_from_java_compile_config(
    base: JavaConfig,
    cfg: &nova_build::JavaCompileConfig,
) -> JavaConfig {
    let release = cfg
        .release
        .as_deref()
        .and_then(|release| JavaVersion::parse(release));
    let source = cfg
        .source
        .as_deref()
        .and_then(|source| JavaVersion::parse(source));
    let target = cfg
        .target
        .as_deref()
        .and_then(|target| JavaVersion::parse(target));

    let mut java = base;
    match (release, source, target) {
        (Some(version), _, _) => {
            java.source = version;
            java.target = version;
        }
        (None, Some(version), None) | (None, None, Some(version)) => {
            java.source = version;
            java.target = version;
        }
        (None, Some(source), Some(target)) => {
            java.source = source;
            java.target = target;
        }
        (None, None, None) => {}
    }
    java.enable_preview |= cfg.enable_preview;
    java
}

fn reuse_previous_build_config_fields(
    mut loaded: ProjectConfig,
    previous: &ProjectConfig,
) -> ProjectConfig {
    loaded.classpath = previous.classpath.clone();
    loaded.module_path = previous.module_path.clone();
    loaded.output_dirs = previous.output_dirs.clone();
    loaded.java = previous.java;

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

fn cached_java_compile_config(
    workspace_root: &Path,
    kind: BuildSystemKind,
    cache_root: &Path,
) -> Option<nova_build::JavaCompileConfig> {
    let files = match kind {
        BuildSystemKind::Maven => nova_build::collect_maven_build_files(workspace_root).ok()?,
        BuildSystemKind::Gradle => nova_build::collect_gradle_build_files(workspace_root).ok()?,
    };
    let fingerprint = BuildFileFingerprint::from_files(workspace_root, files).ok()?;
    let cache = BuildCache::new(cache_root);
    let module = cache
        .get_module(workspace_root, kind, &fingerprint, "<root>")
        .ok()
        .flatten()?;

    module.java_compile_config.or_else(|| {
        module
            .classpath
            .map(|classpath| nova_build::JavaCompileConfig {
                compile_classpath: classpath,
                ..nova_build::JavaCompileConfig::default()
            })
    })
}

fn reload_project_and_sync(
    workspace_root: &Path,
    changed_files: &[PathBuf],
    vfs: &Vfs<LocalFs>,
    query_db: &salsa::Database,
    closed_file_texts: &ClosedFileTextStore,
    workspace_loader: &Arc<Mutex<salsa::WorkspaceLoader>>,
    project_state: &Arc<Mutex<ProjectState>>,
    watch_config: &Arc<RwLock<WatchConfig>>,
    watcher_command_store: &Arc<Mutex<Option<channel::Sender<WatchCommand>>>>,
    subscribers: &Arc<Mutex<Vec<Sender<WorkspaceEvent>>>>,
    build_runner: &Arc<dyn CommandRunner>,
    build_runner_is_default: bool,
    cancellation: Option<CancellationToken>,
) -> Result<()> {
    let mut file_id_for_path = |path: &Path| vfs.file_id(VfsPath::local(path.to_path_buf()));

    let gradle_snapshot_changed = changed_files
        .iter()
        .any(|path| path.ends_with(Path::new(nova_build_model::GRADLE_SNAPSHOT_REL_PATH)));

    let (previous_maven_mode, previous_gradle_mode) = {
        let state = project_state
            .lock()
            .expect("workspace project state mutex poisoned");
        (
            state.load_options.nova_config.build.maven_mode(),
            state.load_options.nova_config.build.gradle_mode(),
        )
    };

    // Load Nova config early so build integration gating/timeouts pick up changes to `nova.toml`
    // during the current reload.
    let nova_config_path = nova_config::discover_config_path(workspace_root);
    let (workspace_config, loaded_config_path) = nova_config::load_for_workspace(workspace_root)
        .unwrap_or_else(|_| {
            // If config loading fails, fall back to defaults; the workspace should still open.
            (nova_config::NovaConfig::default(), nova_config_path.clone())
        });
    {
        let mut state = project_state
            .lock()
            .expect("workspace project state mutex poisoned");
        state.load_options.nova_config = workspace_config.clone();
        state.load_options.nova_config_path = loaded_config_path.clone();
    }

    // 1) Load/reload workspace model via the shared `WorkspaceLoader`.
    //
    // Capture the previous per-project configs/indexes (when possible) so Maven/Gradle build-derived
    // config fields can be reused on reloads that do not modify build inputs.
    let (projects, previous_configs, previous_classpath_indexes) = {
        let mut loader = workspace_loader
            .lock()
            .expect("workspace loader mutex poisoned");

        let should_reload = loader
            .workspace_root()
            .is_some_and(|root| root == workspace_root);

        let previous_projects = if should_reload {
            loader.projects()
        } else {
            Vec::new()
        };
        let (previous_configs, previous_classpath_indexes) = if should_reload {
            query_db.with_snapshot(|snap| {
                let mut configs: HashMap<ProjectId, Arc<ProjectConfig>> = HashMap::new();
                let mut indexes: HashMap<ProjectId, Option<Arc<nova_classpath::ClasspathIndex>>> =
                    HashMap::new();
                for project in &previous_projects {
                    configs.insert(*project, snap.project_config(*project));
                    indexes.insert(*project, snap.classpath_index(*project).map(|index| index.0));
                }
                (configs, indexes)
            })
        } else {
            (HashMap::new(), HashMap::new())
        };

        // Treat an empty `changed_files` slice as an unknown change set (full rescan). In that
        // case, re-run a full `load` so all file contents are refreshed from disk.
        if should_reload && !changed_files.is_empty() {
            loader
                .reload(query_db, changed_files, &mut file_id_for_path)
                .map_err(anyhow::Error::new)
                .with_context(|| {
                    format!(
                        "failed to reload workspace model at {}",
                        workspace_root.display()
                    )
                })?;
        } else {
            loader
                .load(query_db, workspace_root, &mut file_id_for_path)
                .map_err(anyhow::Error::new)
                .with_context(|| {
                    format!(
                        "failed to load workspace model at {}",
                        workspace_root.display()
                    )
                })?;
        }

        (loader.projects(), previous_configs, previous_classpath_indexes)
    };

    // 2) Optional build tool integration (Maven/Gradle).
    //
    // This is intentionally best-effort: failures should not prevent the workspace from loading.
    if !projects.is_empty() {
        let loaded_project_configs: Vec<(ProjectId, Arc<ProjectConfig>)> = query_db
            .with_snapshot(|snap| projects.iter().map(|&project| (project, snap.project_config(project))).collect());

        let mut module_roots: Vec<PathBuf> = loaded_project_configs
            .iter()
            .flat_map(|(_project, cfg)| cfg.modules.iter().map(|m| m.root.clone()))
            .collect();
        module_roots.sort();
        module_roots.dedup();

        let refresh_build_by_files =
            should_refresh_build_config(workspace_root, &module_roots, changed_files);
        let invalidate_build_cache = !changed_files.is_empty()
            && changed_files.iter().any(|path| {
                let mut best: Option<&Path> = None;
                let mut best_len = 0usize;
                for root in
                    std::iter::once(workspace_root).chain(module_roots.iter().map(PathBuf::as_path))
                {
                    if let Ok(stripped) = path.strip_prefix(root) {
                        let len = root.components().count();
                        if len > best_len {
                            best_len = len;
                            best = Some(stripped);
                        }
                    }
                }
                let rel = best.unwrap_or(path.as_path());
                is_build_tool_input_file(rel)
            });

        let build_integration_cfg = &workspace_config.build;
        let maven_mode = build_integration_cfg.maven_mode();
        let gradle_mode = build_integration_cfg.gradle_mode();
        let maven_timeout = build_integration_cfg.maven_timeout();
        let gradle_timeout = build_integration_cfg.gradle_timeout();

        let mode_rank = |mode: BuildIntegrationMode| match mode {
            BuildIntegrationMode::Off => 0u8,
            BuildIntegrationMode::Auto => 1u8,
            BuildIntegrationMode::On => 2u8,
        };

        // Treat build integration mode changes (e.g. `auto` -> `on`) as a signal to refresh build
        // metadata even when the file change set doesn't include build tool inputs. Otherwise, the
        // workspace would keep using heuristic classpaths until a build file changes (or the
        // workspace is restarted).
        let refresh_maven = refresh_build_by_files
            || mode_rank(maven_mode) > mode_rank(previous_maven_mode);
        let refresh_gradle = refresh_build_by_files
            || mode_rank(gradle_mode) > mode_rank(previous_gradle_mode);

        let has_build_projects = loaded_project_configs.iter().any(|(_, cfg)| {
            matches!(
                cfg.build_system,
                BuildSystem::Maven | BuildSystem::Gradle
            )
        });

        if invalidate_build_cache && has_build_projects {
            let cache_dir = build_cache_dir(workspace_root, query_db);
            // Best-effort invalidation: if removing the cache fails (permissions, etc), continue
            // with the existing workspace config rather than failing reload.
            let build = BuildManager::new(cache_dir);
            if let Err(err) = build.reload_project(workspace_root) {
                tracing::warn!(
                    "failed to invalidate nova-build cache for {}: {err}",
                    workspace_root.display()
                );
            }
        }

        let apply_compile_config = |project: ProjectId,
                                    base: &ProjectConfig,
                                    cfg: &nova_build::JavaCompileConfig| {
            let base = base.clone();
            let updated =
                apply_java_compile_config_to_project_config(base.clone(), cfg, &base);
            query_db.set_project_config(project, Arc::new(updated.clone()));

            let requested_release = Some(updated.java.target.0)
                .filter(|release| *release >= 1)
                .or_else(|| Some(updated.java.source.0).filter(|release| *release >= 1));

            let classpath_entries: Vec<nova_classpath::ClasspathEntry> = updated
                .classpath
                .iter()
                .chain(updated.module_path.iter())
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
        };

        // ---------------------------------------------------------------------
        // Maven integration.
        // ---------------------------------------------------------------------
        let maven_projects: Vec<(ProjectId, Arc<ProjectConfig>)> = loaded_project_configs
            .iter()
            .filter(|(_, cfg)| cfg.build_system == BuildSystem::Maven)
            .cloned()
            .collect();

        if !maven_projects.is_empty() {
            if refresh_maven {
                match maven_mode {
                    BuildIntegrationMode::Off => {}
                    BuildIntegrationMode::Auto => {
                        let cache_dir = build_cache_dir(workspace_root, query_db);
                        if let Ok(files) = nova_build::collect_maven_build_files(workspace_root) {
                            if let Ok(fingerprint) =
                                BuildFileFingerprint::from_files(workspace_root, files)
                            {
                                let cache = BuildCache::new(cache_dir.as_path());
                                let single_maven_project = maven_projects.len() == 1;
                                for (project, current_config) in &maven_projects {
                                    let Some(module_root) =
                                        current_config.modules.first().map(|m| m.root.as_path())
                                    else {
                                        continue;
                                    };
                                    let module_key = if module_root == workspace_root {
                                        if single_maven_project {
                                            "<root>".to_string()
                                        } else {
                                            ".".to_string()
                                        }
                                    } else {
                                        let Ok(rel) = module_root.strip_prefix(workspace_root)
                                        else {
                                            continue;
                                        };
                                        rel.to_string_lossy().to_string()
                                    };

                                    let module = cache
                                        .get_module(
                                            workspace_root,
                                            BuildSystemKind::Maven,
                                            &fingerprint,
                                            &module_key,
                                        )
                                        .ok()
                                        .flatten();
                                    let Some(module) = module else {
                                        continue;
                                    };
                                    if let Some(cfg) = module.java_compile_config.or_else(|| {
                                        module.classpath.map(|classpath| nova_build::JavaCompileConfig {
                                            compile_classpath: classpath,
                                            ..nova_build::JavaCompileConfig::default()
                                        })
                                    }) {
                                        apply_compile_config(*project, current_config, &cfg);
                                    }
                                }
                            }
                        }
                    }
                    BuildIntegrationMode::On => {
                        let cache_dir = build_cache_dir(workspace_root, query_db);
                        let deadline = Instant::now() + maven_timeout;
                        let runner: Arc<dyn CommandRunner> = if build_runner_is_default {
                            #[cfg(not(test))]
                            {
                                Arc::new(DeadlineCommandRunner {
                                    deadline,
                                    cancellation: cancellation.clone(),
                                    inner: DeadlineCommandRunnerInner::Default,
                                })
                            }
                            #[cfg(test)]
                            {
                                Arc::new(DeadlineCommandRunner {
                                    deadline,
                                    cancellation: cancellation.clone(),
                                    inner: DeadlineCommandRunnerInner::Custom(Arc::clone(
                                        build_runner,
                                    )),
                                })
                            }
                        } else {
                            Arc::new(DeadlineCommandRunner {
                                deadline,
                                cancellation: cancellation.clone(),
                                inner: DeadlineCommandRunnerInner::Custom(Arc::clone(build_runner)),
                            })
                        };

                        let build = BuildManager::with_runner(cache_dir, runner);
                        let single_maven_project = maven_projects.len() == 1;

                        for (project, current_config) in &maven_projects {
                            let Some(module_root) =
                                current_config.modules.first().map(|m| m.root.as_path())
                            else {
                                continue;
                            };
                            let module_rel = if module_root == workspace_root {
                                PathBuf::from(".")
                            } else {
                                let Ok(rel) = module_root.strip_prefix(workspace_root) else {
                                    continue;
                                };
                                rel.to_path_buf()
                            };

                            let module_relative = if module_root == workspace_root
                                && single_maven_project
                            {
                                None
                            } else {
                                Some(module_rel.as_path())
                            };

                            match build.java_compile_config_maven(workspace_root, module_relative) {
                                Ok(cfg) => apply_compile_config(*project, current_config, &cfg),
                                Err(err) => publish_to_subscribers(
                                    subscribers,
                                    WorkspaceEvent::Status(WorkspaceStatus::IndexingError(format!(
                                        "Build tool classpath extraction failed; falling back to heuristic project config: {err}"
                                    ))),
                                ),
                            }
                        }
                    }
                }
            } else if maven_mode != BuildIntegrationMode::Off {
                for (project, current_config) in &maven_projects {
                    let previous_config = previous_configs.get(project);
                    let previous_config_ok = previous_config.is_some_and(|previous_config| {
                        previous_config.build_system == current_config.build_system
                            && previous_config.workspace_root == current_config.workspace_root
                            && !previous_config.workspace_root.as_os_str().is_empty()
                    });
                    if !previous_config_ok {
                        continue;
                    }
                    if let Some(previous_config) = previous_config {
                        let merged = reuse_previous_build_config_fields(
                            (**current_config).clone(),
                            previous_config,
                        );
                        query_db.set_project_config(*project, Arc::new(merged));
                        let index = previous_classpath_indexes
                            .get(project)
                            .cloned()
                            .unwrap_or(None);
                        query_db.set_classpath_index(*project, index);
                    }
                }
            }
        }

        // ---------------------------------------------------------------------
        // Gradle integration.
        // ---------------------------------------------------------------------
        let gradle_projects: Vec<(ProjectId, Arc<ProjectConfig>)> = loaded_project_configs
            .iter()
            .filter(|(_, cfg)| cfg.build_system == BuildSystem::Gradle)
            .cloned()
            .collect();

        let canonicalize = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

        if !gradle_projects.is_empty() {
            if refresh_gradle {
                match gradle_mode {
                    BuildIntegrationMode::Off => {}
                    BuildIntegrationMode::Auto => {
                        let cache_dir = build_cache_dir(workspace_root, query_db);
                        // Preserve existing single-project behavior: use the `<root>` cache entry
                        // (written by `java_compile_config_gradle(project_path=None)`) instead of
                        // requiring cached Gradle project directory metadata.
                        if gradle_projects.len() == 1 {
                            if let Some(cfg) = cached_java_compile_config(
                                workspace_root,
                                BuildSystemKind::Gradle,
                                &cache_dir,
                            ) {
                                let (project, current_config) = &gradle_projects[0];
                                apply_compile_config(*project, current_config, &cfg);
                            }
                        } else if let Ok(files) = nova_build::collect_gradle_build_files(workspace_root)
                        {
                            if let Ok(fingerprint) =
                                BuildFileFingerprint::from_files(workspace_root, files)
                            {
                                let cache = BuildCache::new(cache_dir.as_path());
                                let data = cache
                                    .load(workspace_root, BuildSystemKind::Gradle, &fingerprint)
                                    .ok()
                                    .flatten();
                                if let Some(data) = data {
                                    if let Some(projects) = data.projects {
                                        let mut dir_to_path: HashMap<PathBuf, String> =
                                            HashMap::new();
                                        for project in projects {
                                            dir_to_path
                                                .entry(canonicalize(&project.dir))
                                                .or_insert(project.path);
                                        }

                                        for (project, current_config) in &gradle_projects {
                                            let Some(module_root) = current_config
                                                .modules
                                                .first()
                                                .map(|m| m.root.as_path())
                                            else {
                                                continue;
                                            };
                                            let module_root = canonicalize(module_root);
                                            let Some(project_path) = dir_to_path.get(&module_root)
                                            else {
                                                continue;
                                            };

                                            let module = if project_path.as_str() == ":" {
                                                cache
                                                    .get_module(
                                                        workspace_root,
                                                        BuildSystemKind::Gradle,
                                                        &fingerprint,
                                                        ":",
                                                    )
                                                    .ok()
                                                    .flatten()
                                                    .or_else(|| {
                                                        cache
                                                            .get_module(
                                                                workspace_root,
                                                                BuildSystemKind::Gradle,
                                                                &fingerprint,
                                                                "<root>",
                                                            )
                                                            .ok()
                                                            .flatten()
                                                    })
                                            } else {
                                                cache
                                                    .get_module(
                                                        workspace_root,
                                                        BuildSystemKind::Gradle,
                                                        &fingerprint,
                                                        project_path,
                                                    )
                                                    .ok()
                                                    .flatten()
                                            };
                                            let Some(module) = module else {
                                                continue;
                                            };
                                            if let Some(cfg) =
                                                module.java_compile_config.or_else(|| {
                                                    module.classpath.map(|classpath| {
                                                        nova_build::JavaCompileConfig {
                                                            compile_classpath: classpath,
                                                            ..nova_build::JavaCompileConfig::default()
                                                        }
                                                    })
                                                })
                                            {
                                                apply_compile_config(
                                                    *project,
                                                    current_config,
                                                    &cfg,
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    BuildIntegrationMode::On => {
                        let cache_dir = build_cache_dir(workspace_root, query_db);
                        let deadline = Instant::now() + gradle_timeout;
                        let runner: Arc<dyn CommandRunner> = if build_runner_is_default {
                            #[cfg(not(test))]
                            {
                                Arc::new(DeadlineCommandRunner {
                                    deadline,
                                    cancellation: cancellation.clone(),
                                    inner: DeadlineCommandRunnerInner::Default,
                                })
                            }
                            #[cfg(test)]
                            {
                                Arc::new(DeadlineCommandRunner {
                                    deadline,
                                    cancellation: cancellation.clone(),
                                    inner: DeadlineCommandRunnerInner::Custom(Arc::clone(
                                        build_runner,
                                    )),
                                })
                            }
                        } else {
                            Arc::new(DeadlineCommandRunner {
                                deadline,
                                cancellation: cancellation.clone(),
                                inner: DeadlineCommandRunnerInner::Custom(Arc::clone(build_runner)),
                            })
                        };

                        let build = BuildManager::with_runner(cache_dir.clone(), runner);

                        // Preserve existing single-project behavior: invoke the per-root Gradle
                        // query (`NOVA_JSON_BEGIN/END`) instead of requiring the batch JSON output.
                        if gradle_projects.len() == 1 {
                            let (project, current_config) = &gradle_projects[0];
                            match build.java_compile_config_gradle(workspace_root, None) {
                                Ok(cfg) => apply_compile_config(*project, current_config, &cfg),
                                Err(err) => publish_to_subscribers(
                                    subscribers,
                                    WorkspaceEvent::Status(WorkspaceStatus::IndexingError(format!(
                                        "Build tool classpath extraction failed; falling back to heuristic project config: {err}"
                                    ))),
                                ),
                            }
                        } else {
                            let configs = match build.java_compile_configs_all_gradle(workspace_root)
                            {
                                Ok(configs) => configs,
                                Err(err) => {
                                    publish_to_subscribers(
                                        subscribers,
                                        WorkspaceEvent::Status(WorkspaceStatus::IndexingError(
                                            format!(
                                                "Build tool classpath extraction failed; falling back to heuristic project config: {err}"
                                            ),
                                        )),
                                    );
                                    Vec::new()
                                }
                            };

                            let configs_by_path: HashMap<String, nova_build::JavaCompileConfig> =
                                configs.into_iter().collect();

                            // Map module roots to Gradle project paths using cached Gradle project
                            // directories (written by `java_compile_configs_all_gradle`).
                            let gradle_dir_map: Option<HashMap<PathBuf, String>> = (|| {
                                let files =
                                    nova_build::collect_gradle_build_files(workspace_root).ok()?;
                                let fingerprint =
                                    BuildFileFingerprint::from_files(workspace_root, files).ok()?;
                                let cache = BuildCache::new(&cache_dir);
                                let data = cache
                                    .load(workspace_root, BuildSystemKind::Gradle, &fingerprint)
                                    .ok()
                                    .flatten()?;
                                let projects = data.projects?;
                                let mut map = HashMap::new();
                                for project in projects {
                                    map.insert(canonicalize(&project.dir), project.path);
                                }
                                Some(map)
                            })();

                            if let Some(dir_to_path) = gradle_dir_map {
                                for (project, current_config) in &gradle_projects {
                                    let Some(module_root) =
                                        current_config.modules.first().map(|m| m.root.as_path())
                                    else {
                                        continue;
                                    };
                                    let module_root = canonicalize(module_root);
                                    let Some(project_path) = dir_to_path.get(&module_root) else {
                                        continue;
                                    };
                                    let Some(cfg) = configs_by_path.get(project_path) else {
                                        continue;
                                    };
                                    apply_compile_config(*project, current_config, cfg);
                                }
                            }
                        }
                    }
                }
            } else if gradle_mode != BuildIntegrationMode::Off && !gradle_snapshot_changed {
                for (project, current_config) in &gradle_projects {
                    let previous_config = previous_configs.get(project);
                    let previous_config_ok = previous_config.is_some_and(|previous_config| {
                        previous_config.build_system == current_config.build_system
                            && previous_config.workspace_root == current_config.workspace_root
                            && !previous_config.workspace_root.as_os_str().is_empty()
                    });
                    if !previous_config_ok {
                        continue;
                    }
                    if let Some(previous_config) = previous_config {
                        let merged = reuse_previous_build_config_fields(
                            (**current_config).clone(),
                            previous_config,
                        );
                        query_db.set_project_config(*project, Arc::new(merged));
                        let index = previous_classpath_indexes
                            .get(project)
                            .cloned()
                            .unwrap_or(None);
                        query_db.set_classpath_index(*project, index);
                    }
                }
            }
        }
    }

    // Snapshot configs for root calculations.
    let project_configs: Vec<(ProjectId, Arc<ProjectConfig>)> = query_db.with_snapshot(|snap| {
        projects
            .iter()
            .map(|&project| (project, snap.project_config(project)))
            .collect()
    });

    // 3) Collect per-project roots and watcher roots based on loaded configs.
    let (project_roots, watch_source_roots, watch_generated_roots, watch_module_roots) = {
        let mut loader = workspace_loader
            .lock()
            .expect("workspace loader mutex poisoned");

        let mut watch_source_roots = Vec::new();
        let mut watch_generated_roots = Vec::new();
        let mut watch_module_roots = Vec::new();
        let mut project_roots = Vec::new();

        for (project, cfg) in project_configs {
            let (src_roots, gen_roots, module_roots) = watch_roots_from_project_config(&cfg);
            watch_source_roots.extend(src_roots);
            watch_generated_roots.extend(gen_roots);
            watch_module_roots.extend(module_roots);

            let mut root_paths: Vec<PathBuf> =
                cfg.source_roots.iter().map(|r| r.path.clone()).collect();
            root_paths.sort();
            root_paths.dedup();

            let source_roots = root_paths
                .into_iter()
                .map(|path| {
                    let id = loader.source_root_id_for_path(project, &path);
                    let path = match VfsPath::local(path) {
                        VfsPath::Local(path) => path,
                        _ => unreachable!("VfsPath::local produced a non-local path"),
                    };
                    SourceRootEntry {
                        path_components: path.components().count(),
                        path,
                        id,
                    }
                })
                .collect::<Vec<_>>();

            project_roots.push(ProjectRoots {
                project,
                source_roots,
            });
        }

        watch_source_roots.sort();
        watch_source_roots.dedup();
        watch_generated_roots.sort();
        watch_generated_roots.dedup();
        watch_module_roots.sort();
        watch_module_roots.dedup();

        (
            project_roots,
            watch_source_roots,
            watch_generated_roots,
            watch_module_roots,
        )
    };

    {
        let mut state = project_state
            .lock()
            .expect("workspace project state mutex poisoned");
        state.projects = projects.clone();
        state.project_roots = project_roots;
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
        cfg.set_module_roots(watch_module_roots.clone());
        cfg.set_nova_config_path(loaded_config_path.clone());
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
        let _ = tx.try_send(WatchCommand::Refresh);
    }

    // 5) Derive the target release for JDK discovery from the loaded project configs. When
    // multiple projects are present, prefer the highest release to avoid indexing a JDK that's too
    // old for any module in the workspace.
    let requested_release = query_db.with_snapshot(|snap| {
        projects
            .iter()
            .filter_map(|&project| {
                let cfg = snap.project_config(project);
                Some(cfg.java.target.0)
                    .filter(|release| *release >= 1)
                    .or_else(|| Some(cfg.java.source.0).filter(|release| *release >= 1))
            })
            .max()
    });

    // Best-effort JDK index discovery.
    //
    // We intentionally do not fail workspace loading when JDK discovery or indexing fails: Nova
    // can still operate with a tiny built-in JDK index (used by unit tests / bootstrapping), but
    // many IDE features (decompilation, richer type info) benefit from a real platform index.
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

    // Only attempt expensive on-disk JDK indexing when the workspace explicitly configures a JDK.
    // Otherwise keep the built-in fallback index installed by `WorkspaceLoader` (fast,
    // deterministic and suitable for tests / bootstrapping).
    let should_discover_jdk = jdk_config.home.is_some() || !jdk_config.toolchains.is_empty();
    if should_discover_jdk {
        let jdk_index =
            nova_jdk::JdkIndex::discover_for_release(Some(&jdk_config), requested_release)
                .unwrap_or_else(|_| nova_jdk::JdkIndex::new());
        let jdk_index = Arc::new(jdk_index);
        for project in &projects {
            query_db.set_jdk_index(*project, Arc::clone(&jdk_index));
        }
    }

    // 6) Ensure open-document overlays remain authoritative: after the loader reads from disk,
    // reapply the current in-memory contents.
    for file_id in vfs.open_documents().snapshot() {
        let Some(path) = vfs.path_for_id(file_id) else {
            continue;
        };
        ensure_file_inputs(file_id, &path, query_db, project_state);
        query_db.set_file_exists(file_id, true);
        if let Ok(text) = vfs.read_to_string(&path) {
            query_db.set_file_content(file_id, Arc::new(text));
        }
        update_project_files_membership(&path, file_id, true, query_db, project_state);
    }

    // 7) Keep the closed-file text store in sync with loader updates.
    //
    // The shared `WorkspaceLoader` updates `file_exists` and refreshes `file_content` for changed
    // paths, but the workspace owns eviction/tracking for closed-file contents. Reconcile here so:
    // - deleted files drop their `file_content` allocations
    // - refreshed files clear any previous "evicted" marker and become tracked again
    // - evicted placeholders remain evicted until restored by another subsystem
    let open_docs = vfs.open_documents();
    let empty_text = empty_file_content();
    let file_ids: Vec<FileId> = query_db.with_snapshot(|snap| snap.all_file_ids().as_ref().clone());
    for file_id in file_ids {
        if open_docs.is_open(file_id) {
            continue;
        }
        let Some(path) = vfs.path_for_id(file_id) else {
            continue;
        };
        // Restrict this pass to local/workspace-managed files. Decompilation/archives may not have
        // stable on-disk state and are handled elsewhere.
        if path.as_local_path().is_none() {
            continue;
        }

        let exists = query_db.with_snapshot(|snap| snap.file_exists(file_id));
        if !exists {
            query_db.set_file_content(file_id, Arc::clone(&empty_text));
            query_db.set_file_is_dirty(file_id, false);
            closed_file_texts.clear(file_id);
            continue;
        }

        let text = query_db.with_snapshot(|snap| snap.file_content(file_id));
        let is_placeholder = Arc::ptr_eq(&text, &empty_text);
        let is_evicted = closed_file_texts.is_evicted(file_id);

        if is_evicted && is_placeholder {
            continue;
        }

        query_db.set_file_is_dirty(file_id, false);
        closed_file_texts.track_closed_file_content(file_id, &text);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    // NOTE(file-watching tests):
    // Avoid tests that rely on real OS watcher timing (starting an OS-backed watcher, touching the filesystem,
    // then sleeping and hoping an event arrives). They are flaky across platforms/CI.
    //
    // Prefer deterministic tests that either:
    // - inject a manual watcher (e.g. `nova_vfs::ManualFileWatcher`) into the workspace, or
    // - bypass the watcher entirely and call `apply_filesystem_events` with `FileChange` events.
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
            should_refresh_build_config(&root, &[], &[root.join("gradlew")]),
            "expected root gradlew to trigger build-tool refresh"
        );

        assert!(
            should_refresh_build_config(&root, &[], &[root.join("gradlew.bat")]),
            "expected root gradlew.bat to trigger build-tool refresh"
        );

        assert!(
            !should_refresh_build_config(&root, &[], &[root.join("sub").join("gradlew")]),
            "expected nested gradlew to be ignored"
        );

        assert!(
            !should_refresh_build_config(
                &root,
                &[root.join("sub")],
                &[root.join("sub").join("gradlew")]
            ),
            "expected module-root gradlew under workspace root to be ignored"
        );

        assert!(
            should_refresh_build_config(&root, &[], &[root.join("libs.versions.toml")]),
            "expected root libs.versions.toml to trigger build-tool refresh"
        );

        assert!(
            !should_refresh_build_config(&root, &[], &[root.join("deps.versions.toml")]),
            "expected root deps.versions.toml to be ignored (non-canonical version catalog location)"
        );

        assert!(
            should_refresh_build_config(
                &root,
                &[],
                &[root.join("gradle").join("foo.versions.toml")]
            ),
            "expected gradle/foo.versions.toml to trigger build-tool refresh"
        );

        assert!(
            !should_refresh_build_config(
                &root,
                &[],
                &[root.join("gradle").join("sub").join("nested.versions.toml")]
            ),
            "expected gradle/sub/nested.versions.toml to be ignored (only direct children of gradle/ are treated as version catalogs)"
        );

        assert!(
            should_refresh_build_config(&root, &[], &[root.join("dependencies.gradle")]),
            "expected dependencies.gradle to trigger build-tool refresh"
        );

        assert!(
            should_refresh_build_config(&root, &[], &[root.join("dependencies.gradle.kts")]),
            "expected dependencies.gradle.kts to trigger build-tool refresh"
        );

        assert!(
            should_refresh_build_config(&root, &[], &[root.join("gradle.lockfile")]),
            "expected gradle.lockfile to trigger build-tool refresh"
        );

        assert!(
            should_refresh_build_config(
                &root,
                &[],
                &[root
                    .join("gradle")
                    .join("dependency-locks")
                    .join("compileClasspath.lockfile")]
            ),
            "expected gradle/dependency-locks/*.lockfile to trigger build-tool refresh"
        );

        assert!(
            !should_refresh_build_config(&root, &[], &[root.join("foo.lockfile")]),
            "expected foo.lockfile (outside dependency-locks) to be ignored"
        );

        assert!(
            should_refresh_build_config(
                &root,
                &[],
                &[root.join("dependency-locks").join("custom.lockfile")]
            ),
            "expected dependency-locks/*.lockfile to trigger build-tool refresh"
        );

        // Build output directories should not trigger build-tool refresh.
        assert!(
            !should_refresh_build_config(
                &root,
                &[],
                &[root.join("build").join("dependencies.gradle")]
            ),
            "expected build/dependencies.gradle to be ignored"
        );

        assert!(
            !should_refresh_build_config(&root, &[], &[root.join("build").join("gradle.lockfile")]),
            "expected build/gradle.lockfile to be ignored"
        );

        assert!(
            !should_refresh_build_config(
                &root,
                &[],
                &[root.join("bazel-out").join("build.gradle")]
            ),
            "expected bazel-out/build.gradle to be ignored"
        );

        assert!(
            !should_refresh_build_config(
                &root,
                &[],
                &[root.join("nested").join("bazel-out").join("build.gradle")]
            ),
            "expected nested/bazel-out/build.gradle to be ignored"
        );

        assert!(
            !should_refresh_build_config(
                &root,
                &[],
                &[root
                    .join("nested")
                    .join("bazel-myworkspace")
                    .join("build.gradle")]
            ),
            "expected nested/bazel-<workspace>/build.gradle to be ignored"
        );

        assert!(
            !should_refresh_build_config(
                &root,
                &[],
                &[root.join("node_modules").join("dependencies.gradle")]
            ),
            "expected node_modules/dependencies.gradle to be ignored"
        );

        // Ensure absolute paths don't spuriously hit ignore heuristics due to parent directories
        // named `build/`.
        let root_under_build = PathBuf::from("/tmp/build/workspace");
        assert!(
            should_refresh_build_config(
                &root_under_build,
                &[],
                &[root_under_build.join("gradle.lockfile")]
            ),
            "expected gradle.lockfile under /tmp/build/... to trigger refresh"
        );

        // Ensure module roots outside the workspace root also participate in relative path
        // normalization (important for Gradle composite builds / included builds).
        let included_root = PathBuf::from("/tmp/build/included");
        assert!(
            should_refresh_build_config(
                &root,
                &[included_root.clone()],
                &[included_root.join("gradle.lockfile")]
            ),
            "expected included-build gradle.lockfile under /tmp/build/... to trigger refresh"
        );

        // Roots with `..` segments should still participate in prefix matching after lexical
        // normalization (events are typically normalized by `nova-vfs` before reaching this
        // function).
        let included_root_with_dotdots = PathBuf::from("/tmp/build/included/../included");
        assert!(
            should_refresh_build_config(
                &root,
                &[included_root_with_dotdots],
                &[included_root.join("gradle.lockfile")]
            ),
            "expected included-build gradle.lockfile to trigger refresh when module root contains dot segments"
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
    fn fixture_root(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../nova-project/testdata")
            .join(name)
    }

    #[test]
    fn build_config_refresh_triggers_for_gradle_dependency_lockfiles() {
        let root = PathBuf::from("/tmp/workspace");
        assert!(
            should_refresh_build_config(&root, &[], &[root.join("gradle.lockfile")]),
            "expected gradle.lockfile to trigger build config refresh"
        );
        assert!(
            should_refresh_build_config(
                &root,
                &[],
                &[root.join("gradle/dependency-locks/compileClasspath.lockfile")]
            ),
            "expected dependency-locks/*.lockfile to trigger build config refresh"
        );
        assert!(
            !should_refresh_build_config(&root, &[], &[root.join("foo.lockfile")]),
            "expected unrelated *.lockfile not to trigger build config refresh"
        );
    }

    #[derive(Debug)]
    struct PanicCommandRunner;

    impl CommandRunner for PanicCommandRunner {
        fn run(
            &self,
            _cwd: &std::path::Path,
            _program: &std::path::Path,
            _args: &[String],
        ) -> std::io::Result<nova_build::CommandOutput> {
            panic!("build command runner invoked unexpectedly");
        }
    }

    fn copy_dir_all(from: &Path, to: &Path) {
        fs::create_dir_all(to).expect("create_dir_all");
        for entry in fs::read_dir(from).expect("read_dir") {
            let entry = entry.expect("read_dir entry");
            let ty = entry.file_type().expect("file_type");
            let dst = to.join(entry.file_name());
            if ty.is_dir() {
                copy_dir_all(&entry.path(), &dst);
            } else {
                fs::copy(entry.path(), dst).expect("copy");
            }
        }
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

        timeout(Duration::from_secs(20), wait_for_indexing_ready(&rx))
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

    #[test]
    fn build_integration_off_does_not_invoke_build_tools() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        std::fs::create_dir_all(root.join("src/main/java")).unwrap();
        std::fs::write(
            root.join("src/main/java/Main.java"),
            "class Main {}".as_bytes(),
        )
        .unwrap();
        std::fs::write(
            root.join("pom.xml"),
            r#"<project><modelVersion>4.0.0</modelVersion><groupId>g</groupId><artifactId>a</artifactId><version>1</version></project>"#,
        )
        .unwrap();
        std::fs::write(
            root.join("nova.toml"),
            r#"
[build_integration]
mode = "off"
"#,
        )
        .unwrap();

        let memory = MemoryManager::new(MemoryBudget::default_for_system_with_env_overrides());
        let engine = WorkspaceEngine::new(WorkspaceEngineConfig {
            workspace_root: root.to_path_buf(),
            persistence: PersistenceConfig {
                mode: PersistenceMode::Disabled,
                cache: CacheConfig::from_env(),
            },
            memory,
            build_runner: Some(Arc::new(PanicCommandRunner)),
        });

        engine.set_workspace_root(root).unwrap();
    }

    #[test]
    fn build_integration_mode_change_to_on_invokes_build_tools() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::{collections::HashMap, process::ExitStatus};

        #[derive(Debug)]
        struct MavenEvaluateRoutingRunner {
            calls: Arc<AtomicUsize>,
            outputs: HashMap<String, nova_build::CommandOutput>,
        }

        impl MavenEvaluateRoutingRunner {
            fn new(
                calls: Arc<AtomicUsize>,
                outputs: HashMap<String, nova_build::CommandOutput>,
            ) -> Self {
                Self { calls, outputs }
            }
        }

        impl nova_build::CommandRunner for MavenEvaluateRoutingRunner {
            fn run(
                &self,
                _cwd: &Path,
                _program: &Path,
                args: &[String],
            ) -> std::io::Result<nova_build::CommandOutput> {
                self.calls.fetch_add(1, Ordering::Relaxed);
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

        fn scalar_output(value: &str) -> nova_build::CommandOutput {
            nova_build::CommandOutput {
                status: success_status(),
                stdout: format!("{value}\n"),
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
        outputs.insert("maven.compiler.target".to_string(), scalar_output("1.8"));

        let calls = Arc::new(AtomicUsize::new(0));
        let runner: Arc<dyn nova_build::CommandRunner> =
            Arc::new(MavenEvaluateRoutingRunner::new(Arc::clone(&calls), outputs));

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
        assert_eq!(
            calls.load(Ordering::Relaxed),
            0,
            "expected build tools not to run when build integration is not enabled"
        );
        engine.query_db.with_snapshot(|snap| {
            let config = snap.project_config(ProjectId::from_raw(0));
            assert!(
                !config.classpath.iter().any(|entry| entry.path == dep_jar),
                "expected heuristic Maven config to not include {}",
                dep_jar.display()
            );
        });

        // Enable build integration via config change; this should trigger a build metadata refresh
        // even though no build files changed.
        let config_path = root.join("nova.toml");
        fs::write(&config_path, "[build]\nmode = \"on\"\n").unwrap();
        engine.reload_project_now(&[config_path]).unwrap();

        assert!(
            calls.load(Ordering::Relaxed) > 0,
            "expected build tools to run after enabling build integration"
        );
        engine.query_db.with_snapshot(|snap| {
            let config = snap.project_config(ProjectId::from_raw(0));
            assert!(
                config
                    .classpath
                    .iter()
                    .any(|entry| entry.kind == ClasspathEntryKind::Jar && entry.path == dep_jar),
                "expected build-derived classpath to include {}",
                dep_jar.display()
            );
        });
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
    fn closed_file_texts_eviction_runs_after_other_query_cache_evictors() {
        use nova_memory::MemoryPressureThresholds;
        use nova_vfs::OpenDocuments;

        // Keep the overall budget large so process RSS doesn't force `Critical` pressure, but set
        // the QueryCache category budget to just below our test usage so eviction runs.
        let budget_total = 2 * 1024 * 1024;
        let mut budget = MemoryBudget::from_total(budget_total);
        budget.categories.query_cache = 2_999;
        let assigned = budget.categories.query_cache
            + budget.categories.syntax_trees
            + budget.categories.indexes
            + budget.categories.type_info;
        budget.categories.other = budget.total.saturating_sub(assigned);

        let memory = MemoryManager::with_thresholds(
            budget,
            MemoryPressureThresholds {
                medium: 1000.0,
                high: 1000.0,
                critical: 1000.0,
            },
        );

        let open_docs = Arc::new(OpenDocuments::default());
        let query_db = salsa::Database::new_with_open_documents(open_docs.clone());
        let closed_file_texts = ClosedFileTextStore::new(&memory, query_db.clone(), open_docs);

        // Track a single closed-file `file_content` input. This should not be evicted when another
        // QueryCache evictor can satisfy the category target.
        let file = FileId::from_raw(1);
        query_db.set_file_exists(file, true);
        let text = Arc::new("x".repeat(2_000));
        query_db.set_file_content(file, Arc::clone(&text));
        query_db.set_file_is_dirty(file, false);
        closed_file_texts.track_closed_file_content(file, &text);

        // Register a second QueryCache evictor with lower eviction priority (default 0) but a
        // smaller footprint than `workspace_closed_file_texts`. With priority ordering, it should
        // be evicted first even though it is smaller.
        let query_cache_evictor = TestEvictor::new(&memory, "test_query_cache", MemoryCategory::QueryCache);
        query_cache_evictor.set_bytes(1_000);

        let before = query_db.with_snapshot(|snap| snap.file_content(file));
        assert!(Arc::ptr_eq(&before, &text));
        assert!(!closed_file_texts.is_evicted(file));

        memory.enforce();

        assert!(
            query_cache_evictor.evict_calls() > 0,
            "expected QueryCache evictor to be invoked"
        );
        assert_eq!(query_cache_evictor.bytes(), 999);

        let after = query_db.with_snapshot(|snap| snap.file_content(file));
        assert!(
            Arc::ptr_eq(&after, &text),
            "expected closed file text input to remain resident when other QueryCache eviction suffices"
        );
        assert!(!closed_file_texts.is_evicted(file));
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
    fn watch_roots_deduplicate_equivalent_paths_after_normalization() {
        let workspace_root = PathBuf::from("/ws");
        let mut config = WatchConfig::new(workspace_root.clone());

        // Intentionally use two distinct paths that normalize to the same directory.
        config.module_roots = vec![PathBuf::from("/ext/src"), PathBuf::from("/ext/dir/../src")];

        let roots = compute_watch_roots(&workspace_root, &config);
        assert_eq!(
            roots.len(),
            2,
            "expected watch roots to deduplicate normalized external paths; roots={roots:?}"
        );
        assert!(roots.contains(&(PathBuf::from("/ext/src"), WatchMode::Recursive)));
    }

    #[test]
    fn watch_roots_normalize_workspace_root_before_watching() {
        let workspace_root = PathBuf::from("/ws/root/..");
        let config = WatchConfig::new(workspace_root.clone());

        let roots = compute_watch_roots(&workspace_root, &config);
        assert!(
            roots.contains(&(PathBuf::from("/ws"), WatchMode::Recursive)),
            "expected workspace root to be normalized; roots={roots:?}"
        );
        assert!(
            !roots.contains(&(workspace_root, WatchMode::Recursive)),
            "expected un-normalized workspace root not to be watched; roots={roots:?}"
        );
    }

    #[test]
    fn watch_roots_normalize_external_roots_before_workspace_prefix_check() {
        let workspace_root = PathBuf::from("/ws");
        let mut config = WatchConfig::new(workspace_root.clone());

        // Lexically starts with the workspace root, but normalizes outside of it.
        config.module_roots = vec![workspace_root.join("..").join("external")];

        let roots = compute_watch_roots(&workspace_root, &config);
        assert!(
            roots.contains(&(PathBuf::from("/external"), WatchMode::Recursive)),
            "expected external root to be watched after normalization; roots={roots:?}"
        );
    }

    #[test]
    fn watch_roots_normalize_config_path_before_workspace_prefix_check() {
        let workspace_root = PathBuf::from("/ws");
        let mut config = WatchConfig::new(workspace_root.clone());

        // Lexically starts with the workspace root, but normalizes outside of it.
        config.nova_config_path = Some(workspace_root.join("..").join("config.toml"));

        let roots = compute_watch_roots(&workspace_root, &config);
        assert!(
            roots.contains(&(PathBuf::from("/config.toml"), WatchMode::NonRecursive)),
            "expected external config path to be watched after normalization; roots={roots:?}"
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
    fn project_indexes_eviction_preserves_symbols_when_budget_allows() {
        let mut indexes = ProjectIndexes::default();

        // Populate the index with enough data so:
        // - the total footprint exceeds the Indexes category budget
        // - the symbols-only footprint fits under it
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

        let total_bytes = indexes.estimated_bytes();
        let symbols_bytes = indexes.symbols.estimated_bytes();
        assert!(
            total_bytes > symbols_bytes,
            "expected non-symbol indexes to contribute bytes"
        );
        assert!(symbols_bytes > 0, "expected symbols to be non-empty");

        // Build a budget whose Indexes category budget equals `symbols_bytes`.
        let budget_total = symbols_bytes.saturating_mul(5).max(1);
        let memory = MemoryManager::with_thresholds(
            MemoryBudget::from_total(budget_total),
            nova_memory::MemoryPressureThresholds {
                // Keep pressure deterministically Low even if process RSS dwarfs the synthetic budget.
                medium: 1e12,
                high: 1e12,
                critical: 1e12,
            },
        );

        let engine = WorkspaceEngine::new(WorkspaceEngineConfig {
            workspace_root: PathBuf::new(),
            persistence: PersistenceConfig::from_env(),
            memory: memory.clone(),
            build_runner: None,
        });
        engine.indexes_evictor.replace_indexes(indexes);

        memory.enforce();

        let kept = engine.indexes.lock().unwrap();
        assert!(
            !kept.symbols.symbols.is_empty(),
            "expected symbol index to be preserved"
        );
        assert!(kept.references.references.is_empty());
        assert!(kept.annotations.annotations.is_empty());
        assert!(kept.inheritance.subtypes.is_empty());
        assert!(kept.inheritance.supertypes.is_empty());
        drop(kept);

        let (_report, components) = memory.report_detailed();
        let component = components
            .iter()
            .find(|c| c.name == "workspace_project_indexes")
            .expect("workspace_project_indexes registered");
        assert!(
            component.bytes <= symbols_bytes,
            "expected eviction to shrink index usage to <= symbols-only footprint (symbols={symbols_bytes}, got={})",
            component.bytes
        );
    }

    #[test]
    fn external_config_path_adds_non_recursive_watch_for_config_path() {
        nova_config::with_config_env_lock(|| {
            let workspace_dir = tempfile::tempdir().unwrap();
            let workspace_root = workspace_dir.path().canonicalize().unwrap();

            let config_dir = tempfile::tempdir().unwrap();
            let config_path = config_dir.path().join("myconfig.toml");
            fs::write(&config_path, b"[generated_sources]\nenabled = true\n").unwrap();
            let config_path = config_path.canonicalize().unwrap();

            let _config_guard = EnvVarGuard::set(nova_config::NOVA_CONFIG_ENV_VAR, &config_path);

            let mut watch_config = WatchConfig::new(workspace_root.clone());
            watch_config.set_nova_config_path(nova_config::discover_config_path(&workspace_root));
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
        })
    }

    #[test]
    fn open_document_shares_text_arc_between_vfs_and_salsa() {
        let workspace = crate::Workspace::new_in_memory();
        let tmp = tempfile::tempdir().unwrap();
        let abs = nova_core::AbsPathBuf::new(tmp.path().join("Main.java")).unwrap();
        let uri = nova_core::path_to_file_uri(&abs).unwrap();
        let path = VfsPath::uri(uri);

        let file_id = workspace.open_document(path.clone(), "class Main {}".to_string(), 1);

        let engine = workspace.engine_for_tests();
        let overlay = engine
            .vfs
            .open_document_text_arc(&path)
            .expect("document is open in overlay");
        let salsa = engine
            .query_db
            .with_snapshot(|snap| snap.file_content(file_id));
        assert!(
            Arc::ptr_eq(&overlay, &salsa),
            "VFS overlay and Salsa should share the same Arc<String>"
        );
    }

    #[test]
    fn apply_changes_updates_salsa_with_overlay_arc() {
        let workspace = crate::Workspace::new_in_memory();
        let tmp = tempfile::tempdir().unwrap();
        let abs = nova_core::AbsPathBuf::new(tmp.path().join("Main.java")).unwrap();
        let uri = nova_core::path_to_file_uri(&abs).unwrap();
        let path = VfsPath::uri(uri);

        let file_id = workspace.open_document(path.clone(), "hello world".to_string(), 1);

        let engine = workspace.engine_for_tests();
        let before_overlay = engine.vfs.open_document_text_arc(&path).unwrap();
        let before_salsa = engine
            .query_db
            .with_snapshot(|snap| snap.file_content(file_id));
        assert!(Arc::ptr_eq(&before_overlay, &before_salsa));

        workspace
            .apply_changes(&path, 2, &[ContentChange::full("hello nova".to_string())])
            .unwrap();

        let after_overlay = engine.vfs.open_document_text_arc(&path).unwrap();
        let after_salsa = engine
            .query_db
            .with_snapshot(|snap| snap.file_content(file_id));
        assert_eq!(after_salsa.as_str(), "hello nova");
        assert!(Arc::ptr_eq(&after_overlay, &after_salsa));
        assert!(
            !Arc::ptr_eq(&before_salsa, &after_salsa),
            "applying changes should produce a new Arc<String> (copy-on-write) so Salsa sees an input change"
        );
    }

    #[test]
    fn close_document_reuses_overlay_arc_when_not_dirty() {
        let workspace = crate::Workspace::new_in_memory();
        let tmp = tempfile::tempdir().unwrap();
        let abs = nova_core::AbsPathBuf::new(tmp.path().join("Main.java")).unwrap();
        fs::write(abs.as_path(), "class Main {}").unwrap();
        let uri = nova_core::path_to_file_uri(&abs).unwrap();
        let path = VfsPath::uri(uri);

        let file_id = workspace.open_document(path.clone(), "class Main {}".to_string(), 1);

        let engine = workspace.engine_for_tests();
        let overlay = engine.vfs.open_document_text_arc(&path).unwrap();
        assert!(
            !engine
                .query_db
                .with_snapshot(|snap| snap.file_is_dirty(file_id)),
            "precondition: opening with on-disk text should mark the file clean"
        );

        workspace.close_document(&path);

        let salsa = engine
            .query_db
            .with_snapshot(|snap| snap.file_content(file_id));
        assert!(Arc::ptr_eq(&overlay, &salsa));
        assert!(!engine
            .query_db
            .with_snapshot(|snap| snap.file_is_dirty(file_id)));
    }

    #[test]
    fn close_document_restores_disk_text_when_dirty() {
        let workspace = crate::Workspace::new_in_memory();
        let tmp = tempfile::tempdir().unwrap();
        let abs = nova_core::AbsPathBuf::new(tmp.path().join("Main.java")).unwrap();
        fs::write(abs.as_path(), "disk").unwrap();
        let uri = nova_core::path_to_file_uri(&abs).unwrap();
        let path = VfsPath::uri(uri);

        let file_id = workspace.open_document(path.clone(), "overlay".to_string(), 1);

        let engine = workspace.engine_for_tests();
        let overlay = engine.vfs.open_document_text_arc(&path).unwrap();
        assert!(
            engine
                .query_db
                .with_snapshot(|snap| snap.file_is_dirty(file_id)),
            "precondition: opening with different text should mark the file dirty"
        );

        workspace.close_document(&path);

        let salsa = engine
            .query_db
            .with_snapshot(|snap| snap.file_content(file_id));
        assert_eq!(salsa.as_str(), "disk");
        assert!(
            !Arc::ptr_eq(&overlay, &salsa),
            "closing a dirty document should restore disk contents"
        );
        assert!(!engine
            .query_db
            .with_snapshot(|snap| snap.file_is_dirty(file_id)));
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
        engine.apply_filesystem_events(vec![FileChange::Modified {
            path: VfsPath::local(file_path),
        }]);

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
                .find(|c| c.name == "vfs_overlay_documents")
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

        engine.apply_filesystem_events(vec![FileChange::Moved {
            from: VfsPath::local(src_path),
            to: VfsPath::local(dst_path),
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
            FileChange::Created {
                path: VfsPath::local(file_a.clone()),
            },
            FileChange::Created {
                path: VfsPath::local(file_b.clone()),
            },
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
        workspace.apply_filesystem_events(vec![FileChange::Moved {
            from: VfsPath::local(file_a.clone()),
            to: VfsPath::local(file_b.clone()),
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

        engine.apply_filesystem_events(vec![FileChange::Created {
            path: VfsPath::local(file_a.clone()),
        }]);

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
        engine.apply_filesystem_events(vec![FileChange::Moved {
            from: VfsPath::local(file_a.clone()),
            to: VfsPath::local(file_b.clone()),
        }]);

        let vfs_b = VfsPath::local(file_b.clone());
        assert_eq!(engine.vfs.get_id(&vfs_a), None);
        assert_eq!(engine.vfs.get_id(&vfs_b), Some(file_id));

        engine.query_db.with_snapshot(|snap| {
            assert!(snap.file_exists(file_id));
            assert_eq!(snap.file_rel_path(file_id).as_str(), "src/B.java");
        });

        fs::write(&file_b, "class B {}".as_bytes()).unwrap();
        engine.apply_filesystem_events(vec![FileChange::Modified {
            path: VfsPath::local(file_b.clone()),
        }]);
        engine.query_db.with_snapshot(|snap| {
            assert_eq!(snap.file_content(file_id).as_str(), "class B {}");
        });

        fs::remove_file(&file_b).unwrap();
        engine.apply_filesystem_events(vec![FileChange::Deleted {
            path: VfsPath::local(file_b.clone()),
        }]);
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
            FileChange::Created {
                path: VfsPath::local(a.clone()),
            },
            FileChange::Created {
                path: VfsPath::local(b.clone()),
            },
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
            FileChange::Moved {
                from: VfsPath::local(a.clone()),
                to: VfsPath::local(b.clone()),
            },
            FileChange::Moved {
                from: VfsPath::local(b.clone()),
                to: VfsPath::local(c.clone()),
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
            state.projects.clear();
            state.project_roots.clear();
        }
        let project = ProjectId::from_raw(0);

        let java_path = root.join("src/A.java");
        fs::write(&java_path, "class A {}".as_bytes()).unwrap();
        engine.apply_filesystem_events(vec![FileChange::Created {
            path: VfsPath::local(java_path.clone()),
        }]);

        let vfs_java = VfsPath::local(java_path.clone());
        let file_id = engine.vfs.get_id(&vfs_java).expect("file id allocated");

        engine.query_db.with_snapshot(|snap| {
            assert!(snap.project_files(project).contains(&file_id));
        });

        let txt_path = root.join("src/A.txt");
        fs::rename(&java_path, &txt_path).unwrap();
        engine.apply_filesystem_events(vec![FileChange::Moved {
            from: VfsPath::local(java_path.clone()),
            to: VfsPath::local(txt_path.clone()),
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
        workspace.apply_filesystem_events(vec![FileChange::Modified {
            path: VfsPath::local(file.clone()),
        }]);

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
        workspace.apply_filesystem_events(vec![FileChange::Deleted {
            path: VfsPath::local(file.clone()),
        }]);

        let engine = workspace.engine_for_tests();
        engine.query_db.with_snapshot(|snap| {
            assert!(snap.file_exists(file_id));
            assert_eq!(
                snap.file_content(file_id).as_str(),
                "class Main { overlay }"
            );
        });

        // Closing the document should release the overlay contents since the file does not exist
        // on disk.
        workspace.close_document(&vfs_path);
        engine.query_db.with_snapshot(|snap| {
            assert!(!snap.file_exists(file_id));
            assert_eq!(snap.file_content(file_id).as_str(), "");
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
        workspace.apply_filesystem_events(vec![FileChange::Moved {
            from: VfsPath::local(file_a.clone()),
            to: VfsPath::local(file_b.clone()),
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
        workspace.apply_filesystem_events(vec![FileChange::Moved {
            // Intentionally construct an un-normalized `VfsPath` (with `..` segments) so the
            // workspace's event normalization logic is exercised.
            from: VfsPath::Local(from_event),
            to: VfsPath::Local(to_event),
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
        workspace.apply_filesystem_events(vec![FileChange::Deleted {
            // Intentionally preserve `..` segments to ensure watcher path normalization is applied.
            path: VfsPath::Local(delete_event),
        }]);

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
            FileChange::Created {
                path: VfsPath::local(file_a.clone()),
            },
            FileChange::Created {
                path: VfsPath::local(file_b.clone()),
            },
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

        engine.apply_filesystem_events(vec![FileChange::Moved {
            from: VfsPath::local(file_a.clone()),
            to: VfsPath::local(file_b.clone()),
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
            FileChange::Created {
                path: VfsPath::local(file_a.clone()),
            },
            FileChange::Created {
                path: VfsPath::local(file_b.clone()),
            },
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

        workspace.apply_filesystem_events(vec![FileChange::Moved {
            from: VfsPath::local(file_a.clone()),
            to: VfsPath::local(file_b.clone()),
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
        workspace.apply_filesystem_events(vec![FileChange::Created {
            path: VfsPath::local(scratch.clone()),
        }]);

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
        let file_id = engine
            .vfs
            .get_id(&VfsPath::local(java.clone()))
            .expect("Main.java should be registered in the VFS");

        engine.query_db.with_snapshot(|snap| {
            let project = snap.file_project(file_id);
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
            let project = snap.file_project(file_id);
            assert_eq!(
                snap.project_config(project).build_system,
                BuildSystem::Maven
            );
            assert!(snap.project_files(project).contains(&file_id));
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

        fn scalar_output(value: &str) -> nova_build::CommandOutput {
            nova_build::CommandOutput {
                status: success_status(),
                stdout: format!("{value}\n"),
                stderr: String::new(),
                truncated: false,
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();

        fs::write(root.join("nova.toml"), "[build]\nenabled = true\n").unwrap();

        fs::write(
            root.join("pom.xml"),
            br#"<project><modelVersion>4.0.0</modelVersion></project>"#,
        )
        .unwrap();
        fs::write(root.join("nova.toml"), "[build]\nmode = \"on\"\n").unwrap();

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
        outputs.insert("maven.compiler.target".to_string(), scalar_output("1.8"));

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
            assert_eq!(
                config.java.target.0, 8,
                "expected Java target to come from nova-build"
            );
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

        fs::write(root.join("nova.toml"), "[build]\nenabled = true\n").unwrap();

        fs::write(root.join("settings.gradle"), "rootProject.name = 'demo'").unwrap();
        fs::write(root.join("build.gradle"), "plugins { id 'java' }").unwrap();
        fs::write(root.join("nova.toml"), "[build]\nmode = \"on\"\n").unwrap();

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
            "sourceCompatibility": "1.8",
            "targetCompatibility": "1.8",
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
            assert_eq!(
                config.java.target.0, 8,
                "expected Java target to come from nova-build"
            );
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

        fs::write(root.join("nova.toml"), "[build]\nenabled = true\n").unwrap();

        fs::write(root.join("settings.gradle"), "rootProject.name = 'demo'").unwrap();
        fs::write(root.join("build.gradle"), "plugins { id 'java' }").unwrap();
        fs::write(root.join("nova.toml"), "[build]\nmode = \"on\"\n").unwrap();

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

        let snapshot_path = root.join(nova_build_model::GRADLE_SNAPSHOT_REL_PATH);
        fs::create_dir_all(snapshot_path.parent().unwrap()).unwrap();

        let snapshot_json = serde_json::json!({
            "schemaVersion": nova_build_model::GRADLE_SNAPSHOT_SCHEMA_VERSION,
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
        fs::write(
            &snapshot_path,
            serde_json::to_vec_pretty(&snapshot_json).unwrap(),
        )
        .unwrap();

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
    fn loads_maven_multi_module_workspace_with_distinct_projects() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("maven-multi");
        copy_dir_all(&fixture_root("maven-multi"), &root);
        let root = root.canonicalize().unwrap_or(root);

        let workspace = crate::Workspace::open(&root).unwrap();
        let engine = workspace.engine_for_tests();

        let (app_project, lib_project) = {
            let loader = engine
                .workspace_loader
                .lock()
                .expect("workspace loader mutex poisoned");
            (
                loader
                    .project_id_for_module("maven:com.example:app")
                    .expect("app project"),
                loader
                    .project_id_for_module("maven:com.example:lib")
                    .expect("lib project"),
            )
        };
        assert_ne!(app_project, lib_project);

        engine.query_db.with_snapshot(|snap| {
            let app_cfg = snap.project_config(app_project);
            assert!(
                app_cfg
                    .source_roots
                    .iter()
                    .any(|r| r.path.ends_with(Path::new("app/src/main/java"))),
                "expected app source root in {:#?}",
                app_cfg.source_roots
            );

            let lib_cfg = snap.project_config(lib_project);
            assert!(
                lib_cfg
                    .source_roots
                    .iter()
                    .any(|r| r.path.ends_with(Path::new("lib/src/main/java"))),
                "expected lib source root in {:#?}",
                lib_cfg.source_roots
            );

            let app_file = snap
                .project_files(app_project)
                .iter()
                .copied()
                .find(|&file| snap.file_rel_path(file).as_ref().ends_with("App.java"))
                .expect("App.java file id");
            let lib_file = snap
                .project_files(lib_project)
                .iter()
                .copied()
                .find(|&file| snap.file_rel_path(file).as_ref().ends_with("Lib.java"))
                .expect("Lib.java file id");

            assert_eq!(snap.file_project(app_file), app_project);
            assert_eq!(snap.file_project(lib_file), lib_project);
            assert_ne!(snap.file_project(app_file), snap.file_project(lib_file));
        });
    }

    #[test]
    fn loads_gradle_multi_module_workspace_with_distinct_projects() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("gradle-multi");
        copy_dir_all(&fixture_root("gradle-multi"), &root);
        let root = root.canonicalize().unwrap_or(root);

        let workspace = crate::Workspace::open(&root).unwrap();
        let engine = workspace.engine_for_tests();

        let (app_project, lib_project) = {
            let loader = engine
                .workspace_loader
                .lock()
                .expect("workspace loader mutex poisoned");
            (
                loader
                    .project_id_for_module("gradle::app")
                    .expect("app project"),
                loader
                    .project_id_for_module("gradle::lib")
                    .expect("lib project"),
            )
        };
        assert_ne!(app_project, lib_project);

        engine.query_db.with_snapshot(|snap| {
            let app_cfg = snap.project_config(app_project);
            assert!(
                app_cfg
                    .source_roots
                    .iter()
                    .any(|r| r.path.ends_with(Path::new("app/src/main/java"))),
                "expected app source root in {:#?}",
                app_cfg.source_roots
            );
            let lib_cfg = snap.project_config(lib_project);
            assert!(
                lib_cfg
                    .source_roots
                    .iter()
                    .any(|r| r.path.ends_with(Path::new("lib/src/main/java"))),
                "expected lib source root in {:#?}",
                lib_cfg.source_roots
            );
        });
    }

    #[test]
    fn project_reload_reuses_classpath_index_when_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("maven-multi");
        copy_dir_all(&fixture_root("maven-multi"), &root);
        let root = root.canonicalize().unwrap_or(root);

        let workspace = crate::Workspace::open(&root).unwrap();
        let engine = workspace.engine_for_tests();

        let app_project = {
            let loader = engine
                .workspace_loader
                .lock()
                .expect("workspace loader mutex poisoned");
            loader
                .project_id_for_module("maven:com.example:app")
                .expect("app project")
        };

        let before = engine.query_db.with_snapshot(|snap| {
            snap.classpath_index(app_project)
                .expect("classpath index should be set")
        });

        // Add a new file under the existing source root and reload. This should not change the
        // module's classpath, so the cached `ClasspathIndex` should be reused.
        let extra = root.join("app/src/main/java/com/example/app/Extra.java");
        fs::write(
            &extra,
            "package com.example.app; public class Extra { int x = 1; }",
        )
        .unwrap();

        engine.reload_project_now(&[extra]).unwrap();

        engine.query_db.with_snapshot(|snap| {
            let after = snap
                .classpath_index(app_project)
                .expect("classpath index should be set");
            assert!(
                std::sync::Arc::ptr_eq(&before.0, &after.0),
                "expected classpath index to be reused when classpath is unchanged"
            );

            assert!(
                snap.project_files(app_project).iter().any(|&file| {
                    snap.file_rel_path(file)
                        .as_ref()
                        .ends_with("app/src/main/java/com/example/app/Extra.java")
                }),
                "expected Extra.java to be part of the app project"
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
            assert_eq!(build.enabled, Some(true));
            assert_eq!(build.timeout_ms, 1234);
            assert_eq!(build.maven.enabled, Some(false));
            assert_eq!(build.gradle.enabled, None);
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
        let event = FileChange::Created {
            path: VfsPath::local(generated_file.clone()),
        };

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
            "schema_version": nova_project::GENERATED_ROOTS_SNAPSHOT_SCHEMA_VERSION,
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
        engine.apply_filesystem_events(vec![FileChange::Created {
            path: VfsPath::local(generated_file.clone()),
        }]);

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
        workspace.apply_filesystem_events(vec![FileChange::Created {
            path: VfsPath::local(module_info.clone()),
        }]);

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
    fn syntax_diagnostics_only_falls_back_to_vfs_when_closed_file_content_evicted() {
        use nova_memory::MemoryPressure;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        let file = root.join("src/Main.java");
        // Intentionally invalid Java to ensure the syntax-only diagnostics path emits errors.
        fs::write(&file, "class Main { /* unterminated".as_bytes()).unwrap();

        let workspace = crate::Workspace::open(root).unwrap();
        let engine = workspace.engine_for_tests();
        let vfs_path = VfsPath::local(file.clone());
        let file_id = engine
            .vfs()
            .get_id(&vfs_path)
            .expect("expected VFS id for Main.java");

        // Ensure the file content is initially resident in Salsa.
        let before = engine
            .query_db
            .with_snapshot(|snap| snap.file_content(file_id));
        assert!(
            !before.is_empty(),
            "expected Main.java content to be loaded into Salsa before eviction"
        );

        // Evict closed-file texts by replacing the Salsa input with the empty placeholder.
        engine.closed_file_texts.evict(EvictionRequest {
            pressure: MemoryPressure::High,
            target_bytes: 0,
        });

        let after = engine
            .query_db
            .with_snapshot(|snap| snap.file_content(file_id));
        assert_eq!(
            after.as_str(),
            "",
            "expected Main.java Salsa file_content to be evicted"
        );
        assert!(
            engine.closed_file_texts.is_evicted(file_id),
            "expected Main.java to be tracked as evicted"
        );

        // In degraded mode, we still want best-effort syntax diagnostics for the real on-disk
        // contents without restoring the Salsa input eagerly.
        let diagnostics = engine.syntax_diagnostics_only(&vfs_path, file_id);
        assert!(
            !diagnostics.is_empty(),
            "expected syntax-only diagnostics to read from disk for evicted file content"
        );

        // The fallback should not restore the Salsa input.
        let final_text = engine
            .query_db
            .with_snapshot(|snap| snap.file_content(file_id));
        assert_eq!(
            final_text.as_str(),
            "",
            "expected syntax-only diagnostics to avoid restoring evicted Salsa file_content"
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
            FileChange::Created {
                path: VfsPath::local(main_path.clone()),
            },
            FileChange::Created {
                path: VfsPath::local(foo_path.clone()),
            },
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
            crate::snapshot::WorkspaceDbView::new(engine.query_db.clone(), engine.vfs.clone());
        assert!(
            view.salsa_db().is_some(),
            "WorkspaceDbView should expose the workspace Salsa DB for classpath-backed completions"
        );
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
        fs::write(
            project_root.join("src/Main.java"),
            "class Main {}".as_bytes(),
        )
        .unwrap();
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
    async fn manual_watcher_directory_modify_triggers_rescan_fallback() {
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

        // Create a new Java file on disk, but only send a directory-level watcher event. Some
        // watcher backends can surface directory metadata changes rather than per-file events.
        let new_file = project_root.join("src/Added.java");
        let new_text = "class Added { int x; }";
        fs::write(&new_file, new_text.as_bytes()).unwrap();

        let src_dir = project_root.join("src");
        handle
            .push(WatchEvent::Changes {
                changes: vec![FileChange::Modified {
                    path: VfsPath::local(src_dir),
                }],
            })
            .unwrap();

        let vfs_path = VfsPath::local(new_file);
        timeout(Duration::from_secs(5), async move {
            loop {
                let Some(file_id) = engine.vfs.get_id(&vfs_path) else {
                    yield_now().await;
                    continue;
                };

                let ready = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    engine.query_db.with_snapshot(|snap| {
                        snap.file_exists(file_id)
                            && snap.file_content(file_id).as_str() == new_text
                            && snap.file_rel_path(file_id).as_str() == "src/Added.java"
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
        .expect("timed out waiting for directory-modified-triggered project reload");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn manual_watcher_directory_event_outside_source_roots_does_not_trigger_rescan() {
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

        let config = engine
            .watch_config
            .read()
            .expect("workspace watch config lock poisoned")
            .clone();
        let expected_src = normalize_vfs_local_path(project_root.join("src"));
        assert!(
            config.source_roots.contains(&expected_src),
            "expected watch_config.source_roots to include {} (got {:?})",
            expected_src.display(),
            config.source_roots
        );

        let manual = ManualFileWatcher::new();
        let handle: ManualFileWatcherHandle = manual.handle();
        let _watcher = engine
            .start_watching_with_watcher(Box::new(manual), WatchDebounceConfig::ZERO)
            .unwrap();

        // Create a new source file on disk but do not send a file-level watcher event for it. If
        // an unrelated directory event triggers a rescan, the workspace would discover this file.
        let added_file = project_root.join("src/Added.java");
        fs::write(&added_file, "class Added {}".as_bytes()).unwrap();
        let added_vfs_path = VfsPath::local(added_file);
        assert!(
            engine.vfs.get_id(&added_vfs_path).is_none(),
            "expected Added.java to be undiscovered before any watcher event"
        );

        // Send a directory-level create event outside source roots (a common pattern during builds,
        // e.g. `target/`). This should *not* trigger a full rescan.
        let target_dir = project_root.join("target");
        fs::create_dir_all(&target_dir).unwrap();
        handle
            .push(WatchEvent::Changes {
                changes: vec![FileChange::Created {
                    path: VfsPath::local(target_dir),
                }],
            })
            .unwrap();

        // Wait for a short interval and assert the workspace did not rescan and discover the file.
        let engine_for_wait = engine.clone();
        let added_for_wait = added_vfs_path.clone();
        let res = timeout(Duration::from_secs(1), async move {
            loop {
                if engine_for_wait.vfs.get_id(&added_for_wait).is_some() {
                    panic!(
                        "unexpected rescan: directory event outside source roots should not discover Added.java"
                    );
                }
                yield_now().await;
            }
        })
        .await;

        assert!(
            res.is_err(),
            "expected test to time out (file should remain undiscovered)"
        );
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

    #[test]
    fn watch_roots_update_after_project_reload_adds_external_generated_root() {
        fn escape_toml_string(value: &str) -> String {
            value.replace('\\', "\\\\")
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        fs::create_dir_all(root.join("src")).unwrap();
        // Canonicalize to resolve macOS /var -> /private/var symlink, matching Workspace::open behavior.
        let root = root.canonicalize().unwrap();
        fs::write(root.join("src/Main.java"), "class Main {}".as_bytes()).unwrap();

        let workspace = crate::Workspace::open(&root).unwrap();
        let engine = workspace.engine_for_tests();

        let mut watcher = ManualFileWatcher::new();
        let mut watch_root_manager = WatchRootManager::new(Duration::from_secs(2));

        let t0 = Instant::now();

        // Establish initial roots (workspace root only).
        let _ = watch_root_manager.set_desired_roots(
            compute_watch_roots(
                &root,
                &engine
                    .watch_config
                    .read()
                    .expect("workspace watch config lock poisoned"),
            )
            .into_iter()
            .collect(),
            t0,
            &mut watcher,
        );
        assert_eq!(
            watcher.watch_calls(),
            &[(root.clone(), WatchMode::Recursive)],
            "expected initial watch to include only the workspace root"
        );

        // Update config to add an external generated-sources root.
        let external = dir.path().join("external-generated");
        fs::create_dir_all(&external).unwrap();
        let external = external.canonicalize().unwrap();

        let config_path = root.join("nova.toml");
        let escaped_external = escape_toml_string(external.to_string_lossy().as_ref());
        fs::write(
            &config_path,
            format!("[generated_sources]\noverride_roots = [\"{escaped_external}\"]\n"),
        )
        .unwrap();

        engine.reload_project_now(&[config_path]).unwrap();

        {
            let cfg = engine
                .watch_config
                .read()
                .expect("workspace watch config lock poisoned");
            assert!(
                cfg.generated_source_roots.contains(&external),
                "expected reload to update watcher config with external root"
            );
        }

        // One "watch loop" step should reconcile roots against the updated config.
        let _ = watch_root_manager.set_desired_roots(
            compute_watch_roots(
                &root,
                &engine
                    .watch_config
                    .read()
                    .expect("workspace watch config lock poisoned"),
            )
            .into_iter()
            .collect(),
            t0,
            &mut watcher,
        );

        assert_eq!(
            watcher.watch_calls(),
            &[
                (root.clone(), WatchMode::Recursive),
                (external.clone(), WatchMode::Recursive),
            ],
            "expected watcher to begin watching newly configured external root"
        );
    }
}
