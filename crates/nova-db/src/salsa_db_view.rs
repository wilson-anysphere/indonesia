//! Compatibility adapter for running legacy `nova_db::Database` code on Salsa.
//!
//! Salsa snapshots naturally return owned values (`Arc<String>` inputs, `Arc<T>`
//! memoized results, ...). The legacy [`crate::Database`] trait returns borrowed
//! `&str`/`&Path` references, which is hard to implement correctly on top of
//! Salsa without leaking or using unsafe code.
//!
//! [`SalsaDbView`] bridges the gap by eagerly caching snapshot-owned values for
//! the lifetime of the view. This makes it safe to hand out borrowed references
//! while keeping the underlying `Arc`s alive.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nova_core::ProjectDatabase;

use crate::{Database, FileId, SourceDatabase};

/// A `Send + Sync` view over a Salsa snapshot that implements the legacy
/// [`crate::Database`] trait.
///
/// The view eagerly caches the snapshot's file contents and file paths so that
/// `&str`/`&Path` references remain valid for the lifetime of the view.
#[derive(Debug, Clone)]
pub struct SalsaDbView {
    file_contents: HashMap<FileId, Arc<String>>,
    file_paths: HashMap<FileId, PathBuf>,
    file_ids: Vec<FileId>,
    path_to_file: HashMap<PathBuf, FileId>,
}

impl SalsaDbView {
    /// Build a new view by snapshotting and caching all known file metadata.
    pub fn new(snapshot: crate::salsa::Snapshot) -> Self {
        Self::from_source_db(&snapshot)
    }

    /// Build a new view from any [`SourceDatabase`].
    ///
    /// This is primarily useful for adapting Salsa snapshots, but it can also be
    /// used to wrap other `SourceDatabase` implementations.
    pub fn from_source_db(db: &dyn SourceDatabase) -> Self {
        let file_ids_arc = SourceDatabase::all_file_ids(db);
        let file_ids: Vec<FileId> = file_ids_arc.as_ref().clone();

        let mut file_contents = HashMap::with_capacity(file_ids.len());
        let mut file_paths = HashMap::new();
        let mut path_to_file = HashMap::new();

        for file_id in &file_ids {
            let content = SourceDatabase::file_content(db, *file_id);
            file_contents.insert(*file_id, content);

            if let Some(path) = SourceDatabase::file_path(db, *file_id) {
                path_to_file.insert(path.clone(), *file_id);
                file_paths.insert(*file_id, path);
            }
        }

        Self {
            file_contents,
            file_paths,
            file_ids,
            path_to_file,
        }
    }
}

impl Database for SalsaDbView {
    fn file_content(&self, file_id: FileId) -> &str {
        self.file_contents
            .get(&file_id)
            .map(|text| text.as_str())
            .unwrap_or("")
    }

    fn file_path(&self, file_id: FileId) -> Option<&Path> {
        self.file_paths.get(&file_id).map(PathBuf::as_path)
    }

    fn all_file_ids(&self) -> Vec<FileId> {
        self.file_ids.clone()
    }

    fn file_id(&self, path: &Path) -> Option<FileId> {
        self.path_to_file.get(path).copied()
    }
}

impl SourceDatabase for SalsaDbView {
    fn file_content(&self, file_id: FileId) -> Arc<String> {
        self.file_contents
            .get(&file_id)
            .cloned()
            .unwrap_or_else(|| Arc::new(String::new()))
    }

    fn file_path(&self, file_id: FileId) -> Option<PathBuf> {
        self.file_paths.get(&file_id).cloned()
    }

    fn all_file_ids(&self) -> Arc<Vec<FileId>> {
        Arc::new(self.file_ids.clone())
    }

    fn file_id(&self, path: &Path) -> Option<FileId> {
        self.path_to_file.get(path).copied()
    }
}

impl ProjectDatabase for SalsaDbView {
    fn project_files(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self.file_paths.values().cloned().collect();
        paths.sort();
        paths.dedup();
        paths
    }

    fn file_text(&self, path: &Path) -> Option<String> {
        let file_id = Database::file_id(self, path)?;
        Some(Database::file_content(self, file_id).to_string())
    }
}
