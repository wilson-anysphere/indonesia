//! Persistence policy and helper APIs for disk-backed caches used inside Salsa queries.
//!
//! ## Why this exists
//!
//! Nova uses Salsa for incremental computation. Salsa queries are assumed to be **deterministic**
//! functions of their tracked inputs: given the same inputs, a query must return the same value
//! every time.
//!
//! At the same time, Nova benefits hugely from warm-starting by **loading previously computed
//! artifacts from disk** (AST/HIR summaries, derived query results, etc.).
//!
//! The key invariant is:
//!
//! > Disk state must never affect the *semantic* result of a query.
//!
//! Disk caches are permitted inside query implementations **only as a performance hint**. They
//! must be best-effort and side-effect hygienic:
//!
//! - Any I/O error, corruption, schema mismatch, or version mismatch must be treated as a cache
//!   miss.
//! - On a miss, the query must fall back to recomputation and return the computed value.
//! - Writes must be best-effort and must not influence the value returned from the query.
//!
//! To make these rules explicit (and debuggable), Nova's query databases carry a non-tracked
//! [`PersistenceMode`] that governs whether queries are allowed to consult and/or update disk
//! caches.
//!
//! ## Environment override
//!
//! `NOVA_PERSISTENCE=off|ro|rw`
//!
//! - `off` / `disabled` ⇒ [`PersistenceMode::Disabled`]
//! - `ro` / `read-only` ⇒ [`PersistenceMode::ReadOnly`]
//! - `rw` / `read-write` / `on` ⇒ [`PersistenceMode::ReadWrite`]
//!
//! Defaults to `rw` in release builds and `off` in debug builds.
//!
//! ## Persisted artifact formats (summary)
//!
//! The concrete on-disk formats are owned by the lower-level cache crates:
//! - `nova-cache` defines the per-project directory layout and persists project metadata.
//! - `nova-index` persists large project indexes via `nova-storage` (`rkyv`) archives.
//! - `nova-deps-cache` persists dependency bundles (JAR/JMOD stubs) via `nova-storage` archives.
//! - `nova-cache`'s `AstArtifactCache` / `DerivedArtifactCache` use `serde` + `bincode` for
//!   smaller per-file/per-query derived values.
//!
//! See `nova-cache` crate docs for an inventory of the current cache directory layout.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;

use nova_cache::{
    AstArtifactCache, CacheConfig, CacheDir, DerivedArtifactCache, FileAstArtifacts, Fingerprint,
};
use serde::de::DeserializeOwned;
use serde::Serialize;

/// Controls whether Salsa queries are allowed to consult and/or update disk caches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PersistenceMode {
    /// Never read from or write to disk caches.
    Disabled,
    /// Read-through is allowed, but queries must never write back to disk caches.
    ReadOnly,
    /// Queries may read from and best-effort write to disk caches.
    ReadWrite,
}

impl PersistenceMode {
    pub fn allows_read(self) -> bool {
        matches!(self, Self::ReadOnly | Self::ReadWrite)
    }

    pub fn allows_write(self) -> bool {
        matches!(self, Self::ReadWrite)
    }

    /// Parse [`PersistenceMode`] from `NOVA_PERSISTENCE`.
    pub fn from_env() -> Self {
        let Some(raw) = std::env::var_os("NOVA_PERSISTENCE") else {
            return Self::default();
        };
        let raw = raw.to_string_lossy();
        let raw = raw.trim().to_ascii_lowercase();

        match raw.as_str() {
            "" => Self::default(),
            "0" | "off" | "disabled" | "false" | "no" => Self::Disabled,
            "ro" | "read-only" | "readonly" => Self::ReadOnly,
            "rw" | "read-write" | "readwrite" | "on" | "enabled" | "true" | "1" => Self::ReadWrite,
            _ => Self::default(),
        }
    }
}

impl Default for PersistenceMode {
    fn default() -> Self {
        // Default to RW in production builds, but keep debug/test builds deterministic and free of
        // surprise disk I/O unless explicitly enabled.
        if cfg!(test) || cfg!(debug_assertions) {
            Self::Disabled
        } else {
            Self::ReadWrite
        }
    }
}

/// Configuration bundle for initializing [`Persistence`].
#[derive(Debug, Clone)]
pub struct PersistenceConfig {
    pub mode: PersistenceMode,
    pub cache: CacheConfig,
}

impl PersistenceConfig {
    pub fn from_env() -> Self {
        Self {
            mode: PersistenceMode::from_env(),
            cache: CacheConfig::from_env(),
        }
    }
}

impl Default for PersistenceConfig {
    fn default() -> Self {
        Self::from_env()
    }
}

/// Snapshot of observed cache behavior.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PersistenceStats {
    pub ast_load_hits: u64,
    pub ast_load_misses: u64,
    pub ast_store_success: u64,
    pub ast_store_failure: u64,
    pub derived_load_hits: u64,
    pub derived_load_misses: u64,
    pub derived_store_success: u64,
    pub derived_store_failure: u64,
}

#[derive(Debug, Default)]
struct AtomicPersistenceStats {
    ast_load_hits: AtomicU64,
    ast_load_misses: AtomicU64,
    ast_store_success: AtomicU64,
    ast_store_failure: AtomicU64,
    derived_load_hits: AtomicU64,
    derived_load_misses: AtomicU64,
    derived_store_success: AtomicU64,
    derived_store_failure: AtomicU64,
}

impl AtomicPersistenceStats {
    fn snapshot(&self) -> PersistenceStats {
        PersistenceStats {
            ast_load_hits: self.ast_load_hits.load(Ordering::Relaxed),
            ast_load_misses: self.ast_load_misses.load(Ordering::Relaxed),
            ast_store_success: self.ast_store_success.load(Ordering::Relaxed),
            ast_store_failure: self.ast_store_failure.load(Ordering::Relaxed),
            derived_load_hits: self.derived_load_hits.load(Ordering::Relaxed),
            derived_load_misses: self.derived_load_misses.load(Ordering::Relaxed),
            derived_store_success: self.derived_store_success.load(Ordering::Relaxed),
            derived_store_failure: self.derived_store_failure.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug)]
struct PersistenceInner {
    mode: PersistenceMode,
    cache_dir: Option<CacheDir>,
    ast_cache: Option<AstArtifactCache>,
    derived_cache: Option<DerivedArtifactCache>,
    stats: AtomicPersistenceStats,
}

/// Best-effort disk cache access for Salsa queries.
///
/// This object is stored *outside* Salsa's dependency graph (as plain database state) and is
/// therefore not tracked for incremental recomputation. Because of that, it must never influence
/// semantic query results.
#[derive(Clone, Debug)]
pub struct Persistence {
    inner: Arc<PersistenceInner>,
}

impl Persistence {
    /// Create persistence helpers for a project root.
    ///
    /// If the cache directory cannot be initialized (e.g. missing `HOME`), the persistence helpers
    /// degrade gracefully: cache reads become misses and cache writes become no-ops.
    pub fn new(project_root: impl AsRef<Path>, config: PersistenceConfig) -> Self {
        static CACHE_DIR_INIT_ERROR_LOGGED: OnceLock<()> = OnceLock::new();

        let mode = config.mode;

        // If persistence is fully disabled, avoid even attempting to create cache directories.
        if mode == PersistenceMode::Disabled {
            return Self::new_disabled();
        }

        let project_root = project_root.as_ref();
        let cache_dir = match CacheDir::new(project_root, config.cache) {
            Ok(dir) => Some(dir),
            Err(err) => {
                if CACHE_DIR_INIT_ERROR_LOGGED.set(()).is_ok() {
                    tracing::debug!(
                        target = "nova.db",
                        project_root = %project_root.display(),
                        error = ?err,
                        "failed to initialize persistence cache directory (best effort)"
                    );
                }
                None
            }
        };
        let (ast_cache, derived_cache) = match &cache_dir {
            Some(dir) => (
                Some(AstArtifactCache::new(dir.ast_dir())),
                Some(DerivedArtifactCache::new(dir.queries_dir())),
            ),
            None => (None, None),
        };

        Self {
            inner: Arc::new(PersistenceInner {
                mode,
                cache_dir,
                ast_cache,
                derived_cache,
                stats: AtomicPersistenceStats::default(),
            }),
        }
    }

    pub fn new_disabled() -> Self {
        Self {
            inner: Arc::new(PersistenceInner {
                mode: PersistenceMode::Disabled,
                cache_dir: None,
                ast_cache: None,
                derived_cache: None,
                stats: AtomicPersistenceStats::default(),
            }),
        }
    }

    pub fn mode(&self) -> PersistenceMode {
        self.inner.mode
    }

    pub fn stats(&self) -> PersistenceStats {
        self.inner.stats.snapshot()
    }

    pub fn cache_dir(&self) -> Option<&CacheDir> {
        self.inner.cache_dir.as_ref()
    }

    /// Best-effort load of persisted AST artifacts.
    ///
    /// Returns `None` on any incompatibility or I/O error.
    pub fn load_ast_artifacts(
        &self,
        file_path: &str,
        fingerprint: &Fingerprint,
    ) -> Option<FileAstArtifacts> {
        if !self.mode().allows_read() {
            return None;
        }

        let Some(cache) = &self.inner.ast_cache else {
            self.inner
                .stats
                .ast_load_misses
                .fetch_add(1, Ordering::Relaxed);
            return None;
        };

        let loaded = match cache.load(file_path, fingerprint) {
            Ok(loaded) => loaded,
            Err(err) => {
                tracing::debug!(
                    target = "nova.db",
                    file_path,
                    error = %err,
                    "failed to load persisted AST artifacts; treating as cache miss"
                );
                None
            }
        };
        match loaded {
            Some(artifacts) => {
                self.inner
                    .stats
                    .ast_load_hits
                    .fetch_add(1, Ordering::Relaxed);
                Some(artifacts)
            }
            None => {
                self.inner
                    .stats
                    .ast_load_misses
                    .fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Best-effort store of persisted AST artifacts.
    pub fn store_ast_artifacts(
        &self,
        file_path: &str,
        fingerprint: &Fingerprint,
        artifacts: &FileAstArtifacts,
    ) {
        if !self.mode().allows_write() {
            return;
        }
        let Some(cache) = &self.inner.ast_cache else {
            self.inner
                .stats
                .ast_store_failure
                .fetch_add(1, Ordering::Relaxed);
            return;
        };

        match cache.store(file_path, fingerprint, artifacts) {
            Ok(()) => {
                self.inner
                    .stats
                    .ast_store_success
                    .fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                self.inner
                    .stats
                    .ast_store_failure
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Best-effort load of a persisted derived query value.
    ///
    /// Returns `None` on any incompatibility or I/O error.
    pub fn load_derived<T: DeserializeOwned>(
        &self,
        query_name: &str,
        query_schema_version: u32,
        args: &impl Serialize,
        input_fingerprints: &BTreeMap<String, Fingerprint>,
    ) -> Option<T> {
        if !self.mode().allows_read() {
            return None;
        }
        let Some(cache) = &self.inner.derived_cache else {
            self.inner
                .stats
                .derived_load_misses
                .fetch_add(1, Ordering::Relaxed);
            return None;
        };

        match cache
            .load(query_name, query_schema_version, args, input_fingerprints)
            .ok()
            .flatten()
        {
            Some(value) => {
                self.inner
                    .stats
                    .derived_load_hits
                    .fetch_add(1, Ordering::Relaxed);
                Some(value)
            }
            None => {
                self.inner
                    .stats
                    .derived_load_misses
                    .fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Best-effort store of a derived query value.
    pub fn store_derived<T: Serialize>(
        &self,
        query_name: &str,
        query_schema_version: u32,
        args: &impl Serialize,
        input_fingerprints: &BTreeMap<String, Fingerprint>,
        value: &T,
    ) {
        if !self.mode().allows_write() {
            return;
        }
        let Some(cache) = &self.inner.derived_cache else {
            self.inner
                .stats
                .derived_store_failure
                .fetch_add(1, Ordering::Relaxed);
            return;
        };

        match cache.store(
            query_name,
            query_schema_version,
            args,
            input_fingerprints,
            value,
        ) {
            Ok(()) => {
                self.inner
                    .stats
                    .derived_store_success
                    .fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                self.inner
                    .stats
                    .derived_store_failure
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Load a persisted derived query value or compute and (best-effort) persist it.
    ///
    /// Disk persistence must never affect semantic query results, so this helper is intentionally
    /// best-effort:
    /// - Any read error or incompatibility becomes a cache miss.
    /// - Any write error is ignored.
    pub fn get_or_compute_derived<T, Args, F>(
        &self,
        query_name: &str,
        query_schema_version: u32,
        args: &Args,
        input_fingerprints: &BTreeMap<String, Fingerprint>,
        compute: F,
    ) -> T
    where
        T: Serialize + DeserializeOwned,
        Args: Serialize,
        F: FnOnce() -> T,
    {
        if let Some(hit) =
            self.load_derived(query_name, query_schema_version, args, input_fingerprints)
        {
            return hit;
        }

        let value = compute();
        self.store_derived(
            query_name,
            query_schema_version,
            args,
            input_fingerprints,
            &value,
        );
        value
    }
}

/// Database functionality needed by Salsa query implementations to access disk caches.
pub trait HasPersistence {
    fn persistence(&self) -> &Persistence;
}
