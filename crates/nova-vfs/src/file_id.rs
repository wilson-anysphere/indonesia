use std::collections::HashMap;
use std::path::Path;

use nova_core::FileId;

use crate::path::VfsPath;

/// Allocates stable `FileId`s for paths and supports reverse lookup.
#[derive(Debug, Default)]
pub struct FileIdRegistry {
    path_to_id: HashMap<VfsPath, FileId>,
    id_to_path: Vec<Option<VfsPath>>,
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
        self.id_to_path.push(Some(path.clone()));
        self.path_to_id.insert(path, id);
        id
    }

    /// Rename (or move) a path, preserving the existing `FileId` when possible.
    ///
    /// If `from` is unknown, this behaves like [`file_id`] on `to`.
    pub fn rename_path(&mut self, from: &VfsPath, to: VfsPath) -> FileId {
        let Some(id_from) = self.path_to_id.remove(from) else {
            return self.file_id(to);
        };

        // If the destination path is already known, keep its id and treat this as a delete + modify.
        if let Some(&id_to) = self.path_to_id.get(&to) {
            if let Some(slot) = self.id_to_path.get_mut(id_from.to_raw() as usize) {
                *slot = None;
            }
            return id_to;
        }

        if let Some(slot) = self.id_to_path.get_mut(id_from.to_raw() as usize) {
            *slot = Some(to.clone());
        }
        self.path_to_id.insert(to, id_from);
        id_from
    }

    /// Rename (or move) a path, preserving the source `FileId` even if the destination path is
    /// already interned.
    pub fn rename_path_displacing_destination(&mut self, from: &VfsPath, to: VfsPath) -> FileId {
        let Some(id_from) = self.path_to_id.remove(from) else {
            return self.file_id(to);
        };

        if let Some(id_to) = self.path_to_id.remove(&to) {
            if id_to != id_from {
                if let Some(slot) = self.id_to_path.get_mut(id_to.to_raw() as usize) {
                    *slot = None;
                }
            }
        }

        if let Some(slot) = self.id_to_path.get_mut(id_from.to_raw() as usize) {
            *slot = Some(to.clone());
        }
        self.path_to_id.insert(to, id_from);
        id_from
    }

    /// Returns all currently-tracked file ids (sorted).
    pub fn all_file_ids(&self) -> Vec<FileId> {
        let mut ids: Vec<_> = self.path_to_id.values().copied().collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// Returns all currently-tracked file ids (unsorted).
    ///
    /// This is intended for callers that will impose their own ordering (or do not require a
    /// stable order) and want to avoid an extra sort.
    pub fn all_file_ids_unsorted(&self) -> Vec<FileId> {
        self.path_to_id.values().copied().collect()
    }

    pub fn for_each_local_path(&self, mut f: impl FnMut(&Path)) {
        for path in self.path_to_id.keys() {
            if let Some(local) = path.as_local_path() {
                f(local);
            }
        }
    }

    /// Returns the id for `path` if it has been interned.
    pub fn get_id(&self, path: &VfsPath) -> Option<FileId> {
        self.path_to_id.get(path).copied()
    }

    /// Returns the path for `id`.
    pub fn get_path(&self, id: FileId) -> Option<&VfsPath> {
        self.id_to_path
            .get(id.to_raw() as usize)
            .and_then(|path| path.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use nova_core::{file_uri_to_path, path_to_file_uri, AbsPathBuf};

    #[test]
    fn file_id_is_stable_across_lookups() {
        let mut registry = FileIdRegistry::new();
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("Main.java");
        let path = VfsPath::local(file_path);
        let id1 = registry.file_id(path.clone());
        let id2 = registry.file_id(path.clone());

        assert_eq!(id1, id2);
        assert_eq!(registry.get_id(&path), Some(id1));
        assert_eq!(registry.get_path(id1), Some(&path));
    }

    #[test]
    fn file_id_is_stable_across_uri_and_path_representations() {
        let mut registry = FileIdRegistry::new();
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("Main.java");
        let abs = AbsPathBuf::new(file_path).unwrap();
        let uri_string = path_to_file_uri(&abs).unwrap();
        let decoded = file_uri_to_path(&uri_string).unwrap().into_path_buf();

        let uri = VfsPath::uri(uri_string);
        let path = VfsPath::local(decoded);

        let id1 = registry.file_id(uri);
        let id2 = registry.file_id(path);

        assert_eq!(id1, id2);
    }

    #[test]
    fn file_id_is_stable_when_local_path_contains_dot_segments() {
        let mut registry = FileIdRegistry::new();
        let dir = tempfile::tempdir().unwrap();
        let normalized_path = dir.path().join("Main.java");
        let abs = AbsPathBuf::new(normalized_path.clone()).unwrap();
        let uri_string = path_to_file_uri(&abs).unwrap();

        let unnormalized_path = dir.path().join("x").join("..").join("Main.java");
        let uri = VfsPath::uri(uri_string);
        let local = VfsPath::local(unnormalized_path);

        let id1 = registry.file_id(uri);
        let id2 = registry.file_id(local);

        assert_eq!(id1, id2);
    }

    #[test]
    fn rename_path_preserves_id() {
        let mut registry = FileIdRegistry::new();
        let dir = tempfile::tempdir().unwrap();
        let from_path = dir.path().join("a.java");
        let to_path = dir.path().join("b.java");
        let from = VfsPath::local(from_path);
        let id = registry.file_id(from.clone());

        let to = VfsPath::local(to_path);
        let moved_id = registry.rename_path(&from, to.clone());

        assert_eq!(id, moved_id);
        assert_eq!(registry.get_id(&from), None);
        assert_eq!(registry.get_id(&to), Some(id));
        assert_eq!(registry.get_path(id), Some(&to));
    }

    #[test]
    fn rename_path_to_existing_path_keeps_destination_id_and_clears_source_path() {
        let mut registry = FileIdRegistry::new();
        let dir = tempfile::tempdir().unwrap();
        let from = VfsPath::local(dir.path().join("a.java"));
        let to = VfsPath::local(dir.path().join("b.java"));
        let from_id = registry.file_id(from.clone());
        let to_id = registry.file_id(to.clone());

        let moved_id = registry.rename_path(&from, to.clone());

        assert_eq!(moved_id, to_id);
        assert_eq!(registry.get_id(&from), None);
        assert_eq!(registry.get_id(&to), Some(to_id));
        assert_eq!(registry.get_path(to_id), Some(&to));
        assert_eq!(registry.get_path(from_id), None);
    }

    #[test]
    fn rename_path_displacing_destination_preserves_source_id() {
        let mut registry = FileIdRegistry::new();
        let dir = tempfile::tempdir().unwrap();
        let from = VfsPath::local(dir.path().join("a.java"));
        let to = VfsPath::local(dir.path().join("b.java"));
        let from_id = registry.file_id(from.clone());
        let to_id = registry.file_id(to.clone());

        let moved_id = registry.rename_path_displacing_destination(&from, to.clone());

        assert_eq!(moved_id, from_id);
        assert_eq!(registry.get_id(&from), None);
        assert_eq!(registry.get_id(&to), Some(from_id));
        assert_eq!(registry.get_path(from_id), Some(&to));
        assert_eq!(registry.get_path(to_id), None);
    }
}
