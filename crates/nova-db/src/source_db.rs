//! Snapshot-safe access to source files.
//!
//! [`crate::Database`] returns borrowed `&str`/`&Path` references. That interface
//! is convenient for simple in-memory stores, but it is a poor fit for Salsa:
//! Salsa inputs are typically stored as `Arc<String>` and snapshots must remain
//! usable even while the main database is being mutated.
//!
//! [`SourceDatabase`] addresses this by returning owned values (`Arc<String>`,
//! `PathBuf`, ...). Production code should prefer this trait when the backing
//! database may be a Salsa snapshot.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::FileId;

/// A minimal, snapshot-friendly database interface.
///
/// Implementations must ensure the returned values remain valid even if the
/// underlying database is mutated concurrently (e.g. Salsa snapshots).
pub trait SourceDatabase {
    /// Return the full text for `file_id`.
    fn file_content(&self, file_id: FileId) -> Arc<String>;

    /// Best-effort file path lookup for `file_id`.
    fn file_path(&self, _file_id: FileId) -> Option<PathBuf> {
        None
    }

    /// Return all file IDs currently known to the database.
    fn all_file_ids(&self) -> Arc<Vec<FileId>> {
        Arc::new(Vec::new())
    }

    /// Look up a `FileId` for an already-known path.
    fn file_id(&self, _path: &Path) -> Option<FileId> {
        None
    }
}
