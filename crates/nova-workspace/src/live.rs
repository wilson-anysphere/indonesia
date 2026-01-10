use crossbeam_channel as channel;
use nova_scheduler::{chunk_vec, Debouncer};
use nova_vfs::{FileId, FileIdRegistry, LocalFs, OverlayFs, VfsPath};
use notify::RecursiveMode;
use notify::Watcher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, Ord, PartialOrd, Eq, PartialEq)]
pub enum ChangeCategory {
    Source,
    Build,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizedEvent {
    Created(PathBuf),
    Modified(PathBuf),
    Deleted(PathBuf),
    Moved { from: PathBuf, to: PathBuf },
}

impl NormalizedEvent {
    fn paths(&self) -> Vec<&Path> {
        match self {
            NormalizedEvent::Created(p)
            | NormalizedEvent::Modified(p)
            | NormalizedEvent::Deleted(p) => vec![p.as_path()],
            NormalizedEvent::Moved { from, to } => vec![from.as_path(), to.as_path()],
        }
    }
}

#[derive(Debug, Default)]
pub struct WorkspaceDb {
    file_exists: HashMap<FileId, bool>,
    file_content: HashMap<FileId, String>,
}

impl WorkspaceDb {
    pub fn file_exists(&self, file_id: FileId) -> Option<bool> {
        self.file_exists.get(&file_id).copied()
    }

    pub fn file_content(&self, file_id: FileId) -> Option<&str> {
        self.file_content.get(&file_id).map(|s| s.as_str())
    }
}

#[derive(Debug, Default)]
pub struct WorkspaceIndexer {
    /// Each entry is a chunk of files we were asked to index.
    indexed_chunks: Vec<Vec<FileId>>,
    diagnostics_chunks: Vec<Vec<FileId>>,
    reloads: usize,
}

impl WorkspaceIndexer {
    fn index_chunk(&mut self, chunk: Vec<FileId>) {
        self.indexed_chunks.push(chunk.clone());
        self.diagnostics_chunks.push(chunk);
    }

    fn reload_project(&mut self) {
        self.reloads += 1;
    }

    pub fn indexed_chunks(&self) -> &[Vec<FileId>] {
        &self.indexed_chunks
    }

    pub fn reload_count(&self) -> usize {
        self.reloads
    }
}

pub trait WorkspaceClient: Send + Sync + 'static {
    fn show_status(&self, message: String);
    fn show_error(&self, message: String);
}

#[derive(Debug, Clone)]
pub struct WorkspaceConfig {
    pub source_roots: Vec<PathBuf>,
    pub generated_source_roots: Vec<PathBuf>,
    pub build_file_roots: Vec<PathBuf>,
    pub source_debounce: Duration,
    pub build_debounce: Duration,
    pub index_chunk_size: usize,
    pub min_reload_interval: Duration,
}

impl WorkspaceConfig {
    pub fn new(
        source_roots: Vec<PathBuf>,
        generated_source_roots: Vec<PathBuf>,
        build_file_roots: Vec<PathBuf>,
    ) -> Self {
        Self {
            source_roots,
            generated_source_roots,
            build_file_roots,
            source_debounce: Duration::from_millis(200),
            build_debounce: Duration::from_millis(1200),
            index_chunk_size: 64,
            min_reload_interval: Duration::from_secs(2),
        }
    }
}

pub struct Workspace {
    file_ids: Arc<Mutex<FileIdRegistry>>,
    overlay: OverlayFs<LocalFs>,
    db: Arc<Mutex<WorkspaceDb>>,
    indexer: Arc<Mutex<WorkspaceIndexer>>,
    client: Arc<dyn WorkspaceClient>,
    config: WorkspaceConfig,
    last_reload_at: Arc<Mutex<Option<Instant>>>,
}

impl Workspace {
    pub fn new(config: WorkspaceConfig, client: Arc<dyn WorkspaceClient>) -> Self {
        Self {
            file_ids: Arc::new(Mutex::new(FileIdRegistry::new())),
            overlay: OverlayFs::new(LocalFs),
            db: Arc::new(Mutex::new(WorkspaceDb::default())),
            indexer: Arc::new(Mutex::new(WorkspaceIndexer::default())),
            client,
            config,
            last_reload_at: Arc::new(Mutex::new(None)),
        }
    }

    pub fn file_ids(&self) -> Arc<Mutex<FileIdRegistry>> {
        Arc::clone(&self.file_ids)
    }

    pub fn overlay_fs(&self) -> OverlayFs<LocalFs> {
        self.overlay.clone()
    }

    pub fn db(&self) -> Arc<Mutex<WorkspaceDb>> {
        Arc::clone(&self.db)
    }

    pub fn indexer(&self) -> Arc<Mutex<WorkspaceIndexer>> {
        Arc::clone(&self.indexer)
    }

    pub fn start_watching(&self) -> notify::Result<WatcherHandle> {
        let (tx, rx) = channel::unbounded::<WorkspaceChangeBatch>();
        let stop = FileWatcher::spawn(self.config.clone(), Arc::clone(&self.client), tx)?;

        let workspace = WorkspaceDriver {
            file_ids: Arc::clone(&self.file_ids),
            overlay: self.overlay.clone(),
            db: Arc::clone(&self.db),
            indexer: Arc::clone(&self.indexer),
            client: Arc::clone(&self.client),
            config: self.config.clone(),
            last_reload_at: Arc::clone(&self.last_reload_at),
        };

        let driver_stop = channel::bounded::<()>(0);
        let driver_stop_tx = driver_stop.0.clone();
        let driver_stop_rx = driver_stop.1;

        let driver_thread = thread::spawn(move || {
            loop {
                channel::select! {
                    recv(driver_stop_rx) -> _ => break,
                    recv(rx) -> msg => {
                        let Ok(batch) = msg else { break };
                        workspace.apply_batch(batch);
                    }
                }
            }
        });

        Ok(WatcherHandle {
            watcher_stop: stop,
            driver_stop: driver_stop_tx,
            driver_thread: Some(driver_thread),
        })
    }
}

struct WorkspaceDriver {
    file_ids: Arc<Mutex<FileIdRegistry>>,
    overlay: OverlayFs<LocalFs>,
    db: Arc<Mutex<WorkspaceDb>>,
    indexer: Arc<Mutex<WorkspaceIndexer>>,
    client: Arc<dyn WorkspaceClient>,
    config: WorkspaceConfig,
    last_reload_at: Arc<Mutex<Option<Instant>>>,
}

impl WorkspaceDriver {
    fn apply_batch(&self, batch: WorkspaceChangeBatch) {
        match batch.category {
            ChangeCategory::Build => self.apply_build_changes(batch.events),
            ChangeCategory::Source => self.apply_source_changes(batch.events),
        }
    }

    fn apply_build_changes(&self, _events: Vec<NormalizedEvent>) {
        // Debouncer already batches build changes; additionally guard against reload storms.
        let now = Instant::now();
        {
            let mut last = self.last_reload_at.lock().unwrap();
            if let Some(prev) = *last {
                if now.duration_since(prev) < self.config.min_reload_interval {
                    return;
                }
            }
            *last = Some(now);
        }

        self.client.show_status("Reloading project…".to_string());
        self.indexer.lock().unwrap().reload_project();

        // On reload, re-index everything we currently know about.
        let all_ids = self.file_ids.lock().unwrap().all_file_ids();
        self.client.show_status("Indexing…".to_string());
        for chunk in chunk_vec(all_ids, self.config.index_chunk_size) {
            self.indexer.lock().unwrap().index_chunk(chunk);
        }
    }

    fn apply_source_changes(&self, events: Vec<NormalizedEvent>) {
        let mut affected = HashSet::new();

        for event in events {
            match event {
                NormalizedEvent::Created(path) | NormalizedEvent::Modified(path) => {
                    if let Some(id) = self.update_file_from_disk(&path) {
                        affected.insert(id);
                    }
                }
                NormalizedEvent::Deleted(path) => {
                    let vfs_path = VfsPath::local(path);
                    let id = self.file_ids.lock().unwrap().file_id(vfs_path);
                    {
                        let mut db = self.db.lock().unwrap();
                        db.file_exists.insert(id, false);
                        db.file_content.remove(&id);
                    }
                    affected.insert(id);
                }
                NormalizedEvent::Moved { from, to } => {
                    let from_path = VfsPath::local(from);
                    let to_buf = to.clone();
                    let to_path = VfsPath::local(to_buf);
                    let id = self
                        .file_ids
                        .lock()
                        .unwrap()
                        .rename_path(&from_path, to_path);
                    if let Some(id) = self.update_file_from_disk(&to).or(Some(id)) {
                        affected.insert(id);
                    }
                }
            }
        }

        if affected.is_empty() {
            return;
        }

        let mut ids: Vec<_> = affected.into_iter().collect();
        ids.sort();

        self.client.show_status("Indexing…".to_string());
        for chunk in chunk_vec(ids, self.config.index_chunk_size) {
            self.indexer.lock().unwrap().index_chunk(chunk);
        }
    }

    fn update_file_from_disk(&self, path: &Path) -> Option<FileId> {
        let vfs_path = VfsPath::local(path.to_path_buf());
        let id = self.file_ids.lock().unwrap().file_id(vfs_path.clone());
        {
            let mut db = self.db.lock().unwrap();
            db.file_exists.insert(id, true);
        }

        if self.overlay.is_open(&vfs_path) {
            return Some(id);
        }

        match fs::read_to_string(path) {
            Ok(contents) => {
                self.db.lock().unwrap().file_content.insert(id, contents);
                Some(id)
            }
            Err(err) => {
                // Transient IO issues shouldn't kill the watcher; surface the error and keep going.
                self.client
                    .show_error(format!("Failed to read {}: {err}", path.display()));
                if err.kind() == std::io::ErrorKind::NotFound {
                    let mut db = self.db.lock().unwrap();
                    db.file_exists.insert(id, false);
                    db.file_content.remove(&id);
                }
                Some(id)
            }
        }
    }
}

pub struct WatcherHandle {
    watcher_stop: channel::Sender<()>,
    driver_stop: channel::Sender<()>,
    driver_thread: Option<thread::JoinHandle<()>>,
}

impl Drop for WatcherHandle {
    fn drop(&mut self) {
        let _ = self.watcher_stop.send(());
        let _ = self.driver_stop.send(());
        if let Some(handle) = self.driver_thread.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Debug)]
struct WorkspaceChangeBatch {
    category: ChangeCategory,
    events: Vec<NormalizedEvent>,
}

struct FileWatcher;

impl FileWatcher {
    fn spawn(
        config: WorkspaceConfig,
        client: Arc<dyn WorkspaceClient>,
        out: channel::Sender<WorkspaceChangeBatch>,
    ) -> notify::Result<channel::Sender<()>> {
        let (raw_tx, raw_rx) = channel::unbounded::<notify::Result<notify::Event>>();
        let (stop_tx, stop_rx) = channel::bounded::<()>(0);

        thread::spawn(move || {
            let now = Instant::now();
            let mut normalizer = EventNormalizer::new();
            let mut debouncer = Debouncer::new([
                (ChangeCategory::Source, config.source_debounce),
                (ChangeCategory::Build, config.build_debounce),
            ]);

            let mut watcher = match notify::recommended_watcher(move |res| {
                let _ = raw_tx.send(res);
            }) {
                Ok(watcher) => watcher,
                Err(err) => {
                    client.show_error(format!("Failed to start file watcher: {err}"));
                    return;
                }
            };

            for root in config
                .source_roots
                .iter()
                .chain(config.generated_source_roots.iter())
            {
                if let Err(err) = watcher.watch(root, RecursiveMode::Recursive) {
                    client.show_error(format!("Failed to watch {}: {err}", root.display()));
                }
            }

            for root in &config.build_file_roots {
                if let Err(err) = watcher.watch(root, RecursiveMode::NonRecursive) {
                    client.show_error(format!("Failed to watch {}: {err}", root.display()));
                }
            }

            // If all watch registrations failed we still keep the thread alive; notify's watcher
            // will produce errors which we surface, but we avoid panicking.
            let _ = now;

            loop {
                let now = Instant::now();
                let deadline = debouncer
                    .next_deadline()
                    .unwrap_or(now + Duration::from_secs(3600));
                let timeout = deadline.saturating_duration_since(now);
                let tick = channel::after(timeout);

                channel::select! {
                    recv(stop_rx) -> _ => {
                        for (cat, events) in debouncer.flush_all() {
                            let _ = out.send(WorkspaceChangeBatch { category: cat, events });
                        }
                        break;
                    }
                    recv(raw_rx) -> msg => {
                        let Ok(res) = msg else { break };
                        match res {
                            Ok(event) => {
                                let now = Instant::now();
                                for norm in normalizer.push(event, now) {
                                    if let Some(cat) = categorize_event(&config, &norm) {
                                        debouncer.push(&cat, norm, now);
                                    }
                                }
                                for (cat, events) in debouncer.flush_due(now) {
                                    let _ = out.send(WorkspaceChangeBatch { category: cat, events });
                                }
                            }
                            Err(err) => {
                                client.show_error(format!("File watcher error: {err}"));
                            }
                        }
                    }
                    recv(tick) -> _ => {
                        let now = Instant::now();
                        for (cat, events) in debouncer.flush_due(now) {
                            let _ = out.send(WorkspaceChangeBatch { category: cat, events });
                        }
                    }
                }
            }
        });

        Ok(stop_tx)
    }
}

fn categorize_event(config: &WorkspaceConfig, event: &NormalizedEvent) -> Option<ChangeCategory> {
    for path in event.paths() {
        if is_build_file(path) {
            return Some(ChangeCategory::Build);
        }
    }

    // We only index Java sources.
    for path in event.paths() {
        if path.extension().and_then(|s| s.to_str()) != Some("java") {
            continue;
        }
        if is_within_any(path, &config.source_roots)
            || is_within_any(path, &config.generated_source_roots)
        {
            return Some(ChangeCategory::Source);
        }
    }

    None
}

fn is_within_any(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

fn is_build_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    name == "pom.xml" || name.starts_with("build.gradle") || name.starts_with("settings.gradle")
}

struct EventNormalizer {
    pending_renames: VecDeque<(Instant, PathBuf)>,
}

impl EventNormalizer {
    fn new() -> Self {
        Self {
            pending_renames: VecDeque::new(),
        }
    }

    fn push(&mut self, event: notify::Event, now: Instant) -> Vec<NormalizedEvent> {
        self.gc_pending(now);

        use notify::event::{ModifyKind, RenameMode};
        use notify::EventKind;

        match event.kind {
            EventKind::Create(_) => event
                .paths
                .into_iter()
                .map(NormalizedEvent::Created)
                .collect(),
            EventKind::Remove(_) => event
                .paths
                .into_iter()
                .map(NormalizedEvent::Deleted)
                .collect(),
            EventKind::Modify(ModifyKind::Data(_))
            | EventKind::Modify(ModifyKind::Metadata(_))
            | EventKind::Modify(ModifyKind::Other)
            | EventKind::Modify(ModifyKind::Any) => event
                .paths
                .into_iter()
                .map(NormalizedEvent::Modified)
                .collect(),
            EventKind::Modify(ModifyKind::Name(rename_mode)) => match rename_mode {
                RenameMode::Both => paths_to_moves(event.paths),
                RenameMode::From => {
                    for path in event.paths {
                        self.pending_renames.push_back((now, path));
                    }
                    Vec::new()
                }
                RenameMode::To => {
                    let mut out = Vec::new();
                    for to in event.paths {
                        if let Some((_, from)) = self.pending_renames.pop_front() {
                            out.push(NormalizedEvent::Moved { from, to });
                        } else {
                            out.push(NormalizedEvent::Created(to));
                        }
                    }
                    out
                }
                RenameMode::Any => event
                    .paths
                    .into_iter()
                    .map(NormalizedEvent::Modified)
                    .collect(),
                RenameMode::Other => event
                    .paths
                    .into_iter()
                    .map(NormalizedEvent::Modified)
                    .collect(),
            },
            // Some backends report a rename as a "modify" without further detail.
            _ => event
                .paths
                .into_iter()
                .map(NormalizedEvent::Modified)
                .collect(),
        }
    }

    fn gc_pending(&mut self, now: Instant) {
        const MAX_AGE: Duration = Duration::from_secs(2);
        while let Some((t, _)) = self.pending_renames.front() {
            if now.duration_since(*t) <= MAX_AGE {
                break;
            }
            self.pending_renames.pop_front();
        }

        // Bound memory for rename storms.
        while self.pending_renames.len() > 512 {
            self.pending_renames.pop_front();
        }
    }
}

fn paths_to_moves(mut paths: Vec<PathBuf>) -> Vec<NormalizedEvent> {
    let mut out = Vec::new();
    while paths.len() >= 2 {
        let from = paths.remove(0);
        let to = paths.remove(0);
        out.push(NormalizedEvent::Moved { from, to });
    }
    // Leftover path: treat as modified.
    if let Some(path) = paths.pop() {
        out.push(NormalizedEvent::Modified(path));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{ModifyKind, RenameMode};
    use notify::EventKind;

    #[test]
    fn normalize_rename_from_to_into_move() {
        let mut normalizer = EventNormalizer::new();
        let t0 = Instant::now();

        let from = PathBuf::from("/tmp/A.java");
        let to = PathBuf::from("/tmp/B.java");

        let ev_from = notify::Event {
            kind: EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            paths: vec![from.clone()],
            attrs: Default::default(),
        };
        let ev_to = notify::Event {
            kind: EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            paths: vec![to.clone()],
            attrs: Default::default(),
        };

        assert!(normalizer.push(ev_from, t0).is_empty());
        assert_eq!(
            normalizer.push(ev_to, t0),
            vec![NormalizedEvent::Moved { from, to }]
        );
    }

    #[test]
    fn normalize_create_and_remove() {
        let mut normalizer = EventNormalizer::new();
        let t0 = Instant::now();
        let p = PathBuf::from("/tmp/A.java");

        let create = notify::Event {
            kind: EventKind::Create(notify::event::CreateKind::File),
            paths: vec![p.clone()],
            attrs: Default::default(),
        };
        let remove = notify::Event {
            kind: EventKind::Remove(notify::event::RemoveKind::File),
            paths: vec![p.clone()],
            attrs: Default::default(),
        };

        assert_eq!(normalizer.push(create, t0), vec![NormalizedEvent::Created(p.clone())]);
        assert_eq!(normalizer.push(remove, t0), vec![NormalizedEvent::Deleted(p)]);
    }

    #[derive(Default)]
    struct TestClient {
        statuses: Mutex<Vec<String>>,
        errors: Mutex<Vec<String>>,
    }

    impl WorkspaceClient for TestClient {
        fn show_status(&self, message: String) {
            self.statuses.lock().unwrap().push(message);
        }

        fn show_error(&self, message: String) {
            self.errors.lock().unwrap().push(message);
        }
    }

    #[test]
    fn workspace_applies_source_changes_to_db_and_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let file_path = root.join("Example.java");
        fs::write(&file_path, "class Example {}".as_bytes()).unwrap();

        let client = Arc::new(TestClient::default());
        let workspace = Workspace::new(
            WorkspaceConfig {
                source_roots: vec![root.clone()],
                generated_source_roots: vec![],
                build_file_roots: vec![],
                source_debounce: Duration::from_millis(1),
                build_debounce: Duration::from_millis(1),
                index_chunk_size: 16,
                min_reload_interval: Duration::from_millis(1),
            },
            client,
        );

        let driver = WorkspaceDriver {
            file_ids: workspace.file_ids(),
            overlay: workspace.overlay_fs(),
            db: workspace.db(),
            indexer: workspace.indexer(),
            client: Arc::new(NoopClient),
            config: workspace.config.clone(),
            last_reload_at: Arc::clone(&workspace.last_reload_at),
        };

        driver.apply_source_changes(vec![NormalizedEvent::Created(file_path.clone())]);

        let file_ids = workspace.file_ids();
        let mut file_ids = file_ids.lock().unwrap();
        let id = file_ids.file_id(VfsPath::local(file_path.clone()));
        drop(file_ids);

        let db = workspace.db();
        let db = db.lock().unwrap();
        assert_eq!(db.file_exists(id), Some(true));
        assert_eq!(db.file_content(id), Some("class Example {}"));
        drop(db);

        let indexer = workspace.indexer();
        let indexer = indexer.lock().unwrap();
        assert!(!indexer.indexed_chunks().is_empty());
    }

    struct NoopClient;

    impl WorkspaceClient for NoopClient {
        fn show_status(&self, _message: String) {}
        fn show_error(&self, _message: String) {}
    }

    #[test]
    #[ignore = "relies on OS file watcher timings"]
    fn watcher_propagates_disk_edits_into_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let file_path = root.join("Example.java");
        fs::write(&file_path, "class Example {}".as_bytes()).unwrap();

        let client = Arc::new(TestClient::default());
        let workspace = Workspace::new(
            WorkspaceConfig {
                source_roots: vec![root.clone()],
                generated_source_roots: vec![],
                build_file_roots: vec![root.clone()],
                source_debounce: Duration::from_millis(50),
                build_debounce: Duration::from_millis(50),
                index_chunk_size: 16,
                min_reload_interval: Duration::from_millis(1),
            },
            client,
        );

        let _handle = workspace.start_watching().unwrap();
        fs::write(&file_path, "class Example { int x; }".as_bytes()).unwrap();

        // Wait for the watcher to fire + debounce.
        thread::sleep(Duration::from_millis(250));

        let file_ids = workspace.file_ids();
        let mut file_ids = file_ids.lock().unwrap();
        let id = file_ids.file_id(VfsPath::local(file_path.clone()));
        drop(file_ids);

        let db = workspace.db();
        let db = db.lock().unwrap();
        assert_eq!(db.file_content(id), Some("class Example { int x; }"));
    }
}

