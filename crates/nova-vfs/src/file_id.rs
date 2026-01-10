use std::collections::HashMap;

use crate::path::VfsPath;

/// A stable identifier assigned to a file (path/URI).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId(u32);

impl FileId {
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// Allocates stable `FileId`s for paths and supports reverse lookup.
#[derive(Debug, Default)]
pub struct FileIdRegistry {
    path_to_id: HashMap<VfsPath, FileId>,
    id_to_path: Vec<VfsPath>,
}

impl FileIdRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the stable id for `path`, allocating a new one if necessary.
    pub fn file_id(&mut self, path: VfsPath) -> FileId {
        if let Some(&id) = self.path_to_id.get(&path) {
            return id;
        }

        let id = FileId::new(self.id_to_path.len() as u32);
        self.id_to_path.push(path.clone());
        self.path_to_id.insert(path, id);
        id
    }

    /// Returns the id for `path` if it has been interned.
    pub fn get_id(&self, path: &VfsPath) -> Option<FileId> {
        self.path_to_id.get(path).copied()
    }

    /// Returns the path for `id`.
    pub fn get_path(&self, id: FileId) -> Option<&VfsPath> {
        self.id_to_path.get(id.raw() as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_id_is_stable_across_lookups() {
        let mut registry = FileIdRegistry::new();
        let path = VfsPath::uri("file:///tmp/Main.java");
        let id1 = registry.file_id(path.clone());
        let id2 = registry.file_id(path.clone());

        assert_eq!(id1, id2);
        assert_eq!(registry.get_id(&path), Some(id1));
        assert_eq!(registry.get_path(id1), Some(&path));
    }
}

