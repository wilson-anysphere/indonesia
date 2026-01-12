use std::io;

use crate::fs::FileSystem;
use crate::path::VfsPath;
use crate::virtual_documents::VirtualDocumentStore;

/// File-system adapter that serves decompiled virtual documents from a [`VirtualDocumentStore`].
///
/// For non-virtual paths, this delegates to the wrapped `base` file system.
#[derive(Debug, Clone)]
pub struct VirtualDocumentsFs<F: FileSystem> {
    base: F,
    store: VirtualDocumentStore,
}

impl<F: FileSystem> VirtualDocumentsFs<F> {
    pub fn new(base: F, store: VirtualDocumentStore) -> Self {
        Self { base, store }
    }
}

impl<F: FileSystem> FileSystem for VirtualDocumentsFs<F> {
    fn read_bytes(&self, path: &VfsPath) -> io::Result<Vec<u8>> {
        match path {
            VfsPath::Decompiled { .. } | VfsPath::LegacyDecompiled { .. } => {
                match self.store.get_text(path) {
                    Some(text) => Ok(text.as_bytes().to_vec()),
                    None => match self.base.read_to_string(path) {
                        Ok(text) => {
                            // Best-effort: if the store is full, it will evict entries based on
                            // its configured budget.
                            self.store.insert_text(path.clone(), text.clone());
                            Ok(text.into_bytes())
                        }
                        Err(err) if err.kind() == io::ErrorKind::Unsupported => Err(
                            io::Error::new(
                                io::ErrorKind::NotFound,
                                format!("virtual document not found ({path})"),
                            ),
                        ),
                        Err(err) => Err(err),
                    },
                }
            }
            _ => self.base.read_bytes(path),
        }
    }

    fn read_to_string(&self, path: &VfsPath) -> io::Result<String> {
        match path {
            VfsPath::Decompiled { .. } | VfsPath::LegacyDecompiled { .. } => {
                match self.store.get_text(path) {
                    Some(text) => Ok(text.to_string()),
                    None => match self.base.read_to_string(path) {
                        Ok(text) => {
                            // Best-effort: if the store is full, it will evict entries based on
                            // its configured budget.
                            self.store.insert_text(path.clone(), text.clone());
                            Ok(text)
                        }
                        Err(err) if err.kind() == io::ErrorKind::Unsupported => Err(
                            io::Error::new(
                                io::ErrorKind::NotFound,
                                format!("virtual document not found ({path})"),
                            ),
                        ),
                        Err(err) => Err(err),
                    },
                }
            }
            _ => self.base.read_to_string(path),
        }
    }

    fn exists(&self, path: &VfsPath) -> bool {
        match path {
            VfsPath::Decompiled { .. } | VfsPath::LegacyDecompiled { .. } => {
                self.store.contains(path) || self.base.exists(path)
            }
            _ => self.base.exists(path),
        }
    }

    fn metadata(&self, path: &VfsPath) -> io::Result<std::fs::Metadata> {
        match path {
            VfsPath::Decompiled { .. } | VfsPath::LegacyDecompiled { .. } => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("virtual document metadata not supported ({path})"),
            )),
            _ => self.base.metadata(path),
        }
    }

    fn read_dir(&self, path: &VfsPath) -> io::Result<Vec<VfsPath>> {
        match path {
            VfsPath::Decompiled { .. } | VfsPath::LegacyDecompiled { .. } => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("virtual document directory listing not supported ({path})"),
            )),
            _ => self.base.read_dir(path),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::*;

    const HASH_64: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[derive(Clone, Debug)]
    struct MockFs {
        path: VfsPath,
        text: String,
        reads: Arc<AtomicUsize>,
    }

    impl MockFs {
        fn new(path: VfsPath, text: String) -> Self {
            Self {
                path,
                text,
                reads: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn reads(&self) -> usize {
            self.reads.load(Ordering::SeqCst)
        }
    }

    impl FileSystem for MockFs {
        fn read_to_string(&self, path: &VfsPath) -> io::Result<String> {
            if path == &self.path {
                self.reads.fetch_add(1, Ordering::SeqCst);
                return Ok(self.text.clone());
            }
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("missing ({path})"),
            ))
        }

        fn exists(&self, path: &VfsPath) -> bool {
            path == &self.path
        }

        fn metadata(&self, path: &VfsPath) -> io::Result<std::fs::Metadata> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("metadata not supported ({path})"),
            ))
        }

        fn read_dir(&self, path: &VfsPath) -> io::Result<Vec<VfsPath>> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("read_dir not supported ({path})"),
            ))
        }
    }

    #[derive(Clone, Debug)]
    struct UnsupportedFs;

    impl FileSystem for UnsupportedFs {
        fn read_to_string(&self, path: &VfsPath) -> io::Result<String> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unsupported ({path})"),
            ))
        }

        fn exists(&self, _path: &VfsPath) -> bool {
            false
        }

        fn metadata(&self, path: &VfsPath) -> io::Result<std::fs::Metadata> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("metadata not supported ({path})"),
            ))
        }

        fn read_dir(&self, path: &VfsPath) -> io::Result<Vec<VfsPath>> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("read_dir not supported ({path})"),
            ))
        }
    }

    #[test]
    fn falls_back_to_base_fs_and_caches_decompiled_text() {
        let path = VfsPath::decompiled(HASH_64, "com.example.Foo");
        let base = MockFs::new(path.clone(), "class Foo {}".to_string());
        let store = VirtualDocumentStore::new(1024);
        let fs = VirtualDocumentsFs::new(base.clone(), store.clone());

        // Cache miss should fall back to base.
        assert_eq!(fs.read_to_string(&path).unwrap(), "class Foo {}");
        assert!(store.contains(&path));
        assert_eq!(base.reads(), 1);

        // Cache hit should not consult the base again.
        assert_eq!(fs.read_to_string(&path).unwrap(), "class Foo {}");
        assert_eq!(base.reads(), 1);
    }

    #[test]
    fn maps_base_unsupported_to_not_found_for_decompiled_paths() {
        let path = VfsPath::decompiled(HASH_64, "com.example.Foo");
        let store = VirtualDocumentStore::new(1024);
        let fs = VirtualDocumentsFs::new(UnsupportedFs, store);

        let err = fs.read_to_string(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
