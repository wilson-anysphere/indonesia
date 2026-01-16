use nova_core::WasmHostDb;
use nova_db::{Database, FileId as DbFileId};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(super) struct SingleFileDb {
    file_id: DbFileId,
    path: Option<PathBuf>,
    text: String,
}

impl SingleFileDb {
    pub(super) fn new(file_id: DbFileId, path: Option<PathBuf>, text: String) -> Self {
        Self {
            file_id,
            path,
            text,
        }
    }
}

impl Database for SingleFileDb {
    fn file_content(&self, file_id: DbFileId) -> &str {
        if file_id == self.file_id {
            self.text.as_str()
        } else {
            ""
        }
    }

    fn file_path(&self, file_id: DbFileId) -> Option<&Path> {
        if file_id == self.file_id {
            self.path.as_deref()
        } else {
            None
        }
    }

    fn all_file_ids(&self) -> Vec<DbFileId> {
        vec![self.file_id]
    }

    fn file_id(&self, path: &Path) -> Option<DbFileId> {
        self.path
            .as_deref()
            .filter(|p| *p == path)
            .map(|_| self.file_id)
    }
}

impl WasmHostDb for SingleFileDb {
    fn file_text(&self, file: DbFileId) -> &str {
        self.file_content(file)
    }

    fn file_path(&self, file: DbFileId) -> Option<&Path> {
        Database::file_path(self, file)
    }
}
