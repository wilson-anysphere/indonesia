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
                    None => Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("virtual document not found ({path})"),
                    )),
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
                    None => Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("virtual document not found ({path})"),
                    )),
                }
            }
            _ => self.base.read_to_string(path),
        }
    }

    fn exists(&self, path: &VfsPath) -> bool {
        match path {
            VfsPath::Decompiled { .. } | VfsPath::LegacyDecompiled { .. } => {
                self.store.contains(path)
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
