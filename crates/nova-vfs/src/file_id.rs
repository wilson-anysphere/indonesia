use std::collections::HashMap;

use nova_core::FileId;

use crate::path::VfsPath;

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

        let raw = u32::try_from(self.id_to_path.len()).expect("too many file ids allocated");
        let id = FileId::from_raw(raw);
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
        self.id_to_path.get(id.to_raw() as usize)
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

    #[test]
    fn file_id_is_stable_across_uri_and_path_representations() {
        let mut registry = FileIdRegistry::new();
        let uri = VfsPath::uri("file:///tmp/Main.java");
        let path = VfsPath::local("/tmp/Main.java");

        let id1 = registry.file_id(uri);
        let id2 = registry.file_id(path);

        assert_eq!(id1, id2);
    }
}
