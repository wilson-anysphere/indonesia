use anyhow::{Context, Result};
use nova_cache::{CacheConfig, CacheDir, CacheMetadata, Fingerprint, ProjectSnapshot};
use nova_index::{
    load_sharded_index_archives_from_fast_snapshot, save_sharded_indexes, shard_id_for_path,
    CandidateStrategy, ProjectIndexes, SearchStats, SearchSymbol, SymbolLocation,
    SymbolSearchIndex, DEFAULT_SHARD_COUNT,
};
use nova_project::ProjectError;
use nova_syntax::SyntaxNode;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use walkdir::WalkDir;

mod engine;

pub use engine::{IndexProgress, WorkspaceEvent, WorkspaceStatus};

/// A minimal, library-first backend for the `nova` CLI.
///
/// This is intentionally lightweight: it provides basic project loading,
/// indexing, diagnostics, and cache management without requiring an editor or
/// LSP transport.
#[derive(Clone)]
pub struct Workspace {
    root: PathBuf,
    engine: Arc<engine::WorkspaceEngine>,
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
        Self {
            root: PathBuf::new(),
            engine: Arc::new(engine::WorkspaceEngine::new()),
        }
    }

    /// Open a workspace rooted at `path`.
    ///
    /// If `path` is a file, its parent directory is treated as the workspace root.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
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
        let root = find_project_root(&root);
        Ok(Self {
            root,
            engine: Arc::new(engine::WorkspaceEngine::new()),
        })
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

    pub fn trigger_indexing(&self) {
        self.engine.trigger_indexing();
    }

    pub fn debug_configurations(&self) -> Vec<nova_ide::DebugConfiguration> {
        self.engine.debug_configurations(&self.root)
    }

    fn java_files_in(&self, root: &Path) -> Result<Vec<PathBuf>> {
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

    pub fn index(&self) -> Result<IndexReport> {
        let (snapshot, cache_dir, _shards, metrics) = self.build_indexes()?;
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
        let (snapshot, cache_dir, mut shards, metrics) = self.build_indexes()?;
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
    ) -> Result<(ProjectSnapshot, CacheDir, Vec<ProjectIndexes>, PerfMetrics)> {
        let start = Instant::now();

        let files = self.project_java_files()?;
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
        let snapshot_ms = snapshot_start.elapsed().as_millis();

        // Load persisted sharded indexes based on the stamp snapshot. This avoids hashing
        // full file contents before deciding whether the cache is reusable.
        let shard_count = DEFAULT_SHARD_COUNT;
        let loaded = load_sharded_index_archives_from_fast_snapshot(
            &cache_dir,
            &stamp_snapshot,
            shard_count,
        )
        .context("failed to load cached indexes")?;

        let (mut shards, mut invalidated_files) = match loaded {
            Some(loaded) => {
                let nova_index::LoadedShardedIndexArchives {
                    shards: loaded_shards,
                    invalidated_files,
                    ..
                } = loaded;

                let mut shards = Vec::with_capacity(shard_count as usize);
                for shard in loaded_shards {
                    let indexes = match shard {
                        Some(archives) => ProjectIndexes {
                            symbols: archives.symbols.to_owned()?,
                            references: archives.references.to_owned()?,
                            inheritance: archives.inheritance.to_owned()?,
                            annotations: archives.annotations.to_owned()?,
                        },
                        None => ProjectIndexes::default(),
                    };
                    shards.push(indexes);
                }
                (shards, invalidated_files)
            }
            None => (
                (0..shard_count)
                    .map(|_| ProjectIndexes::default())
                    .collect(),
                stamp_snapshot.file_fingerprints().keys().cloned().collect(),
            ),
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
        for path in stamp_snapshot.file_fingerprints().keys() {
            if !content_fingerprints.contains_key(path) {
                invalidated_files.push(path.clone());
            }
        }
        invalidated_files.sort();
        invalidated_files.dedup();

        // Remove stale results for invalidated (new/modified/deleted) files before re-indexing.
        for file in &invalidated_files {
            let shard = shard_id_for_path(file, shard_count) as usize;
            shards[shard].invalidate_file(file);
        }

        // `invalidated_files` may include deleted files. Only re-index files that still exist in
        // the current snapshot.
        let files_to_index: Vec<String> = invalidated_files
            .iter()
            .filter(|path| stamp_snapshot.file_fingerprints().contains_key(*path))
            .cloned()
            .collect();

        let (files_indexed, bytes_indexed, updated_fingerprints, index_ms) =
            if files_to_index.is_empty() {
                (0usize, 0u64, std::collections::BTreeMap::new(), 0u128)
            } else {
                let index_start = Instant::now();
                let (files_indexed, bytes_indexed, updated_fingerprints) =
                    self.index_files(&stamp_snapshot, &mut shards, shard_count, &files_to_index)?;
                let index_ms = index_start.elapsed().as_millis();
                (files_indexed, bytes_indexed, updated_fingerprints, index_ms)
            };

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
    ) -> Result<(usize, u64, std::collections::BTreeMap<String, Fingerprint>)> {
        let type_re = Regex::new(
            r"(?m)^\s*(?:public|protected|private)?\s*(?:abstract\s+|final\s+)?(class|interface|enum|record)\s+([A-Za-z_][A-Za-z0-9_]*)",
        )?;
        let method_re = Regex::new(
            r"(?m)^\s*(?:public|protected|private)?\s*(?:static\s+)?(?:final\s+)?(?:synchronized\s+)?[A-Za-z0-9_<>,\[\]\s]+\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(",
        )?;
        let annotation_re = Regex::new(r"(?m)@([A-Za-z_][A-Za-z0-9_]*)")?;

        let mut files_indexed = 0usize;
        let mut bytes_indexed = 0u64;
        let mut file_fingerprints = std::collections::BTreeMap::new();

        for file in files_to_index {
            let shard = shard_id_for_path(file, shard_count) as usize;
            let indexes = &mut shards[shard];
            let full_path = snapshot.project_root().join(file);
            let content = fs::read_to_string(&full_path)
                .with_context(|| format!("failed to read {}", full_path.display()))?;
            files_indexed += 1;
            bytes_indexed += content.len() as u64;
            file_fingerprints.insert(file.clone(), Fingerprint::from_bytes(content.as_bytes()));

            for cap in type_re.captures_iter(&content) {
                let m = cap.get(2).expect("regex capture");
                let (line, column) = line_col_at(&content, m.start());
                indexes.symbols.insert(
                    m.as_str(),
                    SymbolLocation {
                        file: file.clone(),
                        line: as_u32(line),
                        column: as_u32(column),
                    },
                );
            }

            for cap in method_re.captures_iter(&content) {
                let m = cap.get(1).expect("regex capture");
                let (line, column) = line_col_at(&content, m.start());
                indexes.symbols.insert(
                    m.as_str(),
                    SymbolLocation {
                        file: file.clone(),
                        line: as_u32(line),
                        column: as_u32(column),
                    },
                );
            }

            for cap in annotation_re.captures_iter(&content) {
                let m = cap.get(1).expect("regex capture");
                let (line, column) = line_col_at(&content, m.start().saturating_sub(1));
                indexes.annotations.insert(
                    format!("@{}", m.as_str()),
                    nova_index::AnnotationLocation {
                        file: file.clone(),
                        line: as_u32(line),
                        column: as_u32(column),
                    },
                );
            }
        }

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
        // Keep the symbol index up to date by running the incremental indexer
        // and persisting the updated indexes into the on-disk cache.
        let shard_count = DEFAULT_SHARD_COUNT;
        let (snapshot, cache_dir, mut shards, metrics) = self.build_indexes()?;
        if metrics.files_invalidated > 0 {
            save_sharded_indexes(&cache_dir, &snapshot, shard_count, &mut shards)
                .context("failed to persist indexes")?;
        }
        self.write_cache_perf(&cache_dir, &metrics)?;

        const WORKSPACE_SYMBOL_LIMIT: usize = 200;
        let (results, _stats) =
            fuzzy_rank_workspace_symbols_sharded(&shards, query, WORKSPACE_SYMBOL_LIMIT);
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

fn fuzzy_rank_workspace_symbols(
    symbols: &nova_index::SymbolIndex,
    query: &str,
    limit: usize,
) -> (Vec<WorkspaceSymbol>, SearchStats) {
    if query.is_empty() {
        return (
            Vec::new(),
            SearchStats {
                strategy: CandidateStrategy::FullScan,
                candidates_considered: 0,
            },
        );
    }

    let search_symbols: Vec<SearchSymbol> = symbols
        .symbols
        .keys()
        .map(|name| SearchSymbol {
            name: name.clone(),
            qualified_name: name.clone(),
        })
        .collect();

    let search_index = SymbolSearchIndex::build(search_symbols);
    let (results, stats) = search_index.search_with_stats(query, limit);

    let mut ranked = Vec::with_capacity(results.len());
    for res in results {
        let name = res.symbol.name;
        if let Some(locations) = symbols.symbols.get(name.as_str()) {
            ranked.push(WorkspaceSymbol {
                name,
                locations: locations.clone(),
            });
        }
    }

    (ranked, stats)
}

fn fuzzy_rank_workspace_symbols_sharded(
    shards: &[ProjectIndexes],
    query: &str,
    limit: usize,
) -> (Vec<WorkspaceSymbol>, SearchStats) {
    if query.is_empty() {
        return (
            Vec::new(),
            SearchStats {
                strategy: CandidateStrategy::FullScan,
                candidates_considered: 0,
            },
        );
    }

    let mut symbol_names = std::collections::BTreeSet::new();
    for shard in shards {
        symbol_names.extend(shard.symbols.symbols.keys().cloned());
    }

    let search_symbols: Vec<SearchSymbol> = symbol_names
        .into_iter()
        .map(|name| SearchSymbol {
            qualified_name: name.clone(),
            name,
        })
        .collect();

    let search_index = SymbolSearchIndex::build(search_symbols);
    let (results, stats) = search_index.search_with_stats(query, limit);

    let mut ranked = Vec::with_capacity(results.len());
    for res in results {
        let name = res.symbol.name;
        let mut locations = Vec::new();
        for shard in shards {
            if let Some(found) = shard.symbols.symbols.get(name.as_str()) {
                locations.extend(found.iter().cloned());
            }
        }
        if !locations.is_empty() {
            ranked.push(WorkspaceSymbol { name, locations });
        }
    }

    (ranked, stats)
}

#[cfg(test)]
mod fuzzy_symbol_tests {
    use super::*;

    #[test]
    fn workspace_symbol_search_uses_trigram_candidate_filtering() {
        let mut symbols = nova_index::SymbolIndex::default();
        symbols.insert(
            "HashMap",
            SymbolLocation {
                file: "A.java".into(),
                line: 1,
                column: 1,
            },
        );
        symbols.insert(
            "HashSet",
            SymbolLocation {
                file: "B.java".into(),
                line: 1,
                column: 1,
            },
        );
        symbols.insert(
            "FooBar",
            SymbolLocation {
                file: "C.java".into(),
                line: 1,
                column: 1,
            },
        );

        let (_results, stats) = fuzzy_rank_workspace_symbols(&symbols, "Hash", 10);
        assert_eq!(stats.strategy, CandidateStrategy::Trigram);
        assert!(stats.candidates_considered < symbols.symbols.len());
    }

    #[test]
    fn workspace_symbol_search_supports_acronym_queries() {
        let mut symbols = nova_index::SymbolIndex::default();
        symbols.insert(
            "FooBar",
            SymbolLocation {
                file: "A.java".into(),
                line: 1,
                column: 1,
            },
        );

        let (results, _stats) = fuzzy_rank_workspace_symbols(&symbols, "fb", 10);
        assert_eq!(results[0].name, "FooBar");
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
pub struct WorkspaceSymbol {
    pub name: String,
    pub locations: Vec<SymbolLocation>,
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

fn as_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
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

pub mod live;

pub use live::{
    WatcherHandle, Workspace as LiveWorkspace, WorkspaceClient,
    WorkspaceConfig as LiveWorkspaceConfig,
};
fn find_project_root(start: &Path) -> PathBuf {
    nova_project::workspace_root(start).unwrap_or_else(|| start.to_path_buf())
}
