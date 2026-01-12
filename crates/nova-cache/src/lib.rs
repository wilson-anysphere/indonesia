//! Persistent cache directory management and versioned on-disk metadata.
//!
//! This crate implements Nova's "hybrid persistence model" building blocks:
//! - per-project cache directories
//! - versioned metadata and input fingerprints
//! - best-effort derived artifact persistence
//! - persisted AST/HIR artifacts for fast warm starts
//!
//! It also supports **packaging** a project's persistent cache into a single archive
//! (`tar.zst`) so teams/CI can share prebuilt indexes.
//!
//! ## On-disk layout (inventory)
//!
//! Project-scoped caches live under `<cache_root>/<project_hash>/`:
//! - `metadata.bin` + `metadata.json`:
//!   - [`CacheMetadata`], schema [`CACHE_METADATA_SCHEMA_VERSION`]
//!   - `metadata.bin` is a `nova-storage` (`rkyv`) archive (`ArtifactKind::ProjectMetadata`)
//! - `indexes/`:
//!   - project indexes persisted by `nova-index` as `nova-storage` archives (`*.idx`)
//! - `indexes/segments/`:
//!   - incremental index segments (`ArtifactKind::ProjectIndexSegment`) + `manifest.json`
//! - `ast/metadata.bin` + `ast/*.ast`:
//!   - [`AstArtifactCache`] entries persisted via `serde` + `bincode`
//!   - gated by [`AST_ARTIFACT_SCHEMA_VERSION`] plus `nova-syntax`/`nova-hir` schema versions
//! - `queries/<query>/index.json` + `queries/<query>/*.bin`:
//!   - [`DerivedArtifactCache`] entries persisted via `serde` + `bincode`
//! - `queries/query_cache/*.bin`:
//!   - [`QueryDiskCache`] entries persisted via `serde` + `bincode`
//! - `classpath/`:
//!   - per-entry classpath stub caches (see `nova-classpath`)
//!
//! Shared dependency caches live under `<cache_root>/deps/`:
//! - `<sha256>/classpath.idx`:
//!   - dependency index bundles persisted by `nova-deps-cache` as `nova-storage` archives

mod ast_cache;
mod cache_dir;
mod derived_cache;
mod error;
mod fingerprint;
mod gc;
mod lock;
mod metadata;
mod pack;
mod path;
mod prune;
mod query_disk_cache;
mod shard_index;
mod store;
mod util;

pub use ast_cache::{AstArtifactCache, FileAstArtifacts, AST_ARTIFACT_SCHEMA_VERSION};
pub use cache_dir::deps_cache_dir;
pub use cache_dir::{CacheConfig, CacheDir};
pub use derived_cache::{
    DerivedArtifactCache, DerivedCacheGcReport, DerivedCachePolicy, DerivedCacheQueryStats,
    DerivedCacheStats,
};
pub use error::CacheError;
pub use fingerprint::{Fingerprint, ProjectSnapshot};
pub use gc::{
    cache_root, enumerate_project_caches, enumerate_project_caches_from_config, gc_project_caches,
    gc_project_caches_from_config, CacheGcFailure, CacheGcPolicy, CacheGcReport, ProjectCacheInfo,
};
pub use lock::CacheLock;
pub use metadata::{
    CacheMetadata, CacheMetadataArchive, CACHE_METADATA_BIN_FILENAME, CACHE_METADATA_JSON_FILENAME,
    CACHE_METADATA_SCHEMA_VERSION,
};
pub use pack::{
    fetch_cache_package, install_cache_package, pack_cache_package, CachePackageInstallOutcome,
    CACHE_PACKAGE_MANIFEST_PATH,
};
pub use path::{normalize_inputs_map, normalize_rel_path};
pub use prune::{prune_cache, PruneError, PrunePolicy, PruneReport};
pub use query_disk_cache::{QueryDiskCache, QueryDiskCachePolicy, QUERY_DISK_CACHE_SCHEMA_VERSION};
pub use shard_index::{load_shard_index, save_shard_index, shard_cache_path};
pub use store::{store_for_url, CacheStore, HttpStore, LocalStore};
pub use util::{atomic_write, now_millis, BINCODE_PAYLOAD_LIMIT_BYTES};
