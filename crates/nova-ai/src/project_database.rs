use std::path::{Path, PathBuf};

use nova_core::ProjectDatabase;

use crate::workspace::VirtualWorkspace;

impl ProjectDatabase for VirtualWorkspace {
    fn project_files(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self
            .files()
            .map(|(path, _)| PathBuf::from(path))
            .collect();
        paths.sort_by(|a, b| a.to_string_lossy().cmp(&b.to_string_lossy()));
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
#[derive(Clone, Copy)]
pub struct DbProjectDatabase<'a> {
    db: &'a dyn nova_db::Database,
}

impl<'a> DbProjectDatabase<'a> {
    pub fn new(db: &'a dyn nova_db::Database) -> Self {
        Self { db }
    }
}

impl ProjectDatabase for DbProjectDatabase<'_> {
    fn project_files(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self
            .db
            .all_file_ids()
            .into_iter()
            .filter_map(|file_id| self.db.file_path(file_id).map(Path::to_path_buf))
            .collect();
        paths.sort_by(|a, b| a.to_string_lossy().cmp(&b.to_string_lossy()));
        paths
    }

    fn file_text(&self, path: &Path) -> Option<String> {
        let file_id = match self.db.file_id(path) {
            Some(file_id) => file_id,
            None => {
                // `Database::file_id` is an optional surface (default impl returns `None`).
                // Fall back to a best-effort reverse lookup so callers can still index/search
                // databases that only expose `all_file_ids` + `file_path`.
                let mut file_ids = self.db.all_file_ids();
                file_ids.sort();
                file_ids
                    .into_iter()
                    .find(|file_id| self.db.file_path(*file_id) == Some(path))?
            }
        };

        Some(self.db.file_content(file_id).to_string())
    }
}
