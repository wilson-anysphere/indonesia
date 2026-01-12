//! Virtual document support for decompiled virtual documents.
//!
//! Nova historically exposed decompiled stubs using the legacy
//! `nova-decompile:///...` URI scheme. Under ADR0006, decompiled virtual
//! documents use canonical `nova:///decompiled/<hash>/<binary-name>.java` URIs.
//!
//! The full Nova LSP implementation is still evolving; this module provides
//! reusable helpers for exposing decompiled, read-only documents to editors.

use nova_cache::DerivedArtifactCache;
use nova_decompile::{
    class_internal_name_from_uri, decompile_classfile, decompile_classfile_cached,
    parse_decompiled_uri, DECOMPILE_URI_SCHEME,
};
use std::io;

/// A source of `.class` bytes for a given internal path (e.g. `com/example/Foo.class`).
pub trait ClassfileProvider {
    fn read_classfile(&self, internal_path: &str) -> io::Result<Option<Vec<u8>>>;
}

/// Returns whether the URI refers to a legacy decompiled virtual document
/// (`nova-decompile:///...`).
pub fn is_decompile_uri(uri: &str) -> bool {
    debug_assert_eq!(DECOMPILE_URI_SCHEME, "nova-decompile");
    class_internal_name_from_uri(uri).is_some()
}

/// Returns whether the URI refers to a canonical ADR0006 decompiled virtual
/// document (`nova:///decompiled/<hash>/<binary-name>.java`).
pub fn is_canonical_decompiled_uri(uri: &str) -> bool {
    parse_decompiled_uri(uri).is_some()
}

/// Returns whether the URI refers to any decompiled virtual document (canonical
/// ADR0006 or legacy).
pub fn is_decompiled_virtual_uri(uri: &str) -> bool {
    is_decompile_uri(uri) || is_canonical_decompiled_uri(uri)
}

/// Returns whether the given URI should be treated as read-only by the editor.
pub fn is_read_only_uri(uri: &str) -> bool {
    is_decompiled_virtual_uri(uri)
}

/// Loads the virtual document content for a legacy `nova-decompile:///...` URI.
///
/// `provider` is responsible for mapping the internal path to actual bytes (jar, filesystem, etc).
pub fn load_decompiled_document(
    uri: &str,
    provider: &dyn ClassfileProvider,
    cache: Option<&DerivedArtifactCache>,
) -> io::Result<Option<String>> {
    let internal_name = match class_internal_name_from_uri(uri) {
        Some(name) => name,
        None => return Ok(None),
    };
    let internal_path = format!("{internal_name}.class");
    let Some(bytes) = provider.read_classfile(&internal_path)? else {
        return Ok(None);
    };

    let decompiled = match cache {
        Some(cache) => decompile_classfile_cached(&bytes, cache)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
        None => decompile_classfile(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
    };
    Ok(Some(decompiled.text))
}

#[cfg(test)]
mod tests {
    use super::*;

    use nova_decompile::DecompiledDocumentStore;
    use nova_vfs::{FileSystem, Vfs, VfsPath};
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tempfile::TempDir;

    const FOO_CLASS: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../nova-decompile/tests/fixtures/com/example/Foo.class"
    ));
    const FOO_INTERNAL_NAME: &str = "com/example/Foo";
    const HASH_64: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn read_only_uri_matches_canonical_decompiled_uri() {
        let uri = nova_decompile::decompiled_uri_for_classfile(FOO_CLASS, FOO_INTERNAL_NAME);
        assert!(is_read_only_uri(&uri));
    }

    #[test]
    fn read_only_uri_matches_legacy_decompile_uri() {
        let uri = nova_decompile::uri_for_class_internal_name(FOO_INTERNAL_NAME);
        assert!(is_read_only_uri(&uri));
    }

    #[test]
    fn read_only_uri_matches_legacy_decompile_uri_single_slash_form() {
        let uri = "nova-decompile:/com/example/Foo.class";
        assert!(is_read_only_uri(uri));
    }

    #[test]
    fn read_only_uri_does_not_match_unrelated_nova_virtual_uri() {
        assert!(!is_read_only_uri("nova:///something/else"));
    }

    #[test]
    fn file_uris_are_not_read_only() {
        let uri = "file:///tmp/Main.java";
        assert!(!is_read_only_uri(uri));
    }

    #[test]
    fn persisted_decompiled_documents_can_be_read_via_vfs_without_in_memory_cache() {
        let _lock = nova_test_utils::env_lock();
        let cache_dir = TempDir::new().expect("cache dir");
        let _cache_guard = nova_test_utils::EnvVarGuard::set("NOVA_CACHE_DIR", cache_dir.path());

        let store =
            Arc::new(DecompiledDocumentStore::from_env().expect("open decompiled store from env"));

        let binary_name = "com.example.Persisted";
        let text = "package com.example;\n\nclass Persisted {}\n".to_string();

        store
            .store_text(HASH_64, binary_name, &text)
            .expect("store_text");

        #[derive(Clone, Debug)]
        struct StoreFs {
            store: Arc<DecompiledDocumentStore>,
            reads: Arc<AtomicUsize>,
        }

        impl FileSystem for StoreFs {
            fn read_to_string(&self, path: &VfsPath) -> io::Result<String> {
                match path {
                    VfsPath::Decompiled {
                        content_hash,
                        binary_name,
                    } => {
                        self.reads.fetch_add(1, Ordering::SeqCst);
                        match self.store.load_text(content_hash, binary_name) {
                            Ok(Some(text)) => Ok(text),
                            Ok(None) => Err(io::Error::new(
                                io::ErrorKind::NotFound,
                                format!("decompiled document not found: {path}"),
                            )),
                            Err(err) => Err(io::Error::new(io::ErrorKind::Other, err)),
                        }
                    }
                    _ => Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        format!("unsupported: {path}"),
                    )),
                }
            }

            fn exists(&self, path: &VfsPath) -> bool {
                match path {
                    VfsPath::Decompiled {
                        content_hash,
                        binary_name,
                    } => self.store.exists(content_hash, binary_name),
                    _ => false,
                }
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

        let reads = Arc::new(AtomicUsize::new(0));
        let fs = StoreFs {
            store,
            reads: reads.clone(),
        };
        let vfs = Vfs::new(fs);

        let uri = format!("nova:///decompiled/{HASH_64}/{binary_name}.java");
        let path = VfsPath::uri(uri);

        // Ensure the VFS can warm-start by reading from the underlying FS (which reads from the
        // persistent decompiled document store) even if the in-memory cache is empty.
        assert_eq!(vfs.read_to_string(&path).unwrap(), text);
        assert_eq!(reads.load(Ordering::SeqCst), 1);

        // Follow-up reads should hit the in-memory cache instead of consulting the base FS again.
        assert_eq!(vfs.read_to_string(&path).unwrap(), text);
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }
}
