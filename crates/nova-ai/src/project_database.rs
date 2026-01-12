use std::path::{Path, PathBuf};

use nova_core::ProjectDatabase;

use crate::workspace::VirtualWorkspace;
use std::collections::BTreeMap;

impl ProjectDatabase for VirtualWorkspace {
    fn project_files(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self
            .files()
            .map(|(path, _)| PathBuf::from(path))
            .collect();
        paths.sort();
        paths
    }

    fn file_text(&self, path: &Path) -> Option<String> {
        let key = path.to_str()?;
        self.get(key).map(str::to_string)
    }
}

/// `ProjectDatabase` adapter for `nova_db::Database`.
///
/// This is intentionally a small wrapper rather than an impl on `dyn Database`
/// because both the trait and the type live in other crates (orphan rules).
pub struct DbProjectDatabase<'a> {
    db: &'a dyn nova_db::Database,
    files: BTreeMap<PathBuf, nova_core::FileId>,
}

impl<'a> DbProjectDatabase<'a> {
    pub fn new(db: &'a dyn nova_db::Database) -> Self {
        let mut file_ids = db.all_file_ids();
        file_ids.sort();

        let mut files = BTreeMap::new();
        for file_id in file_ids {
            let Some(path) = db.file_path(file_id) else {
                continue;
            };
            // Keep the first `FileId` we see for a given path so behavior stays deterministic
            // even if multiple ids map to the same `file_path`.
            files.entry(path.to_path_buf()).or_insert(file_id);
        }

        Self { db, files }
    }
}

impl ProjectDatabase for DbProjectDatabase<'_> {
    fn project_files(&self) -> Vec<PathBuf> {
        self.files.keys().cloned().collect()
    }

    fn file_text(&self, path: &Path) -> Option<String> {
        let file_id = self.files.get(path)?;
        Some(self.db.file_content(*file_id).to_string())
    }
}

/// `ProjectDatabase` adapter for `nova_db::SourceDatabase`.
///
/// This is useful for indexing Salsa-backed snapshots directly, without having
/// to go through the legacy `nova_db::Database` trait.
pub struct SourceDbProjectDatabase<'a> {
    db: &'a dyn nova_db::SourceDatabase,
    files: BTreeMap<PathBuf, nova_core::FileId>,
}

impl<'a> SourceDbProjectDatabase<'a> {
    pub fn new(db: &'a dyn nova_db::SourceDatabase) -> Self {
        let file_ids = nova_db::SourceDatabase::all_file_ids(db);
        let mut file_ids = file_ids.as_ref().clone();
        file_ids.sort();

        let mut files = BTreeMap::new();
        for file_id in file_ids {
            let Some(path) = nova_db::SourceDatabase::file_path(db, file_id) else {
                continue;
            };
            // Keep the first `FileId` we see for a given path so behavior stays deterministic.
            files.entry(path).or_insert(file_id);
        }

        Self { db, files }
    }
}

impl ProjectDatabase for SourceDbProjectDatabase<'_> {
    fn project_files(&self) -> Vec<PathBuf> {
        self.files.keys().cloned().collect()
    }

    fn file_text(&self, path: &Path) -> Option<String> {
        let file_id = self.files.get(path)?;
        Some(
            nova_db::SourceDatabase::file_content(self.db, *file_id)
                .as_ref()
                .clone(),
        )
    }
}
