use anyhow::{Context, Result};
use nova_cache::{CacheConfig, CacheDir, CacheMetadata, Fingerprint, ProjectSnapshot};
use nova_db::persistence::{PersistenceConfig, PersistenceMode};
use nova_db::{FileId, NovaIndexing, SalsaDatabase};
use nova_index::{
    load_sharded_index_view_lazy_from_fast_snapshot, save_sharded_indexes, shard_id_for_path,
    CandidateStrategy, IndexedSymbol, ProjectIndexes, SearchStats, SearchSymbol,
    WorkspaceSymbolSearcher, DEFAULT_SHARD_COUNT,
};
use nova_memory::{MemoryBudget, MemoryBudgetOverrides, MemoryManager};
use nova_project::ProjectError;
use nova_scheduler::{CancellationToken, Cancelled};
use nova_syntax::SyntaxNode;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use walkdir::WalkDir;

mod engine;
mod snapshot;
mod watch;
mod watch_roots;

pub use engine::{IndexProgress, WatcherHandle, WorkspaceEvent, WorkspaceStatus};
pub use nova_index::SearchSymbol as WorkspaceSymbol;
pub use snapshot::WorkspaceSnapshot;
pub use watch::{ChangeCategory, NormalizedEvent, WatchConfig};

/// A minimal, library-first backend for the `nova` CLI.
///
/// This is intentionally lightweight: it provides basic project loading,
/// indexing, diagnostics, and cache management without requiring an editor or
/// LSP transport.
#[derive(Clone)]
pub struct Workspace {
    root: PathBuf,
    engine: Arc<engine::WorkspaceEngine>,
    memory: MemoryManager,
    symbol_searcher: Arc<WorkspaceSymbolSearcher>,
}

impl fmt::Debug for Workspace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Workspace")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl Workspace {
    /// Construct a workspace that doesn't touch the OS filesystem.
    ///
    /// This is primarily intended for integration tests exercising the workspace
    /// event stream and overlay handling.
    pub fn new_in_memory() -> Self {
        let memory = MemoryManager::new(MemoryBudget::default_for_system_with_env_overrides());
        Self::new_in_memory_with_memory_manager(memory)
    }

    /// Construct an in-memory workspace using a caller-provided [`MemoryManager`].
    ///
    /// This is useful for higher-level hosts (or integration tests) that want to
    /// share a single memory manager across multiple Nova components.
    pub fn new_in_memory_with_memory_manager(memory: MemoryManager) -> Self {
        let symbol_searcher = WorkspaceSymbolSearcher::new(&memory);
        let engine_config = engine::WorkspaceEngineConfig {
            workspace_root: PathBuf::new(),
            persistence: PersistenceConfig {
                mode: PersistenceMode::Disabled,
                cache: CacheConfig::from_env(),
            },
            memory: memory.clone(),
            build_runner: None,
        };
        Self {
            root: PathBuf::new(),
            engine: Arc::new(engine::WorkspaceEngine::new(engine_config)),
            memory,
            symbol_searcher,
        }
    }

    /// Open a workspace rooted at `path`.
    ///
    /// If `path` is a file, its parent directory is treated as the workspace root.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_memory_overrides(path, MemoryBudgetOverrides::default())
    }

    pub fn open_with_config(
        path: impl AsRef<Path>,
        config: &nova_config::NovaConfig,
    ) -> Result<Self> {
        Self::open_with_memory_overrides(path, config.memory_budget_overrides())
    }

    /// Open a workspace rooted at `path`, using a caller-provided [`MemoryManager`].
    ///
    /// This is intended for higher-level hosts (e.g. a server process) that want
    /// multiple components (workspace, query cache, symbol search index, etc.) to
    /// account against a single shared memory manager.
    pub fn open_with_memory_manager(path: impl AsRef<Path>, memory: MemoryManager) -> Result<Self> {
        Self::open_with_memory_manager_and_persistence(path, memory, PersistenceConfig::from_env())
    }

    /// Like [`Self::open_with_memory_manager`], but allows overriding persistence configuration.
    pub fn open_with_memory_manager_and_persistence(
        path: impl AsRef<Path>,
        memory: MemoryManager,
        persistence: PersistenceConfig,
    ) -> Result<Self> {
        let root = resolve_workspace_root(path.as_ref())?;
        Self::open_from_root_with_memory_manager(root, memory, persistence)
    }

    pub fn open_with_memory_overrides(
        path: impl AsRef<Path>,
        config_memory_overrides: MemoryBudgetOverrides,
    ) -> Result<Self> {
        let root = resolve_workspace_root(path.as_ref())?;
        let memory_budget = MemoryBudget::default_for_system()
            .apply_overrides(config_memory_overrides)
            .apply_overrides(MemoryBudgetOverrides::from_env());
        let memory = MemoryManager::new(memory_budget);
        Self::open_from_root_with_memory_manager(root, memory, PersistenceConfig::from_env())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn cache_root(&self) -> Result<PathBuf> {
        Ok(self.open_cache_dir()?.root().to_path_buf())
    }

    fn open_cache_dir(&self) -> Result<CacheDir> {
        CacheDir::new(&self.root, CacheConfig::from_env()).with_context(|| {
            format!(
                "failed to open cache dir for project at {}",
                self.root.display()
            )
        })
    }

    // ---------------------------------------------------------------------
    // Editor/LSP-facing APIs (workspace engine)
    // ---------------------------------------------------------------------

    /// Subscribe to workspace events (diagnostics, indexing progress, file changes).
    ///
    /// This stream is **bounded** to avoid unbounded memory growth under event storms. If a
    /// subscriber does not keep up, events may be dropped.
    pub fn subscribe(&self) -> async_channel::Receiver<WorkspaceEvent> {
        self.engine.subscribe()
    }

    pub fn open_document(
        &self,
        path: nova_vfs::VfsPath,
        text: String,
        version: i32,
    ) -> nova_vfs::FileId {
        self.engine.open_document(path, text, version)
    }

    pub fn close_document(&self, path: &nova_vfs::VfsPath) {
        self.engine.close_document(path);
    }

    pub fn apply_changes(
        &self,
        path: &nova_vfs::VfsPath,
        new_version: i32,
        changes: &[nova_vfs::ContentChange],
    ) -> std::result::Result<Vec<nova_core::TextEdit>, nova_vfs::DocumentError> {
        self.engine.apply_changes(path, new_version, changes)
    }

    pub fn completions(
        &self,
        path: &nova_vfs::VfsPath,
        offset: usize,
    ) -> Vec<nova_types::CompletionItem> {
        self.engine.completions(path, offset)
    }

    /// Capture an owned, thread-safe view of the current workspace files.
    ///
    /// The resulting snapshot implements `nova_db::Database`, which allows callers to run
    /// `nova_ide::code_intelligence` queries (diagnostics, completion, navigation) with
    /// workspace context while preserving the `FileId`s allocated by the VFS.
    pub fn snapshot(&self) -> WorkspaceSnapshot {
        WorkspaceSnapshot::from_engine(self.engine.as_ref())
    }

    /// Force a memory enforcement pass, potentially evicting cold caches.
    ///
    /// This is primarily exposed for integration tests and debug tooling.
    pub fn enforce_memory(&self) -> nova_memory::MemoryReport {
        self.memory.enforce()
    }

    /// Return a detailed memory report including per-component usage.
    ///
    /// This is primarily exposed for integration tests and debug tooling.
    pub fn memory_report_detailed(
        &self,
    ) -> (nova_memory::MemoryReport, Vec<nova_memory::ComponentUsage>) {
        self.memory.report_detailed()
    }

    /// Debug helper to inspect the current Salsa `file_content` input for a `FileId`.
    ///
    /// This does **not** trigger on-demand reload; callers may observe evicted content.
    pub fn debug_salsa_file_content(&self, file_id: FileId) -> Option<Arc<String>> {
        self.engine.salsa_file_content(file_id)
    }

    /// Run Salsa's Java parser for a file, reloading evicted closed-file content on demand.
    pub fn salsa_parse_java(&self, file_id: FileId) -> Arc<nova_syntax::JavaParseResult> {
        self.engine.salsa_parse_java(file_id)
    }

    pub fn trigger_indexing(&self) {
        self.engine.trigger_indexing();
    }

    pub fn debug_configurations(&self) -> Vec<nova_ide::DebugConfiguration> {
        self.engine.debug_configurations(&self.root)
    }

    pub fn apply_filesystem_events(&self, events: Vec<NormalizedEvent>) {
        self.engine.apply_filesystem_events(events);
    }

    pub fn start_watching(&self) -> Result<WatcherHandle> {
        self.engine.start_watching()
    }

    fn java_files_in(&self, root: &Path) -> Result<Vec<PathBuf>> {
        match fs::metadata(root) {
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", root.display()));
            }
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
        files.sort();
        Ok(files)
    }

    fn java_files_in_cancelable(
        &self,
        root: &Path,
        cancel: &CancellationToken,
    ) -> Result<Vec<PathBuf>> {
        Cancelled::check(cancel)?;

        match fs::metadata(root) {
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", root.display()));
            }
        }

        let mut files = Vec::new();
        for entry in WalkDir::new(root).follow_links(true) {
            Cancelled::check(cancel)?;
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("java") {
                files.push(path.to_path_buf());
            }
        }
        files.sort();
        Ok(files)
    }

    fn project_java_files(&self) -> Result<Vec<PathBuf>> {
        match nova_project::load_project_with_workspace_config(&self.root) {
            Ok(config) => {
                let mut files = Vec::new();
                for root in config.source_roots {
                    files.extend(self.java_files_in(&root.path)?);
                }
                files.sort();
                files.dedup();
                Ok(files)
            }
            Err(ProjectError::UnknownProjectType { .. }) => self.java_files_in(&self.root),
            Err(err) => Err(anyhow::anyhow!(err))
                .with_context(|| format!("failed to load project at {}", self.root.display())),
        }
    }

    fn project_java_files_cancelable(&self, cancel: &CancellationToken) -> Result<Vec<PathBuf>> {
        Cancelled::check(cancel)?;

        match nova_project::load_project_with_workspace_config(&self.root) {
            Ok(config) => {
                let mut files = Vec::new();
                for root in config.source_roots {
                    Cancelled::check(cancel)?;
                    files.extend(self.java_files_in_cancelable(&root.path, cancel)?);
                }
                files.sort();
                files.dedup();
                Ok(files)
            }
            Err(ProjectError::UnknownProjectType { .. }) => {
                self.java_files_in_cancelable(&self.root, cancel)
            }
            Err(err) => Err(anyhow::anyhow!(err))
                .with_context(|| format!("failed to load project at {}", self.root.display())),
        }
    }

    pub fn index(&self) -> Result<IndexReport> {
        let cancel = CancellationToken::new();
        let (snapshot, cache_dir, _shards, metrics) = self.build_indexes(false, &cancel)?;
        Ok(IndexReport {
            root: snapshot.project_root().to_path_buf(),
            project_hash: snapshot.project_hash().as_str().to_string(),
            cache_root: cache_dir.root().to_path_buf(),
            metrics,
        })
    }

    /// Index a project and persist the resulting artifacts into Nova's persistent cache.
    pub fn index_and_write_cache(&self) -> Result<IndexReport> {
        let shard_count = DEFAULT_SHARD_COUNT;
        let cancel = CancellationToken::new();
        let (snapshot, cache_dir, mut shards, metrics) = self.build_indexes(false, &cancel)?;
        if metrics.files_invalidated > 0 {
            save_sharded_indexes(&cache_dir, &snapshot, shard_count, &mut shards)
                .context("failed to persist indexes")?;
        }
        self.write_cache_perf(&cache_dir, &metrics)?;
        Ok(IndexReport {
            root: snapshot.project_root().to_path_buf(),
            project_hash: snapshot.project_hash().as_str().to_string(),
            cache_root: cache_dir.root().to_path_buf(),
            metrics,
        })
    }

    fn build_indexes(
        &self,
        load_shards_on_hit: bool,
        cancel: &CancellationToken,
    ) -> Result<(ProjectSnapshot, CacheDir, Vec<ProjectIndexes>, PerfMetrics)> {
        let start = Instant::now();

        Cancelled::check(cancel)?;
        let files = self.project_java_files_cancelable(cancel)?;
        Cancelled::check(cancel)?;
        let cache_dir = self.open_cache_dir()?;

        // ------------------------------------------------------------------
        // Stamp snapshot (metadata-only) + warm-start invalidation.
        // ------------------------------------------------------------------

        let snapshot_start = Instant::now();
        let stamp_snapshot = ProjectSnapshot::new_fast(&self.root, files).with_context(|| {
            format!(
                "failed to build file stamp snapshot for {}",
                self.root.display()
            )
        })?;
        Cancelled::check(cancel)?;
        let snapshot_ms = snapshot_start.elapsed().as_millis();

        // Load persisted sharded indexes based on the stamp snapshot. This avoids hashing
        // full file contents before deciding whether the cache is reusable.
        let shard_count = DEFAULT_SHARD_COUNT;
        let loaded = load_sharded_index_view_lazy_from_fast_snapshot(
            &cache_dir,
            &stamp_snapshot,
            shard_count,
        )
        .context("failed to load cached indexes")?;
        Cancelled::check(cancel)?;
        let (loaded_view, mut invalidated_files) = match loaded {
            Some(loaded) => (Some(loaded.view), loaded.invalidated_files),
            None => (
                None,
                stamp_snapshot.file_fingerprints().keys().cloned().collect(),
            ),
        };

        if invalidated_files.is_empty() && !load_shards_on_hit {
            let symbols_indexed = self
                .read_cache_perf(&cache_dir)
                .ok()
                .flatten()
                .map(|perf| perf.symbols_indexed)
                .unwrap_or(0);

            let metrics = PerfMetrics {
                files_total: stamp_snapshot.file_fingerprints().len(),
                files_indexed: 0,
                bytes_indexed: 0,
                files_invalidated: 0,
                symbols_indexed,
                snapshot_ms,
                index_ms: 0,
                elapsed_ms: start.elapsed().as_millis(),
                rss_bytes: current_rss_bytes(),
            };

            return Ok((stamp_snapshot, cache_dir, Vec::new(), metrics));
        }

        let mut shards = match &loaded_view {
            Some(view) => {
                let invalidated_set: std::collections::HashSet<&str> =
                    invalidated_files.iter().map(|path| path.as_str()).collect();

                // If every existing file is invalidated, we will rebuild all shards from scratch,
                // so there's no need to materialize any persisted shard archives.
                let indexing_all_files = stamp_snapshot
                    .file_fingerprints()
                    .keys()
                    .all(|path| invalidated_set.contains(path.as_str()));

                // Only shards that contain at least one unchanged file need to be loaded from disk.
                // Shards where all files are invalidated can be rebuilt from scratch.
                let mut shard_has_unchanged = vec![false; shard_count as usize];
                if !indexing_all_files {
                    for path in stamp_snapshot.file_fingerprints().keys() {
                        if invalidated_set.contains(path.as_str()) {
                            continue;
                        }
                        let shard_id = shard_id_for_path(path, shard_count) as usize;
                        shard_has_unchanged[shard_id] = true;
                    }
                }

                let mut shards = Vec::with_capacity(shard_count as usize);
                for shard_id in 0..shard_count {
                    Cancelled::check(cancel)?;
                    let indexes = if indexing_all_files || !shard_has_unchanged[shard_id as usize] {
                        ProjectIndexes::default()
                    } else {
                        match view.shard(shard_id) {
                            Some(archives) => ProjectIndexes {
                                symbols: archives.symbols.to_owned()?,
                                references: archives.references.to_owned()?,
                                inheritance: archives.inheritance.to_owned()?,
                                annotations: archives.annotations.to_owned()?,
                            },
                            None => ProjectIndexes::default(),
                        }
                    };
                    shards.push(indexes);
                }

                // If any shards were missing or corrupt, mark all files that map to those shards
                // as invalidated so we rebuild them during this indexing run.
                let missing_shards = view.missing_shards();
                if !missing_shards.is_empty() {
                    let mut invalidated: std::collections::BTreeSet<String> =
                        invalidated_files.into_iter().collect();
                    for path in stamp_snapshot.file_fingerprints().keys() {
                        if missing_shards.contains(&shard_id_for_path(path, shard_count)) {
                            invalidated.insert(path.clone());
                        }
                    }
                    invalidated_files = invalidated.into_iter().collect();
                }
                shards
            }
            None => (0..shard_count)
                .map(|_| ProjectIndexes::default())
                .collect(),
        };

        if invalidated_files.is_empty() {
            let symbols_indexed = self
                .read_cache_perf(&cache_dir)
                .ok()
                .flatten()
                .map(|perf| perf.symbols_indexed)
                .unwrap_or_else(|| count_symbols(&shards));

            let metrics = PerfMetrics {
                files_total: stamp_snapshot.file_fingerprints().len(),
                files_indexed: 0,
                bytes_indexed: 0,
                files_invalidated: 0,
                symbols_indexed,
                snapshot_ms,
                index_ms: 0,
                elapsed_ms: start.elapsed().as_millis(),
                rss_bytes: current_rss_bytes(),
            };

            return Ok((stamp_snapshot, cache_dir, shards, metrics));
        }

        // If everything is invalidated, we will re-index all existing files and
        // can rebuild the content fingerprint map from scratch. Avoid loading
        // the previous metadata (which can be large on big projects).
        let indexing_all_files = invalidated_files
            .iter()
            .filter(|path| stamp_snapshot.file_fingerprints().contains_key(*path))
            .count()
            == stamp_snapshot.file_fingerprints().len();

        // Load cache metadata (if any) so we can reuse content fingerprints for
        // unchanged files when persisting updated metadata.
        let metadata_path = cache_dir.metadata_path();
        let mut content_fingerprints = if indexing_all_files {
            std::collections::BTreeMap::new()
        } else {
            CacheMetadata::load(&metadata_path)
                .ok()
                .filter(|meta| {
                    meta.is_compatible() && &meta.project_hash == stamp_snapshot.project_hash()
                })
                .map(|meta| meta.file_fingerprints)
                .unwrap_or_default()
        };

        // If metadata is missing content fingerprints for a file that still
        // exists, force it through the indexer so we can compute a content hash
        // without re-reading the entire project.
        if !indexing_all_files {
            for path in stamp_snapshot.file_fingerprints().keys() {
                Cancelled::check(cancel)?;
                if !content_fingerprints.contains_key(path) {
                    invalidated_files.push(path.clone());
                }
            }
        }
        if invalidated_files.len() > 1 {
            invalidated_files.sort();
            invalidated_files.dedup();
        }

        // Remove stale results for invalidated (new/modified/deleted) files before re-indexing.
        for file in &invalidated_files {
            Cancelled::check(cancel)?;
            let shard = shard_id_for_path(file, shard_count) as usize;
            shards[shard].invalidate_file(file);
        }

        // `invalidated_files` may include deleted files. Only re-index files that still exist in
        // the current snapshot.
        let files_to_index: Vec<String> = if indexing_all_files {
            stamp_snapshot.file_fingerprints().keys().cloned().collect()
        } else {
            invalidated_files
                .iter()
                .filter(|path| stamp_snapshot.file_fingerprints().contains_key(*path))
                .cloned()
                .collect()
        };

        let (files_indexed, bytes_indexed, updated_fingerprints, index_ms) =
            if files_to_index.is_empty() {
                (0usize, 0u64, std::collections::BTreeMap::new(), 0u128)
            } else {
                Cancelled::check(cancel)?;
                let index_start = Instant::now();
                let (files_indexed, bytes_indexed, updated_fingerprints) = self.index_files(
                    &stamp_snapshot,
                    &mut shards,
                    shard_count,
                    &files_to_index,
                    cancel,
                )?;
                let index_ms = index_start.elapsed().as_millis();
                (files_indexed, bytes_indexed, updated_fingerprints, index_ms)
            };
        Cancelled::check(cancel)?;

        // Apply updated fingerprints and drop deleted files to produce a complete
        // content-hash snapshot for persistence without re-reading unchanged
        // files.
        content_fingerprints
            .retain(|path, _| stamp_snapshot.file_fingerprints().contains_key(path));
        for (path, fp) in updated_fingerprints {
            content_fingerprints.insert(path, fp);
        }

        let snapshot = ProjectSnapshot::from_parts(
            stamp_snapshot.project_root().to_path_buf(),
            stamp_snapshot.project_hash().clone(),
            content_fingerprints,
        );

        let metrics = PerfMetrics {
            files_total: stamp_snapshot.file_fingerprints().len(),
            files_indexed,
            bytes_indexed,
            files_invalidated: invalidated_files.len(),
            snapshot_ms,
            index_ms,
            symbols_indexed: count_symbols(&shards),
            elapsed_ms: start.elapsed().as_millis(),
            rss_bytes: current_rss_bytes(),
        };

        Ok((snapshot, cache_dir, shards, metrics))
    }

    fn index_files(
        &self,
        snapshot: &ProjectSnapshot,
        shards: &mut [ProjectIndexes],
        shard_count: u32,
        files_to_index: &[String],
        cancel: &CancellationToken,
    ) -> Result<(usize, u64, std::collections::BTreeMap<String, Fingerprint>)> {
        use nova_core::{LineIndex, TextSize};
        use nova_db::NovaHir;
        use nova_hir::ast_id::AstId;

        let db = SalsaDatabase::new_with_persistence(
            snapshot.project_root(),
            PersistenceConfig::from_env(),
        );

        let mut files_indexed = 0usize;
        let mut bytes_indexed = 0u64;
        // Keep rel paths as `Arc<String>` while the Salsa DB is alive so the same allocation can be
        // used for:
        // - tracked input `file_rel_path`
        // - non-tracked persistence `file_path`
        // - snapshot metadata (`ProjectSnapshot::file_fingerprints`)
        //
        // We convert back to `String` after the DB is dropped; at that point each Arc is uniquely
        // owned and can be unwrapped without cloning.
        let mut file_fingerprints: std::collections::BTreeMap<Arc<String>, Fingerprint> =
            std::collections::BTreeMap::new();

        for (idx, file) in files_to_index.iter().enumerate() {
            Cancelled::check(cancel)?;
            let shard = shard_id_for_path(file, shard_count) as usize;
            let indexes = &mut shards[shard];
            let full_path = snapshot.project_root().join(file);
            let content = fs::read_to_string(&full_path)
                .with_context(|| format!("failed to read {}", full_path.display()))?;
            files_indexed += 1;
            bytes_indexed += content.len() as u64;
            let fingerprint = Fingerprint::from_bytes(content.as_bytes());
            let rel_path = Arc::new(file.clone());
            file_fingerprints.insert(rel_path.clone(), fingerprint);

            let file_id = FileId::from_raw(idx as u32);
            // Set `file_rel_path` first so `set_file_text` doesn't synthesize (and then discard) a
            // default rel-path like `file-123.java`.
            db.set_file_rel_path(file_id, rel_path);
            // `set_file_text` consumes the text; keep `content` for (line, col) patching below.
            db.set_file_text(file_id, content.clone());

            let (delta, hir) = db.with_snapshot(|snap| {
                let delta = snap.file_index_delta(file_id);
                let hir = snap.hir_item_tree(file_id);
                (delta, hir)
            });

            // Patch (line, col) from HIR name ranges, using UTF-16 positions.
            let line_index = LineIndex::new(&content);
            let mut delta = (*delta).clone();
            for symbols in delta.symbols.symbols.values_mut() {
                for entry in symbols.iter_mut() {
                    let ast_id = AstId::new(entry.ast_id);
                    let offset = match entry.kind {
                        nova_index::IndexSymbolKind::Class => {
                            hir.classes.get(&ast_id).map(|it| it.name_range.start)
                        }
                        nova_index::IndexSymbolKind::Interface => {
                            hir.interfaces.get(&ast_id).map(|it| it.name_range.start)
                        }
                        nova_index::IndexSymbolKind::Enum => {
                            hir.enums.get(&ast_id).map(|it| it.name_range.start)
                        }
                        nova_index::IndexSymbolKind::Record => {
                            hir.records.get(&ast_id).map(|it| it.name_range.start)
                        }
                        nova_index::IndexSymbolKind::Annotation => {
                            hir.annotations.get(&ast_id).map(|it| it.name_range.start)
                        }
                        nova_index::IndexSymbolKind::Field => {
                            hir.fields.get(&ast_id).map(|it| it.name_range.start)
                        }
                        nova_index::IndexSymbolKind::Method => {
                            hir.methods.get(&ast_id).map(|it| it.name_range.start)
                        }
                        nova_index::IndexSymbolKind::Constructor => {
                            hir.constructors.get(&ast_id).map(|it| it.name_range.start)
                        }
                    };

                    let Some(offset) = offset else {
                        continue;
                    };

                    let pos = line_index.position(&content, TextSize::from(offset as u32));
                    entry.location.line = pos.line;
                    entry.location.column = pos.character;
                }
            }

            indexes.merge_from(delta);
        }

        drop(db);

        let file_fingerprints = file_fingerprints
            .into_iter()
            .map(|(path, fp)| {
                let path = match Arc::try_unwrap(path) {
                    Ok(path) => path,
                    Err(path) => (*path).clone(),
                };
                (path, fp)
            })
            .collect();

        Ok((files_indexed, bytes_indexed, file_fingerprints))
    }

    pub fn diagnostics(&self, path: impl AsRef<Path>) -> Result<DiagnosticsReport> {
        let path = path.as_ref();
        let meta = fs::metadata(path)
            .with_context(|| format!("failed to read metadata for {}", path.display()))?;
        let mut diagnostics = Vec::new();

        let requested_file = if meta.is_dir() {
            None
        } else {
            Some(fs::canonicalize(path).with_context(|| {
                format!("failed to canonicalize diagnostics path {}", path.display())
            })?)
        };

        // Always load full project sources so framework diagnostics can see both bean
        // definitions and injection sites, even when diagnostics are requested for a
        // single file.
        let files = self.project_java_files().or_else(|_| {
            if meta.is_dir() {
                self.java_files_in(path)
            } else {
                Ok(vec![path.to_path_buf()])
            }
        })?;

        let mut sources = Vec::with_capacity(files.len());
        for file in &files {
            let file = fs::canonicalize(file).unwrap_or_else(|_| file.clone());
            let content = fs::read_to_string(&file)
                .with_context(|| format!("failed to read {}", file.display()))?;
            sources.push((file, content));
        }

        for (file, content) in &sources {
            if requested_file
                .as_ref()
                .is_some_and(|requested| requested != file.as_path())
            {
                continue;
            }

            let display_path = file
                .strip_prefix(&self.root)
                .unwrap_or(file.as_path())
                .to_path_buf();

            // Parse errors (from the Java syntax parser).
            for err in parse_java_errors(content) {
                diagnostics.push(Diagnostic {
                    file: display_path.clone(),
                    line: err.line,
                    column: err.column,
                    severity: Severity::Error,
                    code: Some("PARSE".to_string()),
                    message: err.message,
                });
            }

            // Heuristic diagnostics: TODO/FIXME markers.
            for (needle, sev, code) in [
                ("TODO", Severity::Warning, "TODO"),
                ("FIXME", Severity::Warning, "FIXME"),
            ] {
                for (line_idx, line) in content.lines().enumerate() {
                    if let Some(col) = line.find(needle) {
                        diagnostics.push(Diagnostic {
                            file: display_path.clone(),
                            line: line_idx + 1,
                            column: col + 1,
                            severity: sev,
                            code: Some(code.to_string()),
                            message: format!("found {}", needle),
                        });
                    }
                }
            }
        }

        diagnostics.extend(self.spring_diagnostics(&sources, requested_file.as_deref())?);
        diagnostics.extend(self.jpa_diagnostics(&sources, requested_file.as_deref())?);

        let summary = DiagnosticsSummary::from_diagnostics(&diagnostics);
        Ok(DiagnosticsReport {
            root: self.root.clone(),
            diagnostics,
            summary,
        })
    }

    fn spring_diagnostics(
        &self,
        sources: &[(PathBuf, String)],
        requested_file: Option<&Path>,
    ) -> Result<Vec<Diagnostic>> {
        let config = match nova_project::load_project_with_workspace_config(&self.root) {
            Ok(config) => Some(config),
            Err(ProjectError::UnknownProjectType { .. }) => None,
            Err(err) => {
                return Err(anyhow::anyhow!(err))
                    .with_context(|| format!("failed to load project at {}", self.root.display()))
            }
        };

        let Some(config) = config else {
            return Ok(Vec::new());
        };

        if !nova_framework_spring::is_spring_applicable(&config) {
            return Ok(Vec::new());
        }

        let texts: Vec<&str> = sources.iter().map(|(_, text)| text.as_str()).collect();
        let analysis = nova_framework_spring::analyze_java_sources(&texts);

        let mut diags = Vec::new();
        for source_diag in analysis.diagnostics {
            let Some((file_path, file_text)) = sources.get(source_diag.source) else {
                continue;
            };
            if requested_file.is_some_and(|requested| requested != file_path.as_path()) {
                continue;
            }

            let (line, column) = source_diag
                .diagnostic
                .span
                .map(|span| line_col_at(file_text, span.start))
                .unwrap_or((1, 1));

            let severity = match source_diag.diagnostic.severity {
                nova_framework_spring::Severity::Error => Severity::Error,
                nova_framework_spring::Severity::Warning => Severity::Warning,
                nova_framework_spring::Severity::Info => Severity::Warning,
            };

            let display_path = file_path
                .strip_prefix(&self.root)
                .unwrap_or(file_path.as_path())
                .to_path_buf();

            diags.push(Diagnostic {
                file: display_path,
                line,
                column,
                severity,
                code: Some(source_diag.diagnostic.code.to_string()),
                message: source_diag.diagnostic.message,
            });
        }

        Ok(diags)
    }

    fn jpa_diagnostics(
        &self,
        sources: &[(PathBuf, String)],
        requested_file: Option<&Path>,
    ) -> Result<Vec<Diagnostic>> {
        let config = match nova_project::load_project_with_workspace_config(&self.root) {
            Ok(config) => Some(config),
            Err(ProjectError::UnknownProjectType { .. }) => None,
            Err(err) => {
                return Err(anyhow::anyhow!(err))
                    .with_context(|| format!("failed to load project at {}", self.root.display()))
            }
        };

        let Some(config) = config else {
            return Ok(Vec::new());
        };

        let dep_strings: Vec<String> = config
            .dependencies
            .iter()
            .map(|d| format!("{}:{}", d.group_id, d.artifact_id))
            .collect();
        let dep_refs: Vec<&str> = dep_strings.iter().map(String::as_str).collect();
        let classpath: Vec<&Path> = config
            .classpath
            .iter()
            .chain(config.module_path.iter())
            .map(|e| e.path.as_path())
            .collect();
        let texts: Vec<&str> = sources.iter().map(|(_, text)| text.as_str()).collect();

        if !nova_framework_jpa::is_jpa_applicable_with_classpath(&dep_refs, &classpath, &texts) {
            return Ok(Vec::new());
        }

        let analysis = nova_framework_jpa::analyze_java_sources(&texts);

        let mut diags = Vec::new();
        for source_diag in analysis.diagnostics {
            let Some((file_path, file_text)) = sources.get(source_diag.source) else {
                continue;
            };
            if requested_file.is_some_and(|requested| requested != file_path.as_path()) {
                continue;
            }

            let (line, column) = source_diag
                .diagnostic
                .span
                .map(|span| line_col_at(file_text, span.start))
                .unwrap_or((1, 1));

            let severity = match source_diag.diagnostic.severity {
                nova_framework_jpa::Severity::Error => Severity::Error,
                nova_framework_jpa::Severity::Warning => Severity::Warning,
                nova_framework_jpa::Severity::Info => Severity::Warning,
            };

            let display_path = file_path
                .strip_prefix(&self.root)
                .unwrap_or(file_path.as_path())
                .to_path_buf();

            diags.push(Diagnostic {
                file: display_path,
                line,
                column,
                severity,
                code: Some(source_diag.diagnostic.code.to_string()),
                message: source_diag.diagnostic.message,
            });
        }

        Ok(diags)
    }

    pub fn workspace_symbols(&self, query: &str) -> Result<Vec<WorkspaceSymbol>> {
        let cancel = CancellationToken::new();
        self.workspace_symbols_cancelable(query, &cancel)
    }

    pub fn workspace_symbols_cancelable(
        &self,
        query: &str,
        cancel: &CancellationToken,
    ) -> Result<Vec<WorkspaceSymbol>> {
        // Keep the symbol index up to date by running the incremental indexer
        // and persisting the updated indexes into the on-disk cache.
        Cancelled::check(cancel)?;
        let shard_count = DEFAULT_SHARD_COUNT;
        let (snapshot, cache_dir, mut shards, metrics) = self.build_indexes(true, cancel)?;
        Cancelled::check(cancel)?;
        if metrics.files_invalidated > 0 {
            save_sharded_indexes(&cache_dir, &snapshot, shard_count, &mut shards)
                .context("failed to persist indexes")?;
        }
        Cancelled::check(cancel)?;
        self.write_cache_perf(&cache_dir, &metrics)?;

        const WORKSPACE_SYMBOL_LIMIT: usize = 200;
        let indexes_changed = metrics.files_invalidated > 0;
        let (results, _stats) = fuzzy_rank_workspace_symbols_sharded(
            self.symbol_searcher.as_ref(),
            &shards,
            query,
            WORKSPACE_SYMBOL_LIMIT,
            indexes_changed,
        );
        self.memory.enforce();
        Ok(results)
    }

    pub fn parse_file(&self, file: impl AsRef<Path>) -> Result<ParseResult> {
        let file = file.as_ref();
        let content = fs::read_to_string(file)
            .with_context(|| format!("failed to read {}", file.display()))?;
        let parsed = nova_syntax::parse_java(&content);
        let tree = debug_dump_syntax(&parsed.syntax());
        let errors = parsed
            .errors
            .into_iter()
            .map(|err| {
                let (line, column) = line_col_at(&content, err.range.start as usize);
                ParseError {
                    message: err.message,
                    line,
                    column,
                }
            })
            .collect();
        Ok(ParseResult { tree, errors })
    }

    pub fn cache_status(&self) -> Result<CacheStatus> {
        let cache_dir = self.open_cache_dir()?;

        let metadata_path = cache_dir.metadata_path();
        let metadata = if metadata_path.exists() || cache_dir.metadata_bin_path().exists() {
            CacheMetadata::load(&metadata_path).ok()
        } else {
            None
        };

        let mut indexes = Vec::new();
        let indexes_dir = cache_dir.indexes_dir();
        let shards_root = indexes_dir.join("shards");
        let shard_manifest = shards_root.join("manifest.txt");
        if shard_manifest.is_file() {
            let bytes = fs::metadata(&shard_manifest).ok().map(|m| m.len());
            indexes.push(CacheArtifact {
                name: "shards_manifest".to_string(),
                path: shard_manifest,
                bytes,
            });

            let total_bytes = walkdir::WalkDir::new(&shards_root)
                .follow_links(false)
                .into_iter()
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_type().is_file())
                .filter_map(|entry| entry.metadata().ok().map(|m| m.len()))
                .sum();
            indexes.push(CacheArtifact {
                name: "shards".to_string(),
                path: shards_root,
                bytes: Some(total_bytes),
            });
        } else {
            for (name, path) in [
                ("symbols", indexes_dir.join("symbols.idx")),
                ("references", indexes_dir.join("references.idx")),
                ("inheritance", indexes_dir.join("inheritance.idx")),
                ("annotations", indexes_dir.join("annotations.idx")),
            ] {
                let bytes = fs::metadata(&path).ok().map(|m| m.len());
                indexes.push(CacheArtifact {
                    name: name.to_string(),
                    path,
                    bytes,
                });
            }
        }

        let perf_path = cache_dir.root().join("perf.json");
        let perf_bytes = fs::metadata(&perf_path).ok().map(|m| m.len());
        let last_perf = self.read_cache_perf(&cache_dir)?;

        Ok(CacheStatus {
            project_root: cache_dir.project_root().to_path_buf(),
            project_hash: cache_dir.project_hash().as_str().to_string(),
            cache_root: cache_dir.root().to_path_buf(),
            metadata_path,
            metadata,
            indexes,
            perf_path,
            perf_bytes,
            last_perf,
        })
    }

    pub fn cache_clean(&self) -> Result<()> {
        let cache_dir = self.open_cache_dir()?;
        let root = cache_dir.root();
        if root.exists() {
            fs::remove_dir_all(root)
                .with_context(|| format!("failed to remove {}", root.display()))?;
        }
        Ok(())
    }

    pub fn cache_warm(&self) -> Result<IndexReport> {
        self.index_and_write_cache()
    }

    pub fn perf_report(&self) -> Result<Option<PerfMetrics>> {
        let cache_dir = self.open_cache_dir()?;
        self.read_cache_perf(&cache_dir)
    }

    fn read_cache_perf(&self, cache_dir: &CacheDir) -> Result<Option<PerfMetrics>> {
        let path = cache_dir.root().join("perf.json");
        if !path.exists() {
            return Ok(None);
        }
        let data = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        Ok(Some(
            serde_json::from_str::<PerfMetrics>(&data)
                .with_context(|| format!("failed to parse {}", path.display()))?,
        ))
    }

    fn write_cache_perf(&self, cache_dir: &CacheDir, metrics: &PerfMetrics) -> Result<()> {
        let path = cache_dir.root().join("perf.json");
        let json = serde_json::to_vec_pretty(metrics)?;
        nova_cache::atomic_write(&path, &json)
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
impl Workspace {
    pub(crate) fn engine_for_tests(&self) -> &engine::WorkspaceEngine {
        self.engine.as_ref()
    }
}

#[cfg(test)]
fn fuzzy_rank_workspace_symbols(
    searcher: &WorkspaceSymbolSearcher,
    symbols: &nova_index::SymbolIndex,
    query: &str,
    limit: usize,
    indexes_changed: bool,
) -> (Vec<WorkspaceSymbol>, SearchStats) {
    if query.is_empty() {
        let mut ranked = Vec::new();
        for (name, defs) in &symbols.symbols {
            for sym in defs {
                ranked.push(WorkspaceSymbol {
                    name: name.clone(),
                    qualified_name: sym.qualified_name.clone(),
                    kind: sym.kind.clone(),
                    container_name: sym.container_name.clone(),
                    location: sym.location.clone(),
                    ast_id: sym.ast_id,
                });
                if ranked.len() >= limit {
                    break;
                }
            }
            if ranked.len() >= limit {
                break;
            }
        }
        return (
            ranked,
            SearchStats {
                strategy: CandidateStrategy::FullScan,
                candidates_considered: symbols.symbols.values().map(|syms| syms.len()).sum(),
            },
        );
    }

    let (results, stats) = searcher.search_with_stats(symbols, query, limit, indexes_changed);
    (results.into_iter().map(|res| res.symbol).collect(), stats)
}

fn fuzzy_rank_workspace_symbols_sharded(
    searcher: &WorkspaceSymbolSearcher,
    shards: &[ProjectIndexes],
    query: &str,
    limit: usize,
    indexes_changed: bool,
) -> (Vec<WorkspaceSymbol>, SearchStats) {
    if query.is_empty() {
        use std::cmp::Ordering;
        use std::collections::BinaryHeap;

        struct ShardEntryIter<'a> {
            map_iter: std::collections::btree_map::Iter<'a, String, Vec<IndexedSymbol>>,
            current_name: Option<&'a str>,
            current_symbols: &'a [IndexedSymbol],
            sym_idx: usize,
        }

        impl<'a> ShardEntryIter<'a> {
            fn new(index: &'a nova_index::SymbolIndex) -> Self {
                let mut map_iter = index.symbols.iter();
                if let Some((name, locs)) = map_iter.next() {
                    Self {
                        map_iter,
                        current_name: Some(name.as_str()),
                        current_symbols: locs.as_slice(),
                        sym_idx: 0,
                    }
                } else {
                    Self {
                        map_iter,
                        current_name: None,
                        current_symbols: &[],
                        sym_idx: 0,
                    }
                }
            }

            fn next_entry(&mut self) -> Option<(&'a str, &'a IndexedSymbol, usize)> {
                loop {
                    let name = self.current_name?;
                    if self.sym_idx < self.current_symbols.len() {
                        let idx = self.sym_idx;
                        let sym = &self.current_symbols[idx];
                        self.sym_idx += 1;
                        return Some((name, sym, idx));
                    }

                    match self.map_iter.next() {
                        Some((name, locs)) => {
                            self.current_name = Some(name.as_str());
                            self.current_symbols = locs.as_slice();
                            self.sym_idx = 0;
                            continue;
                        }
                        None => {
                            self.current_name = None;
                            self.current_symbols = &[];
                            return None;
                        }
                    }
                }
            }
        }

        #[derive(Copy, Clone, Eq, PartialEq)]
        struct HeapEntry<'a> {
            name: &'a str,
            sym: &'a IndexedSymbol,
            shard_idx: usize,
            sym_idx: usize,
        }

        impl Ord for HeapEntry<'_> {
            fn cmp(&self, other: &Self) -> Ordering {
                self.name
                    .cmp(other.name)
                    .then_with(|| self.sym.qualified_name.cmp(&other.sym.qualified_name))
                    .then_with(|| self.sym.location.file.cmp(&other.sym.location.file))
                    .then_with(|| self.sym.location.line.cmp(&other.sym.location.line))
                    .then_with(|| self.sym.location.column.cmp(&other.sym.location.column))
                    .then_with(|| self.sym.ast_id.cmp(&other.sym.ast_id))
                    .then_with(|| self.shard_idx.cmp(&other.shard_idx))
                    .then_with(|| self.sym_idx.cmp(&other.sym_idx))
            }
        }

        impl PartialOrd for HeapEntry<'_> {
            fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
                Some(self.cmp(other))
            }
        }

        let mut iters: Vec<_> = shards
            .iter()
            .map(|shard| ShardEntryIter::new(&shard.symbols))
            .collect();

        let mut heap = BinaryHeap::<std::cmp::Reverse<HeapEntry<'_>>>::new();
        for (shard_idx, iter) in iters.iter_mut().enumerate() {
            if let Some((name, sym, sym_idx)) = iter.next_entry() {
                heap.push(std::cmp::Reverse(HeapEntry {
                    name,
                    shard_idx,
                    sym,
                    sym_idx,
                }));
            }
        }

        let mut ranked = Vec::new();
        while let Some(std::cmp::Reverse(entry)) = heap.pop() {
            ranked.push(WorkspaceSymbol {
                name: entry.name.to_string(),
                qualified_name: entry.sym.qualified_name.clone(),
                kind: entry.sym.kind.clone(),
                container_name: entry.sym.container_name.clone(),
                location: entry.sym.location.clone(),
                ast_id: entry.sym.ast_id,
            });

            if ranked.len() >= limit {
                break;
            }

            if let Some((name, sym, sym_idx)) = iters[entry.shard_idx].next_entry() {
                heap.push(std::cmp::Reverse(HeapEntry {
                    name,
                    shard_idx: entry.shard_idx,
                    sym,
                    sym_idx,
                }));
            }
        }

        return (
            ranked,
            SearchStats {
                strategy: CandidateStrategy::FullScan,
                candidates_considered: shards
                    .iter()
                    .flat_map(|shard| shard.symbols.symbols.values())
                    .map(|syms| syms.len())
                    .sum(),
            },
        );
    }

    if indexes_changed || !searcher.has_index() {
        let symbol_count: usize = shards
            .iter()
            .flat_map(|shard| shard.symbols.symbols.values())
            .map(|syms| syms.len())
            .sum();

        let mut search_symbols = Vec::with_capacity(symbol_count);
        for shard in shards {
            for (name, syms) in &shard.symbols.symbols {
                for sym in syms {
                    search_symbols.push(SearchSymbol {
                        name: name.clone(),
                        qualified_name: sym.qualified_name.clone(),
                        kind: sym.kind.clone(),
                        container_name: sym.container_name.clone(),
                        location: sym.location.clone(),
                        ast_id: sym.ast_id,
                    });
                }
            }
        }

        searcher.rebuild(search_symbols);
    }

    let (results, stats) = searcher.search_with_stats_cached(query, limit);
    (results.into_iter().map(|res| res.symbol).collect(), stats)
}

#[cfg(test)]
mod fuzzy_symbol_tests {
    use super::*;
    use nova_index::SymbolLocation;

    fn indexed(name: &str, location: SymbolLocation) -> nova_index::IndexedSymbol {
        nova_index::IndexedSymbol {
            qualified_name: name.to_string(),
            kind: nova_index::IndexSymbolKind::Class,
            container_name: None,
            location,
            ast_id: 0,
        }
    }

    #[test]
    fn workspace_symbol_search_uses_trigram_candidate_filtering() {
        let memory = MemoryManager::new(MemoryBudget::from_total(256 * nova_memory::MB));
        let searcher = WorkspaceSymbolSearcher::new(&memory);
        let mut symbols = nova_index::SymbolIndex::default();
        symbols.insert(
            "HashMap",
            indexed(
                "HashMap",
                SymbolLocation {
                    file: "A.java".into(),
                    line: 1,
                    column: 1,
                },
            ),
        );
        symbols.insert(
            "HashSet",
            indexed(
                "HashSet",
                SymbolLocation {
                    file: "B.java".into(),
                    line: 1,
                    column: 1,
                },
            ),
        );
        symbols.insert(
            "FooBar",
            indexed(
                "FooBar",
                SymbolLocation {
                    file: "C.java".into(),
                    line: 1,
                    column: 1,
                },
            ),
        );

        let (_results, stats) =
            fuzzy_rank_workspace_symbols(searcher.as_ref(), &symbols, "Hash", 10, true);
        assert_eq!(stats.strategy, CandidateStrategy::Trigram);
        assert!(stats.candidates_considered < symbols.symbols.len());
    }

    #[test]
    fn workspace_symbol_search_supports_acronym_queries() {
        let memory = MemoryManager::new(MemoryBudget::from_total(256 * nova_memory::MB));
        let searcher = WorkspaceSymbolSearcher::new(&memory);
        let mut symbols = nova_index::SymbolIndex::default();
        symbols.insert(
            "FooBar",
            indexed(
                "FooBar",
                SymbolLocation {
                    file: "A.java".into(),
                    line: 1,
                    column: 1,
                },
            ),
        );

        let (results, _stats) =
            fuzzy_rank_workspace_symbols(searcher.as_ref(), &symbols, "fb", 10, true);
        assert_eq!(results[0].name, "FooBar");
    }

    #[test]
    fn workspace_symbol_search_index_is_reused_between_queries() {
        use std::fs;

        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let java_dir = root.join("src/main/java/com/example");
        fs::create_dir_all(&java_dir).expect("mkdir");
        fs::write(
            java_dir.join("Main.java"),
            r#"
                package com.example;

                public class Main {
                    public void methodName() {}
                }
            "#,
        )
        .expect("write");

        let ws = Workspace::open(root).expect("workspace open");

        let _ = ws.workspace_symbols("Main").expect("workspace symbols");
        let builds_after_first = ws.symbol_searcher.build_count();
        assert_eq!(builds_after_first, 1);

        let _ = ws.workspace_symbols("met").expect("workspace symbols");
        assert_eq!(ws.symbol_searcher.build_count(), 1);
    }

    #[test]
    fn workspace_symbol_search_empty_query_is_deterministic_and_includes_duplicates() {
        let memory = MemoryManager::new(MemoryBudget::from_total(256 * nova_memory::MB));
        let searcher = WorkspaceSymbolSearcher::new(&memory);

        let mut shard0 = ProjectIndexes::default();
        shard0.symbols.insert(
            "Alpha",
            indexed(
                "Alpha",
                SymbolLocation {
                    file: "pkg/Alpha.java".into(),
                    line: 1,
                    column: 1,
                },
            ),
        );
        shard0.symbols.insert(
            "Dup",
            indexed(
                "Dup",
                SymbolLocation {
                    file: "com/foo/Dup.java".into(),
                    line: 1,
                    column: 1,
                },
            ),
        );

        let shard1 = ProjectIndexes::default(); // empty shard should not panic

        let mut shard2 = ProjectIndexes::default();
        shard2.symbols.insert(
            "Dup",
            indexed(
                "Dup",
                SymbolLocation {
                    file: "com/bar/Dup.java".into(),
                    line: 1,
                    column: 1,
                },
            ),
        );
        shard2.symbols.insert(
            "Zulu",
            indexed(
                "Zulu",
                SymbolLocation {
                    file: "pkg/Zulu.java".into(),
                    line: 1,
                    column: 1,
                },
            ),
        );

        let shards = vec![shard0, shard1, shard2];

        let (first, _stats) =
            fuzzy_rank_workspace_symbols_sharded(searcher.as_ref(), &shards, "", 10, true);
        let (second, _stats) =
            fuzzy_rank_workspace_symbols_sharded(searcher.as_ref(), &shards, "", 10, true);
        assert_eq!(first, second, "empty query results should be stable");

        let order: Vec<(String, String)> = first
            .iter()
            .map(|sym| (sym.name.clone(), sym.location.file.clone()))
            .collect();

        assert_eq!(
            order,
            vec![
                ("Alpha".to_string(), "pkg/Alpha.java".to_string()),
                ("Dup".to_string(), "com/bar/Dup.java".to_string()),
                ("Dup".to_string(), "com/foo/Dup.java".to_string()),
                ("Zulu".to_string(), "pkg/Zulu.java".to_string()),
            ]
        );

        let dup_count = first.iter().filter(|sym| sym.name == "Dup").count();
        assert_eq!(dup_count, 2, "expected duplicate entries for the same name");
    }

    #[test]
    fn workspace_symbol_locations_are_computed_from_hir_name_ranges() {
        use std::fs;

        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let java_dir = root.join("src/main/java/com/example");
        fs::create_dir_all(&java_dir).expect("mkdir");
        fs::write(
            java_dir.join("Main.java"),
            "package com.example;\n\npublic class Main {\n    public void methodName() {}\n}\n",
        )
        .expect("write");

        let ws = Workspace::open(root).expect("workspace open");
        let results = ws
            .workspace_symbols("methodName")
            .expect("workspace symbols");

        let sym = results
            .iter()
            .find(|sym| sym.qualified_name == "com.example.Main.methodName")
            .expect("expected com.example.Main.methodName in results");

        assert_eq!(sym.location.file, "src/main/java/com/example/Main.java");
        assert_eq!(sym.location.line, 3);
        assert_eq!(sym.location.column, 16);
    }

    #[test]
    fn workspace_symbol_search_disambiguates_by_qualified_name() {
        use std::fs;

        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();

        let dir_a = root.join("src/main/java/com/a");
        fs::create_dir_all(&dir_a).expect("mkdir a");
        fs::write(
            dir_a.join("Foo.java"),
            "package com.a; public class Foo {}\n",
        )
        .expect("write");

        let dir_b = root.join("src/main/java/com/b");
        fs::create_dir_all(&dir_b).expect("mkdir b");
        fs::write(
            dir_b.join("Foo.java"),
            "package com.b; public class Foo {}\n",
        )
        .expect("write");

        let ws = Workspace::open(root).expect("workspace open");

        let by_name = ws.workspace_symbols("Foo").expect("workspace symbols");
        assert!(
            by_name.iter().any(|sym| sym.qualified_name == "com.a.Foo"),
            "expected com.a.Foo in results"
        );
        assert!(
            by_name.iter().any(|sym| sym.qualified_name == "com.b.Foo"),
            "expected com.b.Foo in results"
        );

        let qualified = ws
            .workspace_symbols("com.b.Foo")
            .expect("workspace symbols");
        assert_eq!(qualified[0].qualified_name, "com.b.Foo");
    }
}

#[cfg(test)]
mod memory_manager_injection_tests {
    use super::*;

    #[test]
    fn workspace_uses_injected_memory_manager_for_budget_and_registrations() {
        use std::fs;

        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::create_dir_all(root.join("src")).expect("mkdir");
        fs::write(root.join("src/Main.java"), "class Main {}".as_bytes()).expect("write");

        let budget = MemoryBudget::from_total(8 * nova_memory::MB);
        let memory = MemoryManager::new(budget);

        let workspace = Workspace::open_with_memory_manager(root, memory.clone()).expect("open");

        assert_eq!(workspace.memory.budget(), budget);

        let (_report, components) = memory.report_detailed();
        assert!(
            components.iter().any(|c| c.name == "salsa_memos"),
            "expected Salsa memo evictor to register with injected memory manager; got {components:?}"
        );
        assert!(
            components.iter().any(|c| c.name == "symbol_search_index"),
            "expected symbol search index to register with injected memory manager; got {components:?}"
        );
    }

    #[test]
    fn in_memory_workspace_uses_injected_memory_manager_for_budget() {
        let budget = MemoryBudget::from_total(8 * nova_memory::MB);
        let memory = MemoryManager::new(budget);

        let workspace = Workspace::new_in_memory_with_memory_manager(memory);

        assert_eq!(workspace.memory.budget(), budget);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexReport {
    pub root: PathBuf,
    pub project_hash: String,
    pub cache_root: PathBuf,
    pub metrics: PerfMetrics,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PerfMetrics {
    pub files_total: usize,
    #[serde(alias = "files_scanned")]
    pub files_indexed: usize,
    #[serde(alias = "bytes_scanned")]
    pub bytes_indexed: u64,
    pub files_invalidated: usize,
    pub symbols_indexed: usize,
    pub snapshot_ms: u128,
    pub index_ms: u128,
    pub elapsed_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rss_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsReport {
    pub root: PathBuf,
    pub diagnostics: Vec<Diagnostic>,
    pub summary: DiagnosticsSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsSummary {
    pub errors: usize,
    pub warnings: usize,
}

impl DiagnosticsSummary {
    fn from_diagnostics(diagnostics: &[Diagnostic]) -> Self {
        let mut errors = 0usize;
        let mut warnings = 0usize;
        for d in diagnostics {
            match d.severity {
                Severity::Error => errors += 1,
                Severity::Warning => warnings += 1,
            }
        }
        Self { errors, warnings }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub file: PathBuf,
    pub line: usize,
    pub column: usize,
    pub severity: Severity,
    pub code: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheStatus {
    pub project_root: PathBuf,
    pub project_hash: String,
    pub cache_root: PathBuf,
    pub metadata_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<CacheMetadata>,
    pub indexes: Vec<CacheArtifact>,
    pub perf_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub perf_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_perf: Option<PerfMetrics>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheArtifact {
    pub name: String,
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParseResult {
    pub tree: String,
    pub errors: Vec<ParseError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParseError {
    pub message: String,
    pub line: usize,
    pub column: usize,
}

fn parse_java_errors(text: &str) -> Vec<ParseError> {
    let parsed = nova_syntax::parse_java(text);
    parsed
        .errors
        .into_iter()
        .map(|err| {
            let (line, column) = line_col_at(text, err.range.start as usize);
            ParseError {
                message: err.message,
                line,
                column,
            }
        })
        .collect()
}

fn debug_dump_syntax(node: &SyntaxNode) -> String {
    fn go(node: &SyntaxNode, indent: usize, out: &mut String) {
        use rowan::NodeOrToken;
        use std::fmt::Write;

        let _ = writeln!(out, "{:indent$}{:?}", "", node.kind(), indent = indent);
        for child in node.children_with_tokens() {
            match child {
                NodeOrToken::Node(n) => go(&n, indent + 2, out),
                NodeOrToken::Token(t) => {
                    let _ = writeln!(
                        out,
                        "{:indent$}{:?} {:?}",
                        "",
                        t.kind(),
                        truncate_token_text(t.text(), 120),
                        indent = indent + 2
                    );
                }
            }
        }
    }

    let mut out = String::new();
    go(node, 0, &mut out);
    out
}

fn truncate_token_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }

    let mut out = String::new();
    for (idx, ch) in text.chars().enumerate() {
        if idx >= max_chars {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    out
}

fn line_col_at(text: &str, byte_idx: usize) -> (usize, usize) {
    let mut line = 1usize;
    let mut col = 1usize;
    let mut idx = 0usize;

    for ch in text.chars() {
        if idx >= byte_idx {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
        idx += ch.len_utf8();
    }

    (line, col)
}

fn count_symbols(shards: &[ProjectIndexes]) -> usize {
    shards
        .iter()
        .flat_map(|indexes| indexes.symbols.symbols.values())
        .map(|locations| locations.len())
        .sum()
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

fn find_project_root(start: &Path) -> PathBuf {
    nova_project::workspace_root(start).unwrap_or_else(|| start.to_path_buf())
}

fn resolve_workspace_root(path: &Path) -> Result<PathBuf> {
    let meta = fs::metadata(path)
        .with_context(|| format!("failed to read metadata for {}", path.display()))?;
    let root = if meta.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .map(|p| p.to_path_buf())
            .context("file path has no parent directory")?
    };
    let root = fs::canonicalize(&root)
        .with_context(|| format!("failed to canonicalize {}", root.display()))?;
    Ok(find_project_root(&root))
}

impl Workspace {
    fn open_from_root_with_memory_manager(
        root: PathBuf,
        memory: MemoryManager,
        persistence: PersistenceConfig,
    ) -> Result<Self> {
        let symbol_searcher = WorkspaceSymbolSearcher::new(&memory);
        let engine_config = engine::WorkspaceEngineConfig {
            workspace_root: root.clone(),
            persistence,
            memory: memory.clone(),
            build_runner: None,
        };
        let engine = Arc::new(engine::WorkspaceEngine::new(engine_config));
        engine
            .set_workspace_root(&root)
            .with_context(|| format!("failed to initialize workspace at {}", root.display()))?;
        Ok(Self {
            root,
            engine,
            memory,
            symbol_searcher,
        })
    }
}
