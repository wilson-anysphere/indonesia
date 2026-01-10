//! Minimal database layer used by `nova-dap`.
//!
//! In the full Nova project this crate would expose a query-based incremental
//! database (likely Salsa-inspired). For now we provide a small in-memory file
//! store that is easy to mock and sufficient for unit testing breakpoint
//! mapping.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId(u32);

impl FileId {
    pub fn raw(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Default)]
pub struct RootDatabase {
    next_file_id: u32,
    path_to_file: HashMap<PathBuf, FileId>,
    files: HashMap<FileId, String>,
}

impl RootDatabase {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn file_id_for_path(&mut self, path: impl AsRef<Path>) -> FileId {
        let path = path.as_ref().to_path_buf();
        if let Some(id) = self.path_to_file.get(&path) {
            return *id;
        }

        let id = FileId(self.next_file_id);
        self.next_file_id = self.next_file_id.saturating_add(1);
        self.path_to_file.insert(path, id);
        id
    }

    pub fn set_file_text(&mut self, file_id: FileId, text: String) {
        self.files.insert(file_id, text);
    }

    pub fn file_text(&self, file_id: FileId) -> Option<&str> {
        self.files.get(&file_id).map(String::as_str)
    }
}

