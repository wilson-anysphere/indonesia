use std::hash::{BuildHasher, Hash, Hasher};
use std::path::Path;

use hashbrown::HashMap;
use nova_core::FileId;

use crate::path::VfsPath;
use crate::path::{local_path_needs_normalization, normalize_local_path};

/// Allocates stable `FileId`s for paths and supports reverse lookup.
#[derive(Debug, Default)]
pub struct FileIdRegistry {
    path_to_id: HashMap<VfsPath, FileId>,
    id_to_path: Vec<Option<VfsPath>>,
}

impl FileIdRegistry {
    fn local_key_hash(&self, path: &Path) -> u64 {
        let mut hasher = self.path_to_id.hasher().build_hasher();
        std::mem::discriminant(&VfsPath::Local(std::path::PathBuf::new())).hash(&mut hasher);
        path.hash(&mut hasher);
        let hash = hasher.finish();

        #[cfg(debug_assertions)]
        {
            let mut check = self.path_to_id.hasher().build_hasher();
            VfsPath::Local(path.to_path_buf()).hash(&mut check);
            debug_assert_eq!(
                hash,
                check.finish(),
                "local_key_hash must match VfsPath::Local hash"
            );
        }

        hash
    }

    fn normalize_if_needed(&self, path: &VfsPath) -> Option<VfsPath> {
        // Fast path: most callsites construct local paths via `VfsPath::local(..)`, which already
        // applies logical normalization. Avoid allocating a normalized copy on every lookup/insert
        // when we can cheaply determine the path is already normalized.
        if let VfsPath::Local(local) = path {
            if !local_path_needs_normalization(local.as_path()) {
                #[cfg(debug_assertions)]
                debug_assert_eq!(
                    normalize_local_path(local.as_path()),
                    local.as_path(),
                    "local_path_needs_normalization must be conservative"
                );
                return None;
            }
        }

        let normalized = crate::path::normalize_vfs_path(path.clone());
        if &normalized == path {
            None
        } else {
            Some(normalized)
        }
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the stable id for `path`, allocating a new one if necessary.
    pub fn file_id(&mut self, path: VfsPath) -> FileId {
        if let Some(&id) = self.path_to_id.get(&path) {
            return id;
        }

        let path = if let Some(normalized) = self.normalize_if_needed(&path) {
            if let Some(&id) = self.path_to_id.get(&normalized) {
                return id;
            }
            normalized
        } else {
            path
        };

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
        let (from_key, id_from) = if let Some(id_from) = self.path_to_id.remove(from) {
            (from.clone(), id_from)
        } else if let Some(normalized) = self.normalize_if_needed(from) {
            let Some(id_from) = self.path_to_id.remove(&normalized) else {
                return self.file_id(to);
            };
            (normalized, id_from)
        } else {
            return self.file_id(to);
        };

        let to_key = self.normalize_if_needed(&to).unwrap_or_else(|| to.clone());

        // If the rename collapses into the same key (e.g. dot-segment normalization), restore the
        // mapping and keep the original id.
        if from_key == to_key {
            if let Some(slot) = self.id_to_path.get_mut(id_from.to_raw() as usize) {
                *slot = Some(to_key.clone());
            }
            self.path_to_id.insert(to_key, id_from);
            return id_from;
        }

        // If the destination path is already known, keep its id and treat this as a delete + modify.
        if let Some(&id_to) = self
            .path_to_id
            .get(&to)
            .or_else(|| self.path_to_id.get(&to_key))
        {
            if let Some(slot) = self.id_to_path.get_mut(id_from.to_raw() as usize) {
                *slot = None;
            }
            return id_to;
        }

        if let Some(slot) = self.id_to_path.get_mut(id_from.to_raw() as usize) {
            *slot = Some(to_key.clone());
        }
        self.path_to_id.insert(to_key, id_from);
        id_from
    }

    /// Rename (or move) a path, preserving the source `FileId` even if the destination path is
    /// already interned.
    pub fn rename_path_displacing_destination(&mut self, from: &VfsPath, to: VfsPath) -> FileId {
        let (from_key, id_from) = if let Some(id_from) = self.path_to_id.remove(from) {
            (from.clone(), id_from)
        } else if let Some(normalized) = self.normalize_if_needed(from) {
            let Some(id_from) = self.path_to_id.remove(&normalized) else {
                return self.file_id(to);
            };
            (normalized, id_from)
        } else {
            return self.file_id(to);
        };

        let to_key = self.normalize_if_needed(&to).unwrap_or_else(|| to.clone());

        if from_key == to_key {
            if let Some(slot) = self.id_to_path.get_mut(id_from.to_raw() as usize) {
                *slot = Some(to_key.clone());
            }
            self.path_to_id.insert(to_key, id_from);
            return id_from;
        }

        let removed_destination = self
            .path_to_id
            .remove(&to)
            .or_else(|| self.path_to_id.remove(&to_key));
        if let Some(id_to) = removed_destination {
            if id_to != id_from {
                if let Some(slot) = self.id_to_path.get_mut(id_to.to_raw() as usize) {
                    *slot = None;
                }
            }
        }

        if let Some(slot) = self.id_to_path.get_mut(id_from.to_raw() as usize) {
            *slot = Some(to_key.clone());
        }
        self.path_to_id.insert(to_key, id_from);
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

    /// Returns the id for `path` if it is a normalized local path and has been interned.
    ///
    /// Callers must ensure that `path` is already normalized according to `VfsPath::local`
    /// semantics (dot-segments removed, drive letter normalized on Windows).
    pub fn get_local_id_normalized(&self, path: &Path) -> Option<FileId> {
        debug_assert!(
            !local_path_needs_normalization(path),
            "get_local_id_normalized requires a normalized local path"
        );
        let hash = self.local_key_hash(path);
        self.path_to_id
            .raw_entry()
            .from_hash(hash, |candidate| match candidate {
                VfsPath::Local(local) => local == path,
                _ => false,
            })
            .map(|(_, id)| *id)
    }

    /// Returns the id for a local path if it has been interned.
    ///
    /// This is best-effort and purely lexical (aligns with `VfsPath::local` / `normalize_local_path`):
    /// - If `path` is already normalized, this is a single hashmap lookup.
    /// - If `path` contains dot-segments (or a lowercase drive letter on Windows), we normalize and
    ///   retry (allocating only in that case).
    pub fn get_local_id(&self, path: &Path) -> Option<FileId> {
        if local_path_needs_normalization(path) {
            let normalized = normalize_local_path(path);
            return self.get_local_id_normalized(normalized.as_path());
        }
        self.get_local_id_normalized(path)
    }

    /// Returns the id for `path` if it has been interned.
    pub fn get_id(&self, path: &VfsPath) -> Option<FileId> {
        if let Some(id) = self.path_to_id.get(path).copied() {
            return Some(id);
        }
        let normalized = self.normalize_if_needed(path)?;
        self.path_to_id.get(&normalized).copied()
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
        assert_eq!(
            registry.get_local_id_normalized(path.as_local_path().unwrap()),
            Some(id1)
        );
    }

    #[test]
    fn local_id_index_updates_on_rename() {
        let mut registry = FileIdRegistry::new();
        let dir = tempfile::tempdir().unwrap();
        let from = VfsPath::local(dir.path().join("From.java"));
        let to = VfsPath::local(dir.path().join("To.java"));
        let id_from = registry.file_id(from.clone());

        let id_after = registry.rename_path(&from, to.clone());
        assert_eq!(id_after, id_from);
        assert_eq!(
            registry.get_local_id_normalized(from.as_local_path().unwrap()),
            None
        );
        assert_eq!(
            registry.get_local_id_normalized(to.as_local_path().unwrap()),
            Some(id_from)
        );
    }

    #[test]
    fn local_id_index_handles_rename_into_existing_destination() {
        let mut registry = FileIdRegistry::new();
        let dir = tempfile::tempdir().unwrap();
        let from = VfsPath::local(dir.path().join("From.java"));
        let to = VfsPath::local(dir.path().join("To.java"));
        let id_from = registry.file_id(from.clone());
        let id_to = registry.file_id(to.clone());

        let id_after = registry.rename_path(&from, to.clone());
        assert_eq!(id_after, id_to);
        assert_ne!(id_from, id_to);
        assert_eq!(
            registry.get_local_id_normalized(from.as_local_path().unwrap()),
            None
        );
        assert_eq!(
            registry.get_local_id_normalized(to.as_local_path().unwrap()),
            Some(id_to)
        );
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
    fn file_id_is_stable_when_local_variant_is_constructed_directly() {
        let mut registry = FileIdRegistry::new();
        let dir = tempfile::tempdir().unwrap();
        let normalized_path = dir.path().join("Main.java");
        let unnormalized_path = dir.path().join("x").join("..").join("Main.java");

        let direct = VfsPath::Local(unnormalized_path);
        let via_ctor = VfsPath::local(normalized_path);

        let id1 = registry.file_id(direct);
        let id2 = registry.file_id(via_ctor);
        assert_eq!(id1, id2);
    }

    #[test]
    fn file_id_is_stable_when_uri_variant_is_constructed_directly() {
        let mut registry = FileIdRegistry::new();
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("Main.java");
        let abs = AbsPathBuf::new(file_path).unwrap();
        let uri_string = path_to_file_uri(&abs).unwrap();
        let decoded = file_uri_to_path(&uri_string).unwrap().into_path_buf();

        let via_ctor = VfsPath::local(decoded);
        let id = registry.file_id(via_ctor);

        let direct = VfsPath::Uri(uri_string.clone());
        assert_eq!(registry.get_id(&direct), Some(id));

        let id2 = registry.file_id(VfsPath::Uri(uri_string));
        assert_eq!(id2, id);
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
