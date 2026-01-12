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
