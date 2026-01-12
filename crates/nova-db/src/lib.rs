//! Database layer for Nova.
//!
//! This crate currently provides:
//! - [`InMemoryFileStore`]: a small in-memory file store used by `nova-dap`.
//! - [`SourceDatabase`]: a Salsa-friendly interface returning owned, snapshot-safe
//!   values (e.g. `Arc<String>`).
//! - [`SalsaDbView`]: an adapter that lets legacy code expecting [`Database`]
//!   run on Salsa snapshots.
//! - [`AnalysisDatabase`]: an experimental, non-Salsa cache facade for warm-start
//!   parsing and per-file structural summaries.
//! - [`salsa`]: the Salsa-powered incremental query database and snapshot
//!   concurrency model described in `docs/04-incremental-computation.md`.

mod query_cache;
pub use query_cache::{PersistentQueryCache, QueryCache};

pub mod persistence;

mod salsa_db_view;
mod source_db;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nova_cache::{
    normalize_rel_path, AstArtifactCache, CacheConfig, CacheDir, CacheError, FileAstArtifacts,
    Fingerprint,
};
pub use nova_core::{FileId, ProjectId, SourceRootId};
use nova_hir::token_item_tree::{
    token_item_tree as build_item_tree, TokenItemTree, TokenSymbolSummary,
};
use nova_syntax::{parse as syntax_parse, ParseResult};

pub use salsa_db_view::SalsaDbView;
pub use source_db::SourceDatabase;

/// A small in-memory store for file contents keyed by a compact [`FileId`].
#[derive(Debug, Default)]
pub struct InMemoryFileStore {
    next_file_id: u32,
    path_to_file: HashMap<PathBuf, FileId>,
    file_to_path: HashMap<FileId, PathBuf>,
    files: HashMap<FileId, Arc<String>>,
}

/// Minimal query surface needed by IDE features.
///
/// In the long term this will be backed by an incremental query engine; for now
/// we only expose raw file text for analysis.
///
/// ## Salsa compatibility
///
/// This trait returns borrowed `&str`/`&Path` references, which is awkward to
/// implement on top of Salsa snapshots (inputs are stored as `Arc<String>` and
/// must outlive the call that produced the reference).
///
/// New code should prefer [`SourceDatabase`], which returns owned values.
/// Legacy code can run on Salsa snapshots via [`SalsaDbView`].
pub trait Database {
    fn file_content(&self, file_id: FileId) -> &str;

    /// Best-effort file path lookup for a `FileId`.
    fn file_path(&self, _file_id: FileId) -> Option<&Path> {
        None
    }

    /// Return all file IDs currently known to the database.
    fn all_file_ids(&self) -> Vec<FileId> {
        Vec::new()
    }

    /// Look up a `FileId` for an already-known path.
    fn file_id(&self, _path: &Path) -> Option<FileId> {
        None
    }
}

impl InMemoryFileStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn file_id_for_path(&mut self, path: impl AsRef<Path>) -> FileId {
        let path = path.as_ref().to_path_buf();
        if let Some(id) = self.path_to_file.get(&path) {
            return *id;
        }

        let id = FileId::from_raw(self.next_file_id);
        self.next_file_id = self.next_file_id.saturating_add(1);
        self.path_to_file.insert(path.clone(), id);
        self.file_to_path.insert(id, path);
        id
    }

    pub fn set_file_text(&mut self, file_id: FileId, text: String) {
        self.files.insert(file_id, Arc::new(text));
    }

    pub fn file_text(&self, file_id: FileId) -> Option<&str> {
        self.files.get(&file_id).map(|text| text.as_str())
    }

    pub fn path_for_file(&self, file_id: FileId) -> Option<&Path> {
        self.file_to_path.get(&file_id).map(PathBuf::as_path)
    }
}

impl Database for InMemoryFileStore {
    fn file_content(&self, file_id: FileId) -> &str {
        self.file_text(file_id).unwrap_or("")
    }

    fn file_path(&self, file_id: FileId) -> Option<&Path> {
        self.path_for_file(file_id)
    }

    fn all_file_ids(&self) -> Vec<FileId> {
        let mut ids: Vec<_> = self.files.keys().copied().collect();
        ids.sort_by_key(|id| id.to_raw());
        ids
    }

    fn file_id(&self, path: &Path) -> Option<FileId> {
        self.path_to_file.get(path).copied()
    }
}

impl SourceDatabase for InMemoryFileStore {
    fn file_content(&self, file_id: FileId) -> Arc<String> {
        self.files
            .get(&file_id)
            .cloned()
            .unwrap_or_else(|| Arc::new(String::new()))
    }

    fn file_path(&self, file_id: FileId) -> Option<PathBuf> {
        self.file_to_path.get(&file_id).cloned()
    }

    fn all_file_ids(&self) -> Arc<Vec<FileId>> {
        let mut ids: Vec<_> = self.files.keys().copied().collect();
        ids.sort_by_key(|id| id.to_raw());
        Arc::new(ids)
    }

    fn file_id(&self, path: &Path) -> Option<FileId> {
        self.path_to_file.get(path).copied()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AnalysisDbError {
    #[error("unknown file: {0}")]
    UnknownFile(String),
    #[error(transparent)]
    Cache(#[from] CacheError),
}

#[derive(Debug, Clone)]
struct FileData {
    text: Arc<str>,
    fingerprint: Fingerprint,
}

#[derive(Debug, Clone)]
struct CachedAst {
    fingerprint: Fingerprint,
    parse: Arc<ParseResult>,
    item_tree: Arc<TokenItemTree>,
    symbol_summary: Option<Arc<TokenSymbolSummary>>,
}

/// A minimal query facade with persisted AST/HIR artifacts for warm starts.
///
/// This is *not* Salsa-backed yet; instead it focuses on the persistence
/// plumbing. `parse(file)` will:
/// 1. Reuse in-memory cached results if the content fingerprint matches.
/// 2. Attempt to load persisted artifacts from `nova-cache` if available.
/// 3. Fall back to parsing and (best-effort) persisting the artifacts.
#[derive(Debug)]
pub struct AnalysisDatabase {
    cache_dir: CacheDir,
    ast_cache: AstArtifactCache,
    files: BTreeMap<String, FileData>,
    ast: BTreeMap<String, CachedAst>,
    parse_count: usize,
}

impl AnalysisDatabase {
    pub fn new(project_root: impl AsRef<Path>) -> Result<Self, AnalysisDbError> {
        Self::new_with_cache_config(project_root, CacheConfig::from_env())
    }

    pub fn new_with_cache_config(
        project_root: impl AsRef<Path>,
        config: CacheConfig,
    ) -> Result<Self, AnalysisDbError> {
        let cache_dir = CacheDir::new(project_root, config)?;
        let ast_cache = AstArtifactCache::new(cache_dir.ast_dir());
        Ok(Self {
            cache_dir,
            ast_cache,
            files: BTreeMap::new(),
            ast: BTreeMap::new(),
            parse_count: 0,
        })
    }

    pub fn cache_dir(&self) -> &CacheDir {
        &self.cache_dir
    }

    pub fn parse_count(&self) -> usize {
        self.parse_count
    }

    pub fn set_file_content(&mut self, file_path: impl Into<String>, text: impl Into<String>) {
        let file_path = normalize_rel_path(&file_path.into());
        let text = text.into();
        let fingerprint = Fingerprint::from_bytes(text.as_bytes());
        let text = Arc::<str>::from(text);

        let invalidate = self
            .files
            .get(&file_path)
            .map(|old| old.fingerprint != fingerprint)
            .unwrap_or(true);

        self.files
            .insert(file_path.clone(), FileData { text, fingerprint });

        if invalidate {
            self.ast.remove(&file_path);
        }
    }

    fn file_data(&self, file: &str) -> Result<&FileData, AnalysisDbError> {
        self.files
            .get(file)
            .ok_or_else(|| AnalysisDbError::UnknownFile(file.to_string()))
    }

    pub fn parse(&mut self, file_path: &str) -> Result<Arc<ParseResult>, AnalysisDbError> {
        let file_path = normalize_rel_path(file_path);
        let (text, fingerprint) = {
            let data = self.file_data(&file_path)?;
            (data.text.clone(), data.fingerprint.clone())
        };

        if let Some(cached) = self.ast.get(&file_path) {
            if cached.fingerprint == fingerprint {
                return Ok(cached.parse.clone());
            }
        }

        if let Some(artifacts) = self.ast_cache.load(&file_path, &fingerprint)? {
            let cached = CachedAst {
                fingerprint,
                parse: Arc::new(artifacts.parse),
                item_tree: Arc::new(artifacts.item_tree),
                symbol_summary: artifacts.symbol_summary.map(Arc::new),
            };
            let parse = cached.parse.clone();
            self.ast.insert(file_path.clone(), cached);
            return Ok(parse);
        }

        self.parse_count += 1;
        let parsed = syntax_parse(&text);
        let it = build_item_tree(&parsed, &text);
        let sym = TokenSymbolSummary::from_item_tree(&it);

        let artifacts = FileAstArtifacts {
            parse: parsed,
            item_tree: it,
            symbol_summary: Some(sym),
        };
        self.ast_cache.store(&file_path, &fingerprint, &artifacts)?;

        let FileAstArtifacts {
            parse: parsed,
            item_tree: it,
            symbol_summary,
        } = artifacts;

        let cached = CachedAst {
            fingerprint,
            parse: Arc::new(parsed),
            item_tree: Arc::new(it),
            symbol_summary: symbol_summary.map(Arc::new),
        };
        let parse = cached.parse.clone();
        self.ast.insert(file_path, cached);
        Ok(parse)
    }

    pub fn item_tree(&mut self, file_path: &str) -> Result<Arc<TokenItemTree>, AnalysisDbError> {
        let file_path = normalize_rel_path(file_path);
        let fingerprint = self.file_data(&file_path)?.fingerprint.clone();
        if let Some(cached) = self.ast.get(&file_path) {
            if cached.fingerprint == fingerprint {
                return Ok(cached.item_tree.clone());
            }
        }
        let _ = self.parse(&file_path)?;
        Ok(self
            .ast
            .get(&file_path)
            .expect("parse() populates ast cache")
            .item_tree
            .clone())
    }

    pub fn symbol_summary(
        &mut self,
        file_path: &str,
    ) -> Result<Option<Arc<TokenSymbolSummary>>, AnalysisDbError> {
        let file_path = normalize_rel_path(file_path);
        let fingerprint = self.file_data(&file_path)?.fingerprint.clone();
        if let Some(cached) = self.ast.get(&file_path) {
            if cached.fingerprint == fingerprint {
                return Ok(cached.symbol_summary.clone());
            }
        }
        let _ = self.parse(&file_path)?;
        Ok(self
            .ast
            .get(&file_path)
            .expect("parse() populates ast cache")
            .symbol_summary
            .clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn warm_start_uses_persisted_artifacts_and_invalidates_per_file() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let cache_root = tmp.path().join("cache");
        std::fs::create_dir_all(&cache_root).unwrap();

        let cfg = CacheConfig {
            cache_root_override: Some(cache_root),
        };

        // First run: parse + persist.
        let mut db1 = AnalysisDatabase::new_with_cache_config(&project_root, cfg.clone()).unwrap();
        db1.set_file_content("src/A.java", "class A {}");
        db1.set_file_content("src/B.java", "class B {}");

        let a1 = db1.parse("src/A.java").unwrap();
        let b1 = db1.parse("src/B.java").unwrap();
        let a_it1 = db1.item_tree("src/A.java").unwrap();

        assert_eq!(db1.parse_count(), 2);
        drop(db1);

        // Second run: file A unchanged (cache hit), file B changed (cache miss).
        let mut db2 = AnalysisDatabase::new_with_cache_config(&project_root, cfg).unwrap();
        db2.set_file_content("src/A.java", "class A {}");
        db2.set_file_content("src/B.java", "class B { int x; }");

        let a2 = db2.parse("src/A.java").unwrap();
        assert_eq!(db2.parse_count(), 0, "file A should be loaded from cache");

        let b2 = db2.parse("src/B.java").unwrap();
        assert_eq!(
            db2.parse_count(),
            1,
            "file B should be reparsed after change"
        );

        assert_eq!(&*a2, &*a1);
        assert_eq!(&*a_it1, &*db2.item_tree("src/A.java").unwrap());
        assert_ne!(&*b2, &*b1);
    }
}

pub mod salsa;

pub use salsa::{
    catch_cancelled, ArcEq, Database as SalsaDatabase, NovaDatabase, NovaFlow, NovaHir, NovaIde,
    NovaIndexing, NovaInputs, NovaResolve, NovaSemantic, NovaSyntax, NovaTypeck, QueryStat,
    QueryStatReport, QueryStats, QueryStatsReport, RootDatabase as SalsaRootDatabase, Snapshot,
    SyntaxTree,
};

pub use persistence::{
    HasPersistence, Persistence, PersistenceConfig, PersistenceMode, PersistenceStats,
};
