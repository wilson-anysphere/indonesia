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

mod ast_cache;
mod cache_dir;
mod derived_cache;
mod error;
mod fingerprint;
mod metadata;
mod pack;
mod path;
mod shard_index;
mod store;
mod util;

pub use ast_cache::{AstArtifactCache, FileAstArtifacts, AST_ARTIFACT_SCHEMA_VERSION};
pub use cache_dir::deps_cache_dir;
pub use cache_dir::{CacheConfig, CacheDir};
pub use derived_cache::DerivedArtifactCache;
pub use error::CacheError;
pub use fingerprint::{Fingerprint, ProjectSnapshot};
pub use metadata::{CacheMetadata, CACHE_METADATA_SCHEMA_VERSION};
pub use pack::{
    fetch_cache_package, install_cache_package, pack_cache_package, CachePackageInstallOutcome,
    CACHE_PACKAGE_MANIFEST_PATH,
};
pub use path::{normalize_inputs_map, normalize_rel_path};
pub use shard_index::{load_shard_index, save_shard_index, shard_cache_path};
pub use store::{store_for_url, CacheStore, HttpStore, LocalStore};
pub use util::{atomic_write, now_millis, BINCODE_PAYLOAD_LIMIT_BYTES};
