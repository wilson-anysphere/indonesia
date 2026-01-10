//! Persistent cache directory management and versioned on-disk metadata.
//!
//! This crate implements Nova's "hybrid persistence model" building blocks:
//! - per-project cache directories
//! - versioned metadata and input fingerprints
//! - best-effort derived artifact persistence
//! - persisted AST/HIR artifacts for fast warm starts

mod ast_cache;
mod cache_dir;
mod derived_cache;
mod error;
mod fingerprint;
mod metadata;
mod shard_index;
mod util;

pub use ast_cache::{AstArtifactCache, FileAstArtifacts, AST_ARTIFACT_SCHEMA_VERSION};
pub use cache_dir::{CacheConfig, CacheDir};
pub use derived_cache::DerivedArtifactCache;
pub use error::CacheError;
pub use fingerprint::{Fingerprint, ProjectSnapshot};
pub use metadata::{CacheMetadata, CACHE_METADATA_SCHEMA_VERSION};
pub use shard_index::{load_shard_index, save_shard_index, shard_cache_path};
pub use util::{atomic_write, now_millis};

